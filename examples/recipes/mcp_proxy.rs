//! MCP clients over streamable HTTP through the gateway, with a tool
//! allow-list.
//!
//! The gateway speaks MCP's streamable HTTP to clients and to the tool
//! server. Nothing is relayed as bytes: the server's session id and every
//! header MCP carries travel in the kind ([`Input`], [`Output`]), so the
//! `ToolAllowList` plugin sees every message. It drops tools off the list
//! from `tools/list` answers and answers a `tools/call` for one with a
//! JSON-RPC error itself; the server never sees that call.
//!
//! Run it with `cargo run -p respan-gateway-examples --example mcp_proxy`.
//! A local MCP server offers `echo` and `wipe`; only `echo` is allowed. Look
//! for `tools/list` showing only `echo`, the `echo` call answered, and the
//! `wipe` call refused with -32602.
//!
//! Add `-- --stdio PROGRAM [ARGS...]` to run a real server as a child process
//! instead, such as `-- --stdio npx -y @modelcontextprotocol/server-everything`.

use std::sync::Arc;

use respan_gateway_core::futures::stream;
use respan_gateway_core::{
    ClientContext, Extensions, Fault, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, ProviderIo};
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_mcp::{self as mcp, Input, Output};
use respan_gateway_mcp_http::{SESSION, StdioTransport, StreamableHttp};
use serde_json::{Value, json};

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;
const PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let gw = match args.iter().position(|a| a == "--stdio") {
        Some(i) => {
            let (program, rest) = args[i + 1..]
                .split_first()
                .ok_or_else(|| Fault::config("--stdio PROGRAM"))?;
            gateway(StdioTransport::new(program, rest).connect())?
        }
        None => gateway(HttpTransport::new(mcp_server().await)?.connect())?,
    };

    // A client's session: initialize, list the tools, call the allowed tool,
    // then one that is not allowed.
    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "example-client", "version": "1" }
    });
    let (status, session, answer) = send(&gw, None, Some(1), "initialize", init).await?;
    println!("initialize -> {status}: {answer}");
    let session = session.ok_or_else(|| Fault::upstream("the server gave no session"))?;
    let session = Some(session.as_str());

    let (status, ..) = send(&gw, session, None, "notifications/initialized", json!({})).await?;
    println!("notifications/initialized -> {status}");

    let echo = json!({ "name": "echo", "arguments": { "message": "hi" } });
    for (id, label, method, params) in [
        (2, "tools/list", "tools/list", json!({})),
        (3, "tools/call echo", "tools/call", echo),
        (
            4,
            "tools/call wipe",
            "tools/call",
            json!({ "name": "wipe" }),
        ),
    ] {
        let (status, _, answer) = send(&gw, session, Some(id), method, params).await?;
        println!("{label} -> {status}: {answer}");
    }
    Ok(())
}

/// A gateway that serves MCP clients from the tool server `server`, with
/// only `echo` allowed.
fn gateway(
    server: impl Fn(Extensions) -> ProviderIo + Send + Sync + 'static,
) -> Result<Gateway, Fault> {
    Gateway::new()
        .provider(Provider::new("tools", StreamableHttp::default(), server))?
        .client::<Input, _, _>(StreamableHttp::default())?
        .plugin(ToolAllowList(Arc::from(["echo"])))
        .target("tools")
}

/// Lets clients see and call only these tools.
struct ToolAllowList(Arc<[&'static str]>);

fn allows(allowed: &[&str], tool: &Value) -> bool {
    allowed.iter().any(|a| tool == a)
}

#[async_trait]
impl Plugin for ToolAllowList {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Ok(input) = cx.input::<Input>() else {
            return Ok(None);
        };

        // A call to a tool off the list: every request in the post is
        // answered with an invalid-params error, and nothing goes upstream.
        let refused = input
            .messages
            .iter()
            .filter(|m| m.method() == Some("tools/call"))
            .filter_map(|m| m.params().map(|p| &p["name"]))
            .find(|tool| !allows(&self.0, tool));
        if let Some(tool) = refused {
            let message = format!("tool {tool} is not allowed");
            let errors = input
                .request_ids()
                .map(|id| mcp::error(id, -32602, &message).try_into())
                .collect::<Result<_, String>>()
                .map_err(Fault::internal)?;
            let answer = Output::new(errors);
            return Ok(Some(Response::stream(stream::iter([Ok(answer)]))));
        }

        // The answers to this post's `tools/list` requests keep only allowed tools.
        let list_ids: Vec<Value> = input
            .messages
            .iter()
            .filter(|m| m.method() == Some("tools/list"))
            .filter_map(|m| m.request_id())
            .collect();
        let allowed = self.0.clone();
        cx.filter(move |out: &mut Output| {
            let lists = out
                .messages
                .iter_mut()
                .filter(|m| m.response_id().is_some_and(|id| list_ids.contains(&id)))
                .filter_map(|m| m.result_mut()?.get_mut("tools")?.as_array_mut());
            for tools in lists {
                tools.retain(|t| allows(&allowed, &t["name"]));
            }
            Ok(())
        })?;
        Ok(None)
    }
}

/// Posts one JSON-RPC message as an MCP client does, in `session` if given:
/// the status, the session the answer names, and the response to `id`.
async fn send(
    gw: &Gateway,
    session: Option<&str>,
    id: Option<u64>,
    method: &str,
    params: Value,
) -> Result<(StatusCode, Option<String>, Value), Fault> {
    let mut body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    if let Some(id) = id {
        body["id"] = json!(id);
    }
    let mut req = http::Request::new(Body::full(body.to_string()));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/mcp");
    let headers = req.headers_mut();
    let accept = "application/json, text/event-stream";
    headers.insert("accept", HeaderValue::from_static(accept));
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    if let Some(session) = session.and_then(|s| HeaderValue::from_str(s).ok()) {
        headers.insert(SESSION, session);
    }

    let (resp, finished) = respan_gateway_http::handle(gw, req, LIMIT).await;
    let (head, body) = resp.into_parts();
    let body = body.collect().await?.to_bytes();
    finished.await?;

    let session = head.headers.get(SESSION).and_then(|s| s.to_str().ok());
    // The answer may be JSON or an SSE stream with notifications among the
    // responses; keep the response to `id`.
    let body = String::from_utf8_lossy(&body);
    let answer = body
        .split_inclusive("\n\n")
        .filter_map(respan_gateway_http::content::json)
        .find(|m| id.is_some_and(|id| m["id"] == id) && m.get("method").is_none());
    Ok((
        head.status,
        session.map(str::to_owned),
        answer.unwrap_or_default(),
    ))
}

/// Serves a small MCP server on a local port and returns its URL: it
/// assigns session `s1` and offers the tools `echo` and `wipe`, of which it
/// answers only `echo`.
async fn mcp_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let url = format!("http://{}", listener.local_addr().expect("its address"));
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(|headers: HeaderMap, body: String| async move {
            let in_session = headers.get(SESSION).is_some_and(|s| s == "s1");
            let body: Value = serde_json::from_str(&body).unwrap_or_default();
            let reply = |status, body: String| {
                let headers = [("content-type", "application/json"), (SESSION, "s1")];
                (status, headers, body)
            };
            let result = match body["method"].as_str().unwrap_or_default() {
                "initialize" => json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "example-server", "version": "1" }
                }),
                "notifications/initialized" if in_session => {
                    return reply(StatusCode::ACCEPTED, String::new());
                }
                "tools/list" if in_session => json!({ "tools": [
                    { "name": "echo", "inputSchema": { "type": "object",
                        "properties": { "message": { "type": "string" } } } },
                    { "name": "wipe", "inputSchema": { "type": "object" } }
                ] }),
                "tools/call" if in_session && body["params"]["name"] == "echo" => {
                    let said = &body["params"]["arguments"]["message"];
                    json!({ "content": [{ "type": "text", "text": said }] })
                }
                _ => return reply(StatusCode::BAD_REQUEST, "unexpected request".into()),
            };
            let answer = json!({ "jsonrpc": "2.0", "id": body["id"], "result": result });
            reply(StatusCode::OK, answer.to_string())
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}
