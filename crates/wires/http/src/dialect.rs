//! A format as a vendor speaks it: the format, with the vendor's
//! [`Dialect`] of it laid over it ([`Dialected`]). The format is as its
//! spec has it and names no vendor; the dialect says where the vendor
//! departs from it, and nothing of who the client is. It is a provider
//! format of the spec's name, so a client of the spec is still relayed to it.
//!
//! ```ignore
//! formats: &[(&Dialected::new(OpenAiChat, DeepSeek), Some("https://api.deepseek.com"))]
//! ```
//!
//! A dialect works at a format's two edges, the format untouched between
//! them:
//!
//! ```text
//! client ─► inputs  ─► the format writes the call    ─► request  ─► vendor
//! client ◄─ outputs ◄─ the format reads the answer   ◄─ response ◄─ vendor
//! ```
//!
//! The kind's values on one side ([`Dialect::inputs`], [`Dialect::outputs`]),
//! as they stream; the call and its answer on the wire on the other
//! ([`Dialect::request`], [`Dialect::response`]), their heads, and a whole
//! body if it rewrites one.

use http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_core::{Connection, Fault, Filter, Format, Provider};

use crate::{Connect, KeyIn, ProviderIo, Reach, Rewrite, Upstream};

/// How a vendor departs from a format it speaks. What it does not say is
/// the format's.
pub trait Dialect: Clone + Send + Sync + 'static {
    /// The kind's input, which the format writes.
    type Input: Clone + Send + Sync + 'static;
    /// The kind's output, which the format reads.
    type Output: Clone + Send + Sync + 'static;

    /// What each input passes through before the format writes it, if
    /// anything: the vendor's spelling of what the spec has none for, or
    /// a refusal, before the call is sent, of what it takes none of.
    fn inputs(&self) -> Option<Box<dyn Filter<Self::Input>>> {
        None
    }

    /// What each output passes through after the format read it, if
    /// anything: what the vendor says beyond the spec, which the format
    /// keeps native, read as the kind's values.
    fn outputs(&self) -> Option<Box<dyn Filter<Self::Output>>> {
        None
    }

    /// The call as the format wrote it, before it is sent and before its
    /// transport puts the key on it: its head edited, and its whole body
    /// rewritten if this says how. `Err` fails it unsent.
    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        let _ = head;
        Ok(None)
    }

    /// The answer to the call `sent`, as the format wrote it, before the
    /// format reads it: its head edited, and its whole body rewritten if
    /// this says how. Every answer, a refusal too.
    fn response(&self, sent: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        let _ = (sent, head);
        None
    }

    /// Where the vendor takes its key, if not where the format does.
    fn key(&self) -> Option<KeyIn> {
        None
    }
}

/// Format `F` as a vendor speaks it, in its dialect `D`: a provider format
/// of `F`'s name.
#[derive(Debug, Clone, Copy)]
pub struct Dialected<F, D> {
    format: F,
    dialect: D,
}

impl<F, D> Dialected<F, D> {
    /// `format` in `dialect`.
    pub const fn new(format: F, dialect: D) -> Self {
        Self { format, dialect }
    }
}

/// The format's, but the key, if the dialect takes it elsewhere.
impl<F, D> Upstream for Dialected<F, D>
where
    F: Upstream + Format<ProviderIo, D::Output, D::Input> + Clone,
    D: Dialect,
{
    fn name(&self) -> &str {
        Upstream::name(&self.format)
    }

    fn key(&self) -> KeyIn {
        self.dialect.key().unwrap_or_else(|| self.format.key())
    }

    fn reach(&self) -> Reach {
        self.format.reach()
    }

    fn at_origin(&self) -> bool {
        self.format.at_origin()
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, self.clone(), move |x| connect(x))
    }
}

impl<F, D> Format<ProviderIo, D::Output, D::Input> for Dialected<F, D>
where
    F: Format<ProviderIo, D::Output, D::Input>,
    D: Dialect,
{
    fn name(&self) -> &str {
        Format::name(&self.format)
    }

    /// The format's, the call and its answer through the dialect on the
    /// wire, and its inputs and outputs through it as values.
    fn bind(&self, io: ProviderIo) -> Result<Connection<D::Output, D::Input>, Fault> {
        let (request, response) = (self.dialect.clone(), self.dialect.clone());
        let io = io
            .with_request(move |head| request.request(head))
            .with_response(move |sent, head| response.response(sent, head));
        let (opening, sink) = self.format.bind(io)?;
        let sink = match self.dialect.inputs() {
            Some(filter) => sink.filtered(filter),
            None => sink,
        };
        let opening = match self.dialect.outputs() {
            Some(filter) => Box::pin(async move { Ok(opening.await?.filtered(filter)) }),
            None => opening,
        };
        Ok((opening, sink))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use http::Method;
    use respan_gateway_core::{EventStream, Extensions};

    use super::*;
    use crate::{Body, Decoder, Whole, connect};

    /// A format that posts its input's text to `/echo`, and reads the
    /// answer's body whole.
    #[derive(Debug, Clone, Copy)]
    struct Echo;

    impl Format<ProviderIo, Bytes, String> for Echo {
        fn name(&self) -> &str {
            "echo"
        }

        fn bind(&self, io: ProviderIo) -> Result<Connection<Bytes, String>, Fault> {
            Ok(io.bind(
                |head, _| {
                    head.method = Method::POST;
                    head.uri = http::Uri::from_static("/echo");
                    Ok(Box::new(|text: &String, out: &mut Vec<Bytes>| {
                        out.push(Bytes::from(text.clone()));
                        Ok(())
                    }))
                },
                |_, _| {
                    let read = Whole::new(|body: Bytes, out: &mut Vec<Bytes>| {
                        out.push(body);
                        Ok(())
                    });
                    Some(Box::new(read) as Box<dyn Decoder<Bytes>>)
                },
            ))
        }
    }

    impl Upstream for Echo {
        fn name(&self) -> &str {
            "echo"
        }

        fn provider(&self, name: String, connect: Connect) -> Provider {
            Provider::new(name, *self, move |x| connect(x))
        }
    }

    /// A vendor that departs from it at every edge.
    #[derive(Debug, Clone, Copy)]
    struct Loud;

    /// Ends what it passes with `+`.
    struct Ended;

    impl Filter<Bytes> for Ended {
        fn edit(&mut self, value: Bytes, out: &mut Vec<Bytes>) -> Result<(), Fault> {
            out.push(value);
            Ok(())
        }

        fn end(&mut self, _: &Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
            out.push(Bytes::from_static(b"+"));
            Ok(())
        }
    }

    impl Dialect for Loud {
        type Input = String;
        type Output = Bytes;

        fn inputs(&self) -> Option<Box<dyn Filter<String>>> {
            Some(Box::new(|text: &mut String| {
                *text = text.to_uppercase();
                Ok(())
            }))
        }

        fn outputs(&self) -> Option<Box<dyn Filter<Bytes>>> {
            Some(Box::new(Ended))
        }

        fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
            head.uri = http::Uri::from_static("/loud");
            Ok(Some(Box::new(|body: Bytes| {
                Ok([&body[..], b"!"].concat().into())
            })))
        }

        fn response(&self, sent: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
            // The call as the format wrote it.
            assert_eq!(sent.uri.path(), "/echo");
            head.headers
                .insert("x-loud", http::HeaderValue::from_static("1"));
            Some(Box::new(|body: Bytes| {
                Ok([b"<", &body[..]].concat().into())
            }))
        }
    }

    #[tokio::test]
    async fn a_dialect_departs_at_each_edge_of_its_format() {
        let called = Arc::new(Mutex::new(None));
        let seen = called.clone();
        let send = move |call: http::Request<Body>| {
            let seen = seen.clone();
            async move {
                let path = call.uri().path().to_owned();
                let body = call.body().full_bytes().cloned().unwrap_or_default();
                *seen.lock().expect("lock") = Some((path, body));
                let answer = http::Response::builder().body(Body::full("answer"));
                Ok((answer.unwrap_or_default(), None))
            }
        };
        let io = connect(send, 64)(Extensions::new());
        let (incoming, mut outgoing) = Dialected::new(Echo, Loud).bind(io).expect("bound");
        let mut input = EventStream::single("hi".to_owned());
        outgoing.prepare(&mut input).expect("prepared");
        let (answer, _) = futures::join!(incoming, outgoing.close());
        let read: Vec<_> = answer.expect("an answer").collect().await;
        let read: Vec<_> = read.into_iter().map(|b| b.expect("read")).collect();
        assert_eq!(
            read,
            [Bytes::from_static(b"<answer"), Bytes::from_static(b"+")]
        );
        let called = called.lock().expect("lock").clone();
        assert_eq!(
            called,
            Some(("/loud".to_owned(), Bytes::from_static(b"HI!")))
        );
    }

    #[test]
    fn a_dialected_format_is_the_formats_but_its_key() {
        #[derive(Debug, Clone, Copy)]
        struct Keyed;
        impl Dialect for Keyed {
            type Input = String;
            type Output = Bytes;

            fn key(&self) -> Option<KeyIn> {
                Some(KeyIn::Header("api-key"))
            }
        }
        let format = Dialected::new(Echo, Keyed);
        assert_eq!(Upstream::name(&format), "echo");
        assert_eq!(format.key(), KeyIn::Header("api-key"));
        assert_eq!(Dialected::new(Echo, Loud).key(), KeyIn::Bearer);
    }
}
