//! `GET /v1/models` answered by merging provider model lists, each provider
//! called with its own key.
//!
//! The endpoint is its own format, `Models`, built on `passthrough`: its
//! values are the bytes of the request and of each answer, which is all a
//! plugin that never reads a chat needs. The `MergeModels` plugin answers in
//! `on_client`: it relays the client's request to every provider, reads each
//! answer's JSON, and returns one list in which the first entry of an id
//! wins. A provider that fails is left out; only when every one fails does
//! the request fail.
//!
//! Each provider's key is set on its own transport, so the client's key is
//! never sent upstream.
//!
//! Run it with `cargo run -p respan-gateway-examples --example models_merge`.
//! Providers `a` and `b` list models (both list `shared`); `c` answers 500.
//! Look for the key each provider saw and the merged list, `shared` once.
//!
//! Not covered: list shapes other than OpenAI's (`{"data": [{"id": ..}]}`),
//! prefixing ids with their provider, and caching the merged list.

use respan_gateway_core::futures::future::join_all;
use respan_gateway_core::futures::{TryStreamExt, stream};
use respan_gateway_core::{
    ClientContext, Connection, Fault, Format, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{All, Body, ClientIo, ProviderIo, content};
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_passthrough_http as passthrough;
use serde_json::{Value, json};

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let providers = [
        ("a", Some(&["gpt-x", "shared"][..]), "Bearer key-a"),
        ("b", Some(&["llama", "shared"][..]), "Bearer key-b"),
        ("c", None, "Bearer key-c"),
    ];

    // Registration and the plugin's provider list come from the same table,
    // so every configured provider is asked.
    let mut gw = Gateway::new().client::<passthrough::Input<Models>, _, _>(Models)?;
    for (name, ids, key) in providers {
        let url = models_server(name, ids).await;
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static(key));
        let transport = HttpTransport::new(&url)?.with_headers(headers)?;
        gw = gw.provider(Provider::new(name, Models, transport.connect()))?;
    }
    let gw = gw.plugin(MergeModels(providers.iter().map(|(n, ..)| *n).collect()));

    println!("client asks GET /v1/models with key Bearer client");
    let req = http::Request::get("/v1/models")
        .header("authorization", "Bearer client")
        .body(Body::full(""))
        .map_err(|e| Fault::internal(e.to_string()))?;
    let (resp, _) = respan_gateway_http::handle(&gw, req, LIMIT).await;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    gw.idle().await;
    println!("client got: {status} {}", String::from_utf8_lossy(&body));
    Ok(())
}

/// `GET /v1/models`, relayed unread to one provider, merged by the plugin.
#[derive(Clone)]
struct Models;

impl Format<ClientIo, passthrough::Input<Models>, passthrough::Output<Models>> for Models {
    fn name(&self) -> &str {
        "models"
    }

    fn matches(&self, io: &ClientIo) -> bool {
        io.request().method() == http::Method::GET && io.request().uri().path() == "/v1/models"
    }

    fn bind(
        &self,
        io: ClientIo,
    ) -> Result<Connection<passthrough::Input<Models>, passthrough::Output<Models>>, Fault> {
        let io = io.with_error(|fault, _, head| {
            content::default_content_type(head, "application/json");
            json!({ "error": { "message": fault.message() } })
                .to_string()
                .into()
        });
        Ok(io.bind(passthrough::read_request, |reply| {
            // A merged list has no provider's head to borrow a content type from.
            let mut head = reply.head();
            content::default_content_type(&mut head, "application/json");
            let write = |all: &[passthrough::Output<Models>]| Ok(bytes_of(all));
            Ok(reply.write(head, Box::new(All::new(write))))
        }))
    }
}

impl Format<ProviderIo, passthrough::Output<Models>, passthrough::Input<Models>> for Models {
    fn name(&self) -> &str {
        "models"
    }

    fn bind(
        &self,
        io: ProviderIo,
    ) -> Result<Connection<passthrough::Output<Models>, passthrough::Input<Models>>, Fault> {
        Ok(io.bind(passthrough::write_request, passthrough::read_response))
    }
}

/// The bytes of a passthrough answer's frames, joined.
fn bytes_of(frames: &[passthrough::Output<Models>]) -> Vec<u8> {
    frames.iter().flat_map(|f| f.0.body().to_vec()).collect()
}

/// Answers `GET /v1/models` with the lists of these providers merged.
struct MergeModels(Vec<&'static str>);

#[async_trait]
impl Plugin for MergeModels {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if !cx.is_kind::<passthrough::Input<Models>>() {
            return Ok(None);
        }
        let calls: Vec<_> = self.0.iter().map(|p| cx.call(*p)).collect();
        let lists = join_all(calls.into_iter().map(|call| async move {
            let frames: Vec<passthrough::Output<Models>> =
                call.await?.outputs().try_collect().await?;
            let mut list: Value = serde_json::from_slice(&bytes_of(&frames))
                .map_err(|_| Fault::upstream("a provider's models are not JSON"))?;
            match list["data"].take() {
                Value::Array(models) if models.iter().all(|m| m["id"].is_string()) => Ok(models),
                _ => Err(Fault::upstream("a provider's models are not a list of ids")),
            }
        }))
        .await;

        let mut data: Vec<Value> = Vec::new();
        let mut failure = None;
        for list in lists {
            match list {
                Ok(models) => {
                    for model in models {
                        if !data.iter().any(|seen| seen["id"] == model["id"]) {
                            data.push(model);
                        }
                    }
                }
                Err(fault) => {
                    println!("  a provider failed and is left out: {}", fault.message());
                    failure.get_or_insert(fault);
                }
            }
        }
        if data.is_empty()
            && let Some(fault) = failure
        {
            return Err(fault);
        }
        let merged = json!({ "object": "list", "data": data }).to_string();
        let answer = passthrough::Output::<Models>::new(merged);
        Ok(Some(Response::stream(stream::iter([Ok(answer)]))))
    }
}

/// Serves a provider's `GET /v1/models` on a local port and returns its URL.
/// It lists `ids`, or fails with a 500 if there are none, and prints the key
/// each request carried.
async fn models_server(name: &'static str, ids: Option<&'static [&'static str]>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let url = format!("http://{}", listener.local_addr().expect("its address"));
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move |headers: http::HeaderMap| async move {
            let key = headers.get("authorization").and_then(|k| k.to_str().ok());
            println!(
                "  provider {name} got a request with key {}",
                key.unwrap_or("-")
            );
            let Some(ids) = ids else {
                return (StatusCode::INTERNAL_SERVER_ERROR, "{}".to_owned());
            };
            let data: Vec<Value> = ids
                .iter()
                .map(|id| json!({ "id": id, "object": "model", "owned_by": name }))
                .collect();
            let list = json!({ "object": "list", "data": data });
            (StatusCode::OK, list.to_string())
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}
