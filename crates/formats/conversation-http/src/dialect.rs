//! A format as a vendor speaks it: a spec's [`Schema`] and the vendor's
//! [`Dialect`] of it, what it writes and sends beyond the spec turned into
//! the kind's events and back. It speaks as the spec, by its name, so a
//! client of the spec is still relayed to it; the format names no vendor,
//! and the vendor writes its dialect.
//!
//! ```ignore
//! formats: &[(&Dialected::new(OpenAiChat, DeepSeek), Some("https://api.deepseek.com"))]
//! ```

use std::borrow::Cow;
use std::sync::Arc;

use respan_gateway_conversation::{Input, Output};
use respan_gateway_core::{Connection, Fault, Filter, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::raw::Original;
use respan_gateway_http::{Connect, Decoder, Encoder, KeyIn, ProviderIo, Reach, Upstream};

use serde_json::{Map, Value};

use crate::Schema;
use crate::schema::{StreamResponse, answer, own_request, post};

/// How a vendor departs from a spec it speaks, for one call ([`Call`]):
/// what it sends, before the spec writes it, and what it answers, as the
/// spec reads it. What it does not say is the spec's.
pub trait Dialect: Send + Sync + 'static {
    /// The input of the call as the vendor takes it, before the spec
    /// writes it: `input` itself, unless the vendor takes it otherwise.
    fn input<'a>(&self, input: &'a Input, call: &Call) -> Result<Cow<'a, Input>, Fault> {
        let _ = call;
        Ok(Cow::Borrowed(input))
    }

    /// What the events of the call's answer, as the spec reads them, pass
    /// through, if anything: the vendor's fields beyond the spec, which the
    /// spec keeps native, read as the kind's events.
    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        let _ = call;
        None
    }

    /// Where the vendor takes its key, if not where the spec does.
    fn key(&self) -> Option<KeyIn> {
        None
    }

    /// The path and query of the call for `input`, as the vendor takes it,
    /// streamed if `stream`, if the vendor puts it elsewhere than the spec;
    /// `None` for the spec's ([`Schema::target`]).
    fn target(&self, input: &Input, stream: bool) -> Result<Option<String>, Fault> {
        let _ = (input, stream);
        Ok(None)
    }

    /// The call's JSON `body` as the vendor takes it, after the spec wrote
    /// it: as it is, unless the vendor names a field otherwise.
    fn body(&self, body: &mut Map<String, Value>) {
        let _ = body;
    }
}

/// One call of a [`Dialect`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Call {
    /// Whether its client speaks another format than the spec, so what is
    /// native to the spec cannot reach it.
    pub translated: bool,
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
    S: Schema + Upstream,
    D: Dialect + Clone,
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

impl<S, D> Format<ProviderIo, Output, Input> for Dialected<S, D>
where
    S: Schema,
    D: Dialect + Clone,
{
    fn name(&self) -> &str {
        S::NAME
    }

    /// As [`Schema::bind_provider`], the input through the dialect before
    /// it is written and the answer's events after they are read.
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let (spec, dialect) = (self.spec, self.dialect.clone());
        let stream = io.extensions().get::<StreamResponse>().copied();
        let original = io.extensions().get::<Original>().cloned();
        let call = Call {
            translated: own_request::<S>(original.as_ref()).is_none(),
        };
        let writes = dialect.clone();
        let io = io.with_refusal(move |refused| answer(&spec, refused));
        Ok(io.bind(
            move |head, input| {
                // Its path, from the input as the vendor takes it, and its body.
                let taken = input.map(|i| writes.input(i, &call)).transpose()?;
                let departs: Arc<dyn Dialect> = Arc::new(writes.clone());
                let encoder = post(
                    spec,
                    head,
                    taken.as_deref(),
                    original.as_ref(),
                    stream,
                    Some(departs),
                )?;
                Ok(Box::new(Written {
                    encoder,
                    dialect: writes,
                    call,
                }) as Box<dyn Encoder<Input>>)
            },
            move |head, sent: &respan_gateway_http::http::Request<Bytes>| {
                let decoder = spec.read_response(head, sent)?;
                Some(match dialect.output(&call) {
                    Some(filter) => Box::new(Filtered { decoder, filter }),
                    None => decoder,
                })
            },
        ))
    }
}

/// An encoder of the input as a dialect takes it.
struct Written<D> {
    encoder: Box<dyn Encoder<Input>>,
    dialect: D,
    call: Call,
}

impl<D: Dialect> Encoder<Input> for Written<D> {
    fn encode(&mut self, input: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let input = self.dialect.input(input, &self.call)?;
        self.encoder.encode(&input, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.encoder.end(end, out)
    }
}

/// A decoder's events through a filter.
struct Filtered {
    decoder: Box<dyn Decoder<Output>>,
    filter: Box<dyn Filter<Output>>,
}

impl Filtered {
    fn pass(&mut self, read: Vec<Output>, out: &mut Vec<Output>) -> Result<(), Fault> {
        read.into_iter().try_for_each(|e| self.filter.edit(e, out))
    }
}

impl Decoder<Output> for Filtered {
    fn start(&mut self, out: &mut Vec<Output>) -> Result<(), Fault> {
        let mut read = Vec::new();
        let started = self.decoder.start(&mut read);
        self.pass(read, out)?;
        started
    }

    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let mut read = Vec::new();
        let decoded = self.decoder.decode(raw, &mut read);
        self.pass(read, out)?;
        decoded
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        let mut read = Vec::new();
        let end = self.decoder.end(end, &mut read);
        let end = end.and_then(|()| self.pass(read, out));
        self.filter.end(&end, out)?;
        end
    }

    fn limit(&mut self, bytes: Option<usize>) {
        self.decoder.limit(bytes);
    }

    fn done(&self) -> bool {
        self.decoder.done()
    }
}
