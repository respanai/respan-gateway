//! How HTTP meets the kernel: the client's edge ([`ClientIo`]), the provider's
//! ([`ProviderIo`]), and the bodies and encoders between them.

mod body;
mod client;
mod provider;
mod reader;
mod upload;
mod writer;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use respan_gateway_core::{Fault, guarded};

pub use body::Body;
pub use client::{ClientIo, Reply, handle, handle_live};
pub use provider::{ProviderIo, Rewrite, connect};

/// A body, its bytes one item per data frame, ending with an `Err` if it
/// broke off.
pub type Frames = futures::stream::BoxStream<'static, Result<Bytes, Fault>>;

/// Actual completion of an outgoing transfer, independent of response headers.
pub type Completion = BoxFuture<'static, Result<(), Fault>>;

/// What a transport gives for a call: its answer, and, if it knows it, its
/// upload's actual completion (`None`: only the handoff is complete).
pub type Sent = Result<(http::Response<Body>, Option<Completion>), Fault>;

/// A response, and its delivery's end.
pub(crate) type Written = (http::Response<Body>, Completion);

/// End `encoder` as `end` says, its closing frames in `out`, as
/// [`Encoder::end`](crate::Encoder::end) promises: if ending well fails, it
/// ends once more with that failure, to write its error framing. How it
/// ended, and the fault to break the body off with if even that failed or
/// it panicked, its frames dropped.
pub(crate) fn end_encoder<T: 'static>(
    encoder: &mut dyn crate::Encoder<T>,
    end: Result<(), Fault>,
    out: &mut Vec<Bytes>,
) -> (Result<(), Fault>, Option<Fault>) {
    let well = end.is_ok();
    let first = guarded(|| encoder.end(end.clone(), out));
    let Err(fault) = first else {
        return (end, None);
    };
    out.clear();
    if !well {
        return (Err(fault.clone()), Some(fault));
    }
    match guarded(|| encoder.end(Err(fault.clone()), out)) {
        Ok(()) => (Err(fault), None),
        Err(_) => {
            out.clear();
            (Err(fault.clone()), Some(fault))
        }
    }
}

/// Drive the gateway alongside an HTTP reply, then let its body own the
/// remaining work. Cancelling either stage cancels the gateway exchange.
pub(crate) async fn drive(
    handled: Completion,
    answer: BoxFuture<'static, Option<Written>>,
) -> Result<Written, Fault> {
    use futures::future::{Either, select};
    let (answer, pending) = match select(handled, answer).await {
        Either::Left((end, answer)) => {
            // Preserve an already prepared reply, but do not wait for a
            // sender retained by an unused clone after handling has ended.
            (answer.now_or_never().flatten(), Err(end))
        }
        Either::Right((answer, handled)) => (answer, Ok(handled)),
    };
    match answer {
        Some((response, ended)) => {
            let mut handled = pending.ok();
            // An empty response may already have completed while its head
            // was encoded. Settle that work even if no body is polled.
            futures::future::poll_fn(|cx| {
                poll_handle(&mut handled, cx);
                std::task::Poll::Ready(())
            })
            .await;
            let response =
                response.map(|body| Body::stream(Pump { handled, body }.into_data_stream()));
            Ok((response, ended))
        }
        None => match pending {
            Err(Err(fault)) => Err(fault),
            Ok(handled) => {
                handled.await?;
                Err(Fault::internal("no format answered"))
            }
            Err(Ok(())) => Err(Fault::internal("no format answered")),
        },
    }
}

/// The gateway future drops first, ending its calls before their output
/// streams are dropped by the protocol body.
struct Pump {
    handled: Option<Completion>,
    body: Body,
}

fn poll_handle(handled: &mut Option<Completion>, cx: &mut std::task::Context<'_>) {
    if let Some(work) = handled
        && work.as_mut().poll(cx).is_ready()
    {
        *handled = None;
    }
}

impl http_body::Body for Pump {
    type Data = Bytes;
    type Error = Fault;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Fault>>> {
        let this = self.get_mut();
        poll_handle(&mut this.handled, cx);
        let frame = std::pin::Pin::new(&mut this.body).poll_frame(cx);
        // Reading the final frame can complete the writer. Observe it now,
        // before a consumer drops the body immediately after that frame.
        poll_handle(&mut this.handled, cx);
        if matches!(frame, std::task::Poll::Ready(None)) && this.handled.is_some() {
            return std::task::Poll::Pending;
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.handled.is_none() && self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::{EventSink, EventStream, Format};

    /// A format that reads one message and writes nothing: only the reply's
    /// refusal matters here.
    struct Silent;

    impl Format<ClientIo, Bytes, Bytes> for Silent {
        fn name(&self) -> &str {
            "silent"
        }

        fn bind(
            &self,
            io: ClientIo,
        ) -> Result<respan_gateway_core::Connection<Bytes, Bytes>, Fault> {
            Ok(io.bind(
                |_| {
                    Box::new(|raw: Bytes, out: &mut Vec<Bytes>| {
                        out.push(raw);
                        Ok(())
                    })
                },
                |_| {
                    let sink = futures::sink::unfold((), |(), _: Result<Bytes, Fault>| async {
                        Ok::<(), Fault>(())
                    });
                    Ok(EventSink::new(sink))
                },
            ))
        }
    }

    /// Ends well unless told to fail, then writes `error` as its framing.
    struct Framed {
        fail: bool,
        ends: usize,
    }

    impl crate::Encoder<()> for Framed {
        fn encode(&mut self, _: &(), _: &mut Vec<Bytes>) -> Result<(), Fault> {
            Ok(())
        }
        fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
            self.ends += 1;
            out.push(Bytes::from_static(b"partial"));
            match end {
                Ok(()) if self.fail => Err(Fault::upstream("no usage")),
                Ok(()) => Ok(()),
                Err(_) => {
                    out.push(Bytes::from_static(b"error"));
                    Ok(())
                }
            }
        }
    }

    #[test]
    fn an_encoder_that_fails_to_end_well_writes_its_error_framing() {
        let mut encoder = Framed {
            fail: true,
            ends: 0,
        };
        let mut out = Vec::new();
        let (end, abort) = end_encoder(&mut encoder, Ok(()), &mut out);
        assert!(end.is_err() && abort.is_none());
        assert_eq!(
            out,
            [Bytes::from_static(b"partial"), Bytes::from_static(b"error")]
        );
        assert_eq!(encoder.ends, 2);
    }

    #[test]
    fn an_encoder_that_cannot_frame_a_failure_breaks_the_body_off() {
        let mut out = Vec::new();
        let encoder = &mut |_: &(), _: &mut Vec<Bytes>| Ok(());
        let (end, abort) = end_encoder(encoder, Err(Fault::upstream("x")), &mut out);
        assert!(end.is_err() && abort.is_some() && out.is_empty());
    }

    /// A reply prepared before handling ends is kept, however it ended.
    #[tokio::test]
    async fn a_completed_handler_keeps_an_already_ready_reply() {
        let request = http::Request::post("/x").body(Body::full("q"));
        let request = request.unwrap_or_default();
        let (incoming, answer) = ClientIo::accept(request, 1024);
        let Ok((opening, mut outgoing)) = Silent.bind(incoming) else {
            panic!("bound")
        };
        assert!(opening.await.is_ok());
        let mut refused = EventStream::refused(Fault::upstream("refused"));
        assert!(outgoing.prepare(&mut refused).is_ok());
        let handled = futures::future::ready(Err(Fault::upstream("handling failed"))).boxed();
        let response = drive(handled, answer)
            .await
            .map(|(response, _)| response.status());
        assert_eq!(response.ok(), Some(http::StatusCode::BAD_GATEWAY));
    }
}
