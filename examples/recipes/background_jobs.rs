//! Durable background jobs for the Responses API: submit, poll, cancel, and
//! recovery after a crash.
//!
//! A `POST /v1/responses` with `"background": true` is stored in SQLite and
//! answered at once with a job id and `"status": "queued"`. The gateway then
//! runs the job as a request of its own, without `background`, through
//! every plugin, on a task that outlives the client's connection; what the
//! provider answers is stored. `GET /v1/responses/{id}` polls the job from
//! the store, and `POST /v1/responses/{id}/cancel` (or `DELETE`) cancels
//! it. A job belongs to the tenant whose credentials submitted it: another
//! tenant's poll is a 404. Every other request is relayed as it came.
//!
//! A running job holds a lease that its task renews. If the process dies,
//! the lease lapses, and `Jobs::resume` in the next process claims the job
//! and runs it again, with the job id as the provider's `Idempotency-Key`
//! every time; exactly-once remote execution needs a provider that honors
//! it. Call `resume` at startup, and again periodically from a supervisor.
//!
//! Left out to stay small: streamed jobs, budgets, completion webhooks,
//! signed ids, and chaining (`previous_response_id`) onto a job's id.
//!
//! Run it with `cargo run -p respan-gateway-examples --example background_jobs`.
//! Look for each job's status as it moves from queued to completed or
//! cancelled, and for the crashed job finished by the restarted process.
//! The SQLite file lives in the temp dir and is removed.

use mockllm::{Answer, FakeOpenAi};
use parking_lot::Mutex;
use respan_gateway_core::futures::stream;
use respan_gateway_core::{
    ClientContext, Extensions, Fault, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, Method, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, Credential};
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_passthrough_http::{self as passthrough, Passthrough};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The most bytes of one message the adapter and transports hold.
const LIMIT: usize = 32 << 20;
/// How long a job's claim holds without renewal.
const LEASE: Duration = Duration::from_secs(30);
const ACME: &str = "Bearer acme-key";
const OTHER: &str = "Bearer other-key";

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let path = std::env::temp_dir().join(format!("respan-bg-{}.sqlite3", uuid::Uuid::new_v4()));
    let result = demo(&path).await;
    for suffix in ["", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    result
}

async fn demo(path: &Path) -> Result<(), Fault> {
    let provider = FakeOpenAi::start().await;
    let jobs = Jobs::open(path, LEASE)?;
    let gw = gateway(&provider, &jobs)?;
    jobs.resume(&gw).await?;

    // Submit, poll at once, poll after the job's run ended.
    provider.answer(Answer::text("Done in the background"));
    let (_, queued) = send(&gw, ACME, "POST", "/v1/responses", &job("Write a haiku")).await?;
    let id = queued["id"].as_str().unwrap_or_default().to_owned();
    let poll = format!("/v1/responses/{id}");
    println!("submitted:      {queued}");
    println!(
        "polled at once: {}",
        summary(&send(&gw, ACME, "GET", &poll, "").await?.1)
    );
    gw.idle().await;
    println!(
        "polled later:   {}",
        summary(&send(&gw, ACME, "GET", &poll, "").await?.1)
    );
    let upstream = provider.requests();
    let key = upstream[0].headers.get("idempotency-key");
    println!("provider got Idempotency-Key {key:?}");
    let (status, _) = send(&gw, OTHER, "GET", &poll, "").await?;
    println!("another tenant polling it: {status}");

    // Cancel a job while its provider call is still out.
    provider.answer(Answer::text("too late").delay_first(10_000));
    let (_, slow) = send(&gw, ACME, "POST", "/v1/responses", &job("A long essay")).await?;
    let slow = format!("/v1/responses/{}", slow["id"].as_str().unwrap_or_default());
    while provider.requests().len() < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, cancelled) = send(&gw, ACME, "POST", &format!("{slow}/cancel"), "").await?;
    gw.idle().await;
    println!("cancelled:      {cancelled}");
    println!(
        "polled:         {}",
        summary(&send(&gw, ACME, "GET", &slow, "").await?.1)
    );

    // Another process claims a job with a short lease and dies mid-run.
    let dead = Jobs::open(path, Duration::from_millis(200))?;
    let orphan = dead
        .insert(&tenant_of(&credential(ACME)), job("Recover me"))
        .await?;
    dead.claim(&orphan).await?;
    drop(dead);
    println!("a process claimed {orphan} and died");

    // Restart: a new process on the same file resumes it once its lease lapsed.
    drop(gw);
    drop(jobs);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let jobs = Jobs::open(path, LEASE)?;
    let gw = gateway(&provider, &jobs)?;
    provider.answer(Answer::text("Recovered after the crash"));
    jobs.resume(&gw).await?;
    gw.idle().await;
    let orphan = format!("/v1/responses/{orphan}");
    println!(
        "after restart, the orphan:    {}",
        summary(&send(&gw, ACME, "GET", &orphan, "").await?.1)
    );
    println!(
        "after restart, the first job: {}",
        summary(&send(&gw, ACME, "GET", &poll, "").await?.1)
    );
    Ok(())
}

/// The gateway: Responses relayed unread to one provider, and the jobs.
fn gateway(provider: &FakeOpenAi, jobs: &Jobs) -> Result<Gateway, Fault> {
    let origin = provider.base_url().trim_end_matches("/v1").to_owned();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(&origin)?.with_headers(headers)?;
    Gateway::new()
        .client::<passthrough::Input<Passthrough>, _, _>(Passthrough)?
        .provider(Provider::new("openai", Passthrough, transport.connect()))?
        .plugin(jobs.clone())
        .target("openai")
}

fn job(prompt: &str) -> String {
    json!({ "model": "gpt-6-luna", "input": prompt, "background": true }).to_string()
}

/// One request from the tenant whose key is `token`: its status and JSON.
async fn send(
    gw: &Gateway,
    token: &'static str,
    method: &str,
    path: &str,
    body: &str,
) -> Result<(StatusCode, Value), Fault> {
    let req = http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", token)
        .header("content-type", "application/json")
        .body(Body::full(body.to_owned()))
        .map_err(respan_gateway_http_faults::built_wrong)?;
    let (resp, _end) = respan_gateway_http::handle(gw, req, LIMIT).await;
    let status = resp.status();
    let body = BodyExt::collect(resp.into_body()).await?.to_bytes();
    Ok((status, serde_json::from_slice(&body).unwrap_or_default()))
}

/// A job's status, and its answer's text once it has one.
fn summary(job: &Value) -> String {
    let status = job["status"].as_str().unwrap_or("?");
    match job["output"][0]["content"][0]["text"].as_str() {
        Some(text) => format!("{status}: {text:?}"),
        None => status.to_owned(),
    }
}

fn credential(token: &'static str) -> Credential {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static(token));
    Credential::of(headers)
}

/// Who a request is for: a hash of all the credentials the adapter lifted.
/// Tenants that send none are one.
fn tenant_of(credential: &Credential) -> String {
    let mut parts: Vec<String> = credential
        .headers()
        .iter()
        .map(|(name, value)| format!("{name}: {:?}", value.as_bytes()))
        .chain(credential.query().map(str::to_owned))
        .collect();
    parts.sort();
    let digest = Sha256::digest(serde_json::to_vec(&parts).unwrap_or_default());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn tenant(cx: &ClientContext) -> String {
    tenant_of(
        &cx.extensions()
            .get::<Credential>()
            .cloned()
            .unwrap_or_default(),
    )
}

/// A JSON answer the plugin gives itself.
fn answer(body: &Value) -> Option<Response> {
    let reply = http::Response::builder()
        .header("content-type", "application/json")
        .body(Bytes::from(body.to_string()))
        .expect("a JSON response");
    let reply = passthrough::Output::<Passthrough>::from_http(reply);
    Some(Response::stream(stream::iter([Ok(reply)])))
}

/// Marks the gateway's own run of a job. A request that carries it is
/// relayed, never started as a job: a job's run cannot start another.
#[derive(Clone)]
struct Job {
    id: String,
    owner: String,
    output: Arc<Mutex<BytesMut>>,
}

/// The job store and the plugin that serves it. Clones share it.
#[derive(Clone)]
struct Jobs {
    db: Arc<Mutex<Connection>>,
    /// The tasks running this process's jobs, to stop one on cancel.
    active: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
    lease: Duration,
}

#[async_trait]
impl Plugin for Jobs {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        // The job's own run: keep what the provider answers, and relay it.
        if let Some(job) = cx.extensions().get::<Job>().cloned() {
            cx.filter(move |got: &mut passthrough::Output<Passthrough>| {
                job.output.lock().extend_from_slice(got.0.body());
                Ok(())
            })?;
            return Ok(None);
        }
        let Ok(asked) = cx.input::<passthrough::Input<Passthrough>>() else {
            return Ok(None);
        };
        let (method, path) = (asked.0.method().clone(), asked.0.uri().path().to_owned());
        let body: Value = serde_json::from_slice(asked.0.body()).unwrap_or_default();

        if method == Method::POST && path == "/v1/responses" && body["background"] == true {
            if body["stream"] == true {
                return Err(Fault::bad_request(
                    "streamed background jobs are not supported",
                ));
            }
            let id = self.insert(&tenant(cx), body.to_string()).await?;
            self.run(&cx.gateway(), &id).await?;
            return Ok(answer(
                &json!({ "id": id, "object": "response", "status": "queued" }),
            ));
        }
        let Some((id, rest)) = path
            .strip_prefix("/v1/responses/")
            .map(|tail| tail.split_at(tail.find('/').unwrap_or(tail.len())))
        else {
            return Ok(None);
        };
        // An id that is no job of ours is the provider's: relay it.
        let Some((owner, status, result)) = self.lookup(id).await? else {
            return Ok(None);
        };
        if owner != tenant(cx) {
            return Err(Fault::not_found("no such response"));
        }
        let cancel = method == Method::DELETE || (method == Method::POST && rest == "/cancel");
        match (cancel, rest, result) {
            (true, _, _) => {
                let status = self.cancel(id).await?.unwrap_or(status);
                Ok(answer(
                    &json!({ "id": id, "object": "response", "status": status }),
                ))
            }
            (false, "", Some(result)) if method == Method::GET => {
                let mut result: Value = serde_json::from_slice(&result).unwrap_or_default();
                result["id"] = id.into();
                Ok(answer(&result))
            }
            (false, "", None) if method == Method::GET => Ok(answer(
                &json!({ "id": id, "object": "response", "status": status }),
            )),
            _ => Err(Fault::bad_request(
                "this example serves only GET, DELETE and /cancel of a job",
            )),
        }
    }

    async fn on_client_end(&self, cx: &ClientContext, end: &Result<(), Fault>) {
        let Some(job) = cx.extensions().get::<Job>() else {
            return;
        };
        let (status, result) = match end {
            Ok(()) => ("completed", job.output.lock().clone().freeze()),
            Err(fault) => {
                let failed = json!({ "id": job.id, "status": "failed", "error": fault.message() });
                ("failed", Bytes::from(failed.to_string()))
            }
        };
        let _ = self.finish(&job.id, &job.owner, status, result).await;
        self.active.lock().remove(&job.id);
    }
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY, tenant TEXT NOT NULL, request TEXT NOT NULL,
    status TEXT NOT NULL, result BLOB, owner TEXT NOT NULL DEFAULT '', lease INTEGER NOT NULL DEFAULT 0
);";

fn sql(error: rusqlite::Error) -> Fault {
    Fault::internal("the store could not complete its transaction").with_source(error)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl Jobs {
    fn open(path: &Path, lease: Duration) -> Result<Self, Fault> {
        let db = Connection::open(path).map_err(sql)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            active: Arc::default(),
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

    /// Store a queued job: `request` without `background`, which its run sends.
    async fn insert(&self, tenant: &str, request: String) -> Result<String, Fault> {
        let mut request: Value = serde_json::from_str(&request).unwrap_or_default();
        if let Some(fields) = request.as_object_mut() {
            fields.remove("background");
        }
        let id = format!("resp_{}", uuid::Uuid::new_v4().simple());
        let (saved, tenant) = (id.clone(), tenant.to_owned());
        self.transact(move |tx| {
            tx.execute(
                "INSERT INTO jobs(id,tenant,request,status) VALUES(?1,?2,?3,'queued')",
                (saved, tenant, request.to_string()),
            )
            .map_err(sql)?;
            Ok(())
        })
        .await?;
        Ok(id)
    }

    /// A job's tenant, status and result, if `id` is a job.
    async fn lookup(&self, id: &str) -> Result<Option<(String, String, Option<Vec<u8>>)>, Fault> {
        let id = id.to_owned();
        self.transact(move |tx| {
            tx.query_row(
                "SELECT tenant,status,result FROM jobs WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql)
        })
        .await
    }

    /// Claim a job no live run holds: its owner token and request.
    async fn claim(&self, id: &str) -> Result<Option<(String, String)>, Fault> {
        let (id, owner) = (id.to_owned(), uuid::Uuid::new_v4().to_string());
        let lease = self.lease.as_millis() as i64;
        self.transact(move |tx| {
            let claimed = tx
                .execute(
                    "UPDATE jobs SET status='in_progress',owner=?1,lease=?2
                     WHERE id=?3 AND status IN ('queued','in_progress') AND lease<=?4",
                    (&owner, now_ms() + lease, &id, now_ms()),
                )
                .map_err(sql)?;
            if claimed == 0 {
                return Ok(None);
            }
            let request = tx
                .query_row("SELECT request FROM jobs WHERE id=?1", [&id], |row| {
                    row.get(0)
                })
                .map_err(sql)?;
            Ok(Some((owner, request)))
        })
        .await
    }

    /// Extend a live claim; `false` once it is lost or the job cancelled.
    async fn renew(&self, id: &str, owner: &str) -> Result<bool, Fault> {
        let (id, owner, lease) = (
            id.to_owned(),
            owner.to_owned(),
            self.lease.as_millis() as i64,
        );
        self.transact(move |tx| {
            let renewed = tx
                .execute(
                    "UPDATE jobs SET lease=?1 WHERE id=?2 AND owner=?3 AND status='in_progress' AND lease>?4",
                    (now_ms() + lease, id, owner, now_ms()),
                )
                .map_err(sql)?;
            Ok(renewed == 1)
        })
        .await
    }

    /// Settle a run, once, and only while it still holds its claim: a run
    /// that lost its lease or was cancelled stores nothing.
    async fn finish(
        &self,
        id: &str,
        owner: &str,
        status: &'static str,
        result: Bytes,
    ) -> Result<(), Fault> {
        let (id, owner) = (id.to_owned(), owner.to_owned());
        self.transact(move |tx| {
            tx.execute(
                "UPDATE jobs SET status=?1,result=?2,lease=0
                 WHERE id=?3 AND owner=?4 AND status='in_progress' AND lease>?5",
                (status, result.as_ref(), id, owner, now_ms()),
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Cancel a job that has not finished, and stop its run if it is ours.
    /// `Some("cancelled")` if it was cancelled now.
    async fn cancel(&self, id: &str) -> Result<Option<String>, Fault> {
        let saved = id.to_owned();
        let cancelled = self
            .transact(move |tx| {
                tx.execute(
                    "UPDATE jobs SET status='cancelled',lease=0 WHERE id=?1 AND status IN ('queued','in_progress')",
                    [saved],
                )
                .map_err(sql)
            })
            .await?;
        if let Some(run) = self.active.lock().remove(id) {
            run.abort();
        }
        Ok((cancelled == 1).then(|| "cancelled".to_owned()))
    }

    /// Run every job no live process holds: queued ones, and those whose
    /// process died and whose lease lapsed.
    async fn resume(&self, gateway: &Gateway) -> Result<(), Fault> {
        let ids: Vec<String> = self
            .transact(|tx| {
                let mut rows = tx
                    .prepare("SELECT id FROM jobs WHERE status IN ('queued','in_progress') AND lease<=?1")
                    .map_err(sql)?;
                let ids = rows.query_map([now_ms()], |row| row.get(0)).map_err(sql)?;
                ids.collect::<Result<_, _>>().map_err(sql)
            })
            .await?;
        for id in ids {
            self.run(gateway, &id).await?;
        }
        Ok(())
    }

    /// Claim job `id` and run it as the gateway's own request, on a task
    /// that renews the claim while the run is live.
    async fn run(&self, gateway: &Gateway, id: &str) -> Result<(), Fault> {
        let Some((owner, request)) = self.claim(id).await? else {
            return Ok(());
        };
        let mut req = http::Request::post("/v1/responses")
            .header("content-type", "application/json")
            .header("idempotency-key", id)
            .body(Body::full(request))
            .map_err(respan_gateway_http_faults::built_wrong)?;
        let mut extensions = Extensions::new();
        let output = Arc::default();
        let job = Job {
            id: id.to_owned(),
            owner: owner.clone(),
            output,
        };
        extensions.insert(job);
        req.extensions_mut().insert(extensions);
        // Handled before spawning, so `Gateway::idle` waits for the run.
        let served = respan_gateway_http::handle(gateway, req, LIMIT);

        let (jobs, active, id) = (self.clone(), id.to_owned(), id.to_owned());
        let (start, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = started.await;
            let work = async {
                let (response, ended) = served.await;
                let _ = response.into_body().collect().await;
                let _ = ended.await;
            };
            tokio::pin!(work);
            loop {
                tokio::select! {
                    _ = &mut work => return,
                    _ = tokio::time::sleep(jobs.lease / 3) => {
                        // A lost claim stops the run; another process has it.
                        if !jobs.renew(&id, &owner).await.unwrap_or(false) {
                            return;
                        }
                    }
                }
            }
        });
        // Registered before the task starts, so it cannot finish first.
        self.active.lock().insert(active, task.abort_handle());
        let _ = start.send(());
        Ok(())
    }
}
