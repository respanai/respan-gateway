//! The web UI under `/ui/`, and the admin API it reads under `/ui/api/`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::middleware::from_fn_with_state;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use respan_gateway_core::futures::{Stream, StreamExt, stream};
use respan_gateway_http::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use respan_gateway_http::http::{HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::app::{Process, ReloadError};
use crate::auth::{self, ClientKeys};
use crate::compose::Plan;
use crate::config::{Config, Source};
use crate::formats::{KnownFormat, first_of_each_kind};
use crate::history::{CSV_HEADER, Filter, csv_row};
use crate::mcp::Transport;
use crate::requests::{RequestRecord, rfc3339};
use respan_gateway_core::Gateway;
use respan_gateway_plugin_routing::{Ask, By, Routes};
use respan_gateway_plugin_tool_loop::ToolFilter;

mod config;

mod assets {
    include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));
}

/// What the admin API reports, of one build of the config; what outlives a
/// reload is in its [`Process`].
pub struct Admin {
    process: Arc<Process>,
    source: Source,
    /// The config as the admin API shows it when there is no file.
    stand_in: serde_json::Value,
    auth_required: bool,
    /// Each provider, as `/ui/api/providers` lists it (`name`, `vendor`
    /// (string or null), `formats`, `kinds`, `base_url` (string),
    /// `credentials` (`"gateway"` or `"client"`), and the counts), but its counts.
    providers: Vec<Value>,
    /// The formats served, as `/ui/api/routes` lists them.
    serve: Vec<Value>,
    /// The router, read live for its record of each candidate.
    routes: Arc<Routes>,
    /// A format served of each kind, for the explain endpoint.
    kinds: Vec<(&'static str, String)>,
    /// Each MCP server, as `/ui/api/mcp` lists it but its counts and status.
    mcp: Vec<Value>,
    /// The tool loop's and the aggregate's servers, and which of their
    /// tools each offers.
    tool_loop: Option<(Vec<String>, ToolFilter)>,
    aggregate: Option<(Vec<String>, ToolFilter)>,
    /// The gateway, which the tools of a server are asked through.
    gateway: Gateway,
    /// Each server's tools as last asked, for a minute.
    /// Each MCP server's tools as last listed, for a minute.
    tools: moka::sync::Cache<String, Value>,
    /// How sending traces to Respan goes, if its plugin is on.
    respan: Option<Arc<respan_gateway_plugin_respan::Health>>,
}

impl Admin {
    /// Where the config comes from (`file`, `environment`, `proxy` or
    /// `inline`) and, for a file, its path as given.
    fn source_and_path(&self) -> (&'static str, Option<String>) {
        match &self.source {
            Source::File(path) => ("file", Some(path.display().to_string())),
            Source::Environment => ("environment", None),
            Source::Proxy => ("proxy", None),
            Source::Inline => ("inline", None),
        }
    }

    pub(crate) fn new(
        process: Arc<Process>,
        config: &Config,
        plan: &Plan,
        formats: &[KnownFormat],
        gateway: Gateway,
        aggregate: Option<Vec<String>>,
    ) -> Self {
        let providers = plan
            .upstreams
            .iter()
            .map(|u| {
                let formats: Vec<&str> = u.entries.iter().map(|e| e.format.as_str()).collect();
                let kinds = first_of_each_kind(u.entries.iter().map(|e| (e.kind, ())));
                let kinds: Vec<&str> = kinds.iter().map(|(kind, _)| *kind).collect();
                json!({
                    "name": u.name,
                    "vendor": Some(u.provider.vendor().id).filter(|id| !id.is_empty()),
                    "formats": formats,
                    "kinds": kinds,
                    "base_url": u.entries.first().map_or(String::new(), |e| crate::redact::url(&e.base_url)),
                    "credentials": if u.own_key { "gateway" } else { "client" },
                })
            })
            .collect();
        let serve: Vec<Value> = plan
            .serve
            .iter()
            .filter_map(|name| formats.iter().find(|f| f.name == *name))
            .map(|f| json!({ "format": f.name, "kind": f.kind, "endpoints": f.endpoints }))
            .collect();
        let mcp = plan
            .mcp
            .iter()
            .map(|s| {
                let credentials = match (&s.transport, s.own_key, s.forward_auth) {
                    (Transport::Stdio { .. }, ..) => "none",
                    (_, true, _) => "gateway",
                    (_, false, true) => "client",
                    (_, false, false) => "none",
                };
                json!({
                    "name": s.name,
                    "transport": s.transport_label(),
                    "target": s.target(),
                    "endpoint": s.endpoint(),
                    "credentials": credentials,
                    "auth": s.key_from,
                })
            })
            .collect();
        let served = plan.serve.iter();
        let served = served.filter_map(|name| formats.iter().find(|f| f.name == *name));
        let kinds = first_of_each_kind(served.map(|f| (f.kind, f.name.clone())));
        Self {
            process,
            source: config.source.clone(),
            stand_in: config::stand_in(config),
            auth_required: !plan.keys.is_empty(),
            providers,
            serve,
            routes: plan.routes.clone(),
            kinds,
            mcp,
            tool_loop: (config.plugins.tool_loop.as_ref()).map(|t| {
                let filter = ToolFilter::new(t.allow.clone(), t.deny.clone());
                (crate::tool_loop::servers(t, &plan.mcp), filter)
            }),
            aggregate: aggregate.map(|servers| (servers, ToolFilter::default())),
            gateway,
            tools: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .build(),
            respan: None,
        }
    }

    /// With `health`, the Respan plugin's, to show.
    pub(crate) fn respan(
        mut self,
        health: Option<Arc<respan_gateway_plugin_respan::Health>>,
    ) -> Self {
        self.respan = health;
        self
    }
}

/// `/` to the UI, the UI's files, and the admin API behind `keys`.
pub fn router(admin: Arc<Admin>, keys: Arc<ClientKeys>) -> Router {
    let api = Router::new()
        .route("/ui/api/overview", get(overview))
        .route("/ui/api/providers", get(providers))
        .route("/ui/api/mcp", get(mcp))
        .route("/ui/api/mcp/{name}", get(not_found).put(switch_server))
        .route("/ui/api/mcp/{name}/tools", get(mcp_tools))
        .route(
            "/ui/api/mcp/{name}/tools/{tool}",
            get(not_found).put(switch_tool),
        )
        .route("/ui/api/routes", get(routes))
        .route("/ui/api/routes/explain", get(explain))
        .route("/ui/api/requests", get(requests))
        .route("/ui/api/requests/export", get(export))
        .route("/ui/api/requests/{id}", get(request))
        .route("/ui/api/events", get(events))
        .route("/ui/api/reload", post(reload))
        .route("/ui/api/config", get(config::show))
        .route("/ui/api/{*rest}", get(not_found))
        .route_layer(from_fn_with_state(keys, auth::require_admin))
        .with_state(admin);
    Router::new()
        .route("/", get(|| async { Redirect::to("/ui/") }))
        .route("/ui", get(|| async { Redirect::to("/ui/") }))
        .route("/ui/", get(|| async { asset("index.html") }))
        .route(
            "/ui/{*path}",
            get(|Path(path): Path<String>| async move { asset(&path) }),
        )
        .merge(api)
}

/// `reloaded_at` and `restart_required`, as a reload and a save answer.
fn reloaded_json(
    process: &Process,
    reloaded: &crate::app::Reloaded,
) -> serde_json::Map<String, Value> {
    let at = *process.reloaded_at.lock();
    let Value::Object(map) = json!({
        "reloaded_at": at.map(rfc3339),
        "restart_required": reloaded.restart_required,
    }) else {
        unreachable!("an object literal")
    };
    map
}

/// `POST /ui/api/reload`: reads the config source again and swaps the new
/// gateway in for new requests; requests and sessions open finish on the
/// old one, the request log carries over.
///
/// ```json
/// { "reloaded_at": "2026-10-11T09:30:00Z", "restart_required": [] }
/// ```
///
/// `restart_required` names what only a restart applies, such as a changed
/// `listen`. A config that cannot be read or composed is `422` (error type
/// `invalid_config`, the message saying why) and the previous config keeps
/// serving; a gateway with no config file or environment to read again is
/// `409` (`reload_unavailable`).
async fn reload(State(admin): State<Arc<Admin>>) -> Response {
    let process = admin.process.clone();
    match process.reload() {
        Ok(reloaded) => Json(reloaded_json(&process, &reloaded)).into_response(),
        Err(e @ ReloadError::Unavailable) => {
            auth::error(StatusCode::CONFLICT, "reload_unavailable", &e.to_string())
        }
        Err(e @ ReloadError::Invalid(_)) => auth::error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            &e.to_string(),
        ),
    }
}

/// `GET /ui/api/overview`:
///
/// ```json
/// {
///   "version": "0.1.0",
///   "listen": "127.0.0.1:8080",          // string, or null before the socket is bound
///   "started_at": "2026-10-11T09:00:00Z",
///   "uptime_seconds": 12,
///   "reloaded_at": null,                 // RFC 3339 time of the last config reload, or null if never reloaded
///   "auth_required": false,
///   "config_source": "file",             // "file" | "environment" | "proxy" | "inline"
///   "config_path": "gateway.toml",       // the file as given; null unless config_source is "file"
///   "totals": { ... }
/// }
/// ```
async fn overview(State(admin): State<Arc<Admin>>) -> Json<Value> {
    let (source, path) = admin.source_and_path();
    let process = &admin.process;
    let reloaded_at = *process.reloaded_at.lock();
    Json(json!({
        "version": crate::VERSION,
        "listen": process.listen.get().map(ToString::to_string),
        "started_at": rfc3339(process.started_at),
        "uptime_seconds": process.started.elapsed().as_secs(),
        "reloaded_at": reloaded_at.map(rfc3339),
        "auth_required": admin.auth_required,
        "config_source": source,
        "config_path": path,
        "totals": process.log.totals(),
        "history": process.log.capacity(),
        "respan": admin.respan.as_ref().map(|h| respan_json(&h.report())),
    }))
}

/// How sending traces to Respan goes: spans `sent`, `rejected` and
/// `failed`, traces `dropped`, the last success (`ok_at`) and `error`, and
/// whether it is `failing` now.
fn respan_json(report: &respan_gateway_plugin_respan::Report) -> Value {
    let error = report.error.as_ref();
    json!({
        "sent": report.sent,
        "rejected": report.rejected,
        "failed": report.failed,
        "dropped": report.dropped,
        "ok_at": report.ok_at.map(rfc3339),
        "error": error.map(|(at, why)| json!({ "at": rfc3339(*at), "message": why })),
        "failing": report.failing,
    })
}

async fn providers(State(admin): State<Arc<Admin>>) -> Json<Value> {
    let providers: Vec<Value> = admin
        .providers
        .iter()
        .map(|p| {
            let mut p = p.clone();
            let stats = admin
                .process
                .log
                .provider_stats(p["name"].as_str().unwrap_or_default());
            p["requests"] = stats.requests.into();
            p["errors"] = stats.errors.into();
            p["avg_latency_ms"] = stats.avg_latency_ms().into();
            p
        })
        .collect();
    Json(json!({ "providers": providers }))
}

/// `GET /ui/api/mcp`: each MCP server (`name`, `transport` (`http` or
/// `stdio`), `target` (an HTTP server's origin or a stdio server's program name, no path, arguments or environment), `endpoint` (where clients reach
/// it), `credentials` (`gateway`, `client` or `none`)), its `requests`,
/// `errors`, `avg_latency_ms`, `status`: `idle` before a request,
/// `failing` if every request failed, else `ok`; and `enabled`, whether it
/// is switched on ([`switch_server`]).
async fn mcp(State(admin): State<Arc<Admin>>) -> Json<Value> {
    let servers: Vec<Value> = admin
        .mcp
        .iter()
        .map(|s| {
            let mut s = s.clone();
            let stats = admin
                .process
                .log
                .provider_stats(s["name"].as_str().unwrap_or_default());
            s["requests"] = stats.requests.into();
            s["errors"] = stats.errors.into();
            s["avg_latency_ms"] = stats.avg_latency_ms().into();
            let name = s["name"].as_str().unwrap_or_default();
            s["enabled"] = admin.process.switches.server_on(name).into();
            s["status"] = match (stats.requests, stats.errors) {
                (0, _) => "idle",
                (r, e) if r == e => "failing",
                _ => "ok",
            }
            .into();
            s
        })
        .collect();
    let uses = |used: &Option<(Vec<String>, ToolFilter)>| {
        used.as_ref()
            .map(|(servers, _)| json!({ "servers": servers }))
    };
    Json(json!({
        "servers": servers,
        "tool_loop": uses(&admin.tool_loop),
        "aggregate": uses(&admin.aggregate).map(|mut a| {
            a["endpoint"] = "/mcp".into();
            a
        }),
    }))
}

#[derive(Debug, Deserialize)]
struct Fresh {
    /// Ask the server again rather than answer from the last minute's list.
    refresh: Option<bool>,
}

/// `GET /ui/api/mcp/{name}/tools[?refresh=true]`: the tools server `name`
/// lists now, asked through the gateway as a client asks (a stdio server
/// is started for it), each with whether the tool loop and the `/mcp`
/// aggregate offer it by their config, and whether it is switched on
/// (`enabled`, [`switch_tool`]); kept a minute, the switches read now.
///
/// ```json
/// { "tools": [{ "name": "read_file", "title": null, "description": "..",
///     "read_only": true, "destructive": null,
///     "tool_loop": true, "aggregate": false, "enabled": true }],
///   "took_ms": 812, "asked_at": "2026-10-13T09:30:00Z" }
/// ```
///
/// A server that would not list is `502`, its message saying why; an
/// unknown one `404`.
async fn mcp_tools(
    State(admin): State<Arc<Admin>>,
    Path(name): Path<String>,
    Query(q): Query<Fresh>,
) -> Response {
    if !admin.mcp.iter().any(|s| s["name"] == name.as_str()) {
        let message = format!("no MCP server named '{name}'");
        return auth::error(StatusCode::NOT_FOUND, "not_found_error", &message);
    }
    if q.refresh != Some(true)
        && let Some(listed) = admin.tools.get(&name)
    {
        return Json(switched(&admin, &name, listed)).into_response();
    }
    let started = Instant::now();
    let asked = tokio::time::timeout(
        crate::mcp_tools::TIMEOUT,
        crate::mcp_tools::list(&admin.gateway, &name),
    );
    let tools = match asked.await {
        Ok(Ok(tools)) => tools,
        Ok(Err(why)) => return auth::error(StatusCode::BAD_GATEWAY, "upstream_error", &why),
        Err(_) => {
            let why = format!(
                "it did not list its tools within {}s",
                crate::mcp_tools::TIMEOUT.as_secs()
            );
            return auth::error(StatusCode::GATEWAY_TIMEOUT, "upstream_error", &why);
        }
    };
    let offers = |used: &Option<(Vec<String>, ToolFilter)>, tool: &str| {
        used.as_ref().is_some_and(|(servers, filter)| {
            servers.contains(&name) && filter.offers(&format!("{name}__{tool}"))
        })
    };
    let tools: Vec<Value> = (tools.iter())
        .map(|t| {
            let mut tool = crate::mcp_tools::tool_json(t);
            let named = t["name"].as_str().unwrap_or_default();
            tool["tool_loop"] = offers(&admin.tool_loop, named).into();
            tool["aggregate"] = offers(&admin.aggregate, named).into();
            tool
        })
        .collect();
    let listed = json!({
        "tools": tools,
        "took_ms": started.elapsed().as_millis() as u64,
        "asked_at": rfc3339(std::time::SystemTime::now()),
    });
    admin.tools.insert(name.clone(), listed.clone());
    Json(switched(&admin, &name, listed)).into_response()
}

/// `listed`, the tools of server `name`, each with whether it is on now.
fn switched(admin: &Admin, name: &str, mut listed: Value) -> Value {
    let tools = listed["tools"].as_array_mut().into_iter().flatten();
    for tool in tools {
        let own = tool["name"].as_str().unwrap_or_default();
        tool["enabled"] = admin.process.switches.tool_on(name, own).into();
    }
    listed
}

/// The body of a switch: `{ "enabled": false }`.
#[derive(Debug, Deserialize)]
struct Switch {
    enabled: bool,
}

/// Sets the switch `set` with what `body` says, for the server `name`:
/// `204`; an unknown server `404`, a body that says nothing `400`.
fn switch(
    admin: &Admin,
    name: &str,
    body: Result<Json<Switch>, JsonRejection>,
    set: impl FnOnce(bool),
) -> Response {
    if !admin.mcp.iter().any(|s| s["name"] == name) {
        let message = format!("no MCP server named '{name}'");
        return auth::error(StatusCode::NOT_FOUND, "not_found_error", &message);
    }
    match body {
        Ok(Json(Switch { enabled })) => {
            set(enabled);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(why) => auth::error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!(
                "the body is `{{\"enabled\": true|false}}`: {}",
                why.body_text()
            ),
        ),
    }
}

/// `PUT /ui/api/mcp/{name}` with `{ "enabled": false }` turns the tool
/// server `name` off (`true` on again): the tool loop and `/mcp` offer none
/// of its tools, and `/mcp/{name}` refuses its clients (`403
/// tool_server_off`). `204`; an unknown server `404`. In memory only: kept
/// across reloads, lost on a restart.
async fn switch_server(
    State(admin): State<Arc<Admin>>,
    Path(name): Path<String>,
    body: Result<Json<Switch>, JsonRejection>,
) -> Response {
    let switches = &admin.process.switches;
    switch(&admin, &name, body, |on| switches.set_server(&name, on))
}

/// `PUT /ui/api/mcp/{name}/tools/{tool}` with `{ "enabled": false }` turns
/// the tool `tool` (its own name) of server `name` off (`true` on again):
/// the tool loop and `/mcp` neither offer it nor call it; `/mcp/{name}`,
/// which relays the server as it is, still does. `204`; an unknown server
/// `404` (any tool name may be switched). In memory only, as a server's.
async fn switch_tool(
    State(admin): State<Arc<Admin>>,
    Path((name, tool)): Path<(String, String)>,
    body: Result<Json<Switch>, JsonRejection>,
) -> Response {
    let switches = &admin.process.switches;
    switch(&admin, &name, body, |on| {
        switches.set_tool(&name, &tool, on)
    })
}

/// `GET /ui/api/routes`: the formats served, each kind's default, the
/// routes with each candidate's price and the router's record of it.
///
/// ```json
/// { "serve": [{ "format": "openai-chat", "kind": "chat", "endpoints": [".."] }],
///   "defaults": [{ "kind": "chat", "provider": "openai" }],
///   "rules": [{ "name": "smart", "glob": false, "pick": "first",
///     "needs": [], "clients": [], "kinds": [], "fallback": true,
///     "hedge_ms": null, "tries": 1,
///     "to": [{ "target": "anthropic/claude-haiku-5-5", "provider": "anthropic",
///       "model": "claude-haiku-5-5", "weight": 1,
///       "price": { "input": 15, "output": 75 },
///       "seen": { "latency_ms": 840.2, "error_rate": 0, "calls": 12,
///         "failures": 0, "cooling": false } }] }] }
/// ```
///
/// A candidate's `price` and `seen` are of the model it is sent, so `null`
/// for one that sends each request its own, as a glob's do.
async fn routes(State(admin): State<Arc<Admin>>) -> Json<Value> {
    let routes = &admin.routes;
    let rules: Vec<Value> = (routes.rules().iter())
        .map(|rule| {
            let to: Vec<Value> = (rule.to.iter())
                .map(|c| {
                    // A glob's candidate sends each request its own model,
                    // unless it names one.
                    let model = match rule.is_glob() {
                        true => c.model.clone().filter(|m| !m.contains("{model}")),
                        false => c.model_for(Some(&rule.name)),
                    };
                    let mut row = json!({
                        "target": c.to_string(),
                        "provider": c.provider,
                        "model": model,
                        "weight": c.weight,
                        "price": null,
                        "seen": null,
                    });
                    if let Some(model) = &model {
                        row["price"] = price(routes, &c.provider, model);
                        row["seen"] = seen(routes, &c.provider, model);
                    }
                    row
                })
                .collect();
            json!({
                "name": rule.name,
                "glob": rule.is_glob(),
                "pick": rule.pick.name(),
                "needs": rule.needs,
                "clients": rule.clients,
                "kinds": rule.kinds,
                "fallback": rule.fallback,
                "hedge_ms": rule.hedge.map(|h| h.as_millis() as u64),
                "tries": rule.backoff.tries(),
                "to": to,
            })
        })
        .collect();
    let defaults = routes.defaults();
    let defaults = defaults
        .iter()
        .map(|(kind, provider)| json!({ "kind": kind, "provider": provider }));
    Json(json!({
        "serve": admin.serve,
        "defaults": defaults.collect::<Vec<_>>(),
        "rules": rules,
    }))
}

/// A model's price, per million tokens, as the router reads it; `null` if
/// unknown.
fn price(routes: &Routes, provider: &str, model: &str) -> Value {
    json!(routes.catalog().get(provider, model).and_then(|m| m.price))
}

/// The router's record of `model` of `provider`; `null` before its first call.
fn seen(routes: &Routes, provider: &str, model: &str) -> Value {
    routes
        .stats()
        .seen(provider, model)
        .map_or(Value::Null, |s| {
            json!({
                "latency_ms": s.latency_ms,
                "error_rate": s.error_rate,
                "calls": s.calls,
                "failures": s.failures,
                "cooling": s.cooling,
            })
        })
}

#[derive(Debug, Deserialize)]
struct Explain {
    model: Option<String>,
    kind: Option<String>,
    client: Option<String>,
}

/// `GET /ui/api/routes/explain?model=M&kind=chat&client=NAME`: where a
/// request would go now, sending nothing: why, in which order, and the
/// candidates passed over.
///
/// ```json
/// { "by": { "type": "rule", "name": "smart" }, "pick": "cheapest",
///   "mode": "fallback",
///   "to": [{ "provider": "openai", "model": "gpt-5-mini",
///     "price": { "input": 0.25, "output": 2 }, "seen": null }],
///   "passed": [{ "candidate": "smart → gemini", "why": "lacks tools" }] }
/// ```
///
/// `by.type` is `rule`, `named` (`provider/model`), `listed` (by the
/// provider's model listing) or `default`; `mode` is `single`, `fallback` or `hedge`. A
/// request no provider serves is `{ "by": null }`.
async fn explain(State(admin): State<Arc<Admin>>, Query(q): Query<Explain>) -> Response {
    let kind = q.kind.as_deref().unwrap_or("chat");
    let Some((kind, format)) = admin.kinds.iter().find(|(k, _)| *k == kind) else {
        let message = format!("no format of kind '{kind}' is served");
        return auth::error(StatusCode::BAD_REQUEST, "invalid_request_error", &message);
    };
    let model = q.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let ask = Ask {
        kind,
        format,
        model,
        client: q.client.as_deref(),
        tools: true,
    };
    let Some(planned) = admin.routes.plan(&ask) else {
        return Json(json!({ "by": null })).into_response();
    };
    let (by, rule) = match &planned.by {
        By::Rule(rule) => (json!({ "type": "rule", "name": rule.name }), Some(rule)),
        By::Named => (json!({ "type": "named" }), None),
        By::Listed => (json!({ "type": "listed" }), None),
        By::Default => (json!({ "type": "default" }), None),
    };
    let mode = match rule {
        Some(rule) if rule.hedge.is_some() => "hedge",
        Some(_) if planned.to.len() > 1 => "fallback",
        _ => "single",
    };
    let to: Vec<Value> = (planned.to.iter())
        .map(|c| {
            let model = c.model.as_deref().unwrap_or_default();
            json!({
                "provider": c.provider,
                "model": c.model,
                "price": price(&admin.routes, &c.provider, model),
                "seen": seen(&admin.routes, &c.provider, model),
            })
        })
        .collect();
    let passed = planned.passed.iter();
    let passed = passed.map(|(candidate, why)| json!({ "candidate": candidate, "why": why }));
    Json(json!({
        "by": by,
        "pick": rule.map(|r| r.pick.name()),
        "mode": mode,
        "to": to,
        "passed": passed.collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct Recent {
    limit: Option<usize>,
    #[serde(flatten)]
    filter: Filter,
}

/// `GET /ui/api/requests?limit=N`: the last requests, newest first, narrowed
/// by the export's filters (`kind`, `status`, `provider`, `client`, `model`,
/// `q`, `since`).
async fn requests(State(admin): State<Arc<Admin>>, Query(q): Query<Recent>) -> Json<Value> {
    let requests = admin.process.log.recent(q.limit.unwrap_or(100), &q.filter);
    Json(json!({ "requests": requests }))
}

#[derive(Debug, Deserialize)]
struct ExportQuery {
    /// `jsonl` (the default) or `csv`.
    format: Option<String>,
    #[serde(flatten)]
    filter: Filter,
}

/// `GET /ui/api/requests/export?format=jsonl|csv`: the kept requests,
/// oldest first, one record per line (`jsonl`, the records of
/// `/ui/api/requests`; or `csv` with a header row), as a download. The
/// filters narrow it as the UI's do, all optional: `kind`; `status` (`ok`,
/// `error`, `pending`, or a class such as `2xx`); `provider`; `client` (a key's name); `model`;
/// `q` (free text, matched in any case against the model and the path);
/// `since` (an RFC 3339 time, or a date for its midnight UTC; 400 if
/// neither).
async fn export(State(admin): State<Arc<Admin>>, Query(q): Query<ExportQuery>) -> Response {
    let (content_type, name, header): (_, _, Option<&str>) = match q.format.as_deref() {
        None | Some("jsonl") => ("application/x-ndjson", "requests.jsonl", None),
        Some("csv") => ("text/csv; charset=utf-8", "requests.csv", Some(CSV_HEADER)),
        Some(other) => {
            let message = format!("format: '{other}' is not jsonl or csv");
            return auth::error(StatusCode::BAD_REQUEST, "invalid_request_error", &message);
        }
    };
    let csv = header.is_some();
    let line = move |r: RequestRecord| {
        let line = match csv {
            true => csv_row(&r),
            false => serde_json::to_string(&r).unwrap_or_default(),
        };
        Ok::<_, Infallible>(format!("{line}\n"))
    };
    let head = header.map(|h| Ok(format!("{h}\n")));
    let rows = admin.process.log.export(&q.filter).into_iter().map(line);
    let body = Body::from_stream(stream::iter(head.into_iter().chain(rows)));
    let headers = [
        (CONTENT_TYPE, HeaderValue::from_static(content_type)),
        (
            CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
                .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
        ),
        (CACHE_CONTROL, HeaderValue::from_static("no-store")),
    ];
    (headers, body).into_response()
}

async fn request(State(admin): State<Arc<Admin>>, Path(id): Path<String>) -> Response {
    match admin.process.log.get(&id) {
        Some(record) => Json(record).into_response(),
        None => not_found().await,
    }
}

async fn not_found() -> Response {
    not_found_response()
}

fn not_found_response() -> Response {
    auth::error(StatusCode::NOT_FOUND, "not_found_error", "not found")
}

/// Every request as it starts and finishes, with a comment each 15 s,
/// until the server stops.
async fn events(
    State(admin): State<Arc<Admin>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = admin.process.log.subscribe();
    let events = stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(json) => {
                    let event = Event::default().event("request").data(&*json);
                    return Some((Ok(event), receiver));
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    let mut stopping = admin.process.stopping.subscribe();
    let stopped = async move {
        let _ = stopping.wait_for(|stop| *stop).await;
    };
    Sse::new(events.take_until(stopped)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// The UI's file at `path`, or its index for a path of the app's own; a
/// missing file under `assets/` is a 404.
fn asset(path: &str) -> Response {
    let file = |path: &str| assets::ASSETS.iter().find(|(name, _)| *name == path);
    let found = match path.starts_with("assets/") {
        true => file(path),
        false => file(path).or_else(|| file("index.html")),
    };
    let Some(&(name, bytes)) = found else {
        return not_found_response();
    };
    let cache = match name.starts_with("assets/") {
        true => "public, max-age=31536000, immutable",
        false => "no-cache",
    };
    let headers = [
        (CONTENT_TYPE, HeaderValue::from_static(content_type(name))),
        (CACHE_CONTROL, HeaderValue::from_static(cache)),
    ];
    (headers, bytes).into_response()
}

fn content_type(name: &str) -> &'static str {
    let extension = name.rsplit_once('.').map_or("", |(_, e)| e);
    match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, Request};
    use respan_gateway_http::http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::App;

    const TOOLS: &str = "[tools]\na = { url = \"http://a.test/mcp\" }\n";

    async fn send(app: &App, method: Method, path: &str, body: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_owned()))
            .unwrap_or_else(|e| panic!("{e}"));
        let response = app.router().oneshot(request).await;
        let response = response.unwrap_or_else(|never| match never {});
        let status = response.status();
        let bytes = response.into_body().collect().await;
        let bytes = bytes.map(|b| b.to_bytes()).unwrap_or_default();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_server_switched_off_stays_off_across_a_reload_and_is_refused() {
        let config = || Config::parse(TOOLS).unwrap_or_else(|e| panic!("{e}"));
        let app = App::new(config())
            .unwrap_or_else(|e| panic!("{e}"))
            .reload_from(move || Ok(config()));
        let enabled = |listed: &Value| listed["servers"][0]["enabled"].clone();
        let (_, listed) = send(&app, Method::GET, "/ui/api/mcp", "").await;
        assert_eq!(enabled(&listed), json!(true));

        let off = r#"{"enabled": false}"#;
        let (status, _) = send(&app, Method::PUT, "/ui/api/mcp/a", off).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = send(&app, Method::PUT, "/ui/api/mcp/nope", off).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&app, Method::PUT, "/ui/api/mcp/a", "{}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(&app, Method::PUT, "/ui/api/mcp/a/tools/x", off).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = send(&app, Method::GET, "/ui/api/mcp/a", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert!(app.reload().is_ok());
        let (_, listed) = send(&app, Method::GET, "/ui/api/mcp", "").await;
        assert_eq!(enabled(&listed), json!(false));
        let hello = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let (status, refused) = send(&app, Method::POST, "/mcp/a", &hello.to_string()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(
            refused["error"]["data"]["code"], "tool_server_off",
            "{refused}"
        );

        let on = r#"{"enabled": true}"#;
        let (status, _) = send(&app, Method::PUT, "/ui/api/mcp/a", on).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, listed) = send(&app, Method::GET, "/ui/api/mcp", "").await;
        assert_eq!(enabled(&listed), json!(true));
    }
}
