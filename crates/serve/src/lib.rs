#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use respan_gateway_core::Fault;
use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::response::Response;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use respan_gateway_core::Gateway;
use respan_gateway_core::futures::FutureExt;
use respan_gateway_core::futures::future::{BoxFuture, Shared};
use respan_gateway_http::http::header;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::rules::header_tokens;
use respan_gateway_ws::Limits;

mod responses_ws;

pub use responses_ws::{ResponsesWebSocket, ResponsesWebSocketService};

/// The most bytes of a request [`Host`] reads by default.
const MAX_BODY_BYTES: usize = respan_gateway_http::call::MAX_BODY;

type BodyLimit = dyn Fn(&RequestHead) -> usize + Send + Sync;

/// A router serving `gateway` with [`Host`]'s defaults.
pub fn router(gateway: Gateway) -> Router {
    Host::new(gateway).router()
}

/// How a gateway is hosted: each request goes through the HTTP wire's
/// [`handle`](respan_gateway_http::handle), its body read whole, and a
/// WebSocket upgrade is a session ([`respan_gateway_ws::serve`] says how it
/// goes and ends). Formats decide what a request is.
///
/// ```no_run
/// # use respan_gateway_core::futures::FutureExt;
/// # async fn run(gateway: respan_gateway_core::Gateway) -> std::io::Result<()> {
/// let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
/// let stopped = stopped.map(drop).shared();
/// let app = respan_gateway_serve::Host::new(gateway.clone())
///     .stop(stopped.clone())
///     .router();
/// let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
/// axum::serve(listener, app).with_graceful_shutdown(stopped).await?;
/// gateway.idle().await;
/// # Ok(())
/// # }
/// ```
///
/// Only HTTP/1.1 upgrades.
#[allow(missing_debug_implementations)]
pub struct Host {
    gateway: Gateway,
    limit: Arc<BodyLimit>,
    limits: Limits,
    stop: Shared<BoxFuture<'static, ()>>,
}

impl Host {
    /// A host of `gateway`: bodies up to 32 MiB, [`Limits::default`] for
    /// sessions, and no stop.
    pub fn new(gateway: Gateway) -> Self {
        Self {
            gateway,
            limit: Arc::new(|_| MAX_BODY_BYTES),
            limits: Limits::default(),
            stop: std::future::pending().boxed().shared(),
        }
    }

    /// The most bytes of a request's body, chosen by its head before the body
    /// is read, so a path or an authenticated extension can select it. A body
    /// over it is refused 413 `too_large`, one that breaks off 400
    /// `bad_request`.
    pub fn limit(mut self, limit: impl Fn(&RequestHead) -> usize + Send + Sync + 'static) -> Self {
        self.limit = Arc::new(limit);
        self
    }

    /// What either edge of a session takes.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// When `stop` completes, every open session is closed 1001 (going away),
    /// so that a server's graceful shutdown drains them too.
    pub fn stop(mut self, stop: impl Future<Output = ()> + Send + 'static) -> Self {
        self.stop = stop.boxed().shared();
        self
    }

    /// The router: every request is one for the gateway.
    pub fn router(self) -> Router {
        let Self {
            gateway,
            limit,
            limits,
            stop,
        } = self;
        Router::new().fallback(move |mut req: Request| {
            let (gateway, limit, stop) = (gateway.clone(), limit.clone(), stop.clone());
            async move {
                // As the session's handshake reads it: one token of many.
                let websocket = header_tokens(req.headers(), header::UPGRADE)
                    .any(|token| token.eq_ignore_ascii_case("websocket"));
                if !websocket {
                    return whole(gateway, req, &*limit).await;
                }
                let upgraded = hyper::upgrade::on(&mut req).map(|io| io.ok().map(TokioIo::new));
                let head = req.into_parts().0;
                let resp = respan_gateway_ws::serve(&gateway, head, upgraded, limits, stop).await;
                let (head, body) = resp.into_parts();
                Response::from_parts(head, axum::body::Body::new(body))
            }
        })
    }
}

/// One request that switches no protocols, its body read whole up to the
/// limit its head gets.
async fn whole(gateway: Gateway, req: Request, limit: &BodyLimit) -> Response {
    let (head, body) = req.into_parts();
    let max = limit(&head);
    let broke = |_| Fault::bad_request("the request body broke off");
    let req = Request::from_parts(head, body.map_err(broke).boxed_unsync());
    let (resp, _ended) = respan_gateway_http::handle(&gateway, req, max).await;
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}
