//! One config's gateway and application, and how they are composed.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::Response;
use axum::routing::{MethodRouter, get};
use respan_gateway_core::Gateway;
use respan_gateway_http::http::StatusCode;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_media::video::{LocateContent, VideoJobs};
use respan_gateway_media_http::gemini::GeminiVeo;
use respan_gateway_plugin_credentials::ClientCredentials;
use respan_gateway_plugin_mcp_router::{Closer as McpCloser, McpRouter};
use respan_gateway_plugin_relay::Relay;
use respan_gateway_plugin_respan::Respan;
use respan_gateway_plugin_routing::{Dispatch, Routing};
use respan_gateway_plugin_tool_loop::ToolFilter;
use respan_gateway_ws::Limits;
use tokio::sync::watch;

use super::{BODY_LIMIT, DEFAULT_LISTEN, Process};
use crate::admin::{self, Admin};
use crate::auth::{self, ClientKeys, Cors, LocalOnly};
use crate::compose::{PASSTHROUGH, Plan, compose};
use crate::formats::{KnownFormat, formats, kind};
use crate::mcp;
use crate::models::{self, Models};
use crate::requests::{self, Recorder};
use crate::tool_loop;
use crate::{Config, Error};

/// One config's gateway and application.
pub(super) struct Built {
    pub(super) config: Arc<Config>,
    pub(super) gateway: Gateway,
    pub(super) plan: Plan,
    pub(super) formats: Vec<KnownFormat>,
    pub(super) router: Router,
    /// Without client keys, what keeps it local.
    pub(super) local: Option<Arc<LocalOnly>>,
    /// Ends the tool loop's sessions with its tool servers.
    pub(super) closer: Option<tool_loop::Closer>,
    /// Ends the sessions at `/mcp` with its servers.
    pub(super) aggregate: Option<McpCloser>,
    /// Becomes `true` once its providers' first listings are in; none if it
    /// reads no lists.
    pub(super) listed: Option<tokio::sync::watch::Receiver<bool>>,
}

impl Built {
    pub(super) fn new(config: Config, process: &Arc<Process>) -> Result<Built, Error> {
        let limit = config.body_limit.unwrap_or(BODY_LIMIT);
        let sessions = Limits {
            max_message: config
                .ws_message_limit
                .unwrap_or(Limits::default().max_message),
            ..Limits::default()
        };
        let mut formats = formats();
        let providers: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        let logins = crate::mcp_oauth::logins_dir(config.source());
        let network = crate::network::network(&config)?;
        let mcp = mcp::servers(&config, &network, &providers, &logins)?;
        let fronted = mcp::fronted(&mcp);
        if !mcp.is_empty() {
            formats.push(mcp::known(mcp.clone(), !fronted.is_empty(), limit));
        }
        let aggregate = (!fronted.is_empty()).then_some(fronted);
        let (gateway, mut plan) = compose(&config, &formats, limit, sessions, &network)?;
        plan.mcp = mcp;
        if let Some(tool_loop) = &config.plugins.tool_loop {
            let serving = plan.serves(mcp::FORMAT);
            tool_loop::check(tool_loop, &plan.mcp, serving)?;
        }
        // Respan opens a request's trace first, before any plugin answers
        // it (as the tool loop does), and records its calls last, as sent.
        let mut respan_health = None;
        let mut respan_calls = None;
        let gateway = match &config.plugins.respan {
            Some(respan) => {
                let key = crate::compose::secret("plugins: respan: key", &respan.key)?;
                let url = respan
                    .url
                    .as_deref()
                    .unwrap_or(respan_gateway_plugin_respan::URL);
                let service = respan.service.as_deref().unwrap_or("respan-gateway");
                let content = respan.content.unwrap_or(true);
                // A client's request alone: the gateway's own, such as the
                // tool loop's and the dashboard's to tool servers, are not.
                let http = Respan::client(&network)?;
                let respan = Respan::with_client(&key, url, service, content, http)?
                    .only(requests::is_clients);
                let health = respan.health();
                let log = config.log;
                health.on_change(move |why| match why {
                    Some(why) => log.say(crate::logging::Level::Warn, &format!("respan: {why}")),
                    None => log.say(crate::logging::Level::Info, "respan: sending traces again"),
                });
                respan_health = Some(health);
                respan_calls = Some(respan.calls());
                gateway.plugin(respan)
            }
            None => gateway,
        };
        #[cfg(feature = "ark")]
        let ark = config
            .plugins
            .ark
            .as_ref()
            .map(|config| respan_gateway_plugin_ark::Ark::new(config.clone()))
            .transpose()?;
        #[cfg(feature = "ark")]
        let gateway = match &ark {
            Some(guard) => gateway.plugin(guard.clone()),
            None => gateway,
        };
        let fronted = aggregate.clone();
        let (gateway, closer, aggregate) =
            with_plugins(gateway, &config, aggregate, &plan, &formats, process);
        // Final provider input guard sees edits and tool-loop calls; trace calls see its result.
        #[cfg(feature = "ark")]
        let gateway = match &ark {
            Some(guard) => gateway.plugin(guard.provider_guard()),
            None => gateway,
        };
        let gateway = match respan_calls {
            Some(calls) => gateway.plugin(calls),
            None => gateway,
        };
        let keys = ClientKeys::new(plan.keys.clone());
        let relay = relay(
            &gateway,
            &plan,
            &formats,
            limit,
            sessions,
            &process.stopping,
        )
        .layer(from_fn_with_state(keys.clone(), auth::consume))
        .layer(from_fn_with_state(process.log.clone(), requests::track));
        let admin = Arc::new(
            Admin::new(
                process.clone(),
                &config,
                &plan,
                &formats,
                gateway.clone(),
                fronted,
            )
            .respan(respan_health),
        );
        let cors = match &config.cors {
            Some(c) => Some(Cors::new(&c.origins, keys.required()).map_err(Error::new)?),
            None => None,
        };
        let local = (!keys.required())
            .then(|| LocalOnly::new(config.listen.unwrap_or(DEFAULT_LISTEN), cors.clone()));
        let (router, listed) = routes(&config, &plan, relay, admin, keys, local.clone(), cors);
        Ok(Built {
            config: Arc::new(config),
            gateway,
            plan,
            formats,
            router,
            local,
            closer,
            aggregate,
            listed,
        })
    }

    pub(super) fn listen(&self) -> SocketAddr {
        self.config.listen.unwrap_or(DEFAULT_LISTEN)
    }

    pub(super) fn local_is_on(&self, addr: SocketAddr) {
        if let Some(local) = &self.local {
            local.serving_on(addr);
        }
    }

    pub(super) fn open_refusal(&self, addr: SocketAddr) -> Option<String> {
        let open = self.plan.keys.is_empty() && !addr.ip().is_loopback();
        (open && !self.config.relay).then(|| {
            format!(
                "refusing to listen on {addr} with no client keys: anyone who can reach it \
                 would spend your provider keys, and without keys it answers only to \
                 localhost anyway.\n  Set `client_keys` in the config (`respan-gateway init \
                 --client-key` writes a strong one), or listen on 127.0.0.1."
            )
        })
    }
}

/// Routing by the plan's routes, of each format, knowing each request's
/// client key by name.
fn routing(plan: &Plan, formats: &[KnownFormat]) -> Routing {
    let client = |cx: &respan_gateway_core::ClientContext| {
        let name = cx.extensions().get::<crate::auth::KeyName>();
        name.map(|n| n.0.clone())
    };
    formats.iter().filter(|f| f.kind != kind::MCP).fold(
        Routing::new(plan.routes.clone()).client(client),
        |routing, f| routing.format(&f.name, f.kind, f.model.clone()),
    )
}

/// The gateway with the binary's plugins, and what ends the sessions of the
/// tool loop and of `/mcp`.
type Plugins = (Gateway, Option<tool_loop::Closer>, Option<McpCloser>);

/// The gateway with the binary's plugins, in a fixed order whatever the
/// config's: the request log, routing, MCP, video jobs, whose credentials go
/// where, the tool loop, then the dispatch of routes. The vendors' quirks are
/// none of them: they are in the formats their providers speak.
fn with_plugins(
    gateway: Gateway,
    config: &Config,
    aggregate: Option<Vec<String>>,
    plan: &Plan,
    formats: &[KnownFormat],
    process: &Process,
) -> Plugins {
    let (log, switches) = (&process.log, &process.switches);
    let names: HashMap<String, String> = plan
        .upstreams
        .iter()
        .flat_map(|u| {
            u.entries
                .iter()
                .map(|e| (e.internal.clone(), u.name.clone()))
        })
        .chain(plan.mcp.iter().map(|s| (s.name.clone(), s.name.clone())))
        .collect();
    // Veo serves a job's content only where the job says.
    let veo = plan.upstreams.iter().flat_map(|u| &u.entries);
    let veo = veo.filter(|e| e.format == GeminiVeo::NAME);
    let locate = veo.fold(LocateContent::new(), |l, e| l.provider(&e.internal));
    // The tool loop answers the request itself, so it comes after every plugin
    // that routes or edits it; what ends its sessions comes before them all.
    let tool_loop = config.plugins.tool_loop.as_ref().map(|config| {
        let flags: HashSet<String> = (plan.upstreams.iter())
            .flat_map(|u| &u.entries)
            .filter(|e| e.format == "anthropic-messages")
            .map(|e| e.internal.clone())
            .collect();
        let servers = tool_loop::servers(config, &plan.mcp);
        let anthropic = move |provider: &str| flags.contains(provider);
        tool_loop::plugin(config, servers, switches.clone(), anthropic)
    });
    let gateway = match &tool_loop {
        Some((_, closer)) => gateway.plugin(closer.plugin()),
        None => gateway,
    };
    // `/mcp` answers itself for the servers it fronts; `/mcp/{name}` is
    // routed to its server.
    let fronts = aggregate.is_some();
    let fronted = aggregate.unwrap_or_default();
    let mcp_router = McpRouter::with_switches(fronted, ToolFilter::default(), switches.clone());
    let aggregate_closer = fronts.then(|| mcp_router.closer());
    let gateway = match &aggregate_closer {
        Some(closer) => gateway.plugin(closer.plugin()),
        None => gateway,
    };
    let gateway = gateway
        .plugin(Recorder::new(log.clone(), formats, names.clone()))
        .plugin(routing(plan, formats))
        .plugin(mcp_router);
    let gateway = gateway.plugin(video_jobs(plan, formats)).plugin(locate);
    // A provider without a key of its own gets the client's.
    let keyless = plan.upstreams.iter().filter(|u| !u.own_key);
    let keyless = keyless.flat_map(|u| u.entries.iter().map(|e| e.internal.clone()));
    // An MCP server gets a client's credentials only if it says so.
    let forwarded = plan.mcp.iter().filter(|s| s.forward_auth);
    let forwarded = forwarded.map(|s| s.name.clone());
    let gateway = gateway.plugin(ClientCredentials::new(keyless.chain(forwarded)));
    let responses = &config.plugins.responses;
    let gateway = match responses.enabled {
        true => {
            let keep = responses
                .keep
                .unwrap_or(std::time::Duration::from_secs(60 * 60));
            gateway.plugin(respan_gateway_plugin_responses::Responses::new(keep))
        }
        false => gateway,
    };
    // A relay gives its vendor's response heads and error bodies back.
    let mut gateway = match config.relay {
        true => gateway.plugin(Relay),
        false => gateway,
    };
    let closer = tool_loop.as_ref().map(|(_, closer)| closer.clone());
    if let Some((tool_loop, _)) = tool_loop {
        gateway = gateway.plugin(tool_loop.observe(requests::steps(log.clone())));
    }
    // It answers the request itself: a request the tool loop runs is not
    // sent to a route's other candidates.
    let gateway = gateway.plugin(Dispatch::new(respan_gateway_http_faults::retryable));
    // Last: a call is priced at the model the plugins before chose.
    let pricing = respan_gateway_plugin_pricing::Pricing::new(plan.routes.catalog().clone())
        .provider_names(names)
        .on_charge(requests::charge)
        // A live session names its model in its handshake.
        .model_of(|px| {
            (px.extensions()
                .get::<respan_gateway_realtime_ws::SessionModel>())
            .map(|m| m.0.clone())
        });
    let gateway = gateway.plugin(pricing);
    (gateway, closer, aggregate_closer)
}

/// Video jobs' follow-ups, to the provider of each format whose job ids
/// the format spells.
fn video_jobs(plan: &Plan, formats: &[KnownFormat]) -> VideoJobs {
    let spelling = |format: &str| {
        let known = formats.iter().find(|f| f.name == format);
        known.and_then(|f| f.job_ids)
    };
    let entries = plan.upstreams.iter();
    let entries = entries.flat_map(|u| u.entries.iter().map(move |e| (u, e)));
    entries.fold(VideoJobs::new(), |jobs, (u, e)| match spelling(&e.format) {
        Some(spelling) => jobs.provider(&u.name, &e.internal, spelling),
        None => jobs,
    })
}

/// The router of the formats served: a WebSocket upgrade is a session if a
/// session's format is served. Sessions close 1001 once `stopping` is set.
fn relay(
    gateway: &Gateway,
    plan: &Plan,
    formats: &[KnownFormat],
    limit: usize,
    sessions: Limits,
    stopping: &Arc<watch::Sender<bool>>,
) -> Router {
    let session = |name: &String| formats.iter().any(|f| f.name == *name && f.websocket);
    let host = respan_gateway_serve::Host::new(gateway.clone())
        .limit(move |_| limit)
        .limits(sessions);
    let router = match plan.serve.iter().any(session) {
        true => {
            let stopping = stopping.clone();
            host.stop(async move {
                let _ = stopping.subscribe().wait_for(|stop| *stop).await;
            })
            .router()
        }
        false => host.router(),
    };
    match plan.serves(PASSTHROUGH) {
        true => router,
        false => router.layer(map_response(json_404)),
    }
}

/// The application: `/healthz`, `/v1/models`, the web UI, and the formats
/// served for every other request; without client keys, local alone. And
/// what says when its providers' first listings are in, if it reads them.
fn routes(
    config: &Config,
    plan: &Plan,
    relay: Router,
    admin: Arc<Admin>,
    keys: Arc<ClientKeys>,
    local: Option<Arc<LocalOnly>>,
    cors: Option<Arc<Cors>>,
) -> (Router, Option<tokio::sync::watch::Receiver<bool>>) {
    let mut router = Router::new().route("/healthz", get(|| async { "ok" }));
    let mut listed = None;
    // A relay lets its vendor answer, with the client's credentials.
    if !config.relay {
        let models = Arc::new(Models::new(&plan.upstreams, plan.routes.clone()));
        // The router reads the listings: which provider lists a model, and
        // what it costs and can do. Learnt before a client lists.
        if config.plugins.routing.listings != Some(false) {
            Models::refresh(&models, std::time::Duration::from_secs(10 * 60));
            listed = Some(models.listed());
        }
        let list = get(models::list).with_state(models.clone());
        let list = list.layer(from_fn_with_state(keys.clone(), auth::require));
        router = router.route("/v1/models", list);
        // Gemini SDKs list at `/v1beta`; a POST there is a call of the relay.
        let gemini = |route: MethodRouter<Arc<Models>>| {
            let route = route.fallback_service(relay.clone());
            let route = route.layer(from_fn_with_state(keys.clone(), auth::require));
            route.with_state(models.clone())
        };
        router = router
            .route("/v1beta/models", gemini(get(models::list_gemini)))
            // One segment: a job's operation, `models/{model}/operations/{id}`,
            // is the relay's.
            .route("/v1beta/models/{model}", gemini(get(models::get_gemini)))
            .route(
                "/v1beta/models/models/{model}",
                gemini(get(models::get_gemini)),
            );
    }
    if config.ui {
        router = router.merge(admin::router(admin, keys.clone()));
    }
    let router = router.fallback_service(relay);
    let router = match local {
        Some(local) => router.layer(from_fn_with_state(local, auth::local_only)),
        None => router,
    };
    // Outermost: a preflight carries no key, and no host check applies.
    let router = match cors {
        Some(cors) => router.layer(from_fn_with_state(cors, auth::cors)),
        None => router,
    };
    (router, listed)
}

/// The gateway's own 404, which no format writes, as JSON in OpenAI's
/// error shape. A 404 a provider sent through passthrough is the vendor's,
/// and passthrough takes every request, so it never has one.
async fn json_404(resp: Response) -> Response {
    let plain = resp
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|t| t.as_bytes().starts_with(b"text/plain"));
    if resp.status() != StatusCode::NOT_FOUND || !plain {
        return resp;
    }
    let text = axum::body::to_bytes(resp.into_body(), 64 << 10).await;
    let text = text.map(|b| String::from_utf8_lossy(&b).into_owned());
    let message = text.unwrap_or_default();
    auth::error(StatusCode::NOT_FOUND, "not_found_error", message.trim())
}
