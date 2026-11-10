//! Waiting instead of failing: an admission stage before any provider call.
//!
//! `on_client` may await, so a request parks in a queue until a slot frees
//! or its deadline passes, then gets 429 with `Retry-After`. Paid requests
//! (`x-tier: paid`) go first and wait longer.
//!
//! Within a priority, end users share fairly: the queue is round-robin over
//! users (`x-user` here; your auth plugin in practice), each with its own
//! first-come queue. A weight gives a user several turns per round; equal
//! weights give plain round-robin.
//!
//! The slot is held until the request ends, its response included, and
//! released in `on_client_end`: core keeps a request's context until its
//! end hooks run. A request that leaves while parked just drops out.
//!
//! The queue here is in memory, for one process. To share capacity across
//! processes, keep it in a store they share, with leases that expire when a
//! holder stops renewing them.
//!
//! Run it with `cargo run -p respan-gateway-examples --example admission`.
//! With one slot held, it parks six requests, then frees the slot: look for
//! the paid request first, then `carol` (weight 2) taking two turns for each
//! of `bob`'s. Last, a free request waits out its deadline and gets 429.

use parking_lot::Mutex;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use mockllm::FakeOpenAi;
use respan_gateway_conversation::Input;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Gateway, Plugin, Provider, Response, RetryAfter, async_trait,
};
use respan_gateway_http::Body;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::raw::Original;
use respan_gateway_http_faults::Status;
use respan_gateway_http_transport::HttpTransport;
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep};

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let fake = FakeOpenAi::start().await;
    let slots = Slots::new(1).weight("carol", 2);
    let gateway = gateway(&fake.base_url(), slots)?;

    // The only slot is held while this response is unread.
    println!("alice (paid) takes the only slot");
    let alice = respan_gateway_http::handle(&gateway, request("alice", "paid"), LIMIT).await;

    println!("parking: bob, bob, bob, carol, carol (free), then dave (paid)");
    let mut parked = Vec::new();
    for (user, tier) in [
        ("bob", "free"),
        ("bob", "free"),
        ("bob", "free"),
        ("carol", "free"),
        ("carol", "free"),
        ("dave", "paid"),
    ] {
        parked.push(tokio::spawn(send(gateway.clone(), user, tier)));
        // Let each park before the next, so arrival order is fixed.
        sleep(Duration::from_millis(20)).await;
    }

    println!("alice's response is read; her slot frees");
    alice.0.into_body().collect().await?;
    for request in parked {
        request
            .await
            .map_err(|e| Fault::internal("a request panicked").with_source(e))??;
    }

    println!("\nalice holds the slot again; erin (free) waits out her deadline");
    let alice = respan_gateway_http::handle(&gateway, request("alice", "paid"), LIMIT).await;
    let started = Instant::now();
    let erin = respan_gateway_http::handle(&gateway, request("erin", "free"), LIMIT).await;
    let retry = erin.0.headers().get(RETRY_AFTER).cloned();
    println!(
        "erin: HTTP {} after {} ms, retry-after {retry:?}",
        erin.0.status(),
        started.elapsed().as_millis()
    );
    alice.0.into_body().collect().await?;
    Ok(())
}

// The admission plugin.

/// Admits each request into `slots`: `x-tier: paid` first, waiting up to
/// `paid`; others up to `free`.
struct Admission {
    slots: Slots,
    paid: Duration,
    free: Duration,
}

/// The request's slot, given back at its end.
#[derive(Clone)]
struct Held(Arc<Mutex<Option<Slot>>>);

#[async_trait]
impl Plugin for Admission {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let asked = cx.extensions().get::<Original>().and_then(Original::get);
        let header = |name| asked.and_then(|a| a.headers().get(name)?.to_str().ok());
        let paid = header("x-tier") == Some("paid");
        let user = header("x-user").unwrap_or_default().to_owned();
        let (priority, wait) = if paid { (1, self.paid) } else { (0, self.free) };

        let Some(slot) = self
            .slots
            .acquire(priority, &user, Instant::now() + wait)
            .await
        else {
            let fault = Fault::new("every slot is busy; retry later")
                .with(ErrorCode("busy".into()))
                .with(Status(StatusCode::TOO_MANY_REQUESTS))
                .with(RetryAfter(Duration::from_secs(1)));
            return Err(fault);
        };
        println!("  admitted {user}{}", if paid { " (paid)" } else { "" });
        cx.extensions_mut()
            .insert(Held(Arc::new(Mutex::new(Some(slot)))));
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, _end: &Result<(), Fault>) {
        if let Some(Held(slot)) = cx.extensions().get::<Held>() {
            // Dropping the slot hands it to the next in line.
            drop(slot.lock().take());
        }
    }
}

// The queue.

/// A fixed number of slots, with a weighted fair queue for them. Clones
/// share it.
#[derive(Clone)]
struct Slots {
    queue: Arc<Mutex<Queue>>,
    weights: Arc<HashMap<String, u32>>,
}

#[derive(Default)]
struct Queue {
    free: usize,
    /// Highest priority first.
    levels: BTreeMap<Reverse<u8>, Level>,
}

/// One priority's waiters: users in round-robin order, each with its own
/// first-come queue.
#[derive(Default)]
struct Level {
    order: VecDeque<String>,
    users: HashMap<String, User>,
}

struct User {
    waiting: VecDeque<oneshot::Sender<Slot>>,
    weight: u32,
    turns: u32,
}

/// One slot, held until dropped.
struct Slot(Option<Slots>);

impl Slots {
    fn new(capacity: usize) -> Self {
        let queue = Queue {
            free: capacity,
            ..Queue::default()
        };
        Self {
            queue: Arc::new(Mutex::new(queue)),
            weights: Arc::default(),
        }
    }

    /// Give `user` this many turns in a row before the next user. Priority
    /// still comes first.
    fn weight(mut self, user: &str, weight: u32) -> Self {
        Arc::make_mut(&mut self.weights).insert(user.into(), weight.max(1));
        self
    }

    /// A slot, waiting for one until `deadline`; none if it passes.
    async fn acquire(&self, priority: u8, user: &str, deadline: Instant) -> Option<Slot> {
        let (send, slot) = oneshot::channel();
        let weight = self.weights.get(user).copied().unwrap_or(1);
        self.queue.lock().park(priority, user, weight, send);
        self.dispatch();
        // A slot sent just as the deadline passes is dropped with the
        // receiver, and so freed again.
        tokio::time::timeout_at(deadline, slot).await.ok()?.ok()
    }

    /// Hand free slots to the next waiters. A waiter that left is skipped
    /// and its slot taken back.
    fn dispatch(&self) {
        loop {
            let next = {
                let mut queue = self.queue.lock();
                if queue.free == 0 {
                    return;
                }
                let Some(next) = queue.next() else { return };
                queue.free -= 1;
                next
            };
            if let Err(mut gone) = next.send(Slot(Some(self.clone()))) {
                gone.0 = None;
                self.queue.lock().free += 1;
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(slots) = self.0.take() {
            slots.queue.lock().free += 1;
            slots.dispatch();
        }
    }
}

impl Queue {
    fn park(&mut self, priority: u8, user: &str, weight: u32, send: oneshot::Sender<Slot>) {
        let level = self.levels.entry(Reverse(priority)).or_default();
        let user = level.users.entry(user.to_owned()).or_insert_with(|| {
            level.order.push_back(user.to_owned());
            User {
                waiting: VecDeque::new(),
                weight,
                turns: weight,
            }
        });
        user.waiting.push_back(send);
    }

    /// The next waiter: the highest priority, then the user whose turn it
    /// is. A user who spent their turns goes to the back of the round.
    fn next(&mut self) -> Option<oneshot::Sender<Slot>> {
        let level = self.levels.values_mut().find(|l| !l.order.is_empty())?;
        let name = level.order.pop_front()?;
        let user = level.users.get_mut(&name)?;
        let next = user.waiting.pop_front();
        user.turns -= 1;
        if user.waiting.is_empty() {
            level.users.remove(&name);
        } else if user.turns == 0 {
            user.turns = user.weight;
            level.order.push_back(name);
        } else {
            level.order.push_front(name);
        }
        next
    }
}

// The gateway and a client.

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

fn gateway(base: &str, slots: Slots) -> Result<Gateway, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(base)?.with_headers(headers)?.connect();
    let admission = Admission {
        slots,
        paid: Duration::from_secs(5),
        free: Duration::from_secs(1),
    };
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, transport))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(admission)
        .target("openai")
}

fn request(user: &'static str, tier: &'static str) -> http::Request<Body> {
    let body = r#"{"model":"gpt-x","messages":[{"role":"user","content":"hi"}]}"#;
    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    let headers = req.headers_mut();
    headers.insert("x-user", HeaderValue::from_static(user));
    headers.insert("x-tier", HeaderValue::from_static(tier));
    req
}

/// Send a request and read its response to the end.
async fn send(gateway: Gateway, user: &'static str, tier: &'static str) -> Result<(), Fault> {
    let (resp, _) = respan_gateway_http::handle(&gateway, request(user, tier), LIMIT).await;
    resp.into_body().collect().await?;
    Ok(())
}
