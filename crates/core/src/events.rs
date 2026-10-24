//! Symmetric standard streams and sinks with protocol-owned preparation.

use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, Sink, Stream, StreamExt};

use crate::fault::guarded;
use crate::{Fault, Filter, Result};

/// The most ready events one poll moves before yielding: Tokio's cooperative
/// budget per task poll, so a hot stream yields no later than the runtime
/// would make any other Tokio resource yield.
pub(crate) const MAX_BATCH: usize = 128;

/// Typed events: one replayable value or a live stream. The first fault
/// ends the stream. Protocol data belongs in typed events or explicit extensions.
/// Opening and writing are ordinary futures, outside this stream.
pub struct EventStream<T> {
    shape: Shape<T>,
    ended: bool,
}

enum Shape<T> {
    Single(Arc<Single<T>>),
    Stream(BoxStream<'static, Result<T>>),
    Refused(Fault),
    Empty,
}

/// A replayable, memoized value, independent of the live-stream variant.
pub(crate) struct Single<T> {
    value: SingleValue<T>,
    clone: fn(&T) -> T,
}

enum SingleValue<T> {
    Ready(Result<T>),
    Deferred {
        decoded: OnceLock<Result<T>>,
        decode: Box<dyn Fn() -> Result<T> + Send + Sync>,
    },
}

impl<T> Single<T> {
    pub(crate) fn ready(value: Result<T>, clone: fn(&T) -> T) -> Arc<Self> {
        Arc::new(Self {
            value: SingleValue::Ready(value),
            clone,
        })
    }

    fn deferred(
        decode: impl Fn() -> Result<T> + Send + Sync + 'static,
        clone: fn(&T) -> T,
    ) -> Arc<Self> {
        Arc::new(Self {
            value: SingleValue::Deferred {
                decoded: OnceLock::new(),
                decode: Box::new(decode),
            },
            clone,
        })
    }

    pub(crate) fn known_error(&self) -> Option<Fault> {
        let result = match &self.value {
            SingleValue::Ready(value) => value,
            SingleValue::Deferred { decoded, .. } => decoded.get()?,
        };
        result.as_ref().err().cloned()
    }

    pub(crate) fn value(&self) -> Result<&T> {
        let result = match &self.value {
            SingleValue::Ready(value) => value,
            SingleValue::Deferred { decoded, decode } => decoded.get_or_init(|| guarded(decode)),
        };
        result.as_ref().map_err(Clone::clone)
    }

    fn cloned(&self) -> Result<T> {
        self.value().map(self.clone)
    }

    pub(crate) fn into_events(self: Arc<Self>) -> EventStream<T> {
        EventStream {
            shape: Shape::Single(self),
            ended: false,
        }
    }
}

impl<T> fmt::Debug for EventStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.shape {
            Shape::Single(_) => "single",
            Shape::Stream(_) => "stream",
            Shape::Refused(_) => "refused",
            Shape::Empty => "ended",
        };
        f.debug_struct("EventStream")
            .field("state", &state)
            .finish()
    }
}

impl<T> EventStream<T> {
    /// An empty stream.
    pub fn empty() -> Self {
        Self {
            shape: Shape::Empty,
            ended: false,
        }
    }

    /// Stop yielding events without decoding or polling the source.
    /// Retain its shape and ownership until this stream is dropped,
    /// so already-encoded or bodyless delivery can finish before releasing it.
    pub fn stop(&mut self) {
        self.ended = true;
    }

    /// A refusal classified before any events become available.
    pub fn refused(fault: Fault) -> Self {
        Self {
            shape: Shape::Refused(fault),
            ended: false,
        }
    }

    /// Whether these events contain a replayable single value.
    pub fn is_single(&self) -> bool {
        matches!(self.shape, Shape::Single(_))
    }

    /// The refusal, before the stream has been read.
    pub fn refusal(&self) -> Option<&Fault> {
        match &self.shape {
            Shape::Refused(fault) => Some(fault),
            _ => None,
        }
    }

    /// The single value, decoding it at most once. `None` for a live stream.
    pub fn value(&self) -> Option<Result<&T>> {
        match &self.shape {
            Shape::Single(value) => Some(value.value()),
            _ => None,
        }
    }

    pub(crate) fn take_single(&mut self) -> Option<Arc<Single<T>>> {
        if !self.is_single() {
            return None;
        }
        match std::mem::replace(&mut self.shape, Shape::Empty) {
            Shape::Single(value) => Some(value),
            _ => unreachable!("source checked above"),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> EventStream<T> {
    /// A single value already available to the reader.
    pub fn single(value: T) -> Self {
        Single::ready(Ok(value), T::clone).into_events()
    }

    /// A replayable value decoded only when a reader needs it.
    /// Internal snapshots share its result, including decoding failures.
    pub fn lazy(decode: impl Fn() -> Result<T> + Send + Sync + 'static) -> Self {
        Single::deferred(decode, T::clone).into_events()
    }
}

impl<T> EventStream<T> {
    /// A live stream whose first error is its last event.
    pub fn stream(events: impl Stream<Item = Result<T>> + Send + 'static) -> Self {
        Self {
            shape: Shape::Stream(events.boxed()),
            ended: false,
        }
    }
}

impl<T: Send + Sync + 'static> EventStream<T> {
    /// Change representation without eagerly decoding a single value.
    /// `clone` copies replayable snapshots; live values are moved.
    pub(crate) fn map_values<U: Send + Sync + 'static>(
        self,
        map: impl Fn(T) -> Result<U> + Send + Sync + 'static,
        clone: fn(&U) -> U,
    ) -> EventStream<U> {
        let shape = match self.shape {
            Shape::Single(value) => Shape::Single(Single::deferred(
                move || value.cloned().and_then(&map),
                clone,
            )),
            Shape::Stream(events) => {
                Shape::Stream(events.map(move |event| event.and_then(&map)).boxed())
            }
            Shape::Refused(fault) => Shape::Refused(fault),
            Shape::Empty => Shape::Empty,
        };
        EventStream {
            shape,
            ended: self.ended,
        }
    }
}

impl<T: Clone + Send + Sync + 'static> EventStream<T> {
    /// These events through `filter`, as [`Filter`] says: each value
    /// edited as it comes, what it kept back pushed at the end, its end run
    /// at every end. A single value stays single, the filter run once when
    /// it is read, and must give exactly one value for it. A refusal is
    /// not filtered.
    pub fn filtered(self, filter: Box<dyn Filter<T>>) -> Self {
        let shape = match self.shape {
            Shape::Single(value) => {
                let filter = parking_lot::Mutex::new(filter);
                Shape::Single(Single::deferred(
                    move || one(&mut **filter.lock(), value.cloned()),
                    T::clone,
                ))
            }
            Shape::Stream(events) => Shape::Stream(Box::pin(Filtered::new(events, filter))),
            Shape::Empty => Shape::Stream(Box::pin(Filtered::new(
                futures::stream::empty().boxed(),
                filter,
            ))),
            Shape::Refused(fault) => Shape::Refused(fault),
        };
        EventStream {
            shape,
            ended: self.ended,
        }
    }
}

/// The one value `filter` gives for `value`, its end run after.
fn one<T: 'static>(filter: &mut dyn Filter<T>, value: Result<T>) -> Result<T> {
    let mut out = Vec::new();
    let edited = value.and_then(|value| guarded(|| filter.edit(value, &mut out)));
    let ended = guarded(|| filter.end(&edited, &mut out));
    edited.and(ended)?;
    match <[T; 1]>::try_from(out) {
        Ok([value]) => Ok(value),
        Err(out) => Err(Fault::internal(format!(
            "a filter of one value gave {}",
            out.len()
        ))),
    }
}

/// A live stream through a filter.
struct Filtered<T: 'static> {
    events: BoxStream<'static, Result<T>>,
    /// Until its end has run.
    filter: Option<Box<dyn Filter<T>>>,
    ready: VecDeque<T>,
}

impl<T: 'static> Unpin for Filtered<T> {}

impl<T: 'static> Filtered<T> {
    fn new(events: BoxStream<'static, Result<T>>, filter: Box<dyn Filter<T>>) -> Self {
        Self {
            events,
            filter: Some(filter),
            ready: VecDeque::new(),
        }
    }

    /// Run the filter's end, as `end` ended: what it kept back is ready if
    /// it ended well, dropped if it failed. The fault the stream ends with,
    /// if any.
    fn end(&mut self, end: Result<()>) -> Option<Fault> {
        let mut filter = self.filter.take()?;
        let mut out = Vec::new();
        let ended = guarded(|| filter.end(&end, &mut out));
        match end.and(ended) {
            Ok(()) => {
                self.ready.extend(out);
                None
            }
            Err(fault) => Some(fault),
        }
    }
}

impl<T: 'static> Stream for Filtered<T> {
    type Item = Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(value) = this.ready.pop_front() {
                return Poll::Ready(Some(Ok(value)));
            }
            let Some(filter) = this.filter.as_mut() else {
                return Poll::Ready(None);
            };
            let end = match futures::ready!(this.events.poll_next_unpin(cx)) {
                Some(Ok(value)) => {
                    let mut out = Vec::new();
                    match guarded(|| filter.edit(value, &mut out)) {
                        Ok(()) => {
                            this.ready.extend(out);
                            continue;
                        }
                        Err(fault) => Err(fault),
                    }
                }
                Some(Err(fault)) => Err(fault),
                None => Ok(()),
            };
            if let Some(fault) = this.end(end) {
                return Poll::Ready(Some(Err(fault)));
            }
        }
    }
}

/// A stream dropped before its end ends its filter too.
impl<T: 'static> Drop for Filtered<T> {
    fn drop(&mut self) {
        self.end(Err(Fault::abandoned()));
    }
}

impl<T> Unpin for EventStream<T> {}

impl<T> Stream for EventStream<T> {
    type Item = Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        let item = match &mut this.shape {
            Shape::Single(value) => {
                let item = value.cloned();
                this.ended = true;
                return Poll::Ready(Some(item));
            }
            Shape::Stream(events) => events.as_mut().poll_next(cx),
            Shape::Refused(fault) => Poll::Ready(Some(Err(fault.clone()))),
            Shape::Empty => Poll::Ready(None),
        };
        if matches!(item, Poll::Ready(None | Some(Err(_)))) {
            this.ended = true;
        }
        item
    }
}

type BoxSink<T> = Pin<Box<dyn Sink<Result<T>, Error = Fault> + Send>>;
type Prepare<T> = Box<dyn FnOnce(&mut EventStream<T>) -> Result<BoxSink<T>> + Send>;

enum SinkState<T> {
    Preparing(Prepare<T>),
    Ready(BoxSink<T>),
    Failed(Fault),
    Closed(Result<()>),
}

/// A standard per-event sink. Source faults are items; sink faults are delivery
/// failures. The first submitted source fault is terminal: it is passed to the
/// concrete sink, then only flushing and closing are allowed.
///
/// `poll_flush` advances delivery without ending the transfer. `poll_close`
/// completes final framing and actual delivery, including transport completion.
/// Dropping the sink cancels outstanding delivery.
pub struct EventSink<T> {
    state: SinkState<T>,
    terminal: bool,
}

impl<T> fmt::Debug for EventSink<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.state {
            SinkState::Preparing(_) => "preparing",
            SinkState::Ready(_) => "ready",
            SinkState::Failed(_) => "failed",
            SinkState::Closed(_) => "closed",
        };
        f.debug_struct("EventSink")
            .field("state", &state)
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl<T> EventSink<T> {
    /// Wrap an ordinary sink. No preparation is required for direct SinkExt use.
    pub fn new<S>(sink: S) -> Self
    where
        S: Sink<Result<T>, Error = Fault> + Send + 'static,
    {
        Self {
            state: SinkState::Ready(Box::pin(sink)),
            terminal: false,
        }
    }

    /// Construct the real per-event sink synchronously from the source stream.
    /// Call `prepare` before using the standard sink operations. Preparation
    /// must not initiate I/O or retain a live source to read from the sink.
    /// It may inspect/encode a finite value, stop an already-encoded or
    /// bodyless source, or wrap a live source that remains owned by the caller.
    pub fn preparing<S, F>(prepare: F) -> Self
    where
        S: Sink<Result<T>, Error = Fault> + Send + 'static,
        F: FnOnce(&mut EventStream<T>) -> Result<S> + Send + 'static,
    {
        Self {
            state: SinkState::Preparing(Box::new(move |source| {
                prepare(source).map(|sink| Box::pin(sink) as BoxSink<T>)
            })),
            terminal: false,
        }
    }

    /// Prepare once, before the kernel marks the call sent. Repeated calls do
    /// nothing after success and retain the original failure after an error.
    pub fn prepare(&mut self, source: &mut EventStream<T>) -> Result<()> {
        if matches!(self.state, SinkState::Preparing(_)) {
            let missing = Fault::internal("event sink preparation did not complete");
            let SinkState::Preparing(prepare) =
                std::mem::replace(&mut self.state, SinkState::Failed(missing))
            else {
                unreachable!()
            };
            self.state = match prepare(source) {
                Ok(sink) => SinkState::Ready(sink),
                Err(fault) => SinkState::Failed(fault),
            };
        }
        self.sink().map(|_| ())
    }

    fn sink(&mut self) -> Result<&mut BoxSink<T>> {
        match &mut self.state {
            SinkState::Ready(sink) => Ok(sink),
            SinkState::Preparing(_) => Err(Fault::internal("event sink needs preparation")),
            SinkState::Failed(fault) => Err(fault.clone()),
            SinkState::Closed(_) => Err(Fault::internal("event sink is closed")),
        }
    }

    fn writable(&self) -> Result<()> {
        if self.terminal {
            Err(Fault::internal(
                "event sink is closed or received its terminal fault",
            ))
        } else {
            Ok(())
        }
    }
}

impl<T: Clone + Send + Sync + 'static> EventSink<T> {
    /// This sink, given its source's events through `filter`
    /// ([`EventStream::filtered`]).
    pub fn filtered(mut self, filter: Box<dyn Filter<T>>) -> Self {
        EventSink::preparing(move |events: &mut EventStream<T>| {
            *events = std::mem::replace(events, EventStream::empty()).filtered(filter);
            self.prepare(events)?;
            Ok(self)
        })
    }
}

impl<T> Unpin for EventSink<T> {}

impl<T> Sink<Result<T>> for EventSink<T> {
    type Error = Fault;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.get_mut();
        this.writable()?;
        this.sink()?.as_mut().poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Result<T>) -> Result<()> {
        let this = self.get_mut();
        this.writable()?;
        let terminal = item.is_err();
        this.sink()?.as_mut().start_send(item)?;
        this.terminal = terminal;
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.get_mut();
        if let SinkState::Closed(end) = &this.state {
            return Poll::Ready(end.clone());
        }
        this.sink()?.as_mut().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.get_mut();
        this.terminal = true;
        if let SinkState::Closed(end) = &this.state {
            return Poll::Ready(end.clone());
        }
        let end = futures::ready!(this.sink()?.as_mut().poll_close(cx));
        this.state = SinkState::Closed(end.clone());
        Poll::Ready(end)
    }
}

/// Forward source faults as sink items, not TryStream failures. Flush even
/// before the first event so quiet duplex transfers can open. Bound each poll
/// so an always-ready source/sink cannot starve receiving or cancellation.
pub(crate) fn deliver<T: Send + Sync + 'static>(
    mut source: EventStream<T>,
    mut sink: EventSink<T>,
) -> BoxFuture<'static, Result<()>> {
    let mut ended = source.ended || matches!(source.shape, Shape::Empty);
    // Retain the source after Ready, until the owner drops this future. The
    // client delivery guard must mark failure before unread output is dropped.
    futures::future::poll_fn(move |cx| {
        for _ in 0..MAX_BATCH {
            if ended {
                return Pin::new(&mut sink).poll_close(cx);
            }
            match Pin::new(&mut sink).poll_ready(cx) {
                Poll::Ready(Ok(())) => {}
                other => return other,
            }
            match Pin::new(&mut source).poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    ended = item.is_err();
                    Pin::new(&mut sink).start_send(item)?;
                }
                Poll::Ready(None) => ended = true,
                Poll::Pending => {
                    if let Poll::Ready(Err(fault)) = Pin::new(&mut sink).poll_flush(cx) {
                        return Poll::Ready(Err(fault));
                    }
                    return Poll::Pending;
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::channel::oneshot;
    use futures::{TryStreamExt, stream};

    use super::*;

    /// `r` with a fault as its message, to compare.
    fn says<T>(r: Result<T>) -> std::result::Result<T, String> {
        r.map_err(|f| f.message().to_owned())
    }

    /// What a probe's sink took, faults as messages.
    fn took(items: &parking_lot::Mutex<Vec<Result<u8>>>) -> Vec<std::result::Result<u8, String>> {
        items.lock().iter().cloned().map(says).collect()
    }

    #[test]
    fn snapshots_and_mapping_share_one_lazy_decode() {
        let decodes = Arc::new(AtomicUsize::new(0));
        let seen = decodes.clone();
        let mut events = EventStream::lazy(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(17_u8)
        });
        let Some(single) = events.take_single() else {
            panic!("single expected")
        };
        let mapped = single
            .clone()
            .into_events()
            .map_values(|value| Ok(u16::from(value)), Clone::clone);
        assert_eq!(decodes.load(Ordering::SeqCst), 0);
        assert_eq!(says(single.value()), Ok(&17));
        assert_eq!(mapped.value().map(says), Some(Ok(&17_u16)));
        assert_eq!(mapped.value().map(says), Some(Ok(&17_u16)));
        assert_eq!(decodes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mapping_defers_and_memoizes_failures() {
        for lazy in [false, true] {
            let events = if lazy {
                EventStream::lazy(|| Ok(7_u8))
            } else {
                EventStream::single(7_u8)
            };
            let maps = Arc::new(AtomicUsize::new(0));
            let seen = maps.clone();
            let fault = Fault::new("mapping failed");
            let failure = fault.clone();
            let mut mapped = events.map_values(
                move |_| -> Result<u16> {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err(failure.clone())
                },
                Clone::clone,
            );
            assert_eq!(maps.load(Ordering::SeqCst), 0);
            let Some(single) = mapped.take_single() else {
                panic!("single expected")
            };
            assert!(single.known_error().is_none());
            let replay = single.clone().into_events();
            let failed = Err(fault.message().to_owned());
            assert_eq!(says(single.value()), failed);
            assert_eq!(replay.value().map(says), Some(failed));
            assert_eq!(maps.load(Ordering::SeqCst), 1);
            let known = single.known_error();
            assert_eq!(known.as_ref().map(Fault::message), Some("mapping failed"));
        }
    }

    #[test]
    fn lazy_panic_is_memoized() {
        let decodes = Arc::new(AtomicUsize::new(0));
        let seen = decodes.clone();
        let events = EventStream::<u8>::lazy(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("decoder broke");
        });
        for _ in 0..2 {
            let Some(Err(fault)) = events.value() else {
                panic!("fault expected")
            };
            assert!(fault.code() == "internal");
        }
        assert_eq!(decodes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn first_fault_stops_polling_until_the_owner_drops_the_source() {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let held = Arc::new(());
        let captured = held.clone();
        let mut events = EventStream::stream(stream::poll_fn(move |_| {
            let _keep_alive = &captured;
            let read = seen.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Some(if read == 0 {
                Ok(1)
            } else {
                Err(Fault::new("cut"))
            }))
        }));
        assert_eq!(events.next().await.map(says), Some(Ok(1)));
        assert_eq!(events.next().await.map(says), Some(Err("cut".into())));
        assert_eq!(Arc::strong_count(&held), 2);
        assert!(events.next().await.is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        drop(events);
        assert_eq!(Arc::strong_count(&held), 1);
    }

    #[tokio::test]
    async fn mapping_preserves_single_shape() {
        let events =
            EventStream::single(7_u8).map_values(|value| Ok(u16::from(value)), Clone::clone);
        assert!(events.is_single());
        assert_eq!(says(events.try_collect::<Vec<_>>().await), Ok(vec![7_u16]));
    }

    #[tokio::test]
    async fn refusal_is_yielded_once() {
        let fault = Fault::new("refused");
        let mut events = EventStream::<u8>::refused(fault);
        assert_eq!(events.refusal().map(Fault::message), Some("refused"));
        assert_eq!(events.next().await.map(says), Some(Err("refused".into())));
        assert!(events.next().await.is_none());
    }

    #[derive(Default)]
    struct Probe {
        items: Arc<parking_lot::Mutex<Vec<Result<u8>>>>,
        flushes: Arc<AtomicUsize>,
        ready: Option<BoxFuture<'static, Result<()>>>,
        close: Option<BoxFuture<'static, Result<()>>>,
    }

    impl Sink<Result<u8>> for Probe {
        type Error = Fault;
        fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            if let Some(ready) = &mut self.ready {
                futures::ready!(ready.as_mut().poll(cx))?;
                self.ready = None;
            }
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, item: Result<u8>) -> Result<()> {
            self.items.lock().push(item);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            match &mut self.close {
                Some(close) => close.as_mut().poll(cx),
                None => Poll::Ready(Ok(())),
            }
        }
    }

    #[test]
    fn preparation_failure_happens_on_submission() {
        let fault = Fault::new("cannot encode");
        let mut sink = EventSink::preparing(move |_: &mut EventStream<u8>| Err::<Probe, _>(fault));
        let failed = Err("cannot encode".to_owned());
        assert_eq!(says(sink.prepare(&mut EventStream::single(1))), failed);
        assert_eq!(says(sink.prepare(&mut EventStream::single(2))), failed);
    }

    #[tokio::test]
    async fn delivery_future_waits_for_completion_and_keeps_failure() {
        let (finish, finished) = oneshot::channel();
        let probe = Probe {
            close: Some(async { finished.await.map_err(|_| Fault::abandoned())? }.boxed()),
            ..Probe::default()
        };
        let items = probe.items.clone();
        let mut delivering = deliver(EventStream::single(1), EventSink::new(probe));
        assert!(delivering.as_mut().now_or_never().is_none());
        assert_eq!(took(&items), [Ok(1)]);
        assert!(finish.send(Err(Fault::new("cut"))).is_ok());
        assert_eq!(says(delivering.await), Err("cut".to_owned()));
    }

    #[test]
    fn unused_writer_releases_resources_on_drop() {
        let resource = Arc::new(());
        let captured = resource.clone();
        let sink = EventSink::preparing(move |_: &mut EventStream<u8>| {
            drop(captured);
            Ok(Probe::default())
        });
        assert_eq!(Arc::strong_count(&resource), 2);
        drop(sink);
        assert_eq!(Arc::strong_count(&resource), 1);
    }

    #[tokio::test]
    async fn standard_sink_interoperability_and_terminal_source_fault() {
        use futures::SinkExt;
        let probe = Probe::default();
        let items = probe.items.clone();
        let mut sink = EventSink::new(probe);
        sink.send(Ok(3)).await.expect("send value");
        let fault = Fault::new("source");
        sink.send(Err(fault)).await.expect("send fault item");
        assert!(sink.send(Ok(4)).await.is_err());
        sink.close().await.expect("close delivery");
        assert_eq!(took(&items), [Ok(3), Err("source".to_owned())]);
        let mut ordinary = EventSink::new(futures::sink::unfold((), |(), _: Result<u8>| async {
            Ok::<_, Fault>(())
        }));
        ordinary.send(Ok(5)).await.expect("standard unfold sink");
        ordinary.close().await.expect("close");
        ordinary.close().await.expect("closed twice");
        ordinary.flush().await.expect("flush closed sink");
        assert!(ordinary.send(Ok(6)).await.is_err());
    }

    #[tokio::test]
    async fn pump_forwards_source_fault_but_returns_sink_fault() {
        let probe = Probe::default();
        let items = probe.items.clone();
        let source_fault = Fault::new("source");
        deliver(
            EventStream::refused(source_fault.clone()),
            EventSink::new(probe),
        )
        .await
        .expect("fault delivered");
        assert_eq!(took(&items), [Err("source".to_owned())]);
        let sink_fault = Fault::new("sink");
        let sink = futures::sink::unfold((), move |(), _: Result<u8>| {
            futures::future::ready(Err::<(), _>(sink_fault.clone()))
        });
        let delivered = deliver(EventStream::single(1), EventSink::new(sink)).await;
        assert_eq!(says(delivered), Err("sink".to_owned()));
    }

    #[tokio::test]
    async fn backpressure_prevents_source_reads_and_drop_cancels_both_halves() {
        let (ready, readiness) = oneshot::channel();
        let probe = Probe {
            ready: Some(async { readiness.await.map_err(|_| Fault::abandoned()) }.boxed()),
            ..Probe::default()
        };
        let polls = Arc::new(AtomicUsize::new(0));
        let count = polls.clone();
        let resource = Arc::new(());
        let captured = resource.clone();
        let source = EventStream::stream(stream::poll_fn(move |_| {
            let _held = &captured;
            count.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Some(Ok(1)))
        }));
        let mut pump = deliver(source, EventSink::new(probe));
        assert!(pump.as_mut().now_or_never().is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        drop(pump);
        assert_eq!(Arc::strong_count(&resource), 1);
        assert!(ready.send(()).is_err());
    }

    #[tokio::test]
    async fn pending_source_flushes_and_hot_source_yields() {
        let probe = Probe::default();
        let flushes = probe.flushes.clone();
        let mut quiet = deliver(
            EventStream::stream(stream::pending()),
            EventSink::new(probe),
        );
        assert!(quiet.as_mut().now_or_never().is_none());
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        let probe = Probe::default();
        let items = probe.items.clone();
        let mut hot = deliver(
            EventStream::stream(stream::repeat(Ok(1))),
            EventSink::new(probe),
        );
        assert!(hot.as_mut().now_or_never().is_none());
        assert_eq!(items.lock().len(), 128);
    }

    #[tokio::test]
    async fn stopped_and_empty_sources_close_without_waiting_for_readiness() {
        let mut stopped = EventStream::stream(stream::poll_fn(|_| -> Poll<Option<Result<u8>>> {
            panic!("stopped source was read");
        }));
        stopped.stop();
        for source in [stopped, EventStream::empty()] {
            let sink = EventSink::new(Probe {
                ready: Some(futures::future::pending().boxed()),
                ..Probe::default()
            });
            assert!(matches!(deliver(source, sink).now_or_never(), Some(Ok(()))));
        }
    }

    #[tokio::test]
    async fn stopped_source_is_retained_until_delivery_closes_or_is_cancelled() {
        for cancel in [false, true] {
            let held = Arc::new(());
            let captured = held.clone();
            let mut source =
                EventStream::stream(stream::poll_fn(move |_| -> Poll<Option<Result<u8>>> {
                    let _keep_alive = &captured;
                    panic!("stopped source was read");
                }));
            source.stop();
            let source = source.map_values(Ok, Clone::clone);
            let (finish, finished) = oneshot::channel();
            let sink = EventSink::new(Probe {
                close: Some(async { finished.await.map_err(|_| Fault::abandoned())? }.boxed()),
                ..Probe::default()
            });
            let mut pump = deliver(source, sink);
            assert!(pump.as_mut().now_or_never().is_none());
            assert_eq!(
                Arc::strong_count(&held),
                2,
                "close still owns the stopped source"
            );
            if cancel {
                drop(pump);
                assert!(finish.is_canceled());
            } else {
                finish.send(Ok(())).expect("complete");
                pump.await.expect("delivered");
            }
            assert_eq!(Arc::strong_count(&held), 1);
        }
    }

    #[tokio::test]
    async fn preparation_can_stop_a_lazy_source_without_decoding() {
        let mut source = EventStream::lazy(|| -> Result<u8> { panic!("stopped source decoded") });
        let mut sink = EventSink::preparing(|source: &mut EventStream<u8>| {
            source.stop();
            Ok(Probe::default())
        });
        sink.prepare(&mut source).expect("prepare raw");
        deliver(source, sink).await.expect("close raw");
    }

    /// Doubles each value, keeps the last back for the end, and notes that
    /// it ended.
    struct Held {
        last: Option<u8>,
        ended: Arc<AtomicUsize>,
    }

    impl Filter<u8> for Held {
        fn edit(&mut self, value: u8, out: &mut Vec<u8>) -> Result<()> {
            out.extend(self.last.replace(value * 2));
            Ok(())
        }

        fn end(&mut self, end: &Result<()>, out: &mut Vec<u8>) -> Result<()> {
            self.ended.fetch_add(1, Ordering::SeqCst);
            if end.is_ok() {
                out.extend(self.last.take());
            }
            Ok(())
        }
    }

    fn held() -> (Box<dyn Filter<u8>>, Arc<AtomicUsize>) {
        let ended = Arc::new(AtomicUsize::new(0));
        let filter = Held {
            last: None,
            ended: ended.clone(),
        };
        (Box::new(filter), ended)
    }

    #[tokio::test]
    async fn a_filtered_stream_edits_each_value_and_ends_once() {
        let (filter, ended) = held();
        let events = EventStream::stream(stream::iter([Ok(1_u8), Ok(2), Ok(3)]));
        let read: Vec<u8> = events.filtered(filter).try_collect().await.expect("read");
        assert_eq!(read, [2, 4, 6]);
        assert_eq!(ended.load(Ordering::SeqCst), 1);
        // A fault ends it: what it kept back is dropped.
        let (filter, ended) = held();
        let broken = stream::iter([Ok(1_u8), Err(Fault::truncated())]);
        let read: Vec<_> = EventStream::stream(broken).filtered(filter).collect().await;
        assert!(matches!(read.as_slice(), [Err(_)]), "{read:?}");
        assert_eq!(ended.load(Ordering::SeqCst), 1);
        // So does a drop before its end.
        let (filter, ended) = held();
        let mut events = EventStream::stream(stream::iter([Ok(1_u8), Ok(2)])).filtered(filter);
        assert_eq!(events.next().await.map(says), Some(Ok(2)));
        drop(events);
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_filtered_single_value_stays_single() {
        let double = |value: &mut u8| {
            *value *= 2;
            Ok(())
        };
        let events = EventStream::single(4_u8).filtered(Box::new(double));
        assert!(events.is_single());
        assert_eq!(events.value().map(says), Some(Ok(&8)));
        // A filter that keeps it back gives it at its end.
        let (filter, _) = held();
        let events = EventStream::single(4_u8).filtered(filter);
        assert_eq!(events.value().map(says), Some(Ok(&8)));
        // One that drops it gives none, which a single value cannot be.
        struct Drops;
        impl Filter<u8> for Drops {
            fn edit(&mut self, _: u8, _: &mut Vec<u8>) -> Result<()> {
                Ok(())
            }
        }
        let events = EventStream::single(4_u8).filtered(Box::new(Drops));
        assert!(events.value().is_some_and(|v| v.is_err()));
    }
}
