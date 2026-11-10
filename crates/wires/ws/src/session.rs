//! One WebSocket connection pumped both ways, the same at either edge, and
//! [`serve`], the client edge's upgrade.

use respan_gateway_core::FaultKind;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use respan_gateway_core::futures::channel::oneshot;
use respan_gateway_core::futures::stream::{SplitSink, SplitStream};
use respan_gateway_core::futures::{SinkExt, StreamExt, stream};
use respan_gateway_core::{Fault, Gateway};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::http::header::{self, HeaderValue};
use respan_gateway_http::http::{self, Method, StatusCode, Version};
use respan_gateway_http::rules::{plain_response, remove_if};
use respan_gateway_http::{Body, Credential, Frames};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error, Message, Utf8Bytes};

use crate::{CloseCode, Limits};
use respan_gateway_http::rules::header_tokens;

/// Hand a client's WebSocket upgrade, `head`, to `gateway`, and pump its
/// frames both ways once `upgraded` gives the connection: the HTTP
/// adapter's [`handle`](respan_gateway_http::handle), live, with the
/// client's text frames as the request's body, one message per frame of up
/// to `limits.max_message`, and the response's body as the frames sent
/// back. For an adapter: `upgraded` is the server's upgrade of this
/// request (hyper's `upgrade::on`), `None` if it failed.
///
/// The gateway answers before the client is upgraded, so a request it
/// refuses (no format accepts it, a plugin refuses it, the provider cannot
/// be reached or refuses the upgrade) is an ordinary HTTP answer to the
/// handshake, read whole within `limits.close_wait` up to
/// `limits.max_message`, as clients (tungstenite) do not decode a chunked
/// one: 404, 401, 502... A body that fails, is late or does not fit is
/// written as text under the fault's status, with its error code. With a
/// `101` the client is upgraded with the `101`'s headers, and its
/// subprotocol if the client offered it, unless that is a key
/// ([`Credential`]): a key is never sent back. A request that is no
/// WebSocket handshake (RFC 6455 4.2.1: a `GET` over HTTP/1.1 with
/// `Upgrade: websocket`, `Connection: upgrade`, a key and version 13) is
/// refused 400 `bad_request` before the gateway sees it.
///
/// # How a session ends
///
/// Both edges, this one and [`WsTransport`](crate::WsTransport)'s, pump
/// alike: a peer's frames are read no faster than they are taken, one
/// ahead, so its connection dropping behind a frame not yet taken is seen
/// at once; a peer quiet for `limits.ping_after` while read is pinged, and
/// quiet that long again, is gone. A Close is given `limits.close_send` to
/// leave, and its reason is a fault's code ([`Fault::code`](respan_gateway_core::Fault::code)), never
/// its message. A `CloseCode` no Close may carry (RFC 6455 7.4) is sent
/// as 1011. What the client and the provider are told:
///
/// | The end | Provider (input) | Client (output) |
/// |---|---|---|
/// | Client sends Close | input ends clean; provider is sent Close 1000, and its end, however it comes within `close_wait`, is clean | nothing more: the response is read to its end within `close_wait`, unsent |
/// | Client drops, no Close, or is silent through a ping | input ends `Err(abandoned)`; provider is sent Close 1001 | nothing, as it is gone; request and calls end `abandoned` |
/// | Client's upgrade fails | as when it drops | nothing |
/// | Client sends a binary frame, or a message over `max_message` | input ends `Err(bad_request)` with its [`CloseCode`]; provider is sent that Close | Close 1003 (binary) or 1009 (too large); the response is read to its end, unsent |
/// | Provider sends Close 1000 (or no code) | dropped | Close 1000; ends clean |
/// | Provider sends Close with another code | dropped | an `upstream` fault with that [`ProviderClose`]: Close 1011, or that code if the call was relayed (`respan_gateway_plugin_relay::Relay`) |
/// | Provider drops, no Close, or is silent through a ping | dropped | Close 1011 `truncated` |
/// | Provider sends a binary frame or a message over `max_message` | it is sent Close 1003 or 1009 | Close 1011 `upstream` |
/// | Provider is unreachable, or refuses the upgrade | (no session) | the client's format answers the handshake: 502, or under the provider's status |
/// | `stop` completes | provider is sent Close 1001 | Close 1001 |
/// | A filter or plugin refuses an input | input ends `Err(fault)`; provider is sent the fault's [`CloseCode`], else 1008 for the client's fault, 1011 for another | the fault's [`CloseCode`], else 1008 for the client's fault, 1011 for another |
/// | A fault ends the response | provider is sent Close 1001 | the fault's [`CloseCode`], else 1008 for the client's fault, 1011 for another (1001 if `abandoned`) |
///
/// A provider's close reason text is not relayed, nor is a client's close
/// code or reason. A session that has ended ignores `stop`.
pub async fn serve<S>(
    gateway: &Gateway,
    head: http::request::Parts,
    upgraded: impl Future<Output = Option<S>> + Send + 'static,
    limits: Limits,
    stop: impl Future<Output = ()> + Send + 'static,
) -> http::Response<Body>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(key) = handshake(&head) else {
        let fault = Fault::bad_request("the request is no WebSocket handshake");
        return text(&fault);
    };
    // The subprotocols the client offered that are no key.
    let mut offered = http::Request::new(()).into_parts().0;
    for value in head.headers.get_all(header::SEC_WEBSOCKET_PROTOCOL) {
        (offered.headers).append(header::SEC_WEBSOCKET_PROTOCOL, value.clone());
    }
    Credential::lift(&mut offered);
    let accept = HeaderValue::from_str(&derive_accept_key(key.as_bytes()));
    let (frames, end, input) = inbound(Fault::abandoned);
    let req = http::Request::from_parts(head, Body::stream(input));
    let (resp, _ended) = respan_gateway_http::handle_live(gateway, req, limits.max_message).await;
    let (mut head, output) = resp.into_parts();
    if head.status != StatusCode::SWITCHING_PROTOCOLS {
        return refusal(head, output, limits).await;
    }
    // The provider's other headers go with the upgrade; the handshake's are this one's.
    let chosen = head.headers.remove(header::SEC_WEBSOCKET_PROTOCOL);
    let chosen = chosen.filter(|chosen| {
        let mut offers = header_tokens(&offered.headers, header::SEC_WEBSOCKET_PROTOCOL);
        offers.any(|offer| offer.as_bytes() == chosen.as_bytes())
    });
    remove_if(&mut head.headers, |name| {
        name.as_str().starts_with("sec-websocket-")
    });
    let headers = &mut head.headers;
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    // Base64, always a valid value.
    if let Ok(accept) = accept {
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    }
    if let Some(chosen) = chosen {
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, chosen);
    }
    tokio::spawn(async move {
        // An upgrade that failed drops `end`: the input ends `abandoned`.
        if let Some(io) = upgraded.await {
            let output = output.boxed();
            let edge = Edge::Client;
            let _ = pump(io, Role::Server, output, (frames, end), edge, limits, stop).await;
        }
    });
    http::Response::from_parts(head, Body::default())
}

/// The key of `head` if it is a WebSocket handshake (RFC 6455 4.2.1).
fn handshake(head: &http::request::Parts) -> Option<&HeaderValue> {
    let asks = |name, token: &str| {
        header_tokens(&head.headers, name).any(|t| t.eq_ignore_ascii_case(token))
    };
    let version = head.headers.get(header::SEC_WEBSOCKET_VERSION);
    let valid = head.method == Method::GET
        && head.version == Version::HTTP_11
        && asks(header::UPGRADE, "websocket")
        && asks(header::CONNECTION, "upgrade")
        && version.is_some_and(|v| v == "13");
    head.headers
        .get(header::SEC_WEBSOCKET_KEY)
        .filter(|_| valid)
}

/// An answer that is no upgrade, whole and sized, as [`serve`] says.
async fn refusal(
    head: http::response::Parts,
    mut body: Body,
    limits: Limits,
) -> http::Response<Body> {
    let read = async {
        let mut all = BytesMut::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            if all.len() + chunk.len() > limits.max_message {
                return Err(Fault::internal(
                    "a refusal of the upgrade is over max_message",
                ));
            }
            all.extend_from_slice(&chunk);
        }
        Ok(all.freeze())
    };
    match tokio::time::timeout(limits.close_wait, read).await {
        Ok(Ok(all)) => http::Response::from_parts(head, Body::full(all)),
        Ok(Err(fault)) => text(&fault),
        Err(_) => text(&Fault::internal(
            "a refusal of the upgrade took over close_wait",
        )),
    }
}

/// `fault` as text/plain under its status: its error code, never its message.
fn text(fault: &Fault) -> http::Response<Body> {
    plain_response(fault, fault.code())
}

/// Which edge a connection is, for what its peer's end means.
#[derive(Clone, Copy)]
pub(crate) enum Edge {
    /// The client's: its frames are the request's input, which the gateway
    /// may stop reading while the response goes on.
    Client,
    /// The provider's: its frames are the call's answer. `relayed` passes
    /// its Close code on as a [`CloseCode`]; `binary_text` takes its binary
    /// UTF-8 messages as text.
    Provider { relayed: bool, binary_text: bool },
}

/// Where a peer's frames go: a sender, one frame in flight, and the end of
/// the stream it feeds, which [`inbound`] returns.
pub(crate) type Inbound = (mpsc::Sender<Result<Bytes, Fault>>, End);

/// The end of a peer's frames. Finished, they end clean once all are taken;
/// failed, or dropped unfinished, they end with the fault at once, ahead
/// of frames not yet taken, as nothing can answer a peer that is gone.
pub(crate) struct End {
    tx: Option<oneshot::Sender<Fault>>,
    gone: fn() -> Fault,
}

impl End {
    fn fail(mut self, fault: Fault) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(fault);
        }
    }

    fn finish(mut self) {
        self.tx = None;
    }
}

impl Drop for End {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.gone)());
        }
    }
}

/// A peer's frames ([`Inbound`]) and the stream they feed, which ends with
/// `gone` if its [`End`] is dropped unfinished.
pub(crate) fn inbound(gone: fn() -> Fault) -> (mpsc::Sender<Result<Bytes, Fault>>, End, Frames) {
    let (frames, rx) = mpsc::channel(1);
    let (tx, ended) = oneshot::channel();
    let stream = stream::unfold(Some((rx, ended)), |state| async move {
        let (mut rx, mut ended) = state?;
        tokio::select! {
            biased;
            Ok(fault) = &mut ended => Some((Err(fault), None)),
            Some(frame) = rx.recv() => Some((frame, Some((rx, ended)))),
            else => None,
        }
    });
    let end = End { tx: Some(tx), gone };
    (frames, end, stream.boxed())
}

/// Pump `io`, upgraded in `role`: `out`'s frames to the peer as text
/// messages, then a Close, and the peer's text messages into `inbound`,
/// until one side ends and the other has had its say, as [`serve`]'s table
/// says. `Err` if `out` failed, with its fault.
pub(crate) async fn pump<S>(
    io: S,
    role: Role,
    mut out: Frames,
    (frames, end): Inbound,
    edge: Edge,
    limits: Limits,
    stop: impl Future<Output = ()>,
) -> Result<(), Fault>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.max_message))
        .max_frame_size(Some(limits.max_message));
    let (mut sink, stream) = WebSocketStream::from_raw_socket(io, role, Some(config))
        .await
        .split();
    let (ping, closing) = (Notify::new(), AtomicBool::new(false));
    let peer = Peer {
        stream,
        ping: &ping,
        after: limits.ping_after,
        pinged: false,
    };
    let mut read = pin!(read(peer, frames, &closing, edge, limits.close_send));
    let mut stop = pin!(stop);
    let heard = tokio::select! {
        // A failure of `out`, which ends the peer's frames too, says why.
        biased;
        sent = write(&mut sink, &mut out, &ping, &mut stop) => match sent {
            Sent::Ended => {
                closing.store(true, Ordering::SeqCst);
                close(&mut sink, 1000, "", limits.close_send).await;
                let _ = tokio::time::timeout(limits.close_wait, read).await;
                end.finish();
                return Ok(());
            }
            Sent::Failed(fault) => {
                end.fail(fault.clone());
                let code = match (fault.kind(), fault.get::<CloseCode>()) {
                    (FaultKind::Abandoned, _) => 1001,
                    (_, Some(code)) => code.sendable(),
                    (FaultKind::Client, None) => 1008,
                    _ => 1011,
                };
                close(&mut sink, code, fault.code(), limits.close_send).await;
                return Err(fault);
            }
            Sent::Stopped => {
                close(&mut sink, 1001, "", limits.close_send).await;
                return Ok(());
            }
            // The reader says how the connection broke.
            Sent::Lost => read.await,
        },
        heard = &mut read => heard,
    };
    match heard {
        Heard::Ended(refused) => {
            end.finish();
            if let Some((code, fault)) = refused {
                close(&mut sink, code, fault.code(), limits.close_send).await;
            }
            // The response's end says how the request ended: it is read on, unsent.
            if let Edge::Client = edge {
                let rest = out.for_each(|_| std::future::ready(()));
                let _ = tokio::time::timeout(limits.close_wait, rest).await;
            }
        }
        Heard::Gone => {}
        // No one reads the provider any more: the gateway goes away.
        Heard::Ignored => close(&mut sink, 1001, "", limits.close_send).await,
    }
    Ok(())
}

/// How the frames to the peer ended.
enum Sent {
    /// `out` ended clean.
    Ended,
    /// `out` ended `Err`, or held a frame that is no text.
    Failed(Fault),
    /// `stop` completed.
    Stopped,
    /// The connection broke while sending.
    Lost,
}

/// How the peer's side ended.
enum Heard {
    /// Its frames ended in order: with its Close, a provider's drop
    /// (`truncated`), or a fault for a message it may not send, answered
    /// with Close `code`.
    Ended(Option<(u16, Fault)>),
    /// The client dropped, broke, or was silent through a ping: [`End`]'s
    /// `gone`, at once.
    Gone,
    /// No one takes the provider's frames any more.
    Ignored,
}

/// `out`'s frames as text messages, and a ping when the reader asks, until
/// `out` or the connection ends or `stop` completes.
async fn write<S: AsyncRead + AsyncWrite + Unpin>(
    sink: &mut SplitSink<WebSocketStream<S>, Message>,
    out: &mut Frames,
    ping: &Notify,
    stop: &mut std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Sent {
    loop {
        let message = tokio::select! {
            biased;
            () = stop.as_mut() => return Sent::Stopped,
            () = ping.notified() => Message::Ping(Bytes::new()),
            next = out.next() => match next {
                Some(Ok(bytes)) => match Utf8Bytes::try_from(bytes) {
                    Ok(text) => Message::Text(text),
                    Err(_) => {
                        let text = "a frame to send is not text: only text frames are carried";
                        return Sent::Failed(Fault::config(text));
                    }
                },
                Some(Err(fault)) => return Sent::Failed(fault),
                None => return Sent::Ended,
            },
        };
        // A peer that stopped reading blocks the send: `stop` still ends it.
        tokio::select! {
            sent = sink.send(message) => if sent.is_err() {
                return Sent::Lost;
            },
            () = stop.as_mut() => return Sent::Stopped,
        }
    }
}

/// A peer's messages, pinging it once it has been quiet for `after`.
struct Peer<'a, S> {
    stream: SplitStream<WebSocketStream<S>>,
    ping: &'a Notify,
    after: Duration,
    pinged: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Peer<'_, S> {
    /// The next message, or `None` once the connection ended or the peer
    /// was quiet through a ping.
    async fn next(&mut self) -> Option<Result<Message, Error>> {
        loop {
            match tokio::time::timeout(self.after, self.stream.next()).await {
                Ok(message) => {
                    self.pinged = false;
                    return message;
                }
                Err(_) if !self.pinged => {
                    self.pinged = true;
                    self.ping.notify_one();
                }
                Err(_) => return None,
            }
        }
    }
}

/// The peer's text messages into `frames`, then how it ended. While a frame
/// waits to be taken, one more message is read, so a client that leaves is
/// seen at once, its frames not yet taken dropped, as nothing can answer
/// them. Any other end, a provider's that drops included (`truncated`), is
/// acted on in order, after the frames before it. Once `closing` (a Close
/// was sent), any end is clean.
async fn read<S: AsyncRead + AsyncWrite + Unpin>(
    mut peer: Peer<'_, S>,
    frames: mpsc::Sender<Result<Bytes, Fault>>,
    closing: &AtomicBool,
    edge: Edge,
    close_send: Duration,
) -> Heard {
    let provider = matches!(edge, Edge::Provider { .. });
    let binary_text = matches!(edge, Edge::Provider { binary_text, .. } if binary_text);
    let refused = |code, text: &str| {
        let fault = match edge {
            Edge::Client => Fault::bad_request(text).with(CloseCode(code)),
            Edge::Provider { .. } => Fault::upstream(text),
        };
        (Some(code), fault)
    };
    let mut ahead = None;
    let (code, fault): (Option<u16>, Fault) = loop {
        let message = match ahead.take() {
            Some(message) => message,
            None => tokio::select! {
                biased;
                () = frames.closed(), if provider => return Heard::Ignored,
                message = peer.next() => message,
            },
        };
        let frame = match message {
            Some(Ok(Message::Text(text))) => Bytes::from(text),
            Some(Ok(Message::Binary(bytes))) if binary_text => {
                if std::str::from_utf8(&bytes).is_err() {
                    break refused(1003, "a binary message is not UTF-8 text");
                }
                bytes
            }
            Some(Ok(Message::Binary(_))) => {
                break refused(1003, "a binary frame: only text frames are carried");
            }
            Some(Ok(Message::Close(frame))) => {
                let reason = frame
                    .as_ref()
                    .map(|f| f.reason.to_string())
                    .unwrap_or_default();
                let code = frame.map(|f| u16::from(f.code));
                let clean = closing.load(Ordering::SeqCst) || matches!(code, None | Some(1000));
                if let (Edge::Provider { relayed, .. }, false, Some(code)) = (edge, clean, code) {
                    let mut fault = Fault::from(crate::Error::ProviderClosed { code, reason });
                    if relayed {
                        fault = fault.with(CloseCode(code));
                    }
                    let _ = frames.send(Err(fault)).await;
                }
                // The library sends its reply to the Close as the stream is read on.
                let _ = tokio::time::timeout(close_send, peer.stream.next()).await;
                return Heard::Ended(None);
            }
            Some(Ok(_)) => continue,
            // No Close came. After ours, that is how a peer may end.
            Some(Err(_)) | None if closing.load(Ordering::SeqCst) => return Heard::Ended(None),
            Some(Err(Error::Capacity(_))) => break refused(1009, "a message is too large"),
            Some(Err(_)) | None if provider => break (None, Fault::truncated()),
            Some(Err(_)) | None => return Heard::Gone,
        };
        let mut send = pin!(frames.send(Ok(frame)));
        loop {
            tokio::select! {
                sent = &mut send => {
                    // No one reads the client's input, but the response may
                    // go on: its frames are dropped.
                    if sent.is_err() && provider {
                        return Heard::Ignored;
                    }
                    break;
                }
                message = peer.next(), if ahead.is_none() => match message {
                    Some(Ok(_)) | Some(Err(Error::Capacity(_))) => ahead = Some(message),
                    _ if provider || closing.load(Ordering::SeqCst) => ahead = Some(message),
                    _ => return Heard::Gone,
                },
            }
        }
    };
    let _ = frames.send(Err(fault.clone())).await;
    Heard::Ended(code.map(|code| (code, fault)))
}

/// Send Close `code` with `reason` (cut to the 123 bytes a Close takes)
/// within `wait`, as a peer that has not taken it by then has stopped
/// reading.
async fn close<S: AsyncRead + AsyncWrite + Unpin>(
    sink: &mut SplitSink<WebSocketStream<S>, Message>,
    code: u16,
    reason: &str,
    wait: Duration,
) {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let frame = CloseFrame {
        code: code.into(),
        reason: reason[..end].into(),
    };
    let _ = tokio::time::timeout(wait, async {
        let _ = sink.send(Message::Close(Some(frame))).await;
        let _ = sink.close().await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_peers_frames_end_clean_only_once_finished() {
        let (frames, end, mut stream) = inbound(Fault::abandoned);
        frames
            .send(Ok(Bytes::from_static(b"a")))
            .await
            .expect("taken");
        end.finish();
        drop(frames);
        assert_eq!(stream.next().await.expect("a frame").expect("text"), "a");
        assert!(stream.next().await.is_none());
        // Unfinished, as when an upgrade fails: `gone`, ahead of the frame.
        let (frames, end, mut stream) = inbound(Fault::abandoned);
        frames
            .send(Ok(Bytes::from_static(b"a")))
            .await
            .expect("taken");
        drop((frames, end));
        let fault = stream.next().await.expect("an end").expect_err("gone");
        assert!(fault.is(FaultKind::Abandoned));
        assert!(stream.next().await.is_none());
    }
}
