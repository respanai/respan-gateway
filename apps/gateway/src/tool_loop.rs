//! The tool loop of the binary: `plugins.tool_loop` in the config names tool servers
//! whose tools the gateway offers to the model in chat requests and live
//! sessions and runs for it ([`respan_gateway_plugin_tool_loop`] is the loop; this is the tools).
//!
//! The tools are reached as the gateway reaches any MCP server: as its
//! providers, with the server's own credentials, never a client's. The
//! gateway is one MCP client of its own: it keeps one session per server across requests, started at the first use
//! and started again if the server lost it, and keeps each server's tool list
//! for a while.
//!
//! When the config reloads, the old build's sessions are ended once its
//! requests are done ([`Closer`]), not left to the server's idle timeout:
//! the app sends the old gateway a request of its own, marked with an
//! extension no client can send, which `DELETE`s each session (an HTTP
//! server's, or a stdio server's child) through the same providers.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use respan_gateway_conversation::Tool;
use respan_gateway_core::futures::StreamExt;
use respan_gateway_core::futures::stream;
use respan_gateway_core::{
    Call, ClientContext, Extensions, Fault, Gateway, Plugin, Response, async_trait,
};
use respan_gateway_http::Body;
use respan_gateway_http::http::{self, Method};
use respan_gateway_mcp::{Input, Message, Output, PROTOCOL, RpcError, answer};
use respan_gateway_plugin_tool_loop::{
    Outcome, ToolFilter, ToolHop, ToolLoop, ToolScope, ToolSwitches, Toolbox, Tools,
};
use respan_gateway_realtime_ws::SessionModel;
use serde_json::{Value, json};

use crate::config::ToolLoopConfig;
use crate::mcp::McpServer;

/// How many pages of a server's tool list are read.
const PAGES: usize = 8;
/// The longest a tool's name may be, as every chat API has it.
const NAME: usize = 64;
/// The longest the sessions of a retired runner, or a listing, are given.
const CLOSING: Duration = Duration::from_secs(10);
/// How long a server's tool list is kept unless the config says.
const DEFAULT_CACHE: Duration = Duration::from_secs(300);

/// The servers whose tools `tool_loop` offers: those it names, or every
/// server that takes no client's credentials, as `/mcp` fronts them.
pub fn servers(tool_loop: &ToolLoopConfig, servers: &[McpServer]) -> Vec<String> {
    match &tool_loop.servers {
        Some(listed) => listed.clone(),
        None => (servers.iter().filter(|s| !s.forward_auth))
            .map(|s| s.name.clone())
            .collect(),
    }
}

/// Checks `plugins.tool_loop` against the servers: every tool server is a tool
/// server that is served, and takes no client's credentials.
pub fn check(
    tool_loop: &ToolLoopConfig,
    servers: &[McpServer],
    serving_mcp: bool,
) -> Result<(), Fault> {
    let bad = |e: String| Fault::config(format!("plugins: tool_loop: {e}"));
    let names = self::servers(tool_loop, servers);
    if names.is_empty() {
        return Err(bad(match tool_loop.servers {
            Some(_) => "`servers` names no mcp server".into(),
            None => "no mcp server to offer tools of; add one under `tools` (a server with \
                     `auth = \"forward\"` cannot be used)"
                .into(),
        }));
    }
    let known = || {
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        names.join(", ")
    };
    for (i, name) in names.iter().enumerate() {
        if names[..i].contains(name) {
            return Err(bad(format!("servers: '{name}' is listed twice")));
        }
        let Some(server) = servers.iter().find(|s| s.name == *name) else {
            return Err(bad(format!(
                "servers: unknown mcp server '{name}'; mcp servers: [{}]",
                known()
            )));
        };
        if server.forward_auth {
            return Err(bad(format!(
                "servers: mcp server '{name}' passes on a client's credentials \
                 (`auth = \"forward\"`); the gateway runs tools with the server's own"
            )));
        }
    }
    if !serving_mcp {
        return Err(bad(
            "servers: the mcp servers are served by the `mcp` format; add it to `formats`".into(),
        ));
    }
    if tool_loop.max_steps == Some(0) || tool_loop.max_steps.is_some_and(|n| n > 64) {
        return Err(bad("max_steps: must be from 1 to 64".into()));
    }
    if tool_loop.parallel == Some(0) || tool_loop.parallel.is_some_and(|n| n > 64) {
        return Err(bad("parallel: must be from 1 to 64".into()));
    }
    for (key, patterns) in [("allow", &tool_loop.allow), ("deny", &tool_loop.deny)] {
        for pattern in patterns {
            let fit = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*');
            if pattern.is_empty() || !pattern.chars().all(fit) {
                return Err(bad(format!(
                    "{key}: '{pattern}' is not a tool name as the model has it \
                     (`<server>__<tool>`, letters, digits, '_' and '-', with '*' for any \
                     characters; any other character of a tool's name is '_')"
                )));
            }
        }
    }
    for (key, value) in [
        ("timeout", tool_loop.timeout),
        ("tool_timeout", tool_loop.tool_timeout),
        ("tool_list_ttl", tool_loop.tool_list_ttl),
    ] {
        if value == Some(Duration::ZERO) {
            return Err(bad(format!("{key}: must be above 0")));
        }
    }
    Ok(())
}

/// The loop, with the tools of `servers` ([`servers`]) that `switches`
/// leaves on, as it is at each list and call. Which requests it
/// applies to: chat requests and live sessions of a model `tool_loop.models`
/// names (all, if it names none), unless the client sends
/// `x-respan-tool-loop: off`.
pub fn plugin(
    tool_loop: &ToolLoopConfig,
    servers: Vec<String>,
    switches: Arc<ToolSwitches>,
    anthropic: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> (ToolLoop, Closer) {
    let filter = ToolFilter::new(tool_loop.allow.clone(), tool_loop.deny.clone());
    let ttl = tool_loop.tool_list_ttl.unwrap_or(DEFAULT_CACHE);
    let tools = McpTools::new(servers, ttl, filter, switches);
    let closer = Closer(tools.shared.clone());
    let prefixes = tool_loop.models.clone();
    let mut plugin = ToolLoop::new(tools)
        .flag_errors_for(anthropic)
        .when(move |cx, model| {
            let opted_out = opted_out(cx);
            // A live session's, as routing dialed it.
            let session = cx.extensions().get::<SessionModel>();
            let model = model.or(session.map(|m| m.0.as_str())).unwrap_or_default();
            let named = prefixes.is_empty() || prefixes.iter().any(|p| model.starts_with(p));
            named && !opted_out
        });
    if let Some(steps) = tool_loop.max_steps {
        plugin = plugin.max_steps(steps);
    }
    if let Some(limit) = tool_loop.timeout {
        plugin = plugin.timeout(limit);
    }
    if let Some(limit) = tool_loop.tool_timeout {
        plugin = plugin.tool_timeout(limit);
    }
    if let Some(bytes) = tool_loop.max_result {
        plugin = plugin.max_result(bytes);
    }
    if let Some(calls) = tool_loop.parallel {
        plugin = plugin.parallel(calls);
    }
    if let Some(bytes) = tool_loop.max_results {
        plugin = plugin.max_results(bytes);
    }
    (plugin, closer)
}

/// Marks the request that ends a retired runner's sessions.
#[derive(Debug, Clone, Copy)]
struct Close;

/// Marks the request that lists the tools the tool loop offers, into its slot.
#[derive(Debug, Clone, Default)]
struct List(Arc<Mutex<Vec<String>>>);

/// Ends the sessions the runner kept with its servers.
#[derive(Clone)]
pub struct Closer(Arc<Shared>);

impl Closer {
    /// The plugin that does it for the request [`close`](Self::close) makes.
    /// Register it first, so nothing else sees that request.
    pub fn plugin(&self) -> impl Plugin + use<> {
        Closing(self.0.clone())
    }

    /// Ends every session, through `gateway`: the one the runner was built
    /// into, once its requests are done. A session that does not end is the
    /// server's to time out.
    pub async fn close(&self, gateway: &Gateway) {
        self.send(gateway, Close).await;
    }

    /// The tools the tool loop offers (`<server>__<tool>`), listed now through
    /// `gateway`; a server that cannot be listed offers none.
    pub async fn offered(&self, gateway: &Gateway) -> Vec<String> {
        let list = List::default();
        self.send(gateway, list.clone()).await;
        let names = list.0.lock();
        names.clone()
    }

    /// A request of the gateway's own, marked with `mark`, which the plugin
    /// answers.
    async fn send<M: Clone + Send + Sync + 'static>(&self, gateway: &Gateway, mark: M) {
        let Some(server) = self.0.servers.first() else {
            return;
        };
        let mut request = http::Request::new(Body::full(Vec::<u8>::new()));
        *request.method_mut() = Method::DELETE;
        *request.uri_mut() = match format!("/mcp/{}", server.name).parse() {
            Ok(uri) => uri,
            Err(_) => return,
        };
        // Any session id will do: the plugin answers before it is looked up.
        request
            .headers_mut()
            .insert("mcp-session-id", http::HeaderValue::from_static("closing"));
        let mut marks = Extensions::new();
        marks.insert(mark);
        request.extensions_mut().insert(marks);
        let sent = respan_gateway_http::handle(gateway, request, 1 << 20);
        let work = async {
            let (response, done) = sent.await;
            let body = respan_gateway_http::http_body_util::BodyExt::collect(response.into_body());
            let _ = tokio::join!(body, done);
        };
        let _ = tokio::time::timeout(CLOSING, work).await;
    }
}

struct Closing(Arc<Shared>);

#[async_trait]
impl Plugin for Closing {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let list = cx.extensions().get::<List>().cloned();
        if let Some(List(slot)) = list {
            let servers = Servers::new(self.0.clone(), cx);
            let names = servers.list().await.into_iter().map(|t| t.name);
            *slot.lock() = names.collect();
        } else if cx.extensions().get::<Close>().is_some() {
            Servers::new(self.0.clone(), cx).close().await;
        } else {
            return Ok(None);
        }
        Ok(Some(Response::stream(stream::empty::<
            Result<respan_gateway_mcp::Output, Fault>,
        >())))
    }
}

/// The header a client sends to leave the loop out of one request.
pub const OPT_OUT: &str = "x-respan-tool-loop";

fn opted_out(cx: &ClientContext) -> bool {
    use respan_gateway_http::raw::Original;
    let wire = cx.extensions().get::<Original>();
    let request = wire.and_then(|wire| wire.get());
    let value = request.and_then(|r| r.headers().get(OPT_OUT)?.to_str().ok());
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "0"
        )
    })
}

/// The tools of MCP servers, run through the gateway's providers of them.
struct McpTools {
    shared: Arc<Shared>,
}

struct Shared {
    /// The servers, in the order the config names them.
    servers: Vec<Server>,
    ttl: Duration,
    filter: ToolFilter,
    /// The servers and tools turned off, applied to the cached lists.
    switches: Arc<ToolSwitches>,
    /// The next JSON-RPC id, unique across every session.
    next_id: AtomicU64,
}

struct Server {
    name: String,
    state: tokio::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    session: Option<Opened>,
    listed: Option<(Instant, Vec<Listed>)>,
}

/// A session with a server: its id (none if the server keeps none) and the
/// protocol version it answered `initialize` with.
#[derive(Debug, Clone)]
struct Opened {
    id: Option<String>,
    protocol: String,
}

impl McpTools {
    fn new(
        servers: Vec<String>,
        ttl: Duration,
        filter: ToolFilter,
        switches: Arc<ToolSwitches>,
    ) -> Self {
        let servers = servers
            .into_iter()
            .map(|name| Server {
                name,
                state: tokio::sync::Mutex::default(),
            })
            .collect();
        Self {
            shared: Arc::new(Shared {
                servers,
                ttl,
                filter,
                switches,
                next_id: AtomicU64::new(1),
            }),
        }
    }
}

impl Tools for McpTools {
    fn open(&self, cx: &mut ClientContext) -> Arc<dyn Toolbox> {
        Arc::new(Servers::new(self.shared.clone(), cx))
    }
}

/// One request's use of the servers.
struct Servers {
    shared: Arc<Shared>,
    /// A call to each server, to make copies of.
    calls: HashMap<String, Call>,
    /// The tools offered, by the name the model has: server and tool.
    names: Mutex<HashMap<String, (String, String)>>,
    /// What the request's client key limits it to, if it does.
    scope: Option<ToolScope>,
}

#[respan_gateway_core::async_trait]
impl Toolbox for Servers {
    async fn list(&self) -> Vec<Tool> {
        let each = self.shared.servers.iter().map(|s| self.listing(s));
        let all = respan_gateway_core::futures::future::join_all(each).await;
        let mut names = self.names.lock();
        let mut offered = Vec::new();
        for listed in all.into_iter().flatten() {
            if !self.shared.switches.offers(&listed.server, &listed.tool) {
                continue;
            }
            if !self
                .scope
                .as_ref()
                .is_none_or(|s| s.offers(&listed.offered.name))
            {
                continue;
            }
            if !names.contains_key(&listed.offered.name) {
                let (server, tool) = (listed.server, listed.tool);
                names.insert(listed.offered.name.clone(), (server, tool));
                offered.push(listed.offered);
            }
        }
        offered
    }

    async fn call(&self, name: &str, arguments: Value) -> Outcome {
        let known = {
            let names = self.names.lock();
            names.get(name).cloned()
        };
        let known = known.filter(|(server, tool)| self.shared.switches.offers(server, tool));
        let Some((server, tool)) = known else {
            return Outcome::error(format!("there is no tool '{name}'"));
        };
        let params = json!({ "name": tool, "arguments": arguments });
        match self.request(&server, "tools/call", params).await {
            Ok(Ok(result)) => render(&result),
            Ok(Err(e)) => Outcome::error(e.message),
            Err(fault) => Outcome::error(format!(
                "the tool server '{server}' could not be used: {}",
                fault.message()
            )),
        }
    }
}

/// A tool of a server, as offered to the model.
#[derive(Clone)]
struct Listed {
    server: String,
    /// The tool's own name.
    tool: String,
    /// What the model is offered, named `<server>__<tool>`.
    offered: Tool,
}

impl Servers {
    fn new(shared: Arc<Shared>, cx: &mut ClientContext) -> Self {
        let calls = shared
            .servers
            .iter()
            .map(|s| (s.name.clone(), cx.call(&s.name)))
            .collect();
        let scope = cx.extensions().get::<ToolScope>().cloned();
        Self {
            shared,
            calls,
            names: Mutex::default(),
            scope,
        }
    }

    /// Ends the session with each server, and forgets it.
    async fn close(&self) {
        let each = self.shared.servers.iter().map(|server| async move {
            let session = server.state.lock().await.session.take();
            let Some(Opened {
                id: Some(id),
                protocol,
            }) = session
            else {
                return;
            };
            let mut input = Input::new(Method::DELETE, Vec::new());
            input.session = Some(id);
            input.protocol_version = Some(protocol);
            let _ = self.exchange(&server.name, input).await;
        });
        respan_gateway_core::futures::future::join_all(each).await;
    }

    fn server(&self, name: &str) -> Result<&Server, Fault> {
        let found = self.shared.servers.iter().find(|s| s.name == name);
        found.ok_or_else(|| Fault::internal(format!("no tool loop tool server '{name}'")))
    }

    /// A server's tools: its list, kept for the cache's time; if it cannot
    /// be listed, the last list, else none.
    async fn listing(&self, server: &Server) -> Vec<Listed> {
        let kept = {
            let state = server.state.lock().await;
            state.listed.clone()
        };
        if let Some((at, tools)) = &kept
            && at.elapsed() < self.shared.ttl
        {
            return tools.clone();
        }
        match self.fetch(&server.name).await {
            Ok(tools) => {
                let mut state = server.state.lock().await;
                state.listed = Some((Instant::now(), tools.clone()));
                tools
            }
            Err(_) => kept.map(|(_, tools)| tools).unwrap_or_default(),
        }
    }

    async fn fetch(&self, server: &str) -> Result<Vec<Listed>, Fault> {
        let (mut tools, mut cursor) = (Vec::new(), None::<String>);
        for _ in 0..PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self.request(server, "tools/list", params).await?;
            let page = page.map_err(|e| Fault::new(e.message))?;
            for tool in page["tools"].as_array().into_iter().flatten() {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                if !self.shared.filter.offers(&model_name(server, name)) {
                    continue;
                }
                let mut offered = Tool::new(
                    model_name(server, name),
                    match tool["inputSchema"].is_object() {
                        true => tool["inputSchema"].clone(),
                        false => json!({ "type": "object", "properties": {} }),
                    },
                );
                offered.description = tool["description"].as_str().map(str::to_owned);
                tools.push(Listed {
                    server: server.to_owned(),
                    tool: name.to_owned(),
                    offered,
                });
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// The server's session: kept, else started.
    async fn session(&self, server: &str) -> Result<Opened, Fault> {
        let mut state = self.server(server)?.state.lock().await;
        if let Some(opened) = &state.session {
            return Ok(opened.clone());
        }
        let opened = self.initialize(server).await?;
        state.session = Some(opened.clone());
        Ok(opened)
    }

    async fn forget(&self, server: &str, opened: &Opened) {
        if let Ok(s) = self.server(server) {
            let mut state = s.state.lock().await;
            if state.session.as_ref().map(|o| &o.id) == Some(&opened.id) {
                state.session = None;
            }
        }
    }

    async fn initialize(&self, server: &str) -> Result<Opened, Fault> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let hello = message(json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL, "capabilities": {},
                "clientInfo": { "name": "respan-gateway", "version": env!("CARGO_PKG_VERSION") }
            }
        }))?;
        let (messages, session) = self.send(server, None, hello).await?;
        let result = answer(&messages, id)?
            .map_err(|e| Fault::new(format!("it refused `initialize`: {}", e.message)))?;
        let protocol = result["protocolVersion"].as_str().unwrap_or(PROTOCOL);
        let opened = Opened {
            id: session,
            protocol: protocol.to_owned(),
        };
        let done = message(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        self.send(server, Some(&opened), done).await?;
        Ok(opened)
    }

    /// A request of `method` to `server`, answered with its result or the
    /// server's error. Lists are retried once with a new session; a tool
    /// call only if it surely did not run.
    async fn request(
        &self,
        server: &str,
        method: &str,
        params: Value,
    ) -> Result<Result<Value, RpcError>, Fault> {
        let mut failed = None;
        for _ in 0..2 {
            let opened = self.session(server).await?;
            let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
            let asked = message(json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            }))?;
            let sent = self.send(server, Some(&opened), asked).await;
            match sent.and_then(|(messages, _)| answer(&messages, id)) {
                Ok(answered) => return Ok(answered),
                Err(fault) => {
                    self.forget(server, &opened).await;
                    if method != "tools/list" && !unrun(&fault) {
                        return Err(fault);
                    }
                    failed = Some(fault);
                }
            }
        }
        Err(failed.unwrap_or_else(|| Fault::internal("no attempt was made")))
    }

    /// Posts `message` to `server` in `session`, and reads everything the
    /// server answers: its messages, and the session it names.
    async fn send(
        &self,
        server: &str,
        session: Option<&Opened>,
        message: Message,
    ) -> Result<(Vec<Message>, Option<String>), Fault> {
        let mut input = Input::new(Method::POST, vec![message]);
        if let Some(opened) = session {
            input.session.clone_from(&opened.id);
            input.protocol_version = Some(opened.protocol.clone());
        }
        self.exchange(server, input).await
    }

    /// Sends `input` to `server` as the gateway's own client, and reads
    /// everything it answers: its messages, and the session it names.
    async fn exchange(
        &self,
        server: &str,
        input: Input,
    ) -> Result<(Vec<Message>, Option<String>), Fault> {
        let call = self
            .calls
            .get(server)
            .ok_or_else(|| Fault::internal(format!("no call to '{server}'")))?;
        let call = call.clone().with_input(input).with_extension(ToolHop);
        let mut outputs = call.await?.outputs::<Output>();
        let (mut messages, mut named) = (Vec::new(), None);
        while let Some(output) = outputs.next().await {
            let output = output?;
            named = named.or(output.session);
            messages.extend(output.messages);
        }
        Ok((messages, named))
    }
}

pub(crate) fn message(json: Value) -> Result<Message, Fault> {
    Message::try_from(json).map_err(Fault::internal)
}

/// As in the MCP router, only a lost session or a refused connection says
/// that a tool call surely did not run.
fn unrun(fault: &Fault) -> bool {
    let lost = fault
        .get::<respan_gateway_http_faults::Status>()
        .is_some_and(|s| s.0 == http::StatusCode::NOT_FOUND);
    lost || fault.code() == "no_session"
        || fault
            .cause::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
}

/// The name a tool has for the model: the server and the tool, fit for every
/// chat API (letters, digits, `_` and `-`, at most 64).
fn model_name(server: &str, tool: &str) -> String {
    let fit = |c: char| match c.is_ascii_alphanumeric() || c == '-' {
        true => c,
        false => '_',
    };
    let name: String = format!("{server}__{tool}").chars().map(fit).collect();
    name[..name.len().min(NAME)].to_owned()
}

/// What a tool answered, as text for the model.
fn render(result: &Value) -> Outcome {
    let mut parts: Vec<String> = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        let part = match item["type"].as_str() {
            Some("text") => item["text"].as_str().unwrap_or_default().to_owned(),
            Some("resource") => match item["resource"]["text"].as_str() {
                Some(text) => text.to_owned(),
                None => format!(
                    "[resource {} not shown]",
                    item["resource"]["uri"].as_str().unwrap_or_default()
                ),
            },
            Some("resource_link") => {
                format!("[resource {}]", item["uri"].as_str().unwrap_or_default())
            }
            Some(kind @ ("image" | "audio")) => format!(
                "[{kind} of {} not shown]",
                item["mimeType"].as_str().unwrap_or("unknown type")
            ),
            _ => item.to_string(),
        };
        parts.push(part);
    }
    if parts.is_empty() && !result["structuredContent"].is_null() {
        parts.push(result["structuredContent"].to_string());
    }
    let text = parts.join("\n");
    match result["isError"].as_bool().unwrap_or(false) {
        true => Outcome::error(text),
        false => Outcome::ok(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::Provider;
    use respan_gateway_http::{ClientIo, connect, http_body_util::BodyExt};
    use respan_gateway_mcp_http::StreamableHttp;
    use std::sync::atomic::AtomicUsize;

    type Reply = Result<Result<Value, RpcError>, Fault>;

    struct Request {
        shared: Arc<Shared>,
        method: &'static str,
        reply: Arc<Mutex<Option<Reply>>>,
    }

    #[async_trait]
    impl Plugin for Request {
        async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
            let servers = Servers::new(self.shared.clone(), cx);
            let params = json!({"name": "write", "arguments": {}});
            *self.reply.lock() = Some(servers.request("test", self.method, params).await);
            Ok(Some(Response::stream(
                stream::empty::<Result<Output, Fault>>(),
            )))
        }
    }

    #[derive(Clone, Copy)]
    enum Failure {
        LostReply,
        NoAnswer,
        ExpiredSession,
        NoSession,
        ConnectionRefused,
        ServerError,
    }

    /// A real MCP exchange without sockets: the first request fails, and
    /// another would succeed, recording whether it was sent twice.
    async fn request_with_failure(method: &'static str, failure: Failure) -> (usize, Reply) {
        let tools = McpTools::new(
            vec!["test".into()],
            Duration::ZERO,
            ToolFilter::default(),
            Arc::default(),
        );
        tools.shared.servers[0].state.lock().await.session = Some(Opened {
            id: Some("old".into()),
            protocol: PROTOCOL.into(),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let send = connect(
            move |request: http::Request<Body>| {
                let seen = seen.clone();
                async move {
                    let body = BodyExt::collect(request.into_body())
                        .await
                        .expect("body")
                        .to_bytes();
                    let asked: Value = serde_json::from_slice(&body).expect("request");
                    let result = match asked["method"].as_str().expect("method") {
                        "initialize" => json!({
                            "protocolVersion": PROTOCOL, "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test", "version": "1"}
                        }),
                        "notifications/initialized" => {
                            return Ok((
                                http::Response::builder()
                                    .status(202)
                                    .body(Body::default())
                                    .expect("ack"),
                                None,
                            ));
                        }
                        received => {
                            assert_eq!(received, method);
                            if seen.fetch_add(1, Ordering::Relaxed) == 0 {
                                if matches!(failure, Failure::NoSession) {
                                    return Err(Fault::upstream("session expired").with(
                                        respan_gateway_core::ErrorCode("no_session".into()),
                                    ));
                                }
                                let error = match failure {
                                    Failure::LostReply => Some(std::io::ErrorKind::ConnectionReset),
                                    Failure::ConnectionRefused => {
                                        Some(std::io::ErrorKind::ConnectionRefused)
                                    }
                                    _ => None,
                                };
                                if let Some(kind) = error {
                                    return Err(Fault::upstream("the reply was lost")
                                        .with_source(std::io::Error::from(kind)));
                                }
                                let (status, body) = match failure {
                                    Failure::NoAnswer => (
                                        200,
                                        json!({"jsonrpc": "2.0", "method": "notifications/message"}),
                                    ),
                                    Failure::ExpiredSession => {
                                        (404, json!({"error": "session expired"}))
                                    }
                                    Failure::ServerError => (500, json!({"error": "reply failed"})),
                                    _ => unreachable!(),
                                };
                                return Ok((
                                    http::Response::builder()
                                        .status(status)
                                        .header("content-type", "application/json")
                                        .body(Body::full(body.to_string()))
                                        .expect("failure"),
                                    None,
                                ));
                            }
                            json!({"content": [{"type": "text", "text": "done"}]})
                        }
                    };
                    let body = json!({"jsonrpc": "2.0", "id": asked["id"], "result": result});
                    Ok((
                        http::Response::builder()
                            .header("content-type", "application/json")
                            .header("mcp-session-id", "new")
                            .body(Body::full(body.to_string()))
                            .expect("response"),
                        None,
                    ))
                }
            },
            4096,
        );
        let reply = Arc::new(Mutex::new(None));
        let gateway = Gateway::new()
            .client::<Input, Output, ClientIo>(StreamableHttp::default().serving(["test"]))
            .expect("client")
            .provider(Provider::new("test", StreamableHttp::default(), send))
            .expect("provider")
            .plugin(Request {
                shared: tools.shared.clone(),
                method,
                reply: reply.clone(),
            });
        Closer(tools.shared).send(&gateway, Close).await;
        let answer = reply.lock().take().expect("request ran");
        (attempts.load(Ordering::Relaxed), answer)
    }

    #[tokio::test]
    async fn a_tool_call_is_not_repeated_when_its_reply_is_lost() {
        for failure in [Failure::LostReply, Failure::NoAnswer, Failure::ServerError] {
            let (attempts, reply) = request_with_failure("tools/call", failure).await;
            assert_eq!(attempts, 1, "the tool might already have run");
            assert!(reply.is_err(), "the uncertain result must reach the caller");
        }
    }

    #[tokio::test]
    async fn a_tool_call_is_retried_when_it_could_not_have_run() {
        for failure in [
            Failure::ExpiredSession,
            Failure::NoSession,
            Failure::ConnectionRefused,
        ] {
            let (attempts, reply) = request_with_failure("tools/call", failure).await;
            assert!(matches!(reply, Ok(Ok(_))), "{reply:?}");
            assert_eq!(attempts, 2);
        }
    }

    #[tokio::test]
    async fn listing_tools_is_retried_when_its_reply_is_lost() {
        let (attempts, reply) = request_with_failure("tools/list", Failure::LostReply).await;
        assert!(matches!(reply, Ok(Ok(_))), "{reply:?}");
        assert_eq!(attempts, 2);
    }

    #[test]
    fn a_tool_is_named_for_the_model_after_its_server() {
        assert_eq!(model_name("github", "search"), "github__search");
        assert_eq!(model_name("my.files", "read file"), "my_files__read_file");
        assert_eq!(model_name("s", &"t".repeat(100)).len(), 64);
    }

    #[test]
    fn a_result_is_text_for_the_model() {
        let result = json!({ "content": [
            { "type": "text", "text": "a" },
            { "type": "image", "mimeType": "image/png", "data": "AAAA" },
            { "type": "resource_link", "uri": "file:///x" },
        ], "isError": true });
        let outcome = render(&result);
        assert!(outcome.is_error);
        assert_eq!(
            outcome.text,
            "a\n[image of image/png not shown]\n[resource file:///x]"
        );
        let structured = render(&json!({ "content": [], "structuredContent": { "n": 1 } }));
        assert_eq!(structured, Outcome::ok("{\"n\":1}"));
    }

    #[test]
    fn without_servers_the_loop_uses_every_server_it_can() {
        let mcp: indexmap::IndexMap<String, crate::mcp::McpConfig> = toml::from_str(
            "a = { url = \"http://a.test\" }\nb = { url = \"http://b.test\", auth = \"forward\" }\nc = { url = \"http://c.test\" }",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let config = crate::Config {
            tools: mcp,
            ..crate::Config::default()
        };
        let network = respan_gateway_http_transport::Network::shared();
        let logins = std::path::Path::new(".respan/mcp");
        let mcp =
            crate::mcp::servers(&config, network, &[], logins).unwrap_or_else(|e| panic!("{e}"));
        let config: ToolLoopConfig = toml::from_str("").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(servers(&config, &mcp), ["a", "c"]);
        assert!(check(&config, &mcp, true).is_ok());
        let none = check(&config, &mcp[1..2], true).err();
        assert!(none.is_some_and(|e| e.message().contains("no mcp server")));
        let listed: ToolLoopConfig =
            toml::from_str("servers = [\"c\"]").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(servers(&listed, &mcp), ["c"]);
    }
}
