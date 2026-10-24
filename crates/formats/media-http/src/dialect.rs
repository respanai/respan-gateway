//! A media format as a vendor speaks it: a spec's [`Spec`] and the vendor's
//! [`Dialect`] of it, what it takes and answers beyond the spec turned into
//! the kind's values and back. It speaks as the spec, by its name, so a
//! client of the spec is still relayed to it; the format names no vendor,
//! and the vendor writes its dialect.
//!
//! ```ignore
//! formats: &[(&Dialected::new(OpenAiSpeech, Mistral), Some("https://api.mistral.ai/v1"))]
//! ```

use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{
    self, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{Connect, Decoder, Encoder, KeyIn, ProviderIo, Reach, Upstream};

use crate::edge::bind_provider;

/// A media format's provider edge, in parts a [`Dialected`] runs around:
/// whether a call streams, how it is written, and how its answer is read.
pub trait Spec: Copy + Send + Sync + 'static {
    /// The kind's input.
    type Input: Clone + Send + Sync + 'static;
    /// The kind's output.
    type Output: Clone + Send + Sync + 'static;
    /// The format's name, which tags its native fields.
    const NAME: &'static str;

    /// Whether the provider is asked to stream the call.
    fn streams(&self, io: &ProviderIo) -> bool {
        let _ = io;
        false
    }

    /// Write the call of `input` into `head`, streamed if `stream`, and
    /// return the encoder of its body.
    fn post(
        &self,
        head: &mut RequestHead,
        input: &Self::Input,
        stream: bool,
    ) -> Result<Box<dyn Encoder<Self::Input>>, Fault>;

    /// The decoder of an answer of `head` to the call `sent`, if it has a
    /// body to read.
    fn read_response(
        &self,
        head: &ResponseHead,
        sent: &http::Request<Bytes>,
    ) -> Option<Box<dyn Decoder<Self::Output>>>;
}

/// The provider edge of `spec`, as it is.
pub(crate) fn bind<S: Spec>(spec: S, io: ProviderIo) -> Connection<S::Output, S::Input> {
    let stream = spec.streams(&io);
    bind_provider(
        io,
        S::NAME,
        move |head, input| spec.post(head, input, stream),
        move |head, sent| spec.read_response(head, sent),
    )
}

/// How a vendor departs from a media spec it speaks, of input `I` and
/// output `O`: what it takes, before the spec writes it, how its answer is
/// read, and whether it streams. What it does not say is
/// the spec's.
pub trait Dialect<I, O>: Clone + Send + Sync + 'static {
    /// The input as the vendor takes it, before the spec writes it: `input`
    /// itself, unless the vendor takes it otherwise; `Err` refuses it
    /// before it is sent.
    fn input<'a>(&self, input: &'a I) -> Result<Cow<'a, I>, Fault>
    where
        I: Clone,
    {
        Ok(Cow::Borrowed(input))
    }

    /// The decoder of an answer of `head`, if the vendor answers it
    /// otherwise than the spec: its own, which holds what it reads within
    /// the body's limit. `None` reads it as the spec does.
    fn read_response(&self, head: &ResponseHead) -> Option<Box<dyn Decoder<O>>> {
        let _ = head;
        None
    }

    /// Whether the vendor is asked to stream, given whether the spec would
    /// ask: as the spec would, unless it streams otherwise or not at all.
    fn streams(&self, asked: bool) -> bool {
        asked
    }

    /// Where the vendor takes its key, if not where the spec does.
    fn key(&self) -> Option<KeyIn> {
        None
    }

    /// The call of `input` (as [`input`](Self::input) gave it) as the
    /// vendor takes it, if it is none of the spec's: its head written into
    /// `head`, a `POST` relative to the base URL, the encoder of its body
    /// and the decoder of its answer, which knows what was asked. `None`
    /// lets the spec write it.
    fn call(&self, head: &mut RequestHead, input: &I) -> Option<Result<OwnCall<I, O>, Fault>> {
        let _ = (head, input);
        None
    }
}

/// The decoder of the answer to a call a dialect wrote, once written.
type Answer<O> = Arc<Mutex<Option<Box<dyn Decoder<O>>>>>;

/// A call a vendor writes its own way ([`Dialect::call`]).
pub struct OwnCall<I, O> {
    /// The encoder of its body.
    pub body: Box<dyn Encoder<I>>,
    /// The decoder of a success's body.
    pub answer: Box<dyn Decoder<O>>,
}

impl<I, O> std::fmt::Debug for OwnCall<I, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnCall").finish_non_exhaustive()
    }
}

/// Spec `S` as a vendor speaks it, in its dialect `D`: a provider format of
/// the spec's name.
#[derive(Debug, Clone, Copy)]
pub struct Dialected<S, D> {
    spec: S,
    dialect: D,
}

impl<S, D> Dialected<S, D> {
    /// `spec` in `dialect`.
    pub const fn new(spec: S, dialect: D) -> Self {
        Self { spec, dialect }
    }
}

/// The spec's, but the key, if the dialect takes it elsewhere.
impl<S, D> Upstream for Dialected<S, D>
where
    S: Spec + Upstream,
    D: Dialect<S::Input, S::Output>,
{
    fn name(&self) -> &str {
        S::NAME
    }

    fn key(&self) -> KeyIn {
        self.dialect.key().unwrap_or_else(|| self.spec.key())
    }

    fn reach(&self) -> Reach {
        self.spec.reach()
    }

    fn at_origin(&self) -> bool {
        self.spec.at_origin()
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, self.clone(), move |x| connect(x))
    }
}

impl<S, D> Format<ProviderIo, S::Output, S::Input> for Dialected<S, D>
where
    S: Spec,
    D: Dialect<S::Input, S::Output>,
{
    fn name(&self) -> &str {
        S::NAME
    }

    /// As the spec's, the input through the dialect before it is written,
    /// and the answer read as the dialect reads it, if it does.
    fn bind(&self, io: ProviderIo) -> Result<Connection<S::Output, S::Input>, Fault> {
        let (spec, dialect) = (self.spec, self.dialect.clone());
        let stream = dialect.streams(spec.streams(&io));
        let writes = dialect.clone();
        let own: Answer<S::Output> = Arc::default();
        let reads = Arc::clone(&own);
        Ok(bind_provider(
            io,
            S::NAME,
            move |head, input| {
                let input = writes.input(input)?;
                if let Some(call) = writes.call(head, &input) {
                    let OwnCall { body, answer } = call?;
                    *own.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
                    return Ok(body);
                }
                let encoder = spec.post(head, &input, stream)?;
                Ok(Box::new(Written {
                    encoder,
                    dialect: writes,
                    output: PhantomData,
                }) as Box<dyn Encoder<S::Input>>)
            },
            move |head, sent| {
                let called = reads.lock().unwrap_or_else(|e| e.into_inner()).take();
                let own = called.or_else(|| dialect.read_response(head));
                own.or_else(|| spec.read_response(head, sent))
            },
        ))
    }
}

/// An encoder of the input as a dialect takes it.
struct Written<I, O, D> {
    encoder: Box<dyn Encoder<I>>,
    dialect: D,
    output: PhantomData<fn() -> O>,
}

impl<I, O, D> Encoder<I> for Written<I, O, D>
where
    I: Clone + Send + 'static,
    O: 'static,
    D: Dialect<I, O>,
{
    fn encode(&mut self, input: &I, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let input = self.dialect.input(input)?;
        self.encoder.encode(&input, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.encoder.end(end, out)
    }
}
