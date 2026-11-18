//! OpenAI's Responses WebSocket mode, as a layer over any HTTP service.

use std::collections::{HashSet, VecDeque};
use std::future::{poll_fn, ready};
use std::pin::pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::BoxError;
use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::response::Response;
use http_body_util::{BodyExt, Limited};
use hyper_util::rt::TokioIo;
use respan_gateway_core::futures::future::BoxFuture;
use respan_gateway_core::futures::stream::FuturesUnordered;
use respan_gateway_core::futures::{FutureExt, SinkExt, StreamExt};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::http::header::{self, HeaderValue};
use respan_gateway_http::http::request::Parts;
use respan_gateway_http::http::{Method, StatusCode, Version};
use respan_gateway_http::rules::header_tokens;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tower_service::Service;

/// Responses running at once on a socket; more wait their turn.
const IN_FLIGHT: usize = 16;
/// Named lanes a socket may open.
const LANES: usize = 32;
/// How long a socket lasts.
const SESSION_MAX: Duration = Duration::from_secs(60 * 60);

/// A layer that serves OpenAI's Responses WebSocket mode over the service it
/// wraps: a `GET` upgrade to a path ending in `/responses` is answered with
/// the socket, and each `response.create` it sends is an ordinary streamed
/// `POST` to that path, so every layer and plugin within runs per turn.
///
/// A turn's request has the upgrade's headers (but the handshake's), query
/// and extensions, and its body is the event's but `type` and `stream_id`,
/// with `stream: true`. Its SSE `data:` are sent back as text frames, with
/// the turn's `stream_id` on a named lane's; an answer that is no success is
/// OpenAI's `error` event. Turns of a lane run in order, lanes side by side,
/// up to 16 at once, 32 named lanes, for at most 60 minutes. A
/// `previous_response_id` is the wrapped service's to resolve, as over HTTP,
/// so the socket keeps no responses of its own. Any other request passes
/// through.
///
/// Install it outermost: `router.layer(ResponsesWebSocket)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResponsesWebSocket;

impl<S> tower_layer::Layer<S> for ResponsesWebSocket {
    type Service = ResponsesWebSocketService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ResponsesWebSocketService { inner }
    }
}

/// The service [`ResponsesWebSocket`] wraps `S` in.
#[derive(Clone, Debug)]
pub struct ResponsesWebSocketService<S> {
    inner: S,
}

impl<S, B> Service<Request> for ResponsesWebSocketService<S>
where
    S: Service<Request, Response = http::Response<B>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let websocket = header_tokens(req.headers(), header::UPGRADE)
            .any(|t| t.eq_ignore_ascii_case("websocket"));
        let at = respan_gateway_http::path::path_ends_with(req.uri(), "/responses");
        if !(websocket && at && req.method() == Method::GET) {
            let call = self.inner.call(req);
            return async move { Ok(call.await?.map(Body::new)) }.boxed();
        }
        let Some(accept) = accept(&req) else {
            let refused = (
                StatusCode::BAD_REQUEST,
                "the request is no WebSocket handshake",
            );
            return ready(Ok(axum::response::IntoResponse::into_response(refused))).boxed();
        };
        let upgraded = hyper::upgrade::on(&mut req);
        let (head, _) = req.into_parts();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            if let Ok(io) = upgraded.await {
                session(TokioIo::new(io), head, inner).await;
            }
        });
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let headers = resp.headers_mut();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
        ready(Ok(resp)).boxed()
    }
}

/// The `Sec-WebSocket-Accept` of a valid handshake (RFC 6455 4.2.1).
fn accept(req: &Request) -> Option<HeaderValue> {
    let upgrade =
        header_tokens(req.headers(), header::CONNECTION).any(|t| t.eq_ignore_ascii_case("upgrade"));
    let version = req.headers().get(header::SEC_WEBSOCKET_VERSION);
    let key = req.headers().get(header::SEC_WEBSOCKET_KEY)?;
    let valid = upgrade && req.version() == Version::HTTP_11 && version.is_some_and(|v| v == "13");
    valid.then(|| HeaderValue::from_str(&derive_accept_key(key.as_bytes())).ok())?
}

/// A turn's lane: its `stream_id`, none for the default lane.
type Lane = Option<String>;

async fn session<I, S, B>(io: I, head: Parts, inner: S)
where
    I: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request, Response = http::Response<B>> + Clone + Send + 'static,
    S::Future: Send,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    let limits = respan_gateway_ws::Limits::default();
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.max_message))
        .max_frame_size(Some(limits.max_message));
    let ws = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await;
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<String>(IN_FLIGHT);
    let mut waiting: VecDeque<(Lane, Map<String, Value>)> = VecDeque::new();
    let (mut busy, mut named) = (HashSet::new(), HashSet::new());
    let mut turns = FuturesUnordered::new();
    let mut deadline = pin!(tokio::time::sleep(SESSION_MAX));
    let mut quiet = Box::pin(tokio::time::sleep(limits.ping_after));
    let mut pinged = false;
    let close = loop {
        while turns.len() < IN_FLIGHT {
            let Some(at) = waiting.iter().position(|(lane, _)| !busy.contains(lane)) else {
                break;
            };
            let (lane, body) = waiting.remove(at).expect("a waiting turn");
            busy.insert(lane.clone());
            turns.push(turn(inner.clone(), &head, lane, body, tx.clone()));
        }
        let message = tokio::select! {
            biased;
            () = &mut deadline => {
                let message = "the connection reached its 60-minute limit";
                let event = error(400, "websocket_connection_limit_reached", message, None, &None);
                let _ = sink.send(Message::text(event)).await;
                break CloseCode::Normal;
            }
            Some(frame) = rx.recv() => Message::text(frame),
            Some(lane) = turns.next() => {
                busy.remove(&lane);
                continue;
            }
            () = &mut quiet => {
                if std::mem::replace(&mut pinged, true) {
                    break CloseCode::Away;
                }
                quiet.set(tokio::time::sleep(limits.ping_after));
                Message::Ping(Bytes::new())
            }
            heard = stream.next() => {
                (pinged, quiet) = (false, Box::pin(tokio::time::sleep(limits.ping_after)));
                match heard {
                    Some(Ok(Message::Text(text))) => match create(text.as_str(), &mut named) {
                        Ok(turn) => {
                            waiting.push_back(turn);
                            continue;
                        }
                        Err(event) => Message::text(event),
                    },
                    Some(Ok(Message::Binary(_))) => break CloseCode::Unsupported,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break CloseCode::Normal,
                    Some(Ok(_)) => continue,
                }
            }
        };
        if sink.send(message).await.is_err() {
            break CloseCode::Away;
        }
    };
    let frame = CloseFrame {
        code: close,
        reason: "".into(),
    };
    let _ = tokio::time::timeout(limits.close_send, sink.send(Message::Close(Some(frame)))).await;
}

/// A `response.create` as its lane and the body of its request, or the
/// `error` event refusing it.
fn create(text: &str, named: &mut HashSet<String>) -> Result<(Lane, Map<String, Value>), String> {
    let invalid = |message| error(400, "invalid_request_error", message, None, &None);
    let Ok(Value::Object(mut body)) = serde_json::from_str(text) else {
        return Err(invalid("an event is a JSON object"));
    };
    if body.remove("type").is_none_or(|t| t != "response.create") {
        return Err(invalid("the only event a client sends is response.create"));
    }
    let lane = match body.remove("stream_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if valid_lane(&id) => Some(id),
        Some(_) => {
            let message = "stream_id is 1 to 256 letters, digits, '_', '-' or '.'";
            return Err(error(
                400,
                "invalid_stream_id",
                message,
                Some("stream_id"),
                &None,
            ));
        }
    };
    if let Some(id) = &lane
        && !named.contains(id)
    {
        if named.len() >= LANES {
            let message = "a connection has at most 32 stream_id values";
            let code = "websocket_stream_limit_reached";
            return Err(error(400, code, message, Some("stream_id"), &lane));
        }
        named.insert(id.clone());
    }
    body.insert("stream".into(), true.into());
    Ok((lane, body))
}

fn valid_lane(id: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    (1..=256).contains(&id.len()) && id.chars().all(allowed)
}

/// One turn through `inner`, its events sent to `tx`; its lane once done.
fn turn<S, B>(
    mut inner: S,
    head: &Parts,
    lane: Lane,
    body: Map<String, Value>,
    tx: mpsc::Sender<String>,
) -> BoxFuture<'static, Lane>
where
    S: Service<Request, Response = http::Response<B>> + Send + 'static,
    S::Future: Send,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    let mut req = Request::new(Body::from(Value::Object(body).to_string()));
    *req.method_mut() = Method::POST;
    *req.uri_mut() = head.uri.clone();
    *req.extensions_mut() = head.extensions.clone();
    let headers = req.headers_mut();
    for (name, value) in &head.headers {
        // Encoded events could not be read as SSE.
        let handshake = [
            header::UPGRADE,
            header::CONNECTION,
            header::CONTENT_LENGTH,
            header::ACCEPT_ENCODING,
        ];
        if !handshake.contains(name) && !name.as_str().starts_with("sec-websocket-") {
            headers.append(name, value.clone());
        }
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    async move {
        let ready = poll_fn(|cx| inner.poll_ready(cx)).await.is_ok();
        let answer = match ready {
            true => inner.call(req).await.ok(),
            false => None,
        };
        let event = |status, code, message| error(status, code, message, None, &lane);
        let Some(answer) = answer else {
            let _ = tx
                .send(event(500, "server_error", "the gateway failed"))
                .await;
            return lane;
        };
        let status = answer.status();
        let mut body = pin!(answer.into_body());
        if !status.is_success() {
            let limit = respan_gateway_ws::Limits::default().max_message;
            let read = Limited::new(body, limit).collect().await;
            let read = read.map(|b| b.to_bytes()).unwrap_or_default();
            let _ = tx.send(refusal(status, &read, &lane)).await;
            return lane;
        }
        let mut sse = Sse::default();
        loop {
            let next = body.frame().await.map(Result::ok);
            let Some(frame) = next else { break };
            let Some(frame) = frame else {
                let _ = tx
                    .send(event(502, "server_error", "the response broke off"))
                    .await;
                return lane;
            };
            for data in frame.into_data().map(|d| sse.feed(&d)).unwrap_or_default() {
                if data != "[DONE]" && tx.send(tagged(data, &lane)).await.is_err() {
                    return lane;
                }
            }
        }
        lane
    }
    .boxed()
}

/// An SSE stream's `data:` payloads, a whole event at a time.
#[derive(Default)]
struct Sse {
    buffer: BytesMut,
    data: Option<String>,
}

impl Sse {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let line = self.buffer.split_to(end + 1);
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                events.extend(self.data.take());
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                match &mut self.data {
                    Some(all) => {
                        all.push('\n');
                        all.push_str(data);
                    }
                    None => self.data = Some(data.to_owned()),
                }
            }
        }
        events
    }
}

/// `data` with its lane's `stream_id`, if named.
fn tagged(data: String, lane: &Lane) -> String {
    match (lane, serde_json::from_str::<Value>(&data)) {
        (Some(id), Ok(Value::Object(mut event))) => {
            event.insert("stream_id".into(), id.clone().into());
            Value::Object(event).to_string()
        }
        _ => data,
    }
}

/// An answer that is no success as an `error` event: its body's `error`,
/// as OpenAI's, or one saying its status.
fn refusal(status: StatusCode, body: &[u8], lane: &Lane) -> String {
    let said = serde_json::from_slice::<Value>(body).ok();
    let mut event = json!({"type": "error", "status": status.as_u16()});
    match said.and_then(|mut v| v.get_mut("error").map(Value::take)) {
        Some(said) if said.is_object() => event["error"] = said,
        _ => {
            let kind = match status.is_client_error() {
                true => "invalid_request_error",
                false => "server_error",
            };
            let message = String::from_utf8_lossy(body);
            event["error"] = json!({"type": kind, "code": null, "message": message});
        }
    }
    tagged(event.to_string(), lane)
}

/// OpenAI's `error` event.
fn error(status: u16, code: &str, message: &str, param: Option<&str>, lane: &Lane) -> String {
    let kind = match code {
        "server_error" => "server_error",
        _ => "invalid_request_error",
    };
    let error = json!({"type": kind, "code": code, "message": message, "param": param});
    tagged(
        json!({"type": "error", "status": status, "error": error}).to_string(),
        lane,
    )
}

#[cfg(test)]
mod tests;
