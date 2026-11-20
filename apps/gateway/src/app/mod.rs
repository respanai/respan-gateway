//! A configured gateway and the HTTP application around it.

use crate::mcp;
use crate::models::{Models, Probe};
use crate::report;
use crate::requests::RequestLog;
use crate::{Config, Error};
use axum::Router;
use axum::extract::Request;
use parking_lot::{Mutex, RwLock};
use respan_gateway_core::Gateway;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tower::ServiceExt;

mod banner;
mod built;
mod reload;
mod serve;

pub use reload::{ReloadError, Reloaded};
pub use serve::shutdown_signal;

use built::Built;
pub(crate) use reload::Process;
use reload::Shared;

/// Where the gateway listens unless told.
pub const DEFAULT_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8080);
/// How long open responses and end hooks get, together, after a signal,
/// unless the config's `shutdown_grace` says.
const SHUTDOWN: Duration = Duration::from_secs(30);
/// The most bytes of a client's request, and of a provider's answer held at
/// once, unless the config's `body_limit` says.
const BODY_LIMIT: usize = respan_gateway_http::call::MAX_BODY;

/// A gateway built from a [`Config`], and its HTTP application: the
/// formats served, `/healthz`, `/metrics`, `/v1/models`, the request log,
/// and the web UI with its admin API.
///
/// Serve it with [`serve`](Self::serve), or hand requests to its
/// [`router`](Self::router) in process:
///
/// ```no_run
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// use tower::ServiceExt;
/// let app = respan_gateway::App::new(respan_gateway::Config::from_env()?)?;
/// let request = axum::http::Request::get("/healthz").body(axum::body::Body::empty())?;
/// let response = app.router().oneshot(request).await?;
/// assert_eq!(response.status(), 200);
/// # Ok(()) }
/// ```
///
/// An app given a config source ([`reload_from`](Self::reload_from))
/// [`reload`](Self::reload)s: it builds the new config's gateway and swaps
/// it in for new requests, while requests and sessions open on the old one
/// finish there. Clones share one app.
#[derive(Clone)]
pub struct App {
    shared: Arc<Shared>,
    router: Router,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let built = self.shared.current();
        f.debug_struct("App")
            .field("source", &built.config.source)
            .field("gateway", &built.gateway)
            .finish_non_exhaustive()
    }
}

impl App {
    /// Checks `config` and builds its gateway and application. Needs no
    /// runtime; serving does. It cannot [`reload`](Self::reload) until
    /// given a source ([`reload_from`](Self::reload_from)).
    pub fn new(config: Config) -> Result<App, Error> {
        let process = Arc::new(Process {
            log: RequestLog::new(&config),
            stopping: Arc::new(watch::channel(false).0),
            listen: OnceLock::new(),
            started_at: SystemTime::now(),
            started: std::time::Instant::now(),
            reloaded_at: Mutex::new(None),
            switches: Arc::default(),
            reload: OnceLock::new(),
        });
        let built = Built::new(config, &process)?;
        let shared = Arc::new(Shared {
            process,
            current: RwLock::new(Arc::new(built)),
            retired: Mutex::default(),
            loader: OnceLock::new(),
            reloading: Mutex::new(()),
        });
        let weak = Arc::downgrade(&shared);
        let _ = shared
            .process
            .reload
            .set(Box::new(move || match weak.upgrade() {
                Some(shared) => shared.reload(),
                None => Err(ReloadError::Unavailable),
            }));
        let router = switch(shared.clone());
        Ok(App { shared, router })
    }

    /// Where [`reload`](Self::reload) reads the config from: `loader` gives
    /// the config as it is now.
    #[must_use]
    pub fn reload_from(
        self,
        loader: impl Fn() -> Result<Config, Error> + Send + Sync + 'static,
    ) -> Self {
        let _ = self.shared.loader.set(Box::new(loader));
        self
    }

    /// Reads the config again, and if it is valid builds its gateway and
    /// swaps it in for every request that starts from now on. Requests and
    /// WebSocket sessions already open finish on the gateway they started
    /// on. The request log and its totals carry over. A config that cannot
    /// be read, composed or served (it would drop the client keys under a
    /// non-loopback address) is an [`Err`], and the previous one keeps
    /// serving. What only a restart applies, the listen address, is named
    /// in [`Reloaded::restart_required`].
    pub fn reload(&self) -> Result<Reloaded, ReloadError> {
        self.shared.reload()
    }

    /// The application, to serve or to hand requests to; it follows
    /// reloads.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// The gateway inside it as of now, such as to await [`Gateway::idle`];
    /// a reload makes a new one.
    pub fn gateway(&self) -> Gateway {
        self.shared.current().gateway.clone()
    }

    /// The config as of now.
    pub fn config(&self) -> Arc<Config> {
        self.shared.current().config.clone()
    }

    /// Once its providers have answered their first listings, or failed
    /// to, so the first calls are priced and routed by what the lists say;
    /// at once if it reads no lists. Each listing gives up after a few
    /// seconds, so this does too.
    pub async fn listed(&self) {
        let Some(mut listed) = self.shared.current().listed.clone() else {
            return;
        };
        let waited = listed.wait_for(|done| *done);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), waited).await;
    }

    /// Where it listens: the config's address, or `127.0.0.1:8080`.
    pub fn listen(&self) -> SocketAddr {
        self.shared.current().listen()
    }

    /// What it serves, for a person starting it: the first line says
    /// where it listens.
    pub fn banner(&self, addr: SocketAddr) -> String {
        self.shared.current().banner(addr)
    }

    /// Why it must not serve on `addr`, if it must not: a gateway with
    /// provider keys of its own, on an address other machines reach, and no
    /// client keys, lets anyone who reaches it spend them. (Its `Host`
    /// guard would also refuse every non-local name.) `proxy` is exempt: it
    /// holds no keys, each client brings its own.
    pub fn open_refusal(&self, addr: SocketAddr) -> Option<String> {
        self.shared.current().open_refusal(addr)
    }

    /// The composed configuration, as `check` prints it.
    pub fn report(&self) -> String {
        let built = self.shared.current();
        report::check(&built.config, &built.plan, &built.formats, built.listen())
    }

    /// Every provider's models, as `/v1/models` lists them (`id`, `kind`,
    /// `owned_by`), asking each provider now; and why each provider that did
    /// not list is missing.
    pub async fn models(&self) -> (Vec<Value>, Vec<String>) {
        let built = self.shared.current();
        let models = Models::new(&built.plan.upstreams, built.plan.routes.clone());
        models.collect().await
    }

    /// The report of `check --online`: one call to each provider with its
    /// key, listing its models, and how it answered; and whether every one
    /// of them did.
    pub async fn probe(&self) -> (String, bool) {
        let built = self.shared.current();
        let models = Models::new(&built.plan.upstreams, built.plan.routes.clone());
        let (providers, servers) = tokio::join!(models.probe(), mcp::probe(&built.plan.mcp));
        let probes: Vec<Probe> = providers.into_iter().chain(servers).collect();
        // Listing contacts the servers, so only here, never in `report`.
        let tools = match &built.closer {
            Some(closer) => {
                let tools = closer.offered(&built.gateway).await;
                closer.close(&built.gateway).await;
                Some(tools)
            }
            None => None,
        };
        let report = report::online(&probes, tools.as_deref());
        (report, report::all_reachable(&probes))
    }
}

/// The application of whichever build is current when a request arrives.
fn switch(shared: Arc<Shared>) -> Router {
    Router::new().fallback(move |req: Request| {
        let router = shared.current().router.clone();
        async move {
            match router.oneshot(req).await {
                Ok(response) => response,
                Err(never) => match never {},
            }
        }
    })
}
