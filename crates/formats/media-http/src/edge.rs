//! What the HTTP formats of this crate share, OpenAI's and Veo's.

use respan_gateway_conversation_http::read_provider_error;
use respan_gateway_core::{Connection, Fault};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::http::{
    self, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{Decoder, Encoder, ProviderIo, Sse};
use respan_gateway_media::video;

/// The provider edge of `format`: a call whose head `encode` sets (a `POST`
/// to start with) and whose body it returns, its success read by the
/// decoder `decode` gives, given its head and the call as sent, its
/// failure leniently. A format takes one value per call: live input is
/// refused.
pub(crate) fn bind_provider<I, O>(
    io: ProviderIo,
    format: &'static str,
    encode: impl FnOnce(&mut RequestHead, &I) -> Result<Box<dyn Encoder<I>>, Fault> + Send + 'static,
    decode: impl FnOnce(&ResponseHead, &http::Request<Bytes>) -> Option<Box<dyn Decoder<O>>>
    + Send
    + 'static,
) -> Connection<O, I>
where
    I: Clone + Send + Sync + 'static,
    O: Clone + Send + Sync + 'static,
{
    io.with_refusal(read_provider_error).bind(
        move |head, input: Option<&I>| {
            let input = input.ok_or_else(|| {
                Fault::config(format!("{format} does not accept streaming inputs"))
            })?;
            head.method = http::Method::POST;
            encode(head, input)
        },
        decode,
    )
}

/// The encoder of a call whose body is `body`.
pub(crate) fn sends<I: 'static>(body: Bytes) -> Box<dyn Encoder<I>> {
    Box::new(move |_: &I, out: &mut Vec<Bytes>| {
        out.push(body.clone());
        Ok(())
    })
}

/// The content of a video, chunk by chunk as it comes.
pub(crate) fn content() -> Box<dyn Decoder<video::Output>> {
    Box::new(|bytes: Bytes, out: &mut Vec<video::Output>| {
        out.push(video::Output::Content(bytes));
        Ok(())
    })
}

/// A stream's events, each read by `event`, which says once one said its
/// end: a stream cut before it is `truncated`.
pub(crate) fn ended_events<O, F>(event: F) -> Box<dyn Decoder<O>>
where
    O: Send + 'static,
    F: FnMut(&mut bool, &[u8], &mut Vec<O>) -> Result<(), Fault> + Send + 'static,
{
    Box::new(Sse::new(Ending { done: false, event }))
}

/// The events [`ended_events`] reads, and whether one said its end.
struct Ending<F> {
    done: bool,
    event: F,
}

impl<O, F> Decoder<O> for Ending<F>
where
    O: Send + 'static,
    F: FnMut(&mut bool, &[u8], &mut Vec<O>) -> Result<(), Fault> + Send + 'static,
{
    fn decode(&mut self, raw: Bytes, out: &mut Vec<O>) -> Result<(), Fault> {
        (self.event)(&mut self.done, &raw, out)
    }

    fn end(&mut self, end: Result<(), Fault>, _: &mut Vec<O>) -> Result<(), Fault> {
        end?;
        match self.done {
            true => Ok(()),
            false => Err(Fault::truncated()),
        }
    }
}

/// What `write` writes into one buffer, as one frame if it wrote anything:
/// how an encoder of one body's chunks gives them as frames.
pub(crate) fn frame(
    frames: &mut Vec<Bytes>,
    write: impl FnOnce(&mut BytesMut) -> Result<(), Fault>,
) -> Result<(), Fault> {
    let mut out = BytesMut::new();
    write(&mut out)?;
    if !out.is_empty() {
        frames.push(out.freeze());
    }
    Ok(())
}

/// `text` escaped as a path segment or query value: all but RFC 3986's
/// unreserved characters.
pub(crate) fn escape(text: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    const UNRESERVED: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(text, UNRESERVED).to_string()
}
