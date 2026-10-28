#![warn(missing_docs)]
//! A pooled Reqwest backend for outbound HTTP provider calls: an
//! [`HttpTransport`] reaches one provider at a base URL. What a transport
//! does to a call and its answer is the HTTP wire's
//! (`respan_gateway_http::call`), shared with every transport.

use std::sync::Arc;

use bytes::Bytes;
use http::header::{COOKIE, LOCATION};
use http::{HeaderMap, Method, StatusCode, Uri};
use http_body_util::BodyExt;
use respan_gateway_core::Fault;

use respan_gateway_http::call::{CONNECT_TIMEOUT, MAX_BODY, invalid, own_headers};
use respan_gateway_http::rules::{is_connection_header, remove_if};
use respan_gateway_http::{Body, ProviderIo, Sent};
use respan_gateway_http::{call::is_credential, call::prepare_call, call::strip_origin_headers};

/// The Reqwest this transport is built on, at its version.
pub use reqwest;

mod network;
pub use network::Network;

/// The most redirects a call follows.
const REDIRECTS: usize = 5;

/// Whether a redirect to a URI is followed.
type Allow = dyn Fn(&Uri) -> bool + Send + Sync;

/// Reaches one provider over HTTP(S) at a base URL, which each call's path
/// extends, sharing one connection pool. It prepares
/// a call ([`prepare_call`]) and strips the response
/// ([`strip_origin_headers`]). It sets no timeout on an answer: timing is
/// [`watchdog`](respan_gateway_http::call::watchdog)'s.
#[derive(Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    /// The base URL, without a trailing `/`.
    base: String,
    headers: HeaderMap,
    follow: Option<Arc<Allow>>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Header values may be keys.
        f.debug_struct("HttpTransport")
            .field("base", &self.base)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A network's client for transports: it returns redirects unfollowed, as
/// they are the client's to follow, and gives up connecting after
/// [`CONNECT_TIMEOUT`].
fn client_of(network: &Network) -> Result<reqwest::Client, Fault> {
    network.client("HTTP transport", |builder| {
        Ok(builder
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none()))
    })
}

impl HttpTransport {
    /// A transport to `base_url`: `http` or `https` ([`base_url`](respan_gateway_http::call::base_url)),
    /// else
    /// `Err` with `config`. It connects within 10 s, through
    /// [`Network::shared`], and reads up to 32 MiB of one answer at once,
    /// until told otherwise.
    pub fn new(base_url: impl Into<String>) -> Result<Self, Fault> {
        Ok(Self {
            client: client_of(Network::shared())?,
            base: respan_gateway_http::call::base_url(&base_url.into(), &["http", "https"])?,
            headers: HeaderMap::new(),
            follow: None,
        })
    }

    /// Connect through `network`, sharing its transports' client.
    pub fn with_network(mut self, network: &Network) -> Result<Self, Fault> {
        self.client = client_of(network)?;
        Ok(self)
    }

    /// Send its calls with `client`, in place of a network's: one of your
    /// own, with its proxy, certificates and timeouts. Have it follow no
    /// redirects ([`reqwest::redirect::Policy::none`]); this transport
    /// follows those it is told to ([`follow_redirects`](Self::follow_redirects)).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Send `headers`, such as the API key, with every call, replacing any
    /// set before, in place of the client's ([`credentials`](respan_gateway_http::call::credentials)).
    /// `Err` with `config` for a connection header.
    pub fn with_headers(mut self, headers: HeaderMap) -> Result<Self, Fault> {
        self.headers = own_headers(headers, is_connection_header)?;
        Ok(self)
    }

    /// Follow a redirect of a `GET` or `HEAD` call to a location `allow`
    /// accepts, up to 5 in a row; any other answer is the client's to follow.
    /// A call that leaves its origin leaves its credentials behind: its
    /// `Authorization`, `Cookie` and every header or query parameter that
    /// names a key, token or secret.
    pub fn follow_redirects(
        mut self,
        allow: impl Fn(&Uri) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.follow = Some(Arc::new(allow));
        self
    }

    /// Send an encoded call and return its response.
    pub async fn send(&self, mut call: http::Request<Body>) -> Sent {
        prepare_call(&mut call, &self.base, &self.headers)?;
        let (mut parts, body) = call.into_parts();
        let no_body = !body.is_streaming() && body.full_bytes().is_none_or(|b| b.is_empty());
        let reads = matches!(parts.method, Method::GET | Method::HEAD);
        let follow = self.follow.as_ref().filter(|_| no_body && reads);
        // A whole call is one frame, or none if empty, and goes with its length.
        let mut body = Some(body);
        let mut hops = 0;
        let resp = loop {
            let sent = match body.take() {
                Some(body) if body.is_streaming() => reqwest::Body::wrap_stream(body),
                Some(body) => body.full_bytes().cloned().unwrap_or_default().into(),
                None => reqwest::Body::from(Bytes::new()),
            };
            let mut head = http::Request::new(sent);
            *head.method_mut() = parts.method.clone();
            *head.uri_mut() = parts.uri.clone();
            *head.headers_mut() = parts.headers.clone();
            let req = reqwest::Request::try_from(head).map_err(invalid)?;
            let resp = self
                .client
                .execute(req)
                .await
                // The error names the provider's URL, which is for logs only.
                .map_err(|e| match e.is_builder() {
                    true => invalid(e),
                    false => Fault::upstream("the provider could not be reached").with_source(e),
                })?;
            let next = follow.and_then(|allow| redirect(&resp, &parts.uri, allow.as_ref()));
            match next {
                Some(next) if hops < REDIRECTS => {
                    if origin(&next) != origin(&parts.uri) {
                        leave_origin(&mut parts.headers);
                    }
                    parts.uri = next;
                    hops += 1;
                }
                _ => break resp,
            }
        };
        // A plain-HTTP proxy's refusal comes back as an answer.
        if resp.status() == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
            return Err(Fault::upstream(
                "the proxy refused the gateway (407): check the user and password in its URL",
            ));
        }
        if resp.status() == http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(Fault::upstream(
                "the provider switched protocols, which this transport does not",
            ));
        }
        let broke = |e| Fault::upstream("the provider's body broke off").with_source(e);
        let mut resp = http::Response::from(resp)
            .map(|body| Body::stream(body.map_err(broke).into_data_stream()));
        strip_origin_headers(resp.headers_mut());
        Ok((resp, None))
    }

    /// A provider factory sharing this client's pool and configuration.
    pub fn connect(
        self,
    ) -> impl Fn(respan_gateway_core::Extensions) -> ProviderIo + Clone + Send + Sync + 'static
    {
        let client = Arc::new(self);
        respan_gateway_http::connect(
            move |request| {
                let client = client.clone();
                async move { client.send(request).await }
            },
            MAX_BODY,
        )
    }
}

/// `answer`'s body as JSON, read up to the default limit. An answer other
/// than a success is the provider's refusal
/// ([`provider_fault`](respan_gateway_http_faults::provider_fault)), and a
/// body that is no JSON, or larger, an `upstream` fault.
pub async fn json(answer: http::Response<Body>) -> Result<serde_json::Value, Fault> {
    let (head, body) = answer.into_parts();
    let body = body.read_answer(MAX_BODY).await?;
    if !head.status.is_success() {
        let answer = http::Response::from_parts(head, body);
        return Err(respan_gateway_http_faults::provider_fault(answer));
    }
    serde_json::from_slice(&body).map_err(|_| Fault::upstream("its answer is not JSON"))
}

/// Why a call that failed with `e` could not reach its provider, naming no
/// URL, as the error itself does: `could not be reached: <cause>`.
pub fn unreachable(e: &reqwest::Error) -> String {
    let mut cause = None;
    let mut next = std::error::Error::source(e);
    while let Some(e) = next {
        cause = Some(e.to_string());
        next = e.source();
    }
    let cause = match (cause, e.is_timeout()) {
        (Some(cause), _) => cause,
        (None, true) => "timed out".to_owned(),
        (None, false) => "the connection failed".to_owned(),
    };
    format!("could not be reached: {cause}")
}

/// Where `resp`, an answer to a call to `from`, redirects it, if `allow`
/// accepts that.
fn redirect(resp: &reqwest::Response, from: &Uri, allow: &Allow) -> Option<Uri> {
    let redirects = matches!(
        resp.status(),
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    );
    let location = resp.headers().get(LOCATION)?.to_str().ok()?;
    let to = reqwest::Url::parse(&from.to_string())
        .ok()?
        .join(location)
        .ok()?;
    let to: Uri = to.as_str().parse().ok()?;
    (redirects && allow(&to)).then_some(to)
}

/// A URI's scheme, host and port.
fn origin(uri: &Uri) -> (Option<&str>, Option<&str>, Option<u16>) {
    (uri.scheme_str(), uri.host(), uri.port_u16())
}

/// Remove from `headers` what only the origin they were meant for may see.
/// `Authorization` and `Proxy-Authorization` name a credential too.
fn leave_origin(headers: &mut HeaderMap) {
    headers.remove(COOKIE);
    remove_if(headers, |n| is_credential(n.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(body: impl Into<Bytes>) -> http::Response<Body> {
        http::Response::new(Body::full(body))
    }

    #[tokio::test]
    async fn an_answer_is_read_as_json_up_to_the_limit() {
        let read = json(answer(r#"{"data":[]}"#)).await;
        assert_eq!(read.ok(), Some(serde_json::json!({"data": []})));
        // Regression: a listing was read whole, however large.
        let over = json(answer(vec![b' '; MAX_BODY + 1])).await;
        let fault = over.err().map(|f| f.message().to_owned());
        let said = format!("the provider's answer is over the limit of {MAX_BODY} bytes");
        assert_eq!(fault.as_deref(), Some(said.as_str()));
    }
}
