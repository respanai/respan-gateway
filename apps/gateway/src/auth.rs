//! Whose keys go where. Client keys: when the config sets `client_keys`, every
//! request carries one, as `Authorization: Bearer`, `x-api-key` or
//! `x-goog-api-key`, and the gateway takes it off: no provider sees it. A
//! browser's WebSocket can set no header, so on an upgrade a key may also
//! come as a subprotocol (`realtime, openai-insecure-api-key.<key>`) or as a
//! query parameter (`?key=<key>`), and goes from there likewise.
//! Clients' own provider keys go only to a provider of their format that
//! has none of its own.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use respan_gateway_core::Extensions;
use respan_gateway_http::Credential;
use respan_gateway_http::http::header::{AUTHORIZATION, HOST, HeaderName, ORIGIN, UPGRADE};
use respan_gateway_http::http::{HeaderMap, HeaderValue, Method, StatusCode};
use respan_gateway_plugin_tool_loop::ToolScope;

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const X_GOOG_API_KEY: HeaderName = HeaderName::from_static("x-goog-api-key");
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

/// A client key, read, and the name its requests are logged under.
#[derive(Clone, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    pub key: String,
    /// Whether it reaches the admin API.
    pub admin: bool,
    /// The tools its requests may use, if it is limited.
    pub tools: Option<Vec<String>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("name", &self.name)
            .field("admin", &self.admin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct ClientKeys(Vec<Client>);

/// The name of a request's client key, for routing's rules of some
/// clients alone.
#[derive(Debug, Clone)]
pub struct KeyName(pub String);

/// What a request's headers say to the client keys.
#[derive(Debug, PartialEq, Eq)]
enum Presented {
    /// No key is needed.
    Open,
    /// The headers that hold a valid key, each one to take off.
    Valid(Vec<HeaderName>),
    Refused,
}

impl ClientKeys {
    pub fn new(keys: Vec<Client>) -> Arc<Self> {
        Arc::new(Self(keys))
    }

    pub fn required(&self) -> bool {
        !self.0.is_empty()
    }

    /// The client `key` is the key of, if any.
    fn client(&self, key: &str) -> Option<&Client> {
        let key = key.trim().as_bytes();
        let found = |c: &&Client| constant_time_eq(c.key.as_bytes(), key);
        match key.is_empty() {
            true => None,
            false => self.0.iter().find(found),
        }
    }

    /// Whether `key` is one of the client keys.
    fn is_key(&self, key: &str) -> bool {
        self.client(key).is_some()
    }

    fn presented(&self, headers: &HeaderMap) -> Presented {
        if !self.required() {
            return Presented::Open;
        }
        let valid = |key: &str| self.is_key(key);
        let value = |name: &HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
        let holds_key = |name: &HeaderName| match *name == AUTHORIZATION {
            true => bearer(headers).is_some_and(valid),
            false => value(name).is_some_and(valid),
        };
        let holders: Vec<HeaderName> = [AUTHORIZATION, X_API_KEY, X_GOOG_API_KEY]
            .into_iter()
            .filter(holds_key)
            .collect();
        match holders.is_empty() {
            true => Presented::Refused,
            false => Presented::Valid(holders),
        }
    }

    /// The client whose key `headers` present, if any. A key counts where
    /// [`ClientKeys::presented`] would take it: under `Authorization` only
    /// with the `Bearer` scheme.
    fn identify(&self, headers: &HeaderMap) -> Option<&Client> {
        let others = [X_API_KEY, X_GOOG_API_KEY]
            .into_iter()
            .filter_map(|name| headers.get(name)?.to_str().ok());
        let mut presented = bearer(headers).into_iter().chain(others);
        presented.find_map(|key| self.client(key))
    }

    /// Whether the request may use the admin API: any request when there
    /// are no client keys, else one with an admin client's key.
    pub fn admin_allows(&self, headers: &HeaderMap) -> bool {
        !self.required() || self.identify(headers).is_some_and(|c| c.admin)
    }

    pub fn allows(&self, headers: &HeaderMap) -> bool {
        self.presented(headers) != Presented::Refused
    }
}

/// Puts what the gateway knows of `client` on a request's extensions: its
/// name, in the request record and as its [`KeyName`], and the tools it is
/// limited to, if it is.
fn attribute(req: &mut Request, client: &Client) {
    // The gateway's own extensions ride in the request's.
    let mut calls = req.extensions_mut().remove::<Extensions>();
    let calls = calls.get_or_insert_with(Extensions::new);
    crate::requests::attribute(calls, &client.name);
    calls.insert(KeyName(client.name.clone()));
    if let Some(tools) = &client.tools {
        calls.insert(ToolScope::new(tools.clone()));
    }
    let calls = std::mem::take(calls);
    req.extensions_mut().insert(calls);
}

/// The token of an `Authorization: Bearer <token>` header, if `headers` have
/// one. Another scheme, or none, holds no gateway key.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

/// Whether `a` and `b` are equal, in time that depends on their lengths alone.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The 401 of a request without a valid key, in OpenAI's error shape.
pub fn unauthorized() -> Response {
    let message = "a valid gateway key is required: send it as `Authorization: Bearer <key>`, `x-api-key` or `x-goog-api-key` (a WebSocket from a browser: as a subprotocol `openai-insecure-api-key.<key>` or `?key=<key>`)";
    error(StatusCode::UNAUTHORIZED, "authentication_error", message)
}

/// An error of the gateway's own, in OpenAI's error shape.
pub fn error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({"error": {"message": message, "type": kind}});
    (status, axum::Json(body)).into_response()
}

/// Refuses a request without a valid key, and takes every header holding
/// one off one with: a client's own provider key in another header stays.
pub async fn consume(
    State(keys): State<Arc<ClientKeys>>,
    mut req: Request,
    next: Next,
) -> Response {
    match keys.presented(req.headers()) {
        Presented::Refused if is_upgrade(req.headers()) => match keys.browser(req) {
            Some(req) => next.run(req).await,
            None => unauthorized(),
        },
        Presented::Refused => unauthorized(),
        Presented::Open => next.run(req).await,
        Presented::Valid(holders) => {
            if let Some(client) = keys.identify(req.headers()) {
                attribute(&mut req, client);
            }
            for name in holders {
                req.headers_mut().remove(name);
            }
            next.run(req).await
        }
    }
}

/// Whether the request asks to become a WebSocket.
fn is_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers.get(UPGRADE).and_then(|v| v.to_str().ok());
    upgrade.is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

impl ClientKeys {
    /// The WebSocket upgrade `req` without the gateway key a browser
    /// offered as a subprotocol or query parameter, if it offered one. The
    /// rest of the client's credentials stay.
    fn browser(&self, req: Request) -> Option<Request> {
        let (mut head, body) = req.into_parts();
        let mut credential = Credential::lift(&mut head)?;
        let client = std::cell::Cell::new(None);
        let found = credential.take_keys(|key| {
            let found = self.client(key);
            if client.get().is_none() {
                client.set(found);
            }
            found.is_some()
        });
        credential.restore(&mut head);
        let mut req = Request::from_parts(head, body);
        if let Some(client) = client.get() {
            attribute(&mut req, client);
        }
        found.then_some(req)
    }
}

/// The mark of a request from the process that owns the gateway: the
/// embedding library's own reads of the admin API. It is a typed extension of
/// the in-process request, set only by that code; no header, query or body
/// can make one, so a request from the network never has it. `require`
/// accepts it in place of a client key for reads: the admin API and the
/// model lists.
#[derive(Debug, Clone, Copy)]
pub struct Trusted;

/// Whether `req` is a trusted read: a `GET` of the admin API or of a model
/// list.
fn is_trusted(req: &Request) -> bool {
    let path = req.uri().path();
    let read = req.method() == Method::GET
        && (path.starts_with("/ui/api/")
            || path == "/v1/models"
            || path.starts_with("/v1beta/models"));
    read && req.extensions().get::<Trusted>().is_some()
}

/// Refuses a request without a valid key, leaving it as it is. A
/// [`Trusted`] read needs none.
pub async fn require(State(keys): State<Arc<ClientKeys>>, req: Request, next: Next) -> Response {
    match is_trusted(&req) || keys.allows(req.headers()) {
        true => next.run(req).await,
        false => unauthorized(),
    }
}

/// Like [`require`], and the key must be an admin client's (see
/// `client_keys`): another valid key is refused with `403`.
pub async fn require_admin(
    State(keys): State<Arc<ClientKeys>>,
    req: Request,
    next: Next,
) -> Response {
    if is_trusted(&req) || !keys.required() {
        return next.run(req).await;
    }
    match (keys.allows(req.headers()), keys.admin_allows(req.headers())) {
        (_, true) => next.run(req).await,
        (true, false) => error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this client key is not an admin key (`admin: true` in `client_keys`)",
        ),
        (false, _) => unauthorized(),
    }
}

/// Browser access: which origins' pages may call the gateway.
///
/// A preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`)
/// from an allowed origin is answered here, without a key, since a browser
/// sends none with it. Other requests carry the gateway's usual key, and an
/// allowed origin's get the `Access-Control-*` headers a page needs to read
/// the answer, including the MCP session header. Origins that are not
/// allowed get nothing, so the browser refuses them.
#[derive(Debug)]
pub struct Cors {
    origins: Vec<String>,
    any: bool,
}

/// The headers a preflight that asks for none may be answered with: the
/// keys, the SDKs' own, and MCP's.
const ALLOWED_HEADERS: &str = "Authorization, x-api-key, x-goog-api-key, anthropic-version, \
    anthropic-beta, openai-beta, content-type, mcp-session-id, mcp-protocol-version, \
    last-event-id, x-respan-tool-loop";
/// The headers a page may read.
const EXPOSED_HEADERS: &str = "mcp-session-id, mcp-protocol-version, retry-after";
const ALLOWED_METHODS: &str = "GET, POST, PUT, DELETE, OPTIONS";

impl Cors {
    /// The allowed `origins`: `scheme://host[:port]` each, or `*`, which
    /// needs `keys`, since without them any page could spend the gateway's
    /// provider keys.
    pub fn new(origins: &[String], keys: bool) -> Result<Arc<Self>, String> {
        if origins.is_empty() {
            return Err("cors: `origins` names no origin".to_owned());
        }
        let any = origins.iter().any(|o| o == "*");
        if any && !keys {
            return Err(
                "cors: origin '*' needs `client_keys`; without keys any web page \
                        could spend the gateway's provider keys"
                    .to_owned(),
            );
        }
        for origin in origins.iter().filter(|o| *o != "*") {
            let (scheme, host) = origin.split_once("://").unwrap_or(("", origin));
            let plain = !host.is_empty() && !host.contains(['/', '?', '#', '@', ' ']);
            if !matches!(scheme, "http" | "https") || !plain {
                return Err(format!(
                    "cors: origin '{origin}' is not `scheme://host[:port]` (http or https, \
                     no path, no trailing slash), or `*`"
                ));
            }
        }
        let origins = origins.iter().map(|o| o.to_ascii_lowercase()).collect();
        Ok(Arc::new(Self { origins, any }))
    }

    fn allows(&self, origin: &str) -> bool {
        self.any || self.origins.iter().any(|o| o.eq_ignore_ascii_case(origin))
    }
}

/// The headers a preflight asks to send (`Access-Control-Request-Headers`),
/// as the page's origin is already allowed: browser SDKs send ones of their
/// own (`x-stainless-*`, `anthropic-dangerous-direct-browser-access`). Only
/// header-name characters are echoed. A preflight that asks for none gets
/// the usual list.
fn allowed_headers(req: &Request) -> HeaderValue {
    let asked = req
        .headers()
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok());
    let token = |name: &&str| {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    };
    let names: Vec<&str> = asked
        .into_iter()
        .flat_map(|v| v.split(',').map(str::trim))
        .filter(token)
        .collect();
    match names.is_empty() {
        true => HeaderValue::from_static(ALLOWED_HEADERS),
        false => HeaderValue::from_str(&names.join(", "))
            .unwrap_or_else(|_| HeaderValue::from_static(ALLOWED_HEADERS)),
    }
}

/// Answers an allowed origin's preflight, and marks its other answers.
pub async fn cors(State(cors): State<Arc<Cors>>, req: Request, next: Next) -> Response {
    let origin = req.headers().get(ORIGIN).cloned();
    let Some(origin) = origin.filter(|o| o.to_str().is_ok_and(|o| cors.allows(o))) else {
        return next.run(req).await;
    };
    let preflight = req.method() == Method::OPTIONS
        && req.headers().contains_key("access-control-request-method");
    let mut resp = match preflight {
        true => {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::NO_CONTENT;
            let headers = resp.headers_mut();
            headers.insert(
                "access-control-allow-methods",
                HeaderValue::from_static(ALLOWED_METHODS),
            );
            headers.insert("access-control-allow-headers", allowed_headers(&req));
            headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
            resp
        }
        false => next.run(req).await,
    };
    let headers = resp.headers_mut();
    headers.insert("access-control-allow-origin", origin);
    headers.insert(
        "access-control-expose-headers",
        HeaderValue::from_static(EXPOSED_HEADERS),
    );
    headers.append("vary", HeaderValue::from_static("Origin"));
    resp
}

/// Without client keys, whoever reaches the gateway spends its provider
/// keys. On loopback that is meant to be local programs alone, but a web
/// page the user opens reaches it too: by a cross-site request, which a
/// browser sends without asking when it is "simple", or by a DNS name of
/// the attacker's rebound to 127.0.0.1, which makes its pages same-origin
/// with the gateway. So a gateway without keys refuses a browser's
/// cross-origin request, and a `Host` that is not one of its own names.
/// With keys neither is needed: a browser never adds a gateway key itself.
#[derive(Debug)]
pub struct LocalOnly {
    configured: SocketAddr,
    /// Where it was bound, once served.
    bound: OnceLock<SocketAddr>,
    /// Origins the config lets call it (`cors:`), which are not cross-site
    /// here.
    allowed: Option<Arc<Cors>>,
}

impl LocalOnly {
    pub fn new(configured: SocketAddr, allowed: Option<Arc<Cors>>) -> Arc<Self> {
        let bound = OnceLock::new();
        Arc::new(Self {
            configured,
            bound,
            allowed,
        })
    }

    pub fn serving_on(&self, addr: SocketAddr) {
        let _ = self.bound.set(addr);
    }

    /// Whether it answers to the host `name`: any, when it listens on every
    /// address.
    fn answers_to(&self, name: &str) -> bool {
        let listen = self.bound.get().unwrap_or(&self.configured);
        let own = match listen.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        let names = ["localhost", "127.0.0.1", "[::1]", "respan-gateway"];
        listen.ip().is_unspecified() || names.contains(&name) || name == own
    }

    /// Why a request is refused, if it is.
    fn refusal(&self, req: &Request) -> Option<String> {
        let headers = req.headers();
        let value = |name| {
            headers
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
        };
        let host = value(HOST).or_else(|| req.uri().authority().map(|a| a.as_str()));
        if let Some(host) = host {
            let name = host_name(host).to_ascii_lowercase();
            if !self.answers_to(&name) {
                return Some(format!(
                    "Host '{name}' is not this gateway's: without client keys it answers \
                     only to localhost; set `client_keys` to reach it by another name"
                ));
            }
        }
        if let (Some(cors), Some(origin)) = (&self.allowed, value(ORIGIN))
            && cors.allows(origin)
        {
            return None;
        }
        let cross_site = value(SEC_FETCH_SITE) == Some("cross-site");
        let origin = value(ORIGIN).map(|origin| origin.split_once("://").map(|(_, at)| at));
        let same_origin =
            |at: Option<&str>| at.zip(host).is_some_and(|(a, h)| a.eq_ignore_ascii_case(h));
        if cross_site || origin.is_some_and(|at| !same_origin(at)) {
            return Some(
                "a cross-origin browser request is refused: without client keys any web \
                 page could spend the gateway's provider keys; set `client_keys` to allow it"
                    .to_owned(),
            );
        }
        None
    }
}

/// `host` without its port.
fn host_name(host: &str) -> &str {
    match host.strip_prefix('[') {
        Some(rest) => rest.find(']').map_or(host, |end| &host[..end + 2]),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    }
}

/// Refuses a request [`LocalOnly`] refuses, 403.
pub async fn local_only(State(local): State<Arc<LocalOnly>>, req: Request, next: Next) -> Response {
    match local.refusal(&req) {
        Some(why) => error(StatusCode::FORBIDDEN, "permission_error", &why),
        None => next.run(req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            let value = value.parse().unwrap_or_else(|_| panic!("{value}"));
            headers.insert(*name, value);
        }
        headers
    }

    fn client(name: &str, key: &str) -> Client {
        Client {
            name: name.to_owned(),
            key: key.to_owned(),
            admin: true,
            tools: None,
        }
    }

    #[test]
    fn a_key_is_read_from_any_of_three_headers() {
        let keys = ClientKeys(vec![client("a", "k1"), client("b", "k2")]);
        let presented = |pairs: &[(&'static str, &str)]| keys.presented(&headers(pairs));
        let valid = |names: &[HeaderName]| Presented::Valid(names.to_vec());
        assert_eq!(
            presented(&[("authorization", "Bearer k1")]),
            valid(&[AUTHORIZATION])
        );
        assert_eq!(presented(&[("x-api-key", "k2")]), valid(&[X_API_KEY]));
        assert_eq!(
            presented(&[("x-goog-api-key", "k1")]),
            valid(&[X_GOOG_API_KEY])
        );
        // The client's provider key in one header, the gateway's in another.
        let both = [("authorization", "Bearer sk-provider"), ("x-api-key", "k1")];
        assert_eq!(presented(&both), valid(&[X_API_KEY]));
        assert_eq!(
            presented(&[("authorization", "Bearer k3")]),
            Presented::Refused
        );
        assert_eq!(presented(&[("authorization", "k1")]), Presented::Refused);
        assert_eq!(presented(&[]), Presented::Refused);
    }

    #[test]
    fn a_client_key_is_known_by_its_name() {
        let keys = ClientKeys(vec![client("a", "k1"), client("b", "k2")]);
        let name = |pairs: &[(&'static str, &str)]| {
            let found = keys.identify(&headers(pairs));
            found.map(|c| c.name.clone())
        };
        assert_eq!(name(&[("x-api-key", "k2")]).as_deref(), Some("b"));
        assert_eq!(
            name(&[("authorization", "Bearer k1")]).as_deref(),
            Some("a")
        );
        assert_eq!(name(&[("authorization", "Bearer sk-provider")]), None);
        // The gateway's key is the one named, not the client's own.
        let both = [("authorization", "Bearer sk-provider"), ("x-api-key", "k2")];
        assert_eq!(name(&both).as_deref(), Some("b"));
    }

    #[test]
    fn only_admin_clients_reach_the_admin_api() {
        let mut ops = client("ops", "k2");
        let mut app = client("app", "k1");
        app.admin = false;
        ops.admin = true;
        let keys = ClientKeys(vec![app, ops]);
        let admin = |pairs: &[(&'static str, &str)]| keys.admin_allows(&headers(pairs));
        assert!(admin(&[("x-api-key", "k2")]));
        assert!(!admin(&[("x-api-key", "k1")]));
        assert!(!admin(&[]));
        assert!(ClientKeys(Vec::new()).admin_allows(&headers(&[])));
    }

    #[test]
    fn an_admin_key_under_another_scheme_does_not_reach_the_admin_api() {
        let keys = ClientKeys(vec![client("ops", "k2")]);
        let admin = |value: &str| keys.admin_allows(&headers(&[("authorization", value)]));
        assert!(admin("Bearer k2"));
        assert!(admin("bearer k2"));
        // The rest of the API refuses these, so the admin API does too.
        for value in ["Basic k2", "Token k2", "Digest k2"] {
            assert!(
                !keys.allows(&headers(&[("authorization", value)])),
                "{value}"
            );
            assert!(!admin(value), "{value}");
        }
    }

    #[test]
    fn a_request_is_named_for_the_key_that_let_it_in() {
        let keys = ClientKeys(vec![client("a", "k1"), client("b", "k2")]);
        // `b`'s key lets the request in; `a`'s under another scheme does not.
        let pairs = [("authorization", "Basic k1"), ("x-api-key", "k2")];
        assert_eq!(
            keys.presented(&headers(&pairs)),
            Presented::Valid(vec![X_API_KEY])
        );
        let named = keys.identify(&headers(&pairs)).map(|c| c.name.as_str());
        assert_eq!(named, Some("b"));
    }

    #[test]
    fn a_preflight_is_answered_with_the_headers_it_asks_for() {
        let ask = |value: &str| {
            let req = Request::builder()
                .header("access-control-request-headers", value)
                .body(axum::body::Body::empty())
                .expect("request");
            allowed_headers(&req).to_str().expect("text").to_owned()
        };
        assert_eq!(
            ask("x-stainless-os, Anthropic-Dangerous-Direct-Browser-Access,content-type"),
            "x-stainless-os, Anthropic-Dangerous-Direct-Browser-Access, content-type"
        );
        // Anything that is not a header name is dropped.
        assert_eq!(ask("x-a, bad name, x-b\"c"), "x-a");
        assert_eq!(ask("   "), ALLOWED_HEADERS);
    }

    #[test]
    fn every_header_holding_a_valid_key_is_taken_off() {
        let keys = ClientKeys(vec![client("a", "k1")]);
        let all = [
            ("authorization", "Bearer k1"),
            ("x-api-key", "k1"),
            ("x-goog-api-key", " k1 "),
        ];
        assert_eq!(
            keys.presented(&headers(&all)),
            Presented::Valid(vec![AUTHORIZATION, X_API_KEY, X_GOOG_API_KEY])
        );
    }

    #[test]
    fn an_empty_key_never_matches() {
        let keys = ClientKeys(vec![client("a", "")]);
        assert_eq!(
            keys.presented(&headers(&[("x-api-key", "")])),
            Presented::Refused
        );
        assert_eq!(
            keys.presented(&headers(&[("authorization", "Bearer ")])),
            Presented::Refused
        );
    }

    #[test]
    fn without_keys_everything_passes() {
        let keys = ClientKeys::default();
        assert!(!keys.required());
        assert_eq!(keys.presented(&HeaderMap::new()), Presented::Open);
    }

    fn refused(local: &LocalOnly, pairs: &[(&'static str, &str)]) -> bool {
        let mut req = Request::new(axum::body::Body::empty());
        *req.headers_mut() = headers(pairs);
        local.refusal(&req).is_some()
    }

    #[test]
    fn without_keys_a_cross_origin_browser_request_is_refused() {
        let local = LocalOnly::new(
            "127.0.0.1:8080".parse().unwrap_or_else(|e| panic!("{e}")),
            None,
        );
        let host = ("host", "127.0.0.1:8080");
        assert!(!refused(&local, &[host]), "curl");
        assert!(
            !refused(&local, &[host, ("origin", "http://127.0.0.1:8080")]),
            "the UI"
        );
        assert!(refused(&local, &[host, ("origin", "https://evil.example")]));
        assert!(refused(
            &local,
            &[host, ("origin", "http://localhost:3000")]
        ));
        assert!(refused(&local, &[host, ("origin", "null")]));
        assert!(refused(&local, &[host, ("sec-fetch-site", "cross-site")]));
    }

    #[test]
    fn without_keys_only_the_gateways_own_host_names_are_answered() {
        let local = LocalOnly::new(
            "127.0.0.1:8080".parse().unwrap_or_else(|e| panic!("{e}")),
            None,
        );
        for host in [
            "localhost:8080",
            "127.0.0.1",
            "[::1]:8080",
            "respan-gateway",
            "LOCALHOST",
        ] {
            assert!(!refused(&local, &[("host", host)]), "{host}");
        }
        assert!(!refused(&local, &[]), "no Host");
        assert!(refused(&local, &[("host", "evil.example:8080")]));
        assert!(refused(&local, &[("host", "localhost.evil.example")]));
        let everywhere = LocalOnly::new(
            "0.0.0.0:8080".parse().unwrap_or_else(|e| panic!("{e}")),
            None,
        );
        assert!(!refused(&everywhere, &[("host", "gateway.lan:8080")]));
        let own = LocalOnly::new(
            "10.0.0.2:8080".parse().unwrap_or_else(|e| panic!("{e}")),
            None,
        );
        assert!(!refused(&own, &[("host", "10.0.0.2:8080")]));
        // Served elsewhere than configured, as a host may.
        local.serving_on("0.0.0.0:9".parse().unwrap_or_else(|e| panic!("{e}")));
        assert!(!refused(&local, &[("host", "gateway.lan:9")]));
    }

    fn request(uri: &str, trusted: bool) -> Request {
        let mut req = Request::builder().uri(uri).body(axum::body::Body::empty());
        if let Ok(req) = req.as_mut()
            && trusted
        {
            req.extensions_mut().insert(Trusted);
        }
        req.unwrap_or_else(|e| panic!("{e}"))
    }

    async fn status(keys: &Arc<ClientKeys>, req: Request) -> StatusCode {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route("/ui/api/x", axum::routing::get(|| async { "ok" }))
            .route("/v1/x", axum::routing::get(|| async { "ok" }))
            .route("/v1/models", axum::routing::get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(keys.clone(), require));
        app.oneshot(req)
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .status()
    }

    #[tokio::test]
    async fn only_the_in_process_mark_stands_in_for_a_key_and_only_for_reads() {
        let keys = ClientKeys::new(vec![client("a", "k1")]);
        assert_eq!(status(&keys, request("/ui/api/x", true)).await, 200);
        assert_eq!(status(&keys, request("/ui/api/x", false)).await, 401);
        assert_eq!(status(&keys, request("/v1/x", true)).await, 401);
        assert_eq!(status(&keys, request("/v1/models", true)).await, 200);
        assert_eq!(status(&keys, request("/v1/models", false)).await, 401);
        // Nothing a remote client sends is the mark.
        let mut forged = request("/ui/api/x", false);
        for name in ["trusted", "x-respan-trusted", "x-respan-tool-loop", "host"] {
            let value = HeaderValue::from_static("respan-gateway");
            forged.headers_mut().insert(name, value);
        }
        assert_eq!(status(&keys, forged).await, 401);
    }
}
