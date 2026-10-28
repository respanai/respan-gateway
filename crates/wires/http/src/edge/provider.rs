//! Native HTTP sending bound to the kernel's symmetric event interface.

use super::reader::{Failure, observed};
use super::upload::{self, Delivery, Upload};

use crate::codec;
use crate::{Body, Decoder, Encoder, Sent};
use bytes::Bytes;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{Future, FutureExt, Stream, StreamExt, TryStreamExt, stream};
use http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_core::{Connection, EventSink, EventStream, Extensions, Fault};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

/// Make a cloneable provider factory from an ordinary async sending function.
/// `Some(completion)` remains authoritative even after response EOF; `None`
/// means only the outgoing handoff is complete, not later socket writes.
/// `limit` is the most bytes a reader holds of one answer at once: a whole
/// body, a refusal's, or one event; past it, the answer fails `upstream`.
pub fn connect<F, Fut>(
    send: F,
    limit: usize,
) -> impl Fn(Extensions) -> ProviderIo + Clone + Send + Sync + 'static
where
    F: Fn(http::Request<Body>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Sent> + Send + 'static,
{
    let send: Arc<dyn Fn(http::Request<Body>) -> BoxFuture<'static, Sent> + Send + Sync> =
        Arc::new(move |request| send(request).boxed());
    move |extensions| ProviderIo {
        send: send.clone(),
        extensions,
        limit,
        success: Box::new(crate::rules::success),
        refusal: Box::new(respan_gateway_http_faults::provider_fault),
        codings: false,
        request: None,
        response: None,
    }
}

/// What it takes for an answer to succeed ([`ProviderIo::with_success`]).
type Success = Box<dyn FnOnce(&ResponseHead, &RequestHead) -> bool + Send>;

/// How a refused answer reads as a fault ([`ProviderIo::with_refusal`]).
type Refusal = Box<dyn FnOnce(http::Response<Bytes>) -> Fault + Send>;

/// One provider attempt, owning its extensions and sharing only the sender.
pub struct ProviderIo {
    send: Arc<dyn Fn(http::Request<Body>) -> BoxFuture<'static, Sent> + Send + Sync>,
    extensions: Extensions,
    /// The most bytes a reader holds of its answer at once.
    limit: usize,
    /// What it takes for a success.
    success: Success,
    /// How a refused answer reads as a fault.
    refusal: Refusal,
    /// Whether a success in a content coding is read as it came.
    codings: bool,
    /// What changes the call as its format wrote it ([`ProviderIo::with_request`]).
    request: Option<RequestHook>,
    /// What changes the answer before its format reads it
    /// ([`ProviderIo::with_response`]).
    response: Option<ResponseHook>,
}

/// A whole body, rewritten: a call's before it is sent, or an answer's
/// before it is read.
pub type Rewrite = Box<dyn FnOnce(Bytes) -> Result<Bytes, Fault> + Send>;

/// What changes a call ([`ProviderIo::with_request`]).
type RequestHook = Box<dyn FnOnce(&mut RequestHead) -> Result<Option<Rewrite>, Fault> + Send>;

/// What changes an answer ([`ProviderIo::with_response`]).
type ResponseHook = Box<dyn FnOnce(&RequestHead, &mut ResponseHead) -> Option<Rewrite> + Send>;

impl ProviderIo {
    /// Per-call transport state supplied by the gateway and its plugins.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Take an answer for a success if `success` says so, instead of HTTP's
    /// own rule: a 2xx, or a 101 to a call that asked to switch protocols.
    /// A WebSocket format, say, takes only its handshake's 101.
    pub fn with_success(
        mut self,
        success: impl FnOnce(&ResponseHead, &RequestHead) -> bool + Send + 'static,
    ) -> Self {
        self.success = Box::new(success);
        self
    }

    /// Read a refused answer, its body whole up to the transport's limit,
    /// into its fault with `refusal`, such as from the error body a format
    /// writes, instead of [`provider_fault`], which reads none of it.
    ///
    /// [`provider_fault`]: respan_gateway_http_faults::provider_fault
    pub fn with_refusal(
        mut self,
        refusal: impl FnOnce(http::Response<Bytes>) -> Fault + Send + 'static,
    ) -> Self {
        self.refusal = Box::new(refusal);
        self
    }

    /// Take a success in any content coding, its bytes as they came, as a
    /// format that relays them unread does, instead of failing it
    /// `upstream`: the gateway decodes no coding.
    pub fn with_content_codings(mut self) -> Self {
        self.codings = true;
        self
    }

    /// Change the call after its format wrote it, before it is sent:
    /// `request` edits its head, and may rewrite its whole body, which a
    /// call of live input, whose body streams, cannot have. `Err` fails the
    /// call unsent. The format still reads its answer against the call as
    /// it wrote it.
    pub fn with_request(
        mut self,
        request: impl FnOnce(&mut RequestHead) -> Result<Option<Rewrite>, Fault> + Send + 'static,
    ) -> Self {
        self.request = Some(Box::new(request));
        self
    }

    /// Change the answer before its format reads it: `response` edits its
    /// head, given the call's as its format wrote it, and may rewrite its
    /// whole body, read up to the transport's limit. It runs on every
    /// answer, a refusal too, before its success is decided.
    pub fn with_response(
        mut self,
        response: impl FnOnce(&RequestHead, &mut ResponseHead) -> Option<Rewrite> + Send + 'static,
    ) -> Self {
        self.response = Some(Box::new(response));
        self
    }

    /// Bind the call's encoder and its answer's decoder to core events.
    ///
    /// `encode` writes the call's head, given as `GET /` with no headers, and
    /// returns the encoder of its body. It is given the call's one input, or
    /// `None` for live input, one message per frame, which a format that
    /// takes one value per call refuses. One input is encoded whole now,
    /// before the call is sent; live input is encoded as it comes, as the
    /// provider reads it. Whatever its transport, the call
    /// loses what a proxy does not pass on: a hop's headers but `Upgrade`,
    /// which gets its `Connection: upgrade` (RFC 9110 7.6.1, 7.8); `Host`;
    /// `Expect`; conditions and ranges (13, 14); method overrides;
    /// `Accept-Encoding`, as the gateway decodes no content coding; and the
    /// gateway origin's `Cookie`, `Forwarded`, `Via`, `X-Forwarded-*` and
    /// `X-Real-IP`, which plugins still see ([`Original`](crate::raw::Original)) and a
    /// call's [`CallHeaders`](crate::call::CallHeaders) may set.
    ///
    /// An answer that is a success ([`with_success`](Self::with_success))
    /// opens at its head: `decode` gives its body's [`Decoder`], given its
    /// head and the call as sent (its body the whole bytes of one input, or
    /// empty for live input, which streamed), or `None` for no body to read; a fault its
    /// body says later fails it. Any other answer is refused, read into its
    /// fault ([`with_refusal`](Self::with_refusal)). A success in a content
    /// coding but `identity` fails `upstream` unread, unless relayed or
    /// [taken as it came](Self::with_content_codings). A call
    /// [`Relayed`](crate::raw::Relayed) gives the client its answer's head,
    /// unless the answer fails or is dropped unended first, and its error
    /// answer.
    pub fn bind<I, O>(
        self,
        encode: impl FnOnce(&mut RequestHead, Option<&I>) -> Result<Box<dyn Encoder<I>>, Fault>
        + Send
        + 'static,
        decode: impl FnOnce(&ResponseHead, &http::Request<Bytes>) -> Option<Box<dyn Decoder<O>>>
        + Send
        + 'static,
    ) -> Connection<O, I>
    where
        I: Clone + Send + Sync + 'static,
        O: Clone + Send + Sync + 'static,
    {
        let (answer, answered) = oneshot::channel();
        let incoming = async move { answered.await.map_err(|_| Fault::abandoned())? }.boxed();

        let outgoing = EventSink::preparing(move |events: &mut EventStream<I>| {
            if let Some(fault) = events.refusal() {
                return Err(fault.clone());
            }
            let mut head = http::Request::new(()).into_parts().0;
            // A whole call has one input, encoded now; live input streams,
            // encoded as the transport reads it, and drains when it did.
            let failed = Failure::default();
            let (body, encoding, draining) = match events.is_single() {
                true => {
                    let none = || Fault::internal("a whole request has no input");
                    let input = events.value().transpose()?.ok_or_else(none)?;
                    let mut encoder = encode(&mut head, Some(input))?;
                    let bytes = encoder.encode_all(std::slice::from_ref(input))?;
                    events.stop();
                    (Body::full(bytes), None, None)
                }
                false => {
                    let (encoding, body, draining) = upload::encoding(encode(&mut head, None)?);
                    (body, Some(encoding), Some(draining))
                }
            };
            crate::call::proxy_call(&mut head);
            let streaming = encoding.is_some();
            let sent = http::Request::from_parts(
                head.clone(),
                body.full_bytes().cloned().unwrap_or_default(),
            );
            let (head, body) = match self.request {
                Some(request) => changed(head, body, request)?,
                None => (head, body),
            };
            let response_hook = self.response;
            let mut request = http::Request::from_parts(head, body);
            let relayed = self.extensions.get::<crate::raw::Relayed>().copied();
            let client_head = relayed
                .and_then(|_| self.extensions.get::<crate::raw::ClientHead>())
                .cloned();
            let success = self.success;
            let refusal = self.refusal;
            let limit = self.limit;
            let codings = self.codings || relayed.is_some();
            request.extensions_mut().insert(self.extensions);
            let upload = failed.clone();
            // The call's delivery, which the sink drives and every poll of
            // the upload's source watches, runs in this order:
            //
            // 1. Send the call; a transport error fails the answer and the
            //    delivery alike.
            // 2. Hand the answer over (`answer`): refused, read whole into its
            //    fault, or opened with its decoder.
            // 3. Wait for the upload to settle: the transport's `completion`
            //    if it gave one; else, for a streamed upload, until the byte
            //    reader drained it (`draining`) or the answer ended
            //    (`ending`: the answer's stream drops `ended` at its end, as
            //    a provider that answered in full reads no more input). A
            //    refusal drops `ended` at once: a refused call's upload is
            //    cancelled, not awaited. A whole call has nothing to wait for.
            //
            // A streamed upload's source ends with the delivery: done, the
            // caller's input stops being read; failed, it reads the fault.
            let delivery = async move {
                let (ended, answer_ended) = oneshot::channel::<()>();
                let response = async move {
                    let (response, completion) = (self.send)(request).await?;
                    let (mut head, mut body) = response.into_parts();
                    let call = sent.clone().into_parts().0;
                    if let Some(rewrite) = response_hook.and_then(|hook| hook(&call, &mut head)) {
                        body = Body::full(rewrite(body.read_answer(limit).await?)?);
                        head.headers.remove(http::header::CONTENT_LENGTH);
                    }
                    let succeeded = success(&head, &call);
                    // Claimed now, before the answer is handed over, and held
                    // by its events: a call that fails or is dropped for
                    // another gives the client none of its head.
                    let claim = client_head
                        .filter(|_| succeeded)
                        .map(|slot| slot.claim(head.clone()));
                    let body = observed(body, upload.clone());
                    let applied = move |fault| match &relayed {
                        Some(raw) => raw.apply(fault),
                        None => fault,
                    };
                    if !succeeded {
                        let fault = refused(head, body, limit, refusal, &upload).await;
                        return Ok((EventStream::refused(applied(fault)), completion));
                    }
                    // Nothing decodes a content coding: a typed decoder would
                    // read its bytes as garbage. A relayed answer goes as it
                    // came, its head and coding the client's.
                    if !codings
                        && let Some(coding) = crate::rules::content_coding(&head.headers)
                    {
                        let fault = Fault::upstream(format!(
                            "the provider's answer is in content coding {coding}, which the gateway does not decode"
                        ));
                        return Ok((EventStream::refused(fault), completion));
                    }
                    let events = match decode(&head, &sent) {
                        Some(decoder) => codec::decode(body, decoder, Some(limit)),
                        None => EventStream::empty(),
                    }
                    .map_err(applied);
                    let events = claimed(events, claim);
                    let response = match streaming && completion.is_none() {
                        true => ending(events, ended),
                        false => EventStream::stream(events),
                    };
                    Ok::<_, Fault>((response, completion))
                }
                .await;
                match response {
                    Ok((response, completion)) => {
                        let _ = answer.send(Ok(response));
                        match completion {
                            Some(completion) => completion.await,
                            None => {
                                if let Some(draining) = draining {
                                    let _ = futures::future::select(draining, answer_ended).await;
                                }
                                Ok(())
                            }
                        }
                    }
                    Err(fault) => {
                        let _ = answer.send(Err(fault.clone()));
                        Err(fault)
                    }
                }
            };
            let delivery = delivery.boxed().shared();
            if streaming {
                let source = std::mem::replace(events, EventStream::empty());
                *events = until_delivered(source, delivery.clone());
            }
            Ok(Upload::new(encoding, delivery, failed))
        });
        (incoming, outgoing)
    }
}

/// A call's `head` and `body` as `request` changes them: a rewrite of the
/// body needs the whole of it.
fn changed(
    mut head: RequestHead,
    body: Body,
    request: RequestHook,
) -> Result<(RequestHead, Body), Fault> {
    let Some(rewrite) = request(&mut head)? else {
        return Ok((head, body));
    };
    let whole = body
        .full_bytes()
        .cloned()
        .ok_or_else(|| Fault::internal("a call of live input has no whole body to rewrite"))?;
    head.headers.remove(http::header::CONTENT_LENGTH);
    Ok((head, Body::full(rewrite(whole)?)))
}

/// A refused answer's fault: its body read whole, up to `limit`, by
/// `refusal`. A body over the limit or broken off cannot be read or
/// relayed: the gateway's 502, which keeps the answer's `Retry-After`, or
/// the upload's failure, which says why the call failed.
async fn refused(
    head: ResponseHead,
    body: Body,
    limit: usize,
    refusal: Refusal,
    upload: &Failure,
) -> Fault {
    let read = body.read_answer(limit).await;
    match read {
        Ok(bytes) => refusal(http::Response::from_parts(head, bytes)),
        Err(fault) if upload.get().is_some() => fault,
        Err(fault) => match respan_gateway_http_faults::waits(&head.headers) {
            Some(wait) => fault.with(wait),
            None => fault,
        },
    }
}

/// `events`, holding `claim` of the client's head: kept at a clean end,
/// released at a failure or a drop before the end.
fn claimed<O>(
    mut events: impl Stream<Item = Result<O, Fault>> + Unpin + Send + 'static,
    mut claim: Option<crate::raw::HeadClaim>,
) -> impl Stream<Item = Result<O, Fault>> + Unpin + Send + 'static {
    stream::poll_fn(move |cx| {
        let item = events.poll_next_unpin(cx);
        match (&item, claim.as_mut()) {
            (Poll::Ready(None), Some(claim)) => claim.keep(),
            (Poll::Ready(Some(Err(_))), Some(claim)) => claim.release(),
            _ => {}
        }
        item
    })
}

/// `events`, dropping `ended` at their end: a provider that answered in
/// full reads no more of a streamed upload.
fn ending<O: Send + 'static>(
    mut events: impl Stream<Item = Result<O, Fault>> + Unpin + Send + 'static,
    ended: oneshot::Sender<()>,
) -> EventStream<O> {
    let mut ended = Some(ended);
    EventStream::stream(stream::poll_fn(move |cx| {
        let item = events.poll_next_unpin(cx);
        if matches!(item, Poll::Ready(None | Some(Err(_)))) {
            ended.take();
        }
        item
    }))
}

/// A streamed upload's `source`, which ends with the call's `delivery`:
/// done, it stops being read; failed, it reads the fault.
fn until_delivered<I: Send + Sync + 'static>(
    mut source: EventStream<I>,
    mut delivery: Delivery,
) -> EventStream<I> {
    EventStream::stream(stream::poll_fn(move |cx| {
        if let Poll::Ready(end) = Pin::new(&mut delivery).poll(cx) {
            return Poll::Ready(end.err().map(Err));
        }
        Pin::new(&mut source).poll_next(cx)
    }))
}

impl std::fmt::Debug for ProviderIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderIo").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder;
    use futures::SinkExt;
    use respan_gateway_core::RetryAfter;
    use respan_gateway_http_faults::status;

    /// A refusal whose body is over the limit is the gateway's 502, but
    /// keeps the provider's `Retry-After`.
    #[tokio::test]
    async fn a_refusal_over_the_limit_keeps_its_retry_after() {
        let send = |_| async {
            let answer = http::Response::builder()
                .status(429)
                .header("retry-after", "7")
                .body(Body::full("far more than four bytes"));
            Ok((answer.unwrap_or_default(), None))
        };
        let io = connect(send, 4)(Extensions::new());
        let (incoming, mut outgoing) = io.bind(
            |_, _| {
                Ok(Box::new(|value: &Bytes, out: &mut Vec<Bytes>| {
                    out.push(value.clone());
                    Ok(())
                }))
            },
            |_, _| None::<Box<dyn Decoder<Bytes>>>,
        );
        let mut input = EventStream::single(Bytes::from_static(b"q"));
        assert!(outgoing.prepare(&mut input).is_ok());
        let (answer, _) = futures::join!(incoming, outgoing.close());
        let refused = answer.map(|events| events.refusal().cloned());
        let Ok(Some(fault)) = refused else {
            panic!("refused")
        };
        assert_eq!(status(&fault), http::StatusCode::BAD_GATEWAY);
        assert_eq!(
            fault.get(),
            Some(&RetryAfter(std::time::Duration::from_secs(7)))
        );
    }

    /// The answer `coding` reads as, as a typed format binds it, relayed if
    /// `relayed`: its first output, or its refusal.
    async fn coded(coding: &'static str, relayed: bool) -> Result<Bytes, Fault> {
        let send = move |_| async move {
            let answer = http::Response::builder()
                .header("content-encoding", coding)
                .body(Body::full("bytes"));
            Ok((answer.unwrap_or_default(), None))
        };
        let mut extensions = Extensions::new();
        if relayed {
            extensions.insert(crate::raw::Relayed);
        }
        let (incoming, mut outgoing) = connect(send, 64)(extensions).bind(
            |_, _| {
                Ok(Box::new(|value: &Bytes, out: &mut Vec<Bytes>| {
                    out.push(value.clone());
                    Ok(())
                }))
            },
            |_, _| {
                let read = crate::Whole::new(|bytes: Bytes, out: &mut Vec<Bytes>| {
                    out.push(bytes);
                    Ok(())
                });
                Some(Box::new(read) as Box<dyn Decoder<Bytes>>)
            },
        );
        let mut input = EventStream::single(Bytes::from_static(b"q"));
        outgoing.prepare(&mut input)?;
        let (answer, _) = futures::join!(incoming, outgoing.close());
        let mut events = answer?;
        if let Some(fault) = events.refusal() {
            return Err(fault.clone());
        }
        events.next().await.unwrap_or_else(|| Ok(Bytes::new()))
    }

    /// Nothing decodes a content coding: a success in one is a provider's
    /// fault, but relayed, which goes as it came.
    #[tokio::test]
    async fn a_success_in_a_content_coding_is_refused_unless_relayed() {
        let refused = coded("gzip", false).await.err();
        let said = refused.as_ref().map(|f| (f.kind(), status(f)));
        let upstream = respan_gateway_core::FaultKind::Provider;
        assert_eq!(said, Some((upstream, http::StatusCode::BAD_GATEWAY)));
        assert_eq!(
            coded("Identity", false).await.ok(),
            Some(Bytes::from("bytes"))
        );
        assert_eq!(coded("gzip", true).await.ok(), Some(Bytes::from("bytes")));
    }
}
