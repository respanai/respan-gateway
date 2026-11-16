//! A request's dealings with the servers behind `/mcp`: lists
//! merged, calls routed, and the sessions opened and used.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use respan_gateway_core::Fault;
use respan_gateway_core::futures::future::join_all;
use respan_gateway_core::futures::stream;
use respan_gateway_core::futures::{StreamExt, future};
use respan_gateway_http::http::{Method, StatusCode};
use respan_gateway_http_faults::Status;
use respan_gateway_mcp::{Input, Message, Output, RpcError};
use serde_json::{Map, Value, json};

use super::rewrite::{passed, prefix_field, reply};
use super::run::Run;
use super::session::{Opened, Session};
use super::{Outputs, SEP, message};
use respan_gateway_mcp::{PROTOCOL, answer};

/// A list's page, or the error code and message to answer with.
pub(super) type Listed = Result<Value, (i64, String)>;

impl Run {
    /// A `tools/list`, `prompts/list`, `resources/list` or
    /// `resources/templates/list`: the first page of every server's, or the
    /// next of those the cursor names.
    pub(super) async fn list(
        &self,
        session: &Arc<Session>,
        method: &str,
        params: &Value,
    ) -> Listed {
        let (capability, field) = match method {
            "tools/list" => ("tools", "tools"),
            "prompts/list" => ("prompts", "prompts"),
            "resources/list" => ("resources", "resources"),
            _ => ("resources", "resourceTemplates"),
        };
        let cursors: Option<Map<String, Value>> = match params["cursor"].as_str() {
            Some(cursor) => match serde_json::from_str(cursor) {
                Ok(map) => Some(map),
                Err(_) => return Err((-32602, "invalid cursor".to_owned())),
            },
            None => None,
        };
        let asking = self.inner.servers.iter().filter_map(|server| {
            if !self.inner.switches.server_on(server) {
                return None;
            }
            let cursor = match &cursors {
                Some(map) => Some(map.get(server)?.as_str()?.to_owned()),
                None => None,
            };
            Some((server.clone(), cursor))
        });
        let pages = asking.map(|(server, cursor)| {
            let session = session.clone();
            async move {
                let opened = self.upstream(&session, &server).await.ok()?;
                opened.capabilities.get(capability)?;
                let params = match &cursor {
                    Some(cursor) => json!({ "cursor": cursor }),
                    None => json!({}),
                };
                let page = self
                    .rpc(&session, &server, method, params)
                    .await
                    .ok()?
                    .ok()?;
                Some((server, page))
            }
        });
        let (mut items, mut more) = (Vec::new(), Map::new());
        for (server, page) in join_all(pages).await.into_iter().flatten() {
            for item in page[field].as_array().into_iter().flatten() {
                if let Some(item) = self.namespaced(&server, field, item.clone()) {
                    items.push(item);
                }
            }
            if let Some(next) = page["nextCursor"].as_str() {
                more.insert(server, json!(next));
            }
        }
        let mut page = Map::from_iter([(field.to_owned(), Value::Array(items))]);
        if !more.is_empty() {
            page.insert("nextCursor".into(), json!(Value::Object(more).to_string()));
        }
        Ok(Value::Object(page))
    }

    /// `item` of `server`'s list as the client has it, or none if withheld.
    pub(super) fn namespaced(&self, server: &str, field: &str, mut item: Value) -> Option<Value> {
        if !self.inner.switches.server_on(server) {
            return None;
        }
        match field {
            "tools" | "prompts" => {
                let own = item["name"].as_str()?;
                let name = format!("{server}{SEP}{own}");
                if field == "tools" && !self.offers(server, own, &name) {
                    return None;
                }
                item["name"] = json!(name);
            }
            "resources" => prefix_field(server, &mut item, "uri"),
            _ => prefix_field(server, &mut item, "uriTemplate"),
        }
        Some(item)
    }

    /// A request that goes to the one server `route` finds in its params,
    /// which it rewrites for that server; the answer comes back as the
    /// server sent it, notifications on the way included, with the URIs in
    /// it given the server's prefix.
    pub(super) fn routed(
        self,
        session: &Arc<Session>,
        id: Value,
        method: &str,
        mut params: Value,
        route: impl Fn(&Run, &mut Value) -> Result<String, String>,
    ) -> Outputs {
        let (session, method) = (session.clone(), method.to_owned());
        let server = match route(&self, &mut params) {
            Ok(server) => server,
            Err(why) => {
                let output = reply(&id, Err((-32602, why)));
                return stream::once(future::ready(Ok(output))).boxed();
            }
        };
        stream::once(async move {
            let mut last = None;
            for _ in 0..2 {
                match self.forward(&session, &server, &id, &method, &params).await {
                    Ok(outputs) => return outputs,
                    // Tried again only if it surely did not run.
                    Err((fault, unrun)) => {
                        last = Some(fault);
                        if !unrun {
                            break;
                        }
                    }
                }
            }
            let why = last.map_or(String::new(), |f| f.message().to_owned());
            let why = format!("the server '{server}' could not be used: {why}");
            let output = reply(&id, Err((-32603, why)));
            stream::once(future::ready(Ok(output))).boxed()
        })
        .flatten()
        .boxed()
    }

    /// One try at a routed request: its answer as a stream, once its first
    /// output is in; else its fault, and whether the request surely did not
    /// run ([`unrun`]). A session the server lost is forgotten, for the next
    /// try to open again.
    pub(super) async fn forward(
        &self,
        session: &Arc<Session>,
        server: &str,
        id: &Value,
        method: &str,
        params: &Value,
    ) -> Result<Outputs, (Fault, bool)> {
        let opened = self
            .upstream(session, server)
            .await
            .map_err(|f| (f, true))?;
        let asked = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut input = Input::new(Method::POST, vec![message(asked).map_err(|f| (f, false))?]);
        input.session.clone_from(&opened.id);
        input.protocol_version = Some(opened.protocol.clone());
        let tried = async {
            let mut outputs = self.stream(server, input).await?;
            let first = outputs.next().await;
            match first {
                Some(Err(fault)) => Err(fault),
                first => Ok((first, outputs)),
            }
        };
        let (first, rest) = match tried.await {
            Ok(started) => started,
            Err(fault) => {
                session.forget(server, &opened).await;
                let unrun = unrun(&fault);
                return Err((fault, unrun));
            }
        };
        let (server, id) = (server.to_owned(), id.clone());
        let outputs = stream::iter(first).chain(rest);
        // Whatever breaks midway is an error to the request, and its end.
        let outputs = outputs.scan(false, move |broken, output| {
            if *broken {
                return future::ready(None);
            }
            let said = match output {
                Ok(output) => Ok(passed(&server, output)),
                Err(fault) => {
                    *broken = true;
                    let why = format!("the server '{server}' broke off: {}", fault.message());
                    Ok(reply(&id, Err((-32603, why))))
                }
            };
            future::ready(Some(said))
        });
        Ok(outputs.boxed())
    }

    /// The session with `server`: kept, else opened. Which `initialize`
    /// each server is sent is the gateway's own.
    pub(super) async fn upstream(
        &self,
        session: &Arc<Session>,
        server: &str,
    ) -> Result<Opened, Fault> {
        let slot = session
            .upstream
            .get(server)
            .ok_or_else(|| Fault::internal(format!("no server '{server}' behind /mcp")))?;
        let mut slot = slot.lock().await;
        if let Some(opened) = &*slot {
            return Ok(opened.clone());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let hello = message(json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL, "capabilities": {},
                "clientInfo": { "name": "respan-gateway", "version": env!("CARGO_PKG_VERSION") }
            }
        }))?;
        let (messages, named) = self
            .exchange(server, Input::new(Method::POST, vec![hello]))
            .await?;
        let result = answer(&messages, id)?
            .map_err(|e| Fault::new(format!("it refused `initialize`: {}", e.message)))?;
        let opened = Opened {
            id: named,
            protocol: result["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL)
                .to_owned(),
            capabilities: result["capabilities"].clone(),
        };
        let done = message(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        let mut input = Input::new(Method::POST, vec![done]);
        input.session.clone_from(&opened.id);
        input.protocol_version = Some(opened.protocol.clone());
        self.exchange(server, input).await?;
        *slot = Some(opened.clone());
        Ok(opened)
    }

    /// A request of the gateway's own to `server`: its result or its error,
    /// after one more try with a new session if the first failed.
    pub(super) async fn rpc(
        &self,
        session: &Arc<Session>,
        server: &str,
        method: &str,
        params: Value,
    ) -> Result<Result<Value, RpcError>, Fault> {
        let mut failed = None;
        for _ in 0..2 {
            let opened = self.upstream(session, server).await?;
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            let asked = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            let mut input = Input::new(Method::POST, vec![message(asked)?]);
            input.session.clone_from(&opened.id);
            input.protocol_version = Some(opened.protocol.clone());
            let sent = self.exchange(server, input).await;
            match sent.and_then(|(messages, _)| answer(&messages, id)) {
                Ok(answered) => return Ok(answered),
                Err(fault) => {
                    session.forget(server, &opened).await;
                    failed = Some(fault);
                }
            }
        }
        Err(failed.unwrap_or_else(|| Fault::internal("no attempt was made")))
    }

    /// The outputs of `input` sent to `server` as the client's call.
    pub(super) async fn stream(&self, server: &str, input: Input) -> Result<Outputs, Fault> {
        let call = self
            .calls
            .get(server)
            .ok_or_else(|| Fault::internal(format!("no call to '{server}'")))?;
        let outputs = call.clone().with_input(input).await?.outputs::<Output>();
        Ok(outputs.boxed())
    }

    /// Everything `server` answers to `input`: its messages, and the
    /// session it names.
    pub(super) async fn exchange(
        &self,
        server: &str,
        input: Input,
    ) -> Result<(Vec<Message>, Option<String>), Fault> {
        let mut outputs = self.stream(server, input).await?;
        let (mut messages, mut named) = (Vec::new(), None);
        while let Some(output) = outputs.next().await {
            let output = output?;
            named = named.or(output.session);
            messages.extend(output.messages);
        }
        Ok((messages, named))
    }
}

/// Whether a request that failed with `fault` surely did not run: the
/// server answered 404, as MCP has it answer for a session it ended (or a
/// fault coded `no_session`), or it could not be connected to.
fn unrun(fault: &Fault) -> bool {
    let lost = fault
        .get::<Status>()
        .is_some_and(|s| s.0 == StatusCode::NOT_FOUND);
    let refused = fault.cause::<std::io::Error>();
    lost || fault.code() == "no_session"
        || refused.is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::{Gateway, Provider};
    use respan_gateway_http::{Body, ClientIo, connect, handle, http, http_body_util::BodyExt};
    use respan_gateway_mcp_http::{SESSION, StreamableHttp};
    use respan_gateway_plugin_tool_loop::ToolFilter;
    use std::sync::Mutex;

    async fn request(
        gateway: &Gateway,
        session: Option<&str>,
        value: Value,
    ) -> (Value, Option<String>) {
        let mut request = http::Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json");
        if let Some(session) = session {
            request = request.header(SESSION, session);
        }
        let request = request
            .body(Body::full(value.to_string()))
            .expect("request");
        let (response, done) = handle(gateway, request, 4096).await;
        assert_eq!(response.status(), StatusCode::OK);
        let session = response
            .headers()
            .get(SESSION)
            .map(|s| s.to_str().expect("session").to_owned());
        let (body, end) = tokio::join!(BodyExt::collect(response.into_body()), done);
        end.expect("delivery");
        let body = body.expect("body").to_bytes();
        let value: Value = serde_json::from_slice(&body).expect("JSON response");
        assert!(value.get("error").is_none(), "{value}");
        (value["result"].clone(), session)
    }

    async fn pages(method: &'static str, field: &'static str) {
        let servers = [
            ("empty", Some("")),
            ("named", Some("page-2")),
            ("done", None),
        ];
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new()
            .client::<Input, Output, ClientIo>(StreamableHttp::default())
            .expect("client");
        for (server, cursor) in servers {
            let calls = calls.clone();
            let send = connect(
                move |request: http::Request<Body>| {
                    let calls = calls.clone();
                    async move {
                        let body = BodyExt::collect(request.into_body())
                            .await
                            .expect("request body")
                            .to_bytes();
                        let asked: Value = serde_json::from_slice(&body).expect("JSON request");
                        let result = match asked["method"].as_str().expect("method") {
                            "initialize" => json!({
                                "protocolVersion": PROTOCOL,
                                "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
                                "serverInfo": {"name": server, "version": "1"}
                            }),
                            "notifications/initialized" => {
                                return Ok((
                                    http::Response::builder()
                                        .status(StatusCode::ACCEPTED)
                                        .body(Body::default())
                                        .expect("notification response"),
                                    None,
                                ));
                            }
                            received => {
                                assert_eq!(received, method);
                                let supplied = asked["params"].get("cursor");
                                calls
                                    .lock()
                                    .expect("calls")
                                    .push((server, supplied.cloned()));
                                let mut page = json!({field: []});
                                if let Some(supplied) = supplied {
                                    assert_eq!(supplied.as_str(), cursor);
                                    let item = match field {
                                        "tools" => {
                                            json!({"name": "second", "inputSchema": {"type": "object"}})
                                        }
                                        "prompts" => json!({"name": "second"}),
                                        "resources" => {
                                            json!({"name": "second", "uri": "file:///second"})
                                        }
                                        _ => {
                                            json!({"name": "second", "uriTemplate": "file:///{name}"})
                                        }
                                    };
                                    page[field] = json!([item]);
                                } else if let Some(cursor) = cursor {
                                    page["nextCursor"] = json!(cursor);
                                }
                                page
                            }
                        };
                        let body = json!({"jsonrpc": "2.0", "id": asked["id"], "result": result});
                        Ok((
                            http::Response::builder()
                                .header("content-type", "application/json")
                                .body(Body::full(body.to_string()))
                                .expect("response"),
                            None,
                        ))
                    }
                },
                4096,
            );
            gateway = gateway
                .provider(Provider::new(server, StreamableHttp::default(), send))
                .expect("provider");
        }
        gateway = gateway.plugin(crate::McpRouter::new(
            servers.iter().map(|(name, _)| (*name).to_owned()).collect(),
            ToolFilter::default(),
        ));
        let (_, session) = request(&gateway, None, json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}
        })).await;
        let (first, _) = request(
            &gateway,
            session.as_deref(),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": method, "params": {}
            }),
        )
        .await;
        assert_eq!(first[field], json!([]));
        let cursor = first["nextCursor"].as_str().expect("continuation");
        assert_eq!(
            serde_json::from_str::<Value>(cursor).expect("merged cursor"),
            json!({"empty": "", "named": "page-2"})
        );
        let (second, _) = request(
            &gateway,
            session.as_deref(),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": method, "params": {"cursor": cursor}
            }),
        )
        .await;
        let items = second[field].as_array().expect("items");
        assert_eq!(items.len(), 2);
        let (key, expected) = match field {
            "tools" | "prompts" => ("name", ["empty__second", "named__second"]),
            "resources" => (
                "uri",
                ["mcp://empty/file:///second", "mcp://named/file:///second"],
            ),
            _ => (
                "uriTemplate",
                ["mcp://empty/file:///{name}", "mcp://named/file:///{name}"],
            ),
        };
        assert_eq!(
            [items[0][key].as_str(), items[1][key].as_str()],
            expected.map(Some)
        );
        assert!(second.get("nextCursor").is_none());
        let mut calls = calls.lock().expect("calls").clone();
        calls.sort_by_key(|(server, cursor)| (*server, cursor.is_some()));
        assert_eq!(
            calls,
            vec![
                ("done", None),
                ("empty", None),
                ("empty", Some(json!(""))),
                ("named", None),
                ("named", Some(json!("page-2")))
            ]
        );
    }

    #[tokio::test]
    async fn tools_keep_empty_continuation_cursors() {
        pages("tools/list", "tools").await;
    }

    #[tokio::test]
    async fn prompts_keep_empty_continuation_cursors() {
        pages("prompts/list", "prompts").await;
    }

    #[tokio::test]
    async fn resources_keep_empty_continuation_cursors() {
        pages("resources/list", "resources").await;
    }

    #[tokio::test]
    async fn templates_keep_empty_continuation_cursors() {
        pages("resources/templates/list", "resourceTemplates").await;
    }
}
