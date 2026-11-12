//! How a format's values become a body's frames and back: an [`Encoder`]
//! writes values as frames, a [`Decoder`] reads frames as values, and the
//! binding drives them. A closure is either; [`Sse`] and [`Whole`] frame
//! what another reads, and [`All`] what another writes.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Bytes, BytesMut};
use futures::{Stream, TryStreamExt};
use respan_gateway_core::{EventStream, Fault};

use crate::Body;

/// Writes values of `T` as frames: the chunks of a body, or the messages of
/// a live session. A closure `FnMut(&T, &mut Vec<Bytes>) -> Result<(),
/// Fault>` is one.
pub trait Encoder<T>: Send + 'static {
    /// The frames of what comes before any value, such as a quiet stream's
    /// opening comment.
    fn start(&mut self, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let _ = out;
        Ok(())
    }

    /// Write `value` as zero or more frames. What it pushed before failing
    /// is not sent.
    fn encode(&mut self, value: &T, out: &mut Vec<Bytes>) -> Result<(), Fault>;

    /// The values ended as `end` says: write what closes the body, if
    /// anything. `Ok` means what it wrote closes the body as `end` says,
    /// an `Err` written as error framing; `Err` means it could not, and
    /// what it pushed is not sent. If ending with `Ok(())` fails, `end` is
    /// called once more with that failure, to write its error framing; if
    /// that fails too, or `end` was a failure already, the body breaks off.
    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let _ = out;
        end
    }
}

impl<T, F> Encoder<T> for F
where
    F: FnMut(&T, &mut Vec<Bytes>) -> Result<(), Fault> + Send + 'static,
{
    fn encode(&mut self, value: &T, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self(value, out)
    }
}

/// Reads frames as values of `T`: a body's chunks, a live session's
/// messages, or, behind [`Sse`] or [`Whole`], its events or the whole of it.
/// A closure `FnMut(Bytes, &mut Vec<T>) -> Result<(), Fault>` is one.
pub trait Decoder<T>: Send + 'static {
    /// The values of what comes before any frame, such as a head that must
    /// reach its reader before the body speaks.
    fn start(&mut self, out: &mut Vec<T>) -> Result<(), Fault> {
        let _ = out;
        Ok(())
    }

    /// The values of one frame. Those pushed before failing come before the
    /// failure.
    fn decode(&mut self, raw: Bytes, out: &mut Vec<T>) -> Result<(), Fault>;

    /// The frames ended as `end` says, a broken body's fault or `Ok`: the
    /// values of what was held, if it ended well. `Err` fails it; one that
    /// ends a broken body replaces the body's fault, as one that explains it.
    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<T>) -> Result<(), Fault> {
        let _ = out;
        end
    }

    /// The most bytes it may hold between frames, its body's limit; what
    /// holds bytes, as [`Sse`] and [`Whole`] do, fails `upstream` past it.
    fn limit(&mut self, bytes: Option<usize>) {
        let _ = bytes;
    }

    /// Whether it has read all it will, as a protocol whose answer is
    /// complete before its body ends: the rest goes unread, and the values
    /// end well.
    fn done(&self) -> bool {
        false
    }
}

impl<T, F> Decoder<T> for F
where
    F: FnMut(Bytes, &mut Vec<T>) -> Result<(), Fault> + Send + 'static,
{
    fn decode(&mut self, raw: Bytes, out: &mut Vec<T>) -> Result<(), Fault> {
        self(raw, out)
    }
}

/// Server-sent events: a [`Decoder`] that splits frames into events and
/// gives its inner decoder one event at a time. At the end, a last event of
/// whole lines is read; one cut mid-line is not.
pub struct Sse<D> {
    inner: D,
    parser: respan_gateway_sse::Parser,
}

impl<D> std::fmt::Debug for Sse<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sse").finish_non_exhaustive()
    }
}

impl<D> Sse<D> {
    /// Events of a body, each read by `inner`.
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            parser: respan_gateway_sse::Parser::new(None),
        }
    }
}

impl<T, D: Decoder<T>> Decoder<T> for Sse<D> {
    fn start(&mut self, out: &mut Vec<T>) -> Result<(), Fault> {
        self.inner.start(out)
    }

    fn decode(&mut self, raw: Bytes, out: &mut Vec<T>) -> Result<(), Fault> {
        if self.inner.done() {
            return Ok(());
        }
        for event in self.parser.push(raw)? {
            if self.inner.done() {
                break;
            }
            self.inner.decode(event, out)?;
        }
        Ok(())
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<T>) -> Result<(), Fault> {
        if end.is_ok() {
            for event in self.parser.finish() {
                if self.inner.done() {
                    break;
                }
                self.inner.decode(event, out)?;
            }
        }
        self.inner.end(end, out)
    }

    fn limit(&mut self, bytes: Option<usize>) {
        self.parser = respan_gateway_sse::Parser::new(bytes);
        self.inner.limit(bytes);
    }

    fn done(&self) -> bool {
        self.inner.done()
    }
}

/// The whole body at once: a [`Decoder`] that holds every frame and gives
/// its inner decoder the whole body when it ends. A failure reads nothing.
pub struct Whole<D> {
    inner: D,
    held: Vec<Bytes>,
    limit: Option<usize>,
    size: usize,
}

impl<D> std::fmt::Debug for Whole<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Whole")
            .field("held", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl<D> Whole<D> {
    /// Hold all of a body for `inner`.
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            held: Vec::new(),
            limit: None,
            size: 0,
        }
    }
}

impl<T, D: Decoder<T>> Decoder<T> for Whole<D> {
    fn start(&mut self, out: &mut Vec<T>) -> Result<(), Fault> {
        self.inner.start(out)
    }

    fn decode(&mut self, raw: Bytes, _: &mut Vec<T>) -> Result<(), Fault> {
        self.size += raw.len();
        if let Some(limit) = self.limit.filter(|&limit| self.size > limit) {
            return Err(respan_gateway_http_faults::over_limit(limit));
        }
        self.held.push(raw);
        Ok(())
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<T>) -> Result<(), Fault> {
        end?;
        let whole = match self.held.len() {
            1 => self.held.pop().unwrap_or_default(),
            _ => Bytes::from(self.held.concat()),
        };
        self.inner.decode(whole, out)?;
        self.inner.end(Ok(()), out)
    }

    fn limit(&mut self, bytes: Option<usize>) {
        self.limit = bytes;
    }
}

/// All values at once: an [`Encoder`] that holds every value and, when
/// they end, writes them as one body with `render`, a function
/// `FnMut(&[T]) -> Result<B, Fault>`. A failure writes nothing.
pub struct All<F, T> {
    render: F,
    held: Vec<T>,
}

impl<F, T> std::fmt::Debug for All<F, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("All")
            .field("held", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl<F, T> All<F, T> {
    /// Hold all values for `render`.
    pub fn new(render: F) -> Self {
        Self {
            render,
            held: Vec::new(),
        }
    }
}

impl<T, F, B> Encoder<T> for All<F, T>
where
    T: Clone + Send + 'static,
    F: FnMut(&[T]) -> Result<B, Fault> + Send + 'static,
    B: Into<Bytes>,
{
    fn encode(&mut self, value: &T, _: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.held.push(value.clone());
        Ok(())
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        end?;
        out.push((self.render)(&self.held)?.into());
        Ok(())
    }
}

impl<T: 'static> dyn Encoder<T> {
    /// `values` written whole, the end included, as one body.
    pub fn encode_all(&mut self, values: &[T]) -> Result<Bytes, Fault> {
        let mut frames = Vec::new();
        self.start(&mut frames)?;
        for value in values {
            self.encode(value, &mut frames)?;
        }
        self.end(Ok(()), &mut frames)?;
        Ok(match frames.len() {
            1 => frames.pop().unwrap_or_default(),
            _ => frames.concat().into(),
        })
    }
}

/// One value of a whole body, read by `decoder` when first asked for, as a
/// whole request's one input is.
pub(crate) fn decode_one<T>(bytes: Bytes, decoder: Box<dyn Decoder<T>>) -> EventStream<T>
where
    T: Clone + Send + Sync + 'static,
{
    let decoder = Mutex::new(Some(decoder));
    EventStream::lazy(move || {
        let taken = decoder.lock().take();
        let mut decoder = taken.ok_or_else(|| Fault::internal("a whole input read twice"))?;
        let mut out = Vec::new();
        decoder.start(&mut out)?;
        decoder.decode(bytes.clone(), &mut out)?;
        decoder.end(Ok(()), &mut out)?;
        match <[T; 1]>::try_from(out) {
            Ok([one]) => Ok(one),
            Err(out) => Err(Fault::internal(format!(
                "a whole request decoded to {} inputs, not one",
                out.len()
            ))),
        }
    })
}

/// The values `decoder` reads of `body`, frame by frame, holding at most
/// `limit` bytes ([`Decoder::limit`]), until it is [`done`](Decoder::done). Values read before a
/// failure come before it. It reads no frame
/// ahead while values remain, and yields after [`FRAMES_PER_POLL`] frames
/// that read as nothing.
pub(crate) fn decode<T: Send + 'static>(
    body: Body,
    mut decoder: Box<dyn Decoder<T>>,
    limit: Option<usize>,
) -> EventStream<T> {
    decoder.limit(limit);
    let mut ready = Vec::new();
    let started = decoder.start(&mut ready);
    let mut ready: VecDeque<_> = ready.into_iter().map(Ok).collect();
    if let Err(fault) = &started {
        ready.push_back(Err(fault.clone()));
    }
    EventStream::stream(Decoding {
        body,
        decoder,
        ready,
        ended: started.is_err(),
    })
}

/// The most frames that read as nothing one poll of [`decode`] reads before
/// it yields: what tokio's cooperative budget allows one poll of a task
/// (128), so a body always ready, of comments alone say, cannot hold its
/// thread.
const FRAMES_PER_POLL: usize = 128;

struct Decoding<T> {
    body: Body,
    decoder: Box<dyn Decoder<T>>,
    ready: VecDeque<Result<T, Fault>>,
    ended: bool,
}

impl<T> Unpin for Decoding<T> {}

impl<T: Send + 'static> Stream for Decoding<T> {
    type Item = Result<T, Fault>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut frames = 0;
        loop {
            if let Some(value) = this.ready.pop_front() {
                return Poll::Ready(Some(value));
            }
            if this.ended {
                return Poll::Ready(None);
            }
            if frames == FRAMES_PER_POLL {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let frame = ready!(Pin::new(&mut this.body).poll_next(cx));
            frames += 1;
            let mut values = Vec::new();
            let end = match frame {
                Some(Ok(bytes)) => match this.decoder.decode(bytes, &mut values) {
                    Err(fault) => Some(Err(fault)),
                    Ok(()) => this.decoder.done().then_some(Ok(())),
                },
                tail => {
                    let wire = tail.transpose().map(|_| ());
                    let ended = this.decoder.end(wire.clone(), &mut values);
                    Some(match ended {
                        Err(fault) if wire.is_err() => Err(fault),
                        ended => wire.and(ended),
                    })
                }
            };
            this.ready.extend(values.into_iter().map(Ok));
            if let Some(end) = end {
                this.ended = true;
                if let Err(fault) = end {
                    this.ready.push_back(Err(fault));
                }
            }
        }
    }
}

/// `body` collected whole, `too_big` past `limit` bytes. Each frame is
/// released before the next is polled: a producer may recycle its buffer.
pub(crate) async fn collect(
    body: Body,
    limit: usize,
    too_big: impl Fn() -> Fault,
) -> Result<Bytes, Fault> {
    if let Some(bytes) = body.full_bytes() {
        return (bytes.len() <= limit)
            .then(|| bytes.clone())
            .ok_or_else(too_big);
    }
    let all = body.try_fold(BytesMut::new(), |mut all, frame| {
        let kept = all.len() + frame.len() <= limit;
        if kept {
            all.extend_from_slice(&frame);
        }
        futures::future::ready(kept.then_some(all).ok_or_else(&too_big))
    });
    all.await.map(BytesMut::freeze)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, StreamExt};
    use respan_gateway_core::FaultKind;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn frames_that_read_as_nothing_cannot_spin_in_one_poll() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let body = Body::stream(futures::stream::repeat_with(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::new())
        }));
        let mut read = decode(body, Box::new(|_: Bytes, _: &mut Vec<u8>| Ok(())), None);
        assert!(read.next().now_or_never().is_none());
        assert_eq!(count.load(Ordering::SeqCst), FRAMES_PER_POLL);
    }

    #[test]
    fn a_whole_body_is_written_once_in_order() {
        let mut whole = All::new(|values: &[u32]| Ok(format!("{values:?}")));
        let mut out = Vec::new();
        for value in [1, 2] {
            assert!(whole.encode(&value, &mut out).is_ok());
        }
        assert!(out.is_empty());
        assert!(Encoder::end(&mut whole, Ok(()), &mut out).is_ok());
        assert_eq!(out, [Bytes::from_static(b"[1, 2]")]);
    }

    #[test]
    fn a_failure_writes_nothing() {
        let mut whole = All::new(|values: &[u32]| Ok(format!("{values:?}")));
        let mut out = Vec::new();
        assert!(whole.encode(&1, &mut out).is_ok());
        let ended = Encoder::end(&mut whole, Err(Fault::upstream("x")), &mut out);
        assert!(ended.is_err());
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn a_body_over_its_limit_fails_upstream() {
        let body = Body::full("12345");
        let whole = Whole::new(|_: Bytes, _: &mut Vec<()>| Ok(()));
        let got: Vec<_> = decode(body, Box::new(whole), Some(4)).collect().await;
        let fault = got.into_iter().next().and_then(Result::err);
        assert!(fault.is_some_and(|f| f.is(FaultKind::Provider)));
    }

    #[tokio::test]
    async fn a_stream_stops_at_its_first_failure() {
        let events = Sse::new(|raw: Bytes, out: &mut Vec<Bytes>| {
            if respan_gateway_sse::event_data(&raw).is_some() {
                return Err(Fault::upstream("unreadable"));
            }
            out.push(raw);
            Ok(())
        });
        let body = Body::full(Bytes::from_static(b": ping\n\ndata: x\n\ndata: y\n\n"));
        let out: Vec<_> = decode(body, Box::new(events), None).collect().await;
        assert!(out.len() == 2 && out[0].is_ok() && out[1].is_err());
    }
}
