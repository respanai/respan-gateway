//! [`WsTransport`]: a provider reached over WebSocket.

use std::sync::Arc;
use std::time::Duration;

use respan_gateway_core::futures::{FutureExt, Stream, StreamExt, stream};
use respan_gateway_core::{Extensions, Fault};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::call::{invalid, prepare_call, strip_origin_headers};
use respan_gateway_http::http::header::{self, HeaderName};
use respan_gateway_http::http::{self, HeaderMap, StatusCode};
use respan_gateway_http::rules::is_connection_header;
use respan_gateway_http::{Body, Completion, Frames, Sent, raw::Relayed};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::Limits;
use crate::session::{Edge, inbound, pump};
use respan_gateway_http::rules::header_tokens;
use respan_gateway_http_transport::{Network, reqwest};

/// How long a provider has to answer the upgrade: as long as reaching it
/// may take.
const HANDSHAKE_TIMEOUT: Duration = respan_gateway_http::call::CONNECT_TIMEOUT;

/// Headers of the handshake the WebSocket library writes itself, and the
/// extension and key headers of the client's own connection: none of them
/// is passed on.
fn is_handshake(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-extensions"
            | "sec-websocket-accept"
    ) || is_connection_header(name)
}

/// Reaches one provider over WebSocket at a base URL (`ws` or `wss`), which
/// each call's path and query extend, as `HttpTransport` does for HTTP.
///
/// A call is prepared as `HttpTransport` prepares one ([`prepare_call`]):
/// stripped, and given keys, so a client's key passes only where a plugin
/// put it back; the handshake's own headers are the WebSocket library's.
/// The answer is the provider's `101` (its origin headers,
/// `Sec-WebSocket-Accept` and extensions removed), or the provider's
/// refusal of the upgrade, a non-`101` answer with its body, which the
/// format reads as any error answer. A provider that cannot be reached, or
/// does not answer the upgrade, its refusal's body included, within the
/// handshake timeout (10 s),
/// fails the call `upstream`.
///
/// The body of the call is the session's frames to send, each one text
/// message; the answer's body is the provider's, each one text message.
///
/// It pumps the session as the client edge does ([`serve`](crate::serve)
/// says how each end goes):
///
/// - The call's body ends clean: Close 1000 is sent, and the provider's
///   end, however it comes within [`Limits::close_wait`], ends the answer
///   clean.
/// - It ends `Err` (the client left, or a filter or plugin refused an
///   input): the provider is sent Close (1001 for a client that left, else
///   the fault's [`CloseCode`](crate::CloseCode) or 1011), and the answer
///   ends with that fault. A frame of it that is no text is a `config`
///   fault. The answer's body dropped: Close 1001.
/// - The provider sends Close with no code or 1000: the answer ends clean.
///   With another code: an `Upstream` fault with that
///   [`ProviderClose`](crate::ProviderClose), its code and reason in the
///   message, a 400 for 1003, 1007, 1008 or 1009 (the provider refused what
///   it was sent), and, on a call with
///   [`Relayed`] (as `respan_gateway_plugin_relay::Relay` marks a call to a
///   provider of its client's own format), that `CloseCode` too.
/// - The provider's connection breaks with no Close, or it is silent
///   through a ping: `Err(truncated)`.
/// - A provider's binary frame, unless [`Self::with_binary_text`] enables
///   UTF-8 normalization (invalid UTF-8 remains a fault), or a message over
///   `max_message`: `upstream`, and the provider is sent Close 1003 or 1009.
#[derive(Clone)]
pub struct WsTransport {
    client: reqwest::Client,
    /// The base URL, without a trailing `/`.
    base: String,
    headers: HeaderMap,
    limits: Limits,
    binary_text: bool,
}

impl std::fmt::Debug for WsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Header values may be keys.
        f.debug_struct("WsTransport")
            .field("base", &self.base)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("limits", &self.limits)
            .field("binary_text", &self.binary_text)
            .finish_non_exhaustive()
    }
}

impl WsTransport {
    /// A transport to `base_url`: `ws` or `wss`
    /// ([`base_url`](respan_gateway_http::call::base_url)), else `Err` with
    /// `config`. It takes [`Limits::default`], gives the provider 10 s to
    /// answer the upgrade, and connects through [`Network::shared`], until
    /// told otherwise.
    pub fn new(base_url: impl Into<String>) -> Result<Self, Fault> {
        Ok(Self {
            client: client_of(Network::shared())?,
            base: respan_gateway_http::call::base_url(&base_url.into(), &["ws", "wss"])?,
            headers: HeaderMap::new(),
            limits: Limits::default(),
            binary_text: false,
        })
    }

    /// Send `headers`, such as the API key, with every call, replacing any
    /// set before, in place of the client's
    /// ([`credentials`](respan_gateway_http::call::credentials)). `Err` with
    /// `config` for a header of the connection or the handshake.
    pub fn with_headers(mut self, headers: HeaderMap) -> Result<Self, Fault> {
        self.headers = respan_gateway_http::call::own_headers(headers, is_handshake)?;
        Ok(self)
    }

    /// Connect through `network`, sharing its sessions' client.
    pub fn with_network(mut self, network: &Network) -> Result<Self, Fault> {
        self.client = client_of(network)?;
        Ok(self)
    }

    /// Open its sessions with `client`, in place of a network's: one of your
    /// own. Have it speak HTTP/1.1 alone, which upgrades, and follow no
    /// redirects.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Take `limits` ([`Limits`]): a message, or a refusal's body, over
    /// `max_message` breaks the answer `Err(upstream)`.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Accept the provider's binary messages as UTF-8 text. Some JSON
    /// protocols, including Gemini Live, use binary frames for their JSON.
    /// Disabled by default; invalid UTF-8 still fails `upstream`. Outbound
    /// messages and the normalized response body remain text.
    pub fn with_binary_text(mut self, enabled: bool) -> Self {
        self.binary_text = enabled;
        self
    }
}

/// A network's client for sessions: HTTP/1.1, as only HTTP/1.1 upgrades;
/// no redirects, as they are the client's to follow; and the system's root
/// certificates and the network's with an explicit TLS provider,
/// `aws-lc-rs`, because the workspace's dependencies enable both `ring` and
/// `aws-lc-rs`, which makes rustls's own choice panic.
fn client_of(network: &Network) -> Result<reqwest::Client, Fault> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    network.client("WebSocket transport", |builder| {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let trusted = CertificateDer::pem_slice_iter(network.trusted_pem());
        roots.add_parsable_certificates(trusted.filter_map(Result::ok));
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| Fault::internal("the TLS settings could not be built").with_source(e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(builder
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .tls_backend_preconfigured(tls))
    })
}

/// Verify the negotiated protocol before exposing the upgraded stream.
fn validate_upgrade(sent: &HeaderMap, received: &HeaderMap) -> Result<(), Fault> {
    let upgrade = received.get(header::UPGRADE);
    let upgrade = upgrade.is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let connection =
        header_tokens(received, header::CONNECTION).any(|v| v.eq_ignore_ascii_case("upgrade"));
    let accept = sent
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|key| derive_accept_key(key.as_bytes()));
    let mut accepted = received.get_all(header::SEC_WEBSOCKET_ACCEPT).iter();
    let key_matches = match (accepted.next(), accepted.next(), accept) {
        (Some(got), None, Some(accept)) => got == &accept,
        _ => false,
    };
    let mut chosen = received.get_all(header::SEC_WEBSOCKET_PROTOCOL).iter();
    let protocol = match (chosen.next(), chosen.next()) {
        // RFC 6455 4.2.2: a server may decline every offered subprotocol.
        (None, None) => true,
        (Some(chosen), None) if is_token(chosen.as_bytes()) => {
            header_tokens(sent, header::SEC_WEBSOCKET_PROTOCOL)
                .any(|p| p.as_bytes() == chosen.as_bytes())
        }
        _ => false,
    };
    let extended = received.contains_key(header::SEC_WEBSOCKET_EXTENSIONS);
    match upgrade && connection && key_matches && protocol && !extended {
        true => Ok(()),
        false => Err(Fault::upstream(
            "the provider returned an invalid WebSocket handshake",
        )),
    }
}

/// The fault of a provider that did not answer the upgrade in time.
fn too_slow() -> Fault {
    Fault::upstream("the provider took too long to accept")
}

/// `body`, failing [`too_slow`] once `deadline` passes.
fn until(deadline: Instant, body: Frames) -> impl Stream<Item = Result<Bytes, Fault>> + Send {
    stream::unfold(Some(body), move |body| async move {
        let mut body = body?;
        match tokio::time::timeout_at(deadline, body.next()).await {
            Ok(item) => Some((item?, Some(body))),
            Err(_) => Some((Err(too_slow()), None)),
        }
    })
}

impl WsTransport {
    /// Create fresh outgoing I/O for each call using this backend.
    pub fn connect(
        self,
    ) -> impl Fn(respan_gateway_core::Extensions) -> respan_gateway_http::ProviderIo + Clone + Send + Sync
    {
        // One message, or a refusal's body, is what a reader holds of the
        // provider's answer.
        let limit = self.limits.max_message;
        let backend = Arc::new(self);
        respan_gateway_http::connect(
            move |request| {
                let backend = backend.clone();
                async move { backend.send(request).await }
            },
            limit,
        )
    }

    /// Send an encoded call and return its response.
    pub async fn send(&self, mut call: http::Request<Body>) -> Sent {
        let relayed = call
            .extensions()
            .get::<Extensions>()
            .is_some_and(|e| e.get::<Relayed>().is_some());
        prepare_call(&mut call, &self.base, &self.headers)?;
        let (head, body) = call.into_parts();
        let body = body.boxed();
        let mut req = head
            .uri
            .to_string()
            .into_client_request()
            .map_err(invalid)?;
        for (name, value) in head.headers.iter().filter(|(n, _)| !is_handshake(n)) {
            req.headers_mut().append(name, value.clone());
        }
        let limits = self.limits;
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let connect = async {
            let expected = req.headers().clone();
            let url = req.uri().to_string().replacen("ws", "http", 1);
            let resp = self
                .client
                .get(url)
                .headers(expected.clone())
                .send()
                .await
                .map_err(|e| Fault::upstream("the provider could not be reached").with_source(e))?;
            let mut head = http::Response::new(()).into_parts().0;
            head.status = resp.status();
            head.version = resp.version();
            head.headers = resp.headers().clone();
            strip_origin_headers(&mut head.headers);
            if head.status != StatusCode::SWITCHING_PROTOCOLS {
                let broke = |e| Fault::truncated().with_source(e);
                let body = resp.bytes_stream().map(move |chunk| chunk.map_err(broke));
                let body = body.boxed();
                let body = Body::stream(until(deadline, body));
                return Ok((http::Response::from_parts(head, body), None));
            }
            validate_upgrade(&expected, &head.headers)?;
            let upgraded = resp.upgrade().await.map_err(|e| {
                Fault::upstream("the provider did not upgrade the connection").with_source(e)
            })?;
            head.headers.remove(header::SEC_WEBSOCKET_ACCEPT);
            head.headers.remove(header::SEC_WEBSOCKET_EXTENSIONS);
            let (frames, end, answer) = inbound(Fault::truncated);
            let edge = Edge::Provider {
                relayed,
                binary_text: self.binary_text,
            };
            let stop = std::future::pending();
            let session = pump(
                upgraded,
                Role::Client,
                body,
                (frames, end),
                edge,
                limits,
                stop,
            );
            let writing = tokio::spawn(session);
            let answer = http::Response::from_parts(head, Body::stream(answer));
            let completion: Completion = async move {
                writing.await.map_err(|error| {
                    if error.is_cancelled() {
                        Fault::abandoned()
                    } else {
                        Fault::internal("the WebSocket session panicked")
                    }
                })?
            }
            .boxed();
            Ok((answer, Some(completion)))
        };
        tokio::time::timeout_at(deadline, connect)
            .await
            .map_err(|_| too_slow())?
    }
}

/// Whether `bytes` are a token (RFC 9110 5.6.2), as a subprotocol is.
fn is_token(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && (bytes.iter()).all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    // An incomplete refusal body stops at the handshake's deadline.
    #[tokio::test(start_paused = true)]
    async fn a_body_fails_upstream_once_its_deadline_passes() {
        let wait = Duration::from_millis(300);
        let body = stream::iter([Ok(Bytes::from_static(b"partial"))])
            .chain(stream::pending())
            .boxed();
        let start = Instant::now();
        let read: Vec<_> = until(start + wait, body).collect().await;
        assert_eq!(read.len(), 2);
        assert!(read[0].is_ok());
        assert!(
            read[1]
                .as_ref()
                .is_err_and(|f| f.is(respan_gateway_core::FaultKind::Provider))
        );
        assert_eq!(start.elapsed(), wait);
    }

    #[test]
    fn upgrades_require_the_key_and_only_negotiated_protocols() {
        let mut sent = HeaderMap::new();
        sent.insert(
            header::SEC_WEBSOCKET_KEY,
            "dGhlIHNhbXBsZSBub25jZQ==".parse().expect("header"),
        );
        let mut received = HeaderMap::new();
        received.insert(header::UPGRADE, "websocket".parse().expect("header"));
        received.insert(
            header::CONNECTION,
            "keep-alive, Upgrade".parse().expect("header"),
        );
        received.insert(
            header::SEC_WEBSOCKET_ACCEPT,
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &received).is_ok());
        let mut duplicate = received.clone();
        duplicate.append(
            header::SEC_WEBSOCKET_ACCEPT,
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &duplicate).is_err());
        for name in [
            header::UPGRADE,
            header::CONNECTION,
            header::SEC_WEBSOCKET_ACCEPT,
        ] {
            let mut invalid = received.clone();
            invalid.remove(&name);
            assert!(validate_upgrade(&sent, &invalid).is_err());
            invalid.insert(name, "wrong".parse().expect("header"));
            assert!(validate_upgrade(&sent, &invalid).is_err());
        }
        for name in [
            header::SEC_WEBSOCKET_PROTOCOL,
            header::SEC_WEBSOCKET_EXTENSIONS,
        ] {
            let mut unsolicited = received.clone();
            unsolicited.insert(name, "unsolicited".parse().expect("header"));
            assert!(validate_upgrade(&sent, &unsolicited).is_err());
        }
        sent.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "chat, realtime".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &received).is_ok());
        received.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "realtime".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &received).is_ok());
        received.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "other".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &received).is_err());
        sent.append(
            header::SEC_WEBSOCKET_PROTOCOL,
            "other".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &received).is_ok());
        received.append(
            header::SEC_WEBSOCKET_PROTOCOL,
            "realtime".parse().expect("header"),
        );
        assert!(validate_upgrade(&sent, &received).is_err());
        for invalid in ["bad protocol", "bad/protocol", "", " "] {
            sent.insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                invalid.parse().expect("header"),
            );
            received.insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                invalid.parse().expect("header"),
            );
            assert!(validate_upgrade(&sent, &received).is_err(), "{invalid:?}");
        }
    }
}
