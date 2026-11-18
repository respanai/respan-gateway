//! OpenAI's Responses WebSocket mode, as a layer over any HTTP service.

use std::collections::{HashMap, HashSet, VecDeque};
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
/// but for a `store: false` response, which OpenAI keeps for its socket
/// alone: so does this, the latest of each lane, as the items sent and
/// answered, and a turn continuing one is sent them as its `input`'s start.
/// Any other request passes through.
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
    let mut kept: HashMap<Lane, Kept> = HashMap::new();
    let mut turns = FuturesUnordered::new();
    let mut deadline = pin!(tokio::time::sleep(SESSION_MAX));
    let mut quiet = Box::pin(tokio::time::sleep(limits.ping_after));
    let mut pinged = false;
    let close = loop {
        while turns.len() < IN_FLIGHT {
            let Some(at) = waiting.iter().position(|(lane, _)| !busy.contains(lane)) else {
                break;
            };
            let (lane, mut body) = waiting.remove(at).expect("a waiting turn");
            let replay = replay(&mut body, &lane, &kept);
            busy.insert(lane.clone());
            turns.push(turn(inner.clone(), &head, lane, body, replay, tx.clone()));
        }
        let message = tokio::select! {
            biased;
            () = &mut deadline => {
                let message = "the connection reached its 60-minute limit";
                let code = Some("websocket_connection_limit_reached");
                let event = error(400, code, message, None, &None);
                let _ = sink.send(Message::text(event)).await;
                break CloseCode::Normal;
            }
            Some(frame) = rx.recv() => Message::text(frame),
            Some(done) = turns.next() => {
                busy.remove(&done.lane);
                match done.kept {
                    Some(new) => _ = kept.insert(done.lane, new),
                    None if done.evicts => _ = kept.remove(&done.lane),
                    None => {}
                }
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
    let invalid = |message| error(400, None, message, None, &None);
    let Ok(Value::Object(mut body)) = serde_json::from_str(text) else {
        return Err(invalid(
            "Expected a 'response.create' message as the first websocket event.",
        ));
    };
    if body.remove("type").is_none_or(|t| t != "response.create") {
        return Err(invalid(
            "Expected a 'response.create' message as the first websocket event.",
        ));
    }
    if body.get("stream").is_some_and(|s| s != true) {
        return Err(invalid(
            "The 'stream' parameter is not supported on /v1/responses.",
        ));
    }
    if body.get("background").is_some_and(|b| b == true) {
        return Err(invalid(
            "The 'background' parameter is not supported on /v1/responses over websockets.",
        ));
    }
    let lane = match body.remove("stream_id") {
        None => None,
        Some(Value::String(id)) if valid_lane(&id) => Some(id),
        Some(_) => {
            let message = "The 'stream_id' field must be a non-empty string with at most 256 \
                characters and may only contain letters, numbers, underscores, hyphens, and periods.";
            let code = Some("invalid_stream_id");
            return Err(error(400, code, message, Some("stream_id"), &None));
        }
    };
    if let Some(id) = &lane
        && !named.contains(id)
    {
        if named.len() >= LANES {
            let message = "a connection has at most 32 stream_id values";
            let code = Some("websocket_stream_limit_reached");
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

/// A lane's latest `store: false` response: its id and every item of it,
/// sent and answered.
#[derive(Clone, Debug)]
struct Kept {
    id: String,
    items: Vec<Value>,
}

/// What a turn keeps of itself: for `store: false`, the items it sends; and
/// whether it continues its own lane's kept response.
#[derive(Default)]
struct Replay {
    sent: Option<Vec<Value>>,
    continues_own: bool,
}

/// How a turn ended, for its lane's kept response.
struct Done {
    lane: Lane,
    /// Its own response, to keep in place of the lane's.
    kept: Option<Kept>,
    /// Whether the lane's kept response goes: a stored response is its
    /// latest, or a continuation of it failed.
    evicts: bool,
}

/// Start `body` from the items of the kept response its
/// `previous_response_id` names, if one is kept, and ask a `store: false`
/// one for its reasoning's `encrypted_content`, without which it cannot be
/// sent again.
fn replay(body: &mut Map<String, Value>, lane: &Lane, kept: &HashMap<Lane, Kept>) -> Replay {
    let named = body.get("previous_response_id").and_then(Value::as_str);
    let parent = named.and_then(|id| kept.iter().find(|(_, k)| k.id == id));
    let continues_own = parent.is_some_and(|(owner, _)| owner == lane);
    if let Some((_, parent)) = parent {
        body.remove("previous_response_id");
        let mut items = parent.items.clone();
        items.extend(input_items(body.get("input")));
        body.insert("input".into(), items.into());
    }
    if body.get("store") != Some(&Value::Bool(false)) {
        return Replay {
            sent: None,
            continues_own,
        };
    }
    let include = body.entry("include").or_insert_with(|| json!([]));
    if let Some(include) = include.as_array_mut()
        && !include.iter().any(|i| i == "reasoning.encrypted_content")
    {
        include.push("reasoning.encrypted_content".into());
    }
    Replay {
        sent: Some(input_items(body.get("input"))),
        continues_own,
    }
}

/// `input` as items: a string is one user message.
fn input_items(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// The kept response of a `response.completed` event of a turn that sent
/// `sent`, unless it would be over `limit` bytes: its reasoning without
/// `encrypted_content` left out, as it cannot be sent again.
fn completed(data: &str, sent: &[Value], limit: usize) -> Option<Kept> {
    let event: Value = serde_json::from_str(data).ok()?;
    if event["type"] != "response.completed" {
        return None;
    }
    let response = &event["response"];
    let output = response["output"].as_array()?;
    let replayable = |item: &&Value| {
        item["type"] != "reasoning" || item.get("encrypted_content").is_some_and(Value::is_string)
    };
    let mut items = sent.to_vec();
    items.extend(output.iter().filter(replayable).cloned());
    let size: usize = items.iter().map(|i| i.to_string().len()).sum();
    let id = response["id"].as_str()?.to_owned();
    (size <= limit).then_some(Kept { id, items })
}

/// One turn through `inner`, its events sent to `tx`; how it ended once
/// done.
fn turn<S, B>(
    mut inner: S,
    head: &Parts,
    lane: Lane,
    body: Map<String, Value>,
    replay: Replay,
    tx: mpsc::Sender<String>,
) -> BoxFuture<'static, Done>
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
        let mut done = Done {
            lane,
            kept: None,
            evicts: replay.continues_own,
        };
        let lane = &done.lane.clone();
        let limit = respan_gateway_ws::Limits::default().max_message;
        let ready = poll_fn(|cx| inner.poll_ready(cx)).await.is_ok();
        let answer = match ready {
            true => inner.call(req).await.ok(),
            false => None,
        };
        let event = |status, message| error(status, None, message, None, lane);
        let Some(answer) = answer else {
            let _ = tx.send(event(500, "the gateway failed")).await;
            return done;
        };
        let status = answer.status();
        let mut body = pin!(answer.into_body());
        if !status.is_success() {
            let read = Limited::new(body, limit).collect().await;
            let read = read.map(|b| b.to_bytes()).unwrap_or_default();
            let _ = tx.send(refusal(status, &read, lane)).await;
            return done;
        }
        // The stored response is the lane's latest now.
        done.evicts = replay.sent.is_none();
        let mut sse = Sse::default();
        loop {
            let next = body.frame().await.map(Result::ok);
            let Some(frame) = next else { break };
            let Some(frame) = frame else {
                let _ = tx.send(event(502, "the response broke off")).await;
                return done;
            };
            for data in frame.into_data().map(|d| sse.feed(&d)).unwrap_or_default() {
                if let Some(sent) = &replay.sent
                    && let Some(kept) = completed(&data, sent, limit)
                {
                    done.kept = Some(kept);
                }
                if data != "[DONE]" && tx.send(tagged(data, lane)).await.is_err() {
                    return done;
                }
            }
        }
        done
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
fn error(
    status: u16,
    code: Option<&str>,
    message: &str,
    param: Option<&str>,
    lane: &Lane,
) -> String {
    let kind = match status {
        500.. => "server_error",
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
