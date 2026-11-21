use parking_lot::Mutex;
use std::any::Any;
use std::future::ready;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::{Stream, StreamExt, stream};
use tokio::runtime::Handle;

use crate::call::OpenCalls;
use crate::format::{AnyFormat, is_input};
use crate::gateway::Registry;
use crate::input::Input;
use crate::layers::Layer;
use crate::lifecycle::{Edge, InFlight, Latch};
use crate::live_inputs::{Later, inputs};
use crate::plugin::Filters;
use crate::values::{AnyBatches, AnyEvents, Value, erase, take_value, typed};
use crate::{Call, EventStream, Extensions, Fault, Filter, Gateway, Plugin, Result};

/// One client request, as the client edge's hooks see it.
///
/// Calling and reading inputs take `&mut ClientContext`, which `on_client`
/// alone gets: end hooks and a call's hooks see the request by `&`.
///
/// ```
/// # use respan_gateway_core::ClientContext;
/// fn on_client(cx: &mut ClientContext) {
///     let _ = (cx.call("p"), cx.take_inputs::<String>());
/// }
/// ```
///
/// ```compile_fail
/// # use respan_gateway_core::ClientContext;
/// fn on_client_end(cx: &ClientContext) {
///     let _ = cx.call("p");
/// }
/// ```
///
/// ```compile_fail
/// # use respan_gateway_core::ProviderContext;
/// fn on_provider(px: &mut ProviderContext) {
///     let _ = px.client().take_inputs::<String>();
/// }
/// ```
pub struct ClientContext {
    /// The first client format that accepted the request.
    pub(crate) format: Arc<dyn AnyFormat>,
    pub(crate) extensions: Extensions,
    /// The provider called if every plugin passes.
    pub(crate) target: Option<String>,
    /// The opening value; none for a streaming flow.
    pub(crate) input: Input,
    /// The inputs that follow it.
    pub(crate) later: Arc<Later>,
    /// Closed once the request ends; copies share it.
    pub(crate) ended: Latch,
    /// The runtime end hooks run on.
    pub(crate) runtime: Handle,
    /// Its calls that ended, until their end hooks finish; its own end
    /// hooks wait for them.
    pub(crate) settling: Arc<InFlight>,
    /// Its calls being sent or with an open response, ended when it ends.
    pub(crate) open: Arc<OpenCalls>,
    /// The stages of the response sent to the client; only the client edge
    /// has them, never a call's copy.
    pub(crate) stages: Option<Mutex<Stages>>,
    pub(crate) gateway: Arc<Registry>,
}

/// The stages of the response sent to the client, which end with it, or
/// with the request if it ends first.
#[derive(Default)]
pub(crate) struct Stages {
    /// Filters added since the last [`wrap`](ClientContext::wrap), which
    /// run first.
    pub(crate) latest: Layer,
    /// Each wrap, in the order added, with the filters added before it,
    /// which run after it; taken when sent.
    pub(crate) wrapped: Vec<(Option<Wrap>, Layer)>,
}

/// A plugin's wrap of the outputs sent to the client, erased.
pub(crate) type Wrap = Box<dyn FnOnce(AnyBatches) -> AnyEvents + Send>;

impl std::fmt::Debug for ClientContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientContext")
            .field("format", &self.format.name())
            .finish_non_exhaustive()
    }
}

impl Edge for ClientContext {
    fn client(&self) -> &ClientContext {
        self
    }

    fn plugins(&self) -> impl Iterator<Item = &Arc<dyn Plugin>> + Send {
        self.gateway.plugins.iter()
    }

    fn hook<'a>(&'a self, plugin: &'a dyn Plugin, end: &'a Result<()>) -> BoxFuture<'a, ()> {
        plugin.on_client_end(self, end)
    }

    /// The inputs left are cut off, first, so a plugin's stream that ends
    /// as the one it wraps is cut ends after the cut; open calls end
    /// abandoned; output filters still open end as the request did.
    fn close(&mut self, end: &Result<()>) {
        self.later.close(Err(Fault::abandoned()));
        self.ended.close();
        self.open.abandon();
        let Some(stages) = &self.stages else { return };
        let stages = stages.lock();
        let wrapped = stages.wrapped.iter().rev().map(|w| &w.1);
        for layer in std::iter::once(&stages.latest).chain(wrapped) {
            let _ = layer.lock().end(end.clone(), &mut None);
        }
    }
}

impl ClientContext {
    pub(crate) fn new(
        format: Arc<dyn AnyFormat>,
        mut incoming: EventStream<Value>,
        extensions: Extensions,
        gateway: Arc<Registry>,
    ) -> Self {
        let single = incoming.take_single();
        let later = match single.is_some() || incoming.refusal().is_some() {
            true => Later::new(None),
            false => Later::new(Some(incoming.boxed())),
        };
        Self {
            input: Input::new(format.clone(), single),
            format,
            extensions,
            target: gateway.target.clone(),
            later: Arc::new(later),
            ended: Latch::default(),
            runtime: Handle::current(),
            settling: Arc::default(),
            open: Arc::default(),
            stages: Some(Mutex::default()),
            gateway,
        }
    }

    /// The stages of the response sent to the client, which only the client
    /// edge has.
    pub(crate) fn stages(&mut self) -> Result<&mut Stages> {
        let stages = self.stages.as_mut().ok_or_else(|| {
            Fault::internal("a call's copy of the request has no response stages")
        })?;
        Ok(stages.get_mut())
    }

    /// The name of the client's format.
    pub fn format(&self) -> &str {
        self.format.name()
    }

    /// The request's extensions, from the adapter, such as the client's
    /// credential, and earlier plugins; on a provider edge, the call's.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Like [`ClientContext::extensions`], to change; calls made later copy
    /// them.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    /// Whether `T` is the client's format's input type
    /// or output type, which the typed accessors take.
    pub fn is_kind<T: Any>(&self) -> bool {
        is_input::<T>(&*self.format).is_ok()
    }

    /// The request's one input as `T`, as filters left it. `Err` with
    /// [`internal`](crate::Fault::internal) for another type or a
    /// streaming request; or the decoder's or a filter's `Err`. A plugin
    /// that vets input must refuse on `Err`.
    pub fn input<T: Any>(&self) -> Result<&T> {
        self.input.get()
    }

    /// The request's inputs as `T`, as filters and wraps left them: a copy
    /// of a whole request's one; a streaming request's as they arrive, read
    /// once, so no call gets them, taken when first awaited and given back
    /// if none was read. Ends with the client's inputs, `Err` if the body
    /// broke, and [`Abandoned`](crate::FaultKind::Abandoned) if the
    /// request ended first, never as if they were complete; one `Err` with
    /// [`internal`](crate::Fault::internal) for another type or if
    /// another reader took or read them.
    pub fn take_inputs<T: Any + Clone + Send>(
        &mut self,
    ) -> futures::stream::BoxStream<'static, Result<T>> {
        if !matches!(is_input::<T>(&*self.format), Ok(true)) {
            return stream::once(async { Err(Fault::wrong_kind::<T>()) }).boxed();
        }
        if !self.input.is_live() {
            return stream::once(ready(self.input.get::<T>().cloned())).boxed();
        }
        let items = inputs(self.later.clone(), None, &self.ended);
        items.map(|item| item.and_then(take_value::<T>)).boxed()
    }

    /// The provider called if every plugin passes; initially the gateway's
    /// ([`Gateway::target`](crate::Gateway::target)).
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// Call `provider` if every plugin passes, so later plugins still run.
    pub fn set_target(&mut self, provider: impl Into<String>) {
        self.target = Some(provider.into());
    }

    /// The gateway this request came to, such as for a job that outlives
    /// the request to hand it requests of its own; [`Gateway::idle`] waits
    /// for each from the moment [`Gateway::handle`] is called, so call it
    /// here and await its future in the job's task. Move the gateway into
    /// that job, not into the plugin: the gateway holds its plugins, so a
    /// plugin that keeps it frees neither.
    ///
    /// A request handed back this way is a new one: it inherits none of
    /// this request's extensions, and every plugin runs on it again, with
    /// no limit on how deep requests nest. So a plugin that hands one back
    /// marks it with an extension of its own and passes a request it marked,
    /// or it may loop.
    pub fn gateway(&self) -> Gateway {
        Gateway {
            inner: self.gateway.clone(),
        }
    }

    /// A copy of it as it is now, for a call to keep: without the stages
    /// of the response sent to the client.
    pub(crate) fn copy(&self) -> Self {
        Self {
            format: self.format.clone(),
            extensions: self.extensions.clone(),
            target: self.target.clone(),
            input: self.input.clone(),
            later: self.later.clone(),
            ended: self.ended.clone(),
            runtime: self.runtime.clone(),
            settling: self.settling.clone(),
            open: self.open.clone(),
            stages: None,
            gateway: self.gateway.clone(),
        }
    }

    /// Run `filter` over the request's inputs if `T` is the client's
    /// input type, or the outputs sent to it if its
    /// output type.
    ///
    /// Input filters run the first added first, before calls and
    /// [`ClientContext::take_inputs`]. A whole request's one input is edited
    /// at once and the filter ended; it stays one input. A streaming
    /// request's are edited as read; once they ended, a filter sees their
    /// end at once. Output filters run after the provider edge's,
    /// the last added first.
    ///
    /// `Err` with [`internal`](crate::Fault::internal) for another
    /// type or a whole input filtered to other than one; or the filter's
    /// `Err`, which reading or sending the input then gets too; or the
    /// failure the inputs ended with.
    pub fn filter<T: Clone + Send + Sync + 'static>(
        &mut self,
        filter: impl Filter<T>,
    ) -> Result<()> {
        match is_input::<T>(&*self.format)? {
            false => (self.stages()?.latest).lock().add(filter, false),
            true if !self.input.is_live() => self.input.filter::<T>(Filters::of(filter)),
            true => self.later.filters.lock().add(filter, true),
        }
    }

    /// Wrap the request's inputs if `T` is the client's
    /// input type, or the outputs sent to it if its
    /// output type, in a stream of `wrap`'s: for work
    /// a [filter](ClientContext::filter) cannot do, as it waits, such as a
    /// remote check of each input or a deadline that cuts the outputs.
    ///
    /// ```
    /// # use respan_gateway_core::futures::{StreamExt, TryStreamExt};
    /// # use respan_gateway_core::{ClientContext, EventStream, Result};
    /// # use tokio::time::{Duration, sleep};
    /// # #[derive(Clone)]
    /// # struct Turn;
    /// # #[derive(Clone)]
    /// # struct Event;
    /// /// A remote check of one input, such as a classifier's call.
    /// async fn check(turn: Turn) -> Result<Turn> {
    ///     Ok(turn)
    /// }
    ///
    /// fn on_client(cx: &mut ClientContext, limit: Duration) -> Result<()> {
    ///     cx.wrap(|turns: EventStream<Turn>| turns.and_then(check))?;
    ///     cx.wrap(move |events: EventStream<Event>| events.take_until(sleep(limit)))
    /// }
    /// ```
    ///
    /// `take_until` ends the stream cleanly, so the call ends `abandoned`, as
    /// if the client had left. To end it with a reason, cut with a fault:
    /// a last item `Err(Fault::new(message))`, as `MaxLength` in
    /// `examples/recipes/realtime.rs` does.
    ///
    /// The stream takes the place of the one it wraps.
    /// Wraps and filters apply in plugin
    /// order: a wrap sees inputs after the filters and wraps added before it,
    /// outputs before them. Its stream is pulled as its reader reads; the
    /// request's end cuts what it wraps and what it gives with
    /// [`Abandoned`](crate::FaultKind::Abandoned), never a clean end.
    /// Inputs that fail end the call that reads them with that fault, so
    /// the client edge can say it. A call made before the wrap keeps the
    /// inputs as they were.
    ///
    /// `Err` with [`internal`](crate::Fault::internal) for another
    /// type, or for the inputs of a whole request, whose one input came with
    /// its head: vet it here and edit it with a filter.
    pub fn wrap<T, S>(
        &mut self,
        wrap: impl FnOnce(EventStream<T>) -> S + Send + 'static,
    ) -> Result<()>
    where
        T: Any + Clone + Send + Sync,
        S: Stream<Item = Result<T>> + Send + 'static,
    {
        if !is_input::<T>(&*self.format)? {
            let wrap: Wrap =
                Box::new(|outputs| erase(wrap(EventStream::stream(typed(outputs))).boxed()));
            let stages = self.stages()?;
            let before = std::mem::take(&mut stages.latest);
            stages.wrapped.push((Some(wrap), before));
            return Ok(());
        }
        if !self.input.is_live() {
            let whole = "a whole request's one input came with its head: edit it with a filter";
            return Err(Fault::internal(whole));
        }
        let inputs = erase(wrap(EventStream::stream(self.take_inputs::<T>())).boxed());
        self.later = Arc::new(Later::wrapping(inputs, self.later.clone()));
        Ok(())
    }

    /// A call to `provider`, the one way to reach one: a copy of the request
    /// as it is now, sent when first awaited, carrying its inputs as filters
    /// and wraps left them, translated for another format of the kind.
    /// Every plugin's [`on_provider`](crate::Plugin::on_provider) runs
    /// first. A provider's
    /// error answer is `Ok` here and fails the response as read.
    ///
    /// `Err` with [`config`](crate::Fault::config) if `provider` is
    /// unknown or cannot take the inputs; with
    /// [`internal`](crate::Fault::internal) for a
    /// [`Call::with_input`] of another kind or once the request ended; with
    /// [`Abandoned`](crate::FaultKind::Abandoned) if the request ends
    /// while it is pending; or the `Err` of a hook, the codec or the
    /// transport.
    pub fn call(&mut self, provider: impl Into<String>) -> Call {
        Call::new(Arc::new(self.copy()), provider.into())
    }
}
