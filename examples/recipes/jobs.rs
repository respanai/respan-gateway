//! Durable idempotency: a request retried with the same `Idempotency-Key`
//! gets the first run's result instead of a second provider call.
//!
//! The first request with a key claims it in SQLite and runs; what it sent
//! the client is stored under a hash of the tenant's credentials, the key
//! and the request body. So the same key with another body runs on its
//! own, and replaying another tenant's result takes its credentials, not
//! just its key. A retry gets the stored result; a duplicate sent while
//! the first still runs waits for it.
//!
//! A claim is a lease that its run renews while it is live. If the process
//! dies mid-run, the lease lapses and the next request with the key takes
//! the claim over and runs again. A run that lost its lease cannot store a
//! result over its successor's. Every run of a key sends the provider the
//! same `Idempotency-Key`, so a provider that honors it does the work once
//! even across a crash; exactly-once remote execution needs that support.
//!
//! The gateway cannot keep a call running once its client left: a request
//! that ends, a disconnect included, ends its calls. A cut run stores
//! nothing and releases its key, so the retry runs again rather than
//! replaying half an answer. `background_jobs` runs work that outlives
//! the connection.
//!
//! Run it with `cargo run -p respan-gateway-examples --example jobs`.
//! Look for the provider call count: it rises only for a new key, a
//! crashed claim taken over, never for a retry, a duplicate or a retry
//! after restart. The SQLite file lives in the temp dir and is removed.

use mockllm::{Answer, FakeOpenAi};
use parking_lot::Mutex;
use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::futures::{future::join, stream};
use respan_gateway_core::{
    ClientContext, Extensions, Fault, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, header::AUTHORIZATION};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, Credential, ProviderIo, raw::Original};
use respan_gateway_http_transport::HttpTransport;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

/// The most bytes of one message the adapter and transports hold.
const LIMIT: usize = 32 << 20;
/// How long a claim holds without renewal.
const LEASE: Duration = Duration::from_secs(30);
const ACME: &str = "Bearer acme-key";
const BODY: &str = r#"{"model":"gpt-6-luna","messages":[{"role":"user","content":"hi"}]}"#;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let path = std::env::temp_dir().join(format!("respan-jobs-{}.sqlite3", uuid::Uuid::new_v4()));
    let result = demo(&path).await;
    for suffix in ["", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    result
}

async fn demo(path: &Path) -> Result<(), Fault> {
    let provider = FakeOpenAi::start().await;
    let calls = || provider.requests().len();
    let store = Store::open(path, LEASE)?;
    let gw = gateway(&provider, store.clone())?;

    provider.answer(Answer::text("Hello world"));
    let first = ask(&gw, "op-1").await?;
    println!("first op-1:  {first}   (provider calls: {})", calls());
    println!(
        "retry op-1:  {}   (provider calls: {})",
        ask(&gw, "op-1").await?,
        calls()
    );

    // Two at once: one runs, the other waits for its result.
    provider.answer(Answer::text("Two at once").delay_first(300));
    let (a, b) = join(ask(&gw, "op-2"), ask(&gw, "op-2")).await;
    println!(
        "twin op-2:   {} / {}   (provider calls: {})",
        a?,
        b?,
        calls()
    );

    // Another process claims op-3 with a short lease and dies mid-run.
    let crashed = Store::open(path, Duration::from_millis(200))?;
    let key = operation_key(Some(&credential(ACME)), "op-3", BODY.as_bytes());
    crashed.claim(&key).await?;
    drop(crashed);
    provider.answer(Answer::text("Recovered"));
    let started = Instant::now();
    let answer = ask(&gw, "op-3").await?;
    println!(
        "crash op-3:  {answer}   (took the claim over after {} ms; provider calls: {})",
        started.elapsed().as_millis(),
        calls()
    );

    // Restart: a new store on the same file still has op-1's result.
    drop(gw);
    drop(store);
    let gw = gateway(&provider, Store::open(path, LEASE)?)?;
    println!(
        "after restart, retry op-1:  {}   (provider calls: {})",
        ask(&gw, "op-1").await?,
        calls()
    );

    let upstream: Vec<_> = provider.requests();
    let keys: Vec<_> = upstream
        .iter()
        .filter_map(|r| r.headers.get("idempotency-key")?.to_str().ok())
        .map(|k| &k[..12])
        .collect();
    println!("Idempotency-Key the provider got per call (prefix): {keys:?}");
    Ok(())
}

fn gateway(provider: &FakeOpenAi, store: Store) -> Result<Gateway, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(provider.base_url())?.with_headers(headers)?;
    Gateway::new()
        .provider(Provider::new(
            "openai",
            OpenAiChat,
            keyed_transport(transport),
        ))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(Idempotent(store))
        .target("openai")
}

/// The transport, sending each claimed run's key upstream as its
/// `Idempotency-Key`, the same on every retry of the operation.
fn keyed_transport(transport: HttpTransport) -> impl Fn(Extensions) -> ProviderIo + Send + Sync {
    respan_gateway_http::connect(
        move |mut call: http::Request<Body>| {
            let transport = transport.clone();
            let running = call
                .extensions()
                .get::<Extensions>()
                .and_then(|extensions| extensions.get::<Running>())
                .map(|running| running.key.clone());
            async move {
                if let Some(key) = running {
                    let key =
                        HeaderValue::from_str(&key).map_err(|e| Fault::internal(e.to_string()))?;
                    call.headers_mut().insert("idempotency-key", key);
                }
                transport.send(call).await
            }
        },
        LIMIT,
    )
}

/// One request from tenant ACME with `key`; the answer's text.
async fn ask(gw: &Gateway, key: &'static str) -> Result<String, Fault> {
    let mut req = http::Request::new(Body::full(BODY));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    req.headers_mut()
        .insert(AUTHORIZATION, HeaderValue::from_static(ACME));
    req.headers_mut()
        .insert("idempotency-key", HeaderValue::from_static(key));
    let (resp, _end) = respan_gateway_http::handle(gw, req, LIMIT).await;
    let body = BodyExt::collect(resp.into_body()).await?.to_bytes();
    gw.idle().await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    Ok(format!(
        "{:?}",
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("?")
    ))
}

fn credential(token: &'static str) -> Credential {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static(token));
    Credential::of(headers)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The stored key: a hash of the tenant's every credential, the client's
/// key and the body as the client sent it, which is stable across
/// restarts and versions, unlike any in-memory form.
fn operation_key(credential: Option<&Credential>, key: &str, body: &[u8]) -> String {
    let tenant = credential.map(|credential| {
        let mut headers: Vec<_> = credential
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
            .collect();
        headers.sort();
        (headers, credential.query().collect::<Vec<_>>())
    });
    let tenant = serde_json::to_vec(&tenant).unwrap_or_default();
    digest(&serde_json::to_vec(&(digest(&tenant), key, digest(body))).unwrap_or_default())
}

/// The claimed run this request is: its key, its owner token, the replies
/// sent so far, and the switch that stops its lease renewal.
#[derive(Clone)]
struct Running {
    key: String,
    owner: String,
    sent: Arc<Mutex<Vec<Output>>>,
    renewing: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

/// Runs each keyed chat request once. Register it before any plugin that
/// edits the request or takes the [`Credential`] off.
struct Idempotent(Store);

#[async_trait]
impl Plugin for Idempotent {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Some(asked) = cx.extensions().get::<Original>().and_then(Original::get) else {
            return Ok(None);
        };
        let header = asked
            .headers()
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok());
        let Some(header) = header.filter(|_| cx.is_kind::<Input>()) else {
            return Ok(None);
        };
        let key = operation_key(cx.extensions().get::<Credential>(), header, asked.body());

        let owner = match self.0.claim(&key).await? {
            Claim::Done(replies) => {
                return Ok(Some(Response::stream(stream::iter(
                    replies.into_iter().map(Ok),
                ))));
            }
            Claim::Run(owner) => owner,
        };
        let (renewing, mut stopped) = oneshot::channel();
        let running = Running {
            key: key.clone(),
            owner: owner.clone(),
            sent: Arc::default(),
            renewing: Arc::new(Mutex::new(Some(renewing))),
        };
        // In the extensions first, so `on_client_end` settles it whatever follows.
        cx.extensions_mut().insert(running.clone());

        // Renew the lease while the run is live; a crash stops renewing.
        let store = self.0.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => return,
                    _ = tokio::time::sleep(store.lease / 3) => {
                        if !store.renew(&key, &owner).await.unwrap_or(false) {
                            return;
                        }
                    }
                }
            }
        });
        cx.filter(move |reply: &mut Output| {
            running.sent.lock().push(reply.clone());
            Ok(())
        })?;
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, end: &Result<(), Fault>) {
        let Some(running) = cx.extensions().get::<Running>() else {
            return;
        };
        running.renewing.lock().take();
        let replies = end.is_ok().then(|| running.sent.lock().clone());
        let _ = self.0.finish(&running.key, &running.owner, replies).await;
    }
}

/// What a request with a key does: run it, or replay its result.
enum Claim {
    Run(String),
    Done(Vec<Output>),
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS idempotency (
    key TEXT PRIMARY KEY, owner TEXT NOT NULL, lease INTEGER NOT NULL, result BLOB
);";

/// Claims and results in SQLite. One connection per process; SQLite
/// coordinates processes. Stored results have no expiry here.
#[derive(Clone)]
struct Store {
    db: Arc<Mutex<Connection>>,
    lease: Duration,
}

fn sql(error: rusqlite::Error) -> Fault {
    Fault::internal("the store could not complete its transaction").with_source(error)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl Store {
    fn open(path: &Path, lease: Duration) -> Result<Self, Fault> {
        let db = Connection::open(path).map_err(sql)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            lease,
        })
    }

    /// Run `work` in one immediate transaction, off the async threads.
    async fn transact<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, Fault> + Send + 'static,
    ) -> Result<T, Fault> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut db = db.lock();
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql)?;
            let value = work(&tx)?;
            tx.commit().map_err(sql)?;
            Ok(value)
        })
        .await
        .map_err(|e| Fault::internal("store task failed").with_source(e))?
    }

    /// `Run` for the first request with `key` or one whose claim lapsed;
    /// else its result, polling while another live claim runs it.
    async fn claim(&self, key: &str) -> Result<Claim, Fault> {
        let owner = uuid::Uuid::new_v4().to_string();
        let lease = self.lease.as_millis() as i64;
        loop {
            let (key, owner) = (key.to_owned(), owner.clone());
            let claimed = self
                .transact(move |tx| {
                    let row: Option<(Option<Vec<u8>>, i64)> = tx
                        .query_row("SELECT result,lease FROM idempotency WHERE key=?1", [&key], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })
                        .optional()
                        .map_err(sql)?;
                    match row {
                        Some((Some(bytes), _)) => Ok(Some(Claim::Done(decode(bytes)?))),
                        Some((None, until)) if until > now_ms() => Ok(None),
                        _ => {
                            tx.execute(
                                "INSERT INTO idempotency(key,owner,lease) VALUES(?1,?2,?3)
                                 ON CONFLICT(key) DO UPDATE SET owner=excluded.owner,lease=excluded.lease,result=NULL",
                                (key, &owner, now_ms() + lease),
                            )
                            .map_err(sql)?;
                            Ok(Some(Claim::Run(owner)))
                        }
                    }
                })
                .await?;
            if let Some(claimed) = claimed {
                return Ok(claimed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Extend a live claim; `false` once it is lost.
    async fn renew(&self, key: &str, owner: &str) -> Result<bool, Fault> {
        let (key, owner, lease) = (
            key.to_owned(),
            owner.to_owned(),
            self.lease.as_millis() as i64,
        );
        self.transact(move |tx| {
            let at = now_ms();
            let updated = tx
                .execute(
                    "UPDATE idempotency SET lease=?1 WHERE key=?2 AND owner=?3 AND result IS NULL AND lease>?4",
                    (at + lease, key, owner, at),
                )
                .map_err(sql)?;
            Ok(updated == 1)
        })
        .await
    }

    /// Keep a run's replies, only while it still holds the claim; or with
    /// `None` release the key to the next request.
    async fn finish(
        &self,
        key: &str,
        owner: &str,
        replies: Option<Vec<Output>>,
    ) -> Result<(), Fault> {
        let (key, owner) = (key.to_owned(), owner.to_owned());
        let result = replies.as_deref().map(encode).transpose()?;
        self.transact(move |tx| {
            match result {
                Some(bytes) => tx.execute(
                    "UPDATE idempotency SET result=?1 WHERE key=?2 AND owner=?3 AND lease>?4",
                    (bytes, key, owner, now_ms()),
                ),
                None => tx.execute(
                    "DELETE FROM idempotency WHERE key=?1 AND owner=?2",
                    (key, owner),
                ),
            }
            .map_err(sql)?;
            Ok(())
        })
        .await
    }
}

/// Store replies in the format's own streamed encoding rather than a
/// second serialization of the events; replay decodes them with it.
fn encode(replies: &[Output]) -> Result<Vec<u8>, Fault> {
    let request = http::Request::new(Bytes::from_static(br#"{"stream":true}"#));
    Ok(OpenAiChat
        .encode_response(&request, replies)?
        .into_body()
        .to_vec())
}

fn decode(bytes: Vec<u8>) -> Result<Vec<Output>, Fault> {
    let mut response = http::Response::new(Bytes::from(bytes));
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    let request = http::Request::new(()).into_parts().0;
    OpenAiChat.decode_response(response, &request)
}
