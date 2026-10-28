//! What a call to a provider is made of, and what a transport does to it and
//! to its response.

use futures::future::BoxFuture;
use http::header::{self, CONNECTION, HeaderName, UPGRADE};
use http::{HeaderMap, HeaderValue};
use respan_gateway_core::{Extensions, Fault};

use crate::credential::{take_protocol_keys, take_query};
use crate::rules::{remove_if, strip_hops};
use crate::{Body, Sent};
use futures::{FutureExt, StreamExt, stream};
use std::sync::Arc;
use std::time::Duration;

/// The most bytes of one message by default, a client's request or a
/// provider's answer held at once: 32 MiB, the largest request the vendors'
/// JSON APIs take (Anthropic's Messages API), so nothing a vendor would take
/// is refused, and an answer may be as large as a request.
pub const MAX_BODY: usize = 32 << 20;

/// How long reaching a provider may take by default: TCP and TLS to a
/// reachable host take well under a second, and 10 s spans TCP's first
/// retransmits of a lost handshake (RFC 6298: after 1 s, then 2 s more, then
/// 4 s more), so a fallback still tries its next provider while the client
/// waits.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A call its provider cannot be sent, `config`; the error is for logs.
pub fn invalid(e: impl std::error::Error + Send + Sync + 'static) -> Fault {
    Fault::config("the provider request is not valid").with_source(e)
}

/// `headers` for a transport to send with every call, `Err` with `config`
/// for one `reserved` says is the transport's own.
pub fn own_headers(
    headers: HeaderMap,
    reserved: impl Fn(&HeaderName) -> bool,
) -> Result<HeaderMap, Fault> {
    match headers.keys().find(|name| reserved(name)) {
        Some(name) => Err(Fault::config(format!(
            "header '{}' is the connection's",
            name.as_str()
        ))),
        None => Ok(headers),
    }
}

/// `url` as a transport's base URL, without a trailing `/`: one of
/// `schemes`, a host with an optional port other than 0, an optional path
/// prefix such as `/v1`, and no query, fragment or credentials, else `Err`
/// with `config`. It is read and normalized as the URL standard says
/// ([`url::Url`]). The error does not echo the URL, which may hold a key.
pub fn base_url(url: &str, schemes: &[&str]) -> Result<String, Fault> {
    let parsed = url::Url::parse(url).ok().filter(|u| {
        schemes.contains(&u.scheme())
            && u.host_str().is_some_and(|h| !h.is_empty())
            && u.username().is_empty()
            && u.password().is_none()
            && u.port() != Some(0)
            && u.query().is_none()
            && u.fragment().is_none()
    });
    match parsed {
        Some(url) => Ok(url.as_str().trim_end_matches('/').to_owned()),
        None => Err(Fault::config(format!(
            "bad base URL: {} with a host, and no query, fragment or credentials \
             (those go in its headers)",
            schemes.join(" or "),
        ))),
    }
}

/// Make `call` the request a transport sends to `base` ([`base_url`]): its
/// keys decided ([`credentials`]) over the transport's `headers`, and its
/// path and query joined to `base`. `Err` with `config` if they make no URL,
/// and with `bad_request` if the path has a `.` or `..` segment, even
/// percent-encoded, which would leave `base`'s prefix.
/// What a proxy does not pass on is already gone
/// ([`ProviderIo::bind`](crate::ProviderIo::bind)).
pub fn prepare_call(
    call: &mut http::Request<crate::Body>,
    base: &str,
    headers: &HeaderMap,
) -> Result<(), Fault> {
    credentials(call, headers);
    let path = call.uri().path_and_query().map_or("/", |p| p.as_str());
    if has_dot_segment(call.uri().path()) {
        return Err(Fault::bad_request("the request path has a dot segment"));
    }
    let uri = format!("{base}{path}").parse::<http::Uri>();
    *call.uri_mut() = uri.map_err(invalid)?;
    Ok(())
}

/// Whether `path` has a `.` or `..` segment, after one percent-decoding.
fn has_dot_segment(path: &str) -> bool {
    crate::path::decoded(path)
        .split(['/', '\\'])
        .any(|segment| matches!(segment, "." | ".."))
}

/// Strip from a call, whose head may be the client's, what a proxy does not
/// pass on: a hop's headers (RFC 9110 7.6.1) but `Upgrade`, which gets its
/// `Connection: upgrade` (7.8); `Host`; `Expect`; conditions and ranges
/// (13, 14); method overrides; `Accept-Encoding`, as the gateway decodes
/// no content coding, so asks for none; and the gateway origin's `Cookie`, `Forwarded`, `Via`,
/// `X-Forwarded-*` and `X-Real-IP`, which plugins still see
/// ([`Original`](crate::raw::Original)) and a call's [`CallHeaders`] may set.
pub(crate) fn proxy_call(head: &mut http::request::Parts) {
    strip_hops(&mut head.headers, true);
    remove_if(&mut head.headers, denied);
    if head.headers.contains_key(UPGRADE) {
        let upgrade = HeaderValue::from_static("upgrade");
        head.headers.insert(CONNECTION, upgrade);
    }
}

/// Whether a proxy keeps header `name` from a call, beyond a hop's own
/// ([`proxy_call`]).
fn denied(name: &HeaderName) -> bool {
    let name = name.as_str();
    let denied = matches!(
        name,
        "host"
            | "expect"
            | "range"
            | "x-http-method-override"
            | "x-method-override"
            | "x-http-method"
            | "x-original-url"
            | "x-rewrite-url"
            | "accept-encoding"
            | "cookie"
            | "forwarded"
            | "via"
            | "x-real-ip"
    );
    denied || name.starts_with("if-") || name.starts_with("x-forwarded-")
}

pub use crate::credential::is_credential;
use crate::rules::is_connection_header;

/// Remove from a response the headers bound to the provider's origin:
/// cookies, alternative services and HSTS.
pub fn strip_origin_headers(headers: &mut HeaderMap) {
    for name in [
        header::SET_COOKIE,
        header::ALT_SVC,
        header::STRICT_TRANSPORT_SECURITY,
    ] {
        headers.remove(name);
    }
}

/// Headers for one call, replacing the transport's own of their names, such
/// as a tenant's key set in `on_provider`.
#[derive(Clone, Default)]
pub struct CallHeaders(pub HeaderMap);

impl std::fmt::Debug for CallHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Header values may be keys.
        let names: Vec<_> = self.0.keys().collect();
        f.debug_tuple("CallHeaders").field(&names).finish()
    }
}

/// Decide whose keys `call` carries: its [`CallHeaders`] over the
/// transport's own `headers`, replacing credentials already in the head.
/// With neither, the client's `Credential` if the call is
/// [`Relayed`](crate::raw::Relayed), else only credentials explicitly
/// supplied in the head: the exchange's `Credential` is never copied unasked.
pub fn credentials(call: &mut http::Request<crate::Body>, headers: &HeaderMap) {
    let (mut head, body) = std::mem::take(call).into_parts();
    let extensions = head.extensions.remove::<Extensions>().unwrap_or_default();
    let mut own = headers.clone();
    if let Some(CallHeaders(call)) = extensions.get::<CallHeaders>() {
        let sent = |n: &&HeaderName| !is_connection_header(n);
        for name in call.keys().filter(sent) {
            own.remove(name);
        }
        for (name, value) in call.iter().filter(|(n, _)| sent(n)) {
            own.append(name, value.clone());
        }
    }
    if !own.is_empty() {
        remove_if(&mut head.headers, |n| is_credential(n.as_str()));
        take_protocol_keys(&mut head.headers, true);
        take_query(&mut head.uri, is_credential);
        for name in own.keys() {
            head.headers.remove(name);
        }
        head.headers.extend(own);
    } else if extensions.get::<crate::raw::Relayed>().is_some()
        && let Some(credential) = extensions.get::<crate::Credential>()
    {
        // With no keys of its own, only a relayed call carries the client's.
        credential.restore(&mut head);
    }
    head.extensions.insert(extensions);
    *call = http::Request::from_parts(head, body);
}

/// Decorate an async sending function with head and per-polled-frame timeouts.
/// Native response headers/extensions and independent write completion survive.
/// Idle time is measured only while the response body is being polled.
// Keep the native callback signature rather than introducing a sender facade.
#[allow(clippy::type_complexity)]
pub fn watchdog<F, Fut>(
    inner: F,
    head: Duration,
    idle: Duration,
) -> impl Fn(http::Request<Body>) -> BoxFuture<'static, Sent> + Clone + Send + Sync + 'static
where
    F: Fn(http::Request<Body>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Sent> + Send + 'static,
{
    let inner = Arc::new(inner);
    move |call| {
        let inner = inner.clone();
        async move {
            let stalled =
                |d: Duration| Fault::upstream(format!("the provider sent nothing for {d:?}"));
            let (response, completion) = tokio::time::timeout(head, inner(call))
                .await
                .map_err(|_| stalled(head))??;
            let response = response.map(|body| {
                Body::stream(stream::unfold(Some(body), move |body| async move {
                    let mut body = body?;
                    match tokio::time::timeout(idle, body.next()).await {
                        Ok(item) => Some((item?, Some(body))),
                        Err(_) => Some((Err(stalled(idle)), None)),
                    }
                }))
            });
            Ok((response, completion))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Credential;
    use bytes::Bytes;
    use http::request::Parts as RequestHead;
    use respan_gateway_core::FaultKind;

    fn head(headers: &[(&'static str, &'static str)]) -> http::request::Parts {
        let mut req = http::Request::new(());
        for (name, value) in headers {
            let value = HeaderValue::from_static(value);
            req.headers_mut().append(*name, value);
        }
        req.into_parts().0
    }

    #[test]
    fn a_proxy_passes_end_to_end_headers_and_upgrade() {
        let mut parts = head(&[
            ("connection", "x-hop"),
            ("x-hop", "1"),
            ("host", "gw"),
            ("expect", "100-continue"),
            ("if-none-match", "\"a\""),
            ("range", "bytes=0-1"),
            ("accept-encoding", "gzip"),
            ("x-http-method-override", "GET"),
            ("x-http-method", "GET"),
            ("x-original-url", "/v1/chat/completions"),
            ("x-rewrite-url", "/v1/chat/completions"),
            ("te", "trailers"),
            ("upgrade", "websocket"),
            ("openai-beta", "v9"),
        ]);
        proxy_call(&mut parts);
        let names: Vec<&str> = parts.headers.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["upgrade", "openai-beta", "connection"]);
        assert_eq!(parts.headers[CONNECTION], "upgrade");
    }

    /// A call with `head` and the client's `credential`, if any, put back
    /// on its head if `restored`, as a plugin that relays it does.
    fn call(
        head: RequestHead,
        credential: Option<HeaderMap>,
        restored: bool,
    ) -> http::Request<crate::Body> {
        let mut call = http::Request::from_parts(head, crate::Body::default());
        let mut extensions = Extensions::new();
        if let Some(client) = credential {
            extensions.insert(Credential::of(client));
        }
        call.extensions_mut().insert(extensions);
        if restored {
            let (mut head, body) = call.into_parts();
            if let Some(credential) = head
                .extensions
                .get::<Extensions>()
                .and_then(|e| e.get::<Credential>())
                .cloned()
            {
                credential.restore(&mut head);
            }
            call = http::Request::from_parts(head, body);
        }
        call
    }

    #[test]
    fn with_no_keys_of_its_own_the_heads_pass_through() {
        let mut client = HeaderMap::new();
        client.insert("x-api-key", HeaderValue::from_static("k"));
        let mut restored = call(head(&[]), Some(client.clone()), true);
        credentials(&mut restored, &HeaderMap::new());
        assert_eq!(restored.headers()["x-api-key"], "k");
        let mut own = HeaderMap::new();
        own.insert("authorization", HeaderValue::from_static("Bearer own"));
        let mut restored = call(head(&[]), Some(client), true);
        credentials(&mut restored, &own);
        assert!(restored.headers().get("x-api-key").is_none());
        assert_eq!(restored.headers()["authorization"], "Bearer own");
    }

    #[test]
    fn the_clients_credentials_pass_only_where_a_plugin_restored_them() {
        // A call whose head has none of the client's keys carries none,
        // even to a provider with none of its own: the Credential
        // extension is never copied.
        let mut client = HeaderMap::new();
        client.insert("authorization", HeaderValue::from_static("Bearer sk-c"));
        client.insert("x-api-key", HeaderValue::from_static("k"));
        let mut kept = call(head(&[]), Some(client.clone()), false);
        credentials(&mut kept, &HeaderMap::new());
        assert!(kept.headers().is_empty());

        // A Credential a plugin removed is restored by none either.
        let mut removed = call(head(&[]), None, true);
        credentials(&mut removed, &HeaderMap::new());
        assert!(removed.headers().is_empty());

        let mut restored = call(head(&[]), Some(client), true);
        credentials(&mut restored, &HeaderMap::new());
        assert_eq!(restored.headers()["authorization"], "Bearer sk-c");
        assert_eq!(restored.headers()["x-api-key"], "k");
    }

    #[test]
    fn a_subprotocol_marker_goes_where_its_key_does() {
        let marked = || head(&[("sec-websocket-protocol", "realtime, token")]);
        // Own keys replace the client's: no key is left for the marker.
        let mut own = HeaderMap::new();
        own.insert("authorization", HeaderValue::from_static("Bearer own"));
        let mut keyed = call(marked(), None, true);
        credentials(&mut keyed, &own);
        assert_eq!(keyed.headers()["sec-websocket-protocol"], "realtime");
        // No key at all, as on a call whose head has none.
        let mut kept = call(marked(), None, false);
        credentials(&mut kept, &HeaderMap::new());
        assert_eq!(kept.headers()["sec-websocket-protocol"], "realtime, token");
        // The client's own key comes back after its marker.
        let mut client = HeaderMap::new();
        client.insert("sec-websocket-protocol", HeaderValue::from_static("dg"));
        let mut restored = call(marked(), Some(client), true);
        credentials(&mut restored, &HeaderMap::new());
        assert_eq!(
            restored.headers()["sec-websocket-protocol"],
            "realtime, token, dg"
        );
    }

    #[test]
    fn a_dot_segment_cannot_leave_the_base_path() {
        let send = |path: &str| {
            let mut call = http::Request::new(crate::Body::full(Bytes::new()));
            *call.uri_mut() = path.parse().expect("a uri");
            prepare_call(&mut call, "http://h/tenant-a", &HeaderMap::new()).map(|()| call)
        };
        for path in [
            "/../tenant-b/v1/chat",
            "/%2e%2e/tenant-b/v1/chat",
            "/v1/%2E./x",
            "/v1/./x?a=1",
        ] {
            let refused = send(path).err();
            assert!(refused.is_some_and(|f| f.is(FaultKind::Client)), "{path}");
        }
        // Only one decoding counts: a twice-encoded dot is a name.
        assert!(send("/v1/%252e%252e/x").is_ok());
        let call = send("/v1/chat?q=a.b").expect("a clean path");
        assert_eq!(call.uri(), "http://h/tenant-a/v1/chat?q=a.b");
    }

    #[test]
    fn a_base_url_host_may_hold_an_underscore() {
        assert!(base_url("http://vllm_server:8000/v1", &["http"]).is_ok());
    }

    #[test]
    fn a_base_url_is_normalized() {
        let url = base_url("HTTPS://API.Example.test:443/v1/", &["https"]);
        assert_eq!(url.ok().as_deref(), Some("https://api.example.test/v1"));
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;
    use crate::Completion;
    use bytes::Bytes;
    use respan_gateway_core::FaultKind;
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn request() -> http::Request<Body> {
        http::Request::new(Body::default())
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_head_times_out() {
        let send = watchdog(|_| async { std::future::pending().await }, secs(5), secs(1));
        let start = tokio::time::Instant::now();
        let fault = send(request()).await.err().expect("head timeout");
        assert!(fault.is(FaultKind::Provider));
        assert_eq!(fault.message(), "the provider sent nothing for 5s");
        assert_eq!(start.elapsed(), secs(5));
    }
    #[tokio::test(start_paused = true)]
    async fn idle_is_measured_between_polled_frames() {
        let send = watchdog(
            |_| async {
                Ok((
                    http::Response::new(Body::stream(
                        stream::iter([Ok(Bytes::from_static(b"first"))]).chain(stream::pending()),
                    )),
                    None,
                ))
            },
            secs(5),
            secs(1),
        );
        let (mut resp, _) = send(request()).await.expect("head");
        assert_eq!(
            resp.body_mut()
                .next()
                .await
                .expect("expected value")
                .expect("expected value"),
            "first"
        );
        tokio::time::sleep(secs(10)).await;
        let start = tokio::time::Instant::now();
        let fault = resp
            .body_mut()
            .next()
            .await
            .expect("expected value")
            .expect_err("expected failure");
        assert!(fault.is(FaultKind::Provider));
        assert_eq!(fault.message(), "the provider sent nothing for 1s");
        assert_eq!(start.elapsed(), secs(1));
        assert!(resp.body_mut().next().await.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn a_whole_response_can_take_longer_than_idle_to_open() {
        let send = watchdog(
            |_| async {
                tokio::time::sleep(secs(3)).await;
                Ok((http::Response::new(Body::full("whole")), None))
            },
            secs(5),
            secs(1),
        );
        let start = tokio::time::Instant::now();
        let (mut resp, _) = send(request()).await.expect("head before deadline");
        assert_eq!(start.elapsed(), secs(3));
        assert_eq!(
            resp.body_mut()
                .next()
                .await
                .expect("expected value")
                .expect("expected value"),
            "whole"
        );
        assert!(resp.body_mut().next().await.is_none());
    }
    #[tokio::test]
    async fn response_exchange_fields_and_write_completion_survive() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Marker(&'static str);
        let send = watchdog(
            |_| async {
                let mut response = http::Response::new(Body::full("data"));
                response
                    .headers_mut()
                    .insert("x-provider", "there".parse().expect("expected value"));
                response.extensions_mut().insert(Marker("preserved"));
                let completion: Completion =
                    async { Err(Fault::upstream("late write failure")) }.boxed();
                Ok((response, Some(completion)))
            },
            secs(5),
            secs(1),
        );
        let (mut response, completion) = send(request()).await.expect("head");
        assert_eq!(response.headers()["x-provider"], "there");
        assert_eq!(
            response.extensions().get::<Marker>(),
            Some(&Marker("preserved"))
        );
        assert_eq!(
            response
                .body_mut()
                .next()
                .await
                .expect("expected value")
                .expect("expected value"),
            "data"
        );
        assert!(response.body_mut().next().await.is_none());
        assert_eq!(
            completion
                .expect("expected value")
                .await
                .expect_err("expected failure")
                .message(),
            "late write failure"
        );
    }
}
