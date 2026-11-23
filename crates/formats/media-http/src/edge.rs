//! What the HTTP formats of this crate share.

use futures::{FutureExt, SinkExt, StreamExt};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::invalid;
use respan_gateway_conversation_http::read_provider_error;
use respan_gateway_core::{Connection, EventStream, Fault};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{
    self, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::set_path;
use respan_gateway_http::{Decoder, Encoder, ProviderIo, Sse, Whole};
use respan_gateway_media::video;
use serde_json::{Map, Value};
use std::time::Duration;

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
    bind_provider_with(io, format, read_provider_error, encode, decode)
}

/// [`bind_provider`], its failure read by `refusal`.
pub(crate) fn bind_provider_with<I, O>(
    io: ProviderIo,
    format: &'static str,
    refusal: impl FnOnce(http::Response<Bytes>) -> Fault + Send + 'static,
    encode: impl FnOnce(&mut RequestHead, &I) -> Result<Box<dyn Encoder<I>>, Fault> + Send + 'static,
    decode: impl FnOnce(&ResponseHead, &http::Request<Bytes>) -> Option<Box<dyn Decoder<O>>>
    + Send
    + 'static,
) -> Connection<O, I>
where
    I: Clone + Send + Sync + 'static,
    O: Clone + Send + Sync + 'static,
{
    io.with_refusal(refusal).bind(
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

/// A `GET` of `path`, below the base URL's, with no body.
pub(crate) fn gets<I: 'static>(
    head: &mut RequestHead,
    path: &str,
) -> Result<Box<dyn Encoder<I>>, Fault> {
    head.method = http::Method::GET;
    set_path(head, path)?;
    Ok(sends(Bytes::new()))
}

/// A `POST` to `path`, below the base URL's, of `body` as JSON.
pub(crate) fn posts<I: 'static>(
    head: &mut RequestHead,
    path: &str,
    body: &Map<String, Value>,
) -> Result<Box<dyn Encoder<I>>, Fault> {
    let body = serde_json::to_vec(body)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    head.method = http::Method::POST;
    set_path(head, path)?;
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(sends(Bytes::from(body)))
}

/// The provider edge of `format`, a format of video jobs: `call` is its
/// call, and each answer one job, read by `read`.
pub(crate) fn bind_jobs(
    io: ProviderIo,
    format: &'static str,
    call: impl FnOnce(&mut RequestHead, &video::Input) -> Result<Box<dyn Encoder<video::Input>>, Fault>
    + Send
    + 'static,
    read: fn(Bytes, &mut Vec<video::Output>) -> Result<(), Fault>,
) -> Connection<video::Output, video::Input> {
    bind_provider(io, format, call, move |_, _| {
        Some(Box::new(Whole::new(read)) as Box<dyn Decoder<video::Output>>)
    })
}

/// How long a job run within a call rests before each poll: the fastest
/// image models take about five seconds.
const POLL: Duration = Duration::from_secs(2);

/// What an answer says of a call whose provider may run it as a job.
#[derive(Clone)]
pub(crate) enum Step<O> {
    /// A job not ended, by its id as its video format gives it.
    Wait(String),
    /// Its outputs.
    Done(Vec<O>),
}

/// The call of a video format's operation.
pub(crate) type JobCall =
    fn(&mut RequestHead, &video::Input) -> Result<Box<dyn Encoder<video::Input>>, Fault>;

/// The job of an answer, as a video format's `read` reads it.
pub(crate) fn job_in(
    read: fn(Bytes, &mut Vec<video::Output>) -> Result<(), Fault>,
    raw: Bytes,
) -> Result<video::Job, Fault> {
    let mut out = Vec::new();
    read(raw, &mut out)?;
    match out.pop() {
        Some(video::Output::Job(job)) => Ok(job),
        _ => Err(Fault::upstream("the provider's answer holds no job")),
    }
}

/// The provider edge of `format`, whose provider may run a call as a job,
/// run to its end within the call: `call` is its call and `read` reads
/// each answer, the call's and a poll's alike. A job not ended is polled
/// with `poll`, its video format's call, after a rest, until one is; a
/// client that leaves ends the polling. Nothing times the wait but each
/// call's own transport.
pub(crate) fn bind_waited<I, O>(
    io: ProviderIo,
    format: &'static str,
    call: impl FnOnce(&mut RequestHead, &I) -> Result<Box<dyn Encoder<I>>, Fault> + Send + 'static,
    poll: JobCall,
    read: fn(Bytes) -> Result<Step<O>, Fault>,
) -> Connection<O, I>
where
    I: Clone + Send + Sync + 'static,
    O: Clone + Send + Sync + 'static,
{
    let more = io.another();
    let (opening, sink) = bind_provider(io, format, call, move |_, _| steps(read));
    let opening = async move {
        let mut answer = opening.await?;
        loop {
            if let Some(fault) = answer.refusal() {
                return Ok(EventStream::refused(fault.clone()));
            }
            let step = answer.next().await;
            let step = step.ok_or_else(|| Fault::upstream("the provider answered nothing"))??;
            let id = match step {
                Step::Done(outputs) => {
                    let outputs = futures::stream::iter(outputs.into_iter().map(Ok));
                    return Ok(EventStream::stream(outputs));
                }
                Step::Wait(id) => id,
            };
            tokio::time::sleep(POLL).await;
            let (polled, mut sink) =
                bind_provider(more.another(), format, poll, move |_, _| steps(read));
            let mut retrieve = EventStream::single(video::Input::Retrieve { id });
            sink.prepare(&mut retrieve)?;
            let (polled, sent) = futures::join!(polled, sink.close());
            answer = polled?;
            sent?;
        }
    };
    (opening.boxed(), sink)
}

/// The decoder of an answer, whole, as the one step `read` reads of it.
fn steps<O: 'static>(
    read: fn(Bytes) -> Result<Step<O>, Fault>,
) -> Option<Box<dyn Decoder<Step<O>>>> {
    Some(Box::new(Whole::new(
        move |raw: Bytes, out: &mut Vec<Step<O>>| {
            out.push(read(raw)?);
            Ok(())
        },
    )))
}

/// A video operation `who` has no form for, refused: content, where it
/// serves a video only at the URL its job names, or another's operation.
pub(crate) fn unsupported(who: &str, input: &video::Input) -> Fault {
    let what = match input {
        video::Input::Native { .. } => "operation of another format",
        video::Input::Content { .. } => "content: it serves a video at the URL its job names",
        _ => "operation of this kind",
    };
    respan_gateway_http_faults::unsupported(format!("{who} has no video {what}"))
}

/// `who`'s own id of a job `id`, which the gateway gives out after
/// `prefix`: one spelled otherwise is no job of its.
pub(crate) fn job_of<'a>(who: &str, prefix: &str, id: &'a str) -> Result<&'a str, Fault> {
    let job = id.strip_prefix(prefix).filter(|job| !job.is_empty());
    job.ok_or_else(|| Fault::not_found(format!("no {who} video job is {id:?}")))
}

/// A job's error, a `{code, message}`, if it has a message.
pub(crate) fn job_error(error: &Value) -> Option<video::JobError> {
    let code = error["code"].as_str().map(str::to_owned);
    let message = error["message"].as_str()?;
    Some(video::JobError::new(code, message))
}

/// The error of a job that ended as `state` with no video, such as
/// `cancelled`: it failed, as that, unless its provider said why.
pub(crate) fn ended_as(error: Option<video::JobError>, state: &str) -> Option<video::JobError> {
    error.or_else(|| Some(video::JobError::new(None, state)))
}

/// A video a job names, at `url`.
pub(crate) fn video_at(url: &str) -> Media {
    Media::new("video/mp4", MediaSource::Url(url.to_owned()))
}

/// The width and height of a size `WxH`, neither of them zero.
pub(crate) fn dimensions(size: &str) -> Option<(u32, u32)> {
    let (w, h) = size.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// The aspect ratio of `ratios`, each a width to a height, nearest to a
/// `width` by `height`, within 5%.
pub(crate) fn nearest_ratio(width: u32, height: u32, ratios: &[(u32, u32)]) -> Option<(u32, u32)> {
    nearest(width, height, ratios, |&(a, b)| (a, b)).copied()
}

/// An aspect ratio: its name, a width and a height.
pub(crate) type Aspect = (&'static str, u32, u32);

/// The aspect ratio of `ratios` nearest to a size `WxH`, within 5%, and the
/// size's long side; none for `auto`. A size of no such ratio is refused,
/// 400, naming the ratios `who` draws.
pub fn named_aspect(
    size: &str,
    ratios: &[Aspect],
    who: &str,
) -> Result<Option<(Aspect, u32)>, Fault> {
    if size == "auto" {
        return Ok(None);
    }
    let found = dimensions(size).and_then(|(w, h)| {
        let best = nearest(w, h, ratios, |&(_, a, b)| (a, b))?;
        Some((*best, w.max(h)))
    });
    found.map(Some).ok_or_else(|| {
        let names: Vec<&str> = ratios.iter().map(|(name, ..)| *name).collect();
        let names = names.join(", ");
        invalid(
            "size",
            &format!("is not of an aspect ratio {who} draws: {names}"),
        )
    })
}

/// The entry of `ratios` whose width to height, by `of`, is nearest to a
/// `width` by `height`, within 5%.
fn nearest<T>(width: u32, height: u32, ratios: &[T], of: impl Fn(&T) -> (u32, u32)) -> Option<&T> {
    let ratio = |w: u32, h: u32| f64::from(w) / f64::from(h);
    let off = |entry: &T| {
        let (a, b) = of(entry);
        (ratio(width, height) / ratio(a, b) - 1.0).abs()
    };
    let best = ratios.iter().min_by(|x, y| off(x).total_cmp(&off(y)))?;
    (off(best) <= 0.05).then_some(best)
}

/// A field's whole number: a JSON one, or a form's text of one.
pub(crate) fn whole_number(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A field's boolean: JSON's, or a form's `true` or `false`, in any case.
pub(crate) fn boolean(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

/// A vector as a list of numbers, each as its shortest `f32` text, not as
/// the `f64` it widens to.
pub(crate) fn floats(vector: &[f32]) -> serde_json::Value {
    let number = |f: &f32| {
        f.to_string()
            .parse::<f64>()
            .map_or(serde_json::Value::Null, serde_json::Value::from)
    };
    vector.iter().map(number).collect::<Vec<_>>().into()
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
