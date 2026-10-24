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

use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{
    self, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::set_path;
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
/// output `O`: what it takes, before the spec writes it, where it is
/// served, how its answer is read, and whether it streams. What it does not
/// say is the spec's.
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

    /// Where the vendor serves the call, relative to its base URL, if not
    /// where the spec does: the path the spec writes, whichever of its
    /// endpoints it is, is replaced by this.
    fn path(&self) -> Option<&'static str> {
        None
    }

    /// Where the vendor takes its key, if not where the spec does.
    fn key(&self) -> Option<KeyIn> {
        None
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
        Ok(bind_provider(
            io,
            S::NAME,
            move |head, input| {
                let encoder = spec.post(head, &*writes.input(input)?, stream)?;
                if let Some(path) = writes.path() {
                    set_path(head, path)?;
                }
                Ok(Box::new(Written {
                    encoder,
                    dialect: writes,
                    output: PhantomData,
                }) as Box<dyn Encoder<S::Input>>)
            },
            move |head, sent| {
                let own = dialect.read_response(head);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::OpenAiImages;
    use futures::SinkExt;
    use respan_gateway_core::{EventStream, Extensions};
    use respan_gateway_http::{Body, connect};
    use respan_gateway_media::image;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Same;

    impl Dialect<image::Input, image::Output> for Same {}

    #[derive(Clone)]
    struct Elsewhere;

    impl Dialect<image::Input, image::Output> for Elsewhere {
        fn path(&self) -> Option<&'static str> {
            Some("/pictures")
        }
    }

    /// The path of the call `format` makes of an image request.
    fn called<F>(format: F) -> Option<String>
    where
        F: Format<ProviderIo, image::Output, image::Input>,
    {
        let path = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&path);
        let send = move |request: http::Request<Body>| {
            *seen.lock().expect("lock") = Some(request.uri().path().to_owned());
            async { Ok((http::Response::new(Body::full("{}")), None)) }
        };
        let io = connect(send, 1024)(Extensions::new());
        let (incoming, mut outgoing) = format.bind(io).expect("bound");
        let mut input = EventStream::single(image::Input::new("a fox"));
        assert!(outgoing.prepare(&mut input).is_ok());
        _ = futures::executor::block_on(async { futures::join!(incoming, outgoing.close()) });
        path.lock().expect("lock").clone()
    }

    #[test]
    fn a_dialect_serves_a_call_at_its_path_else_the_specs() {
        let own = called(Dialected::new(OpenAiImages, Elsewhere));
        assert_eq!(own.as_deref(), Some("/pictures"));
        let spec = called(Dialected::new(OpenAiImages, Same));
        assert_eq!(spec.as_deref(), Some("/images/generations"));
    }
}
