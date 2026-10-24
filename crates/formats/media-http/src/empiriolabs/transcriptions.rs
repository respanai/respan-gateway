//! EmpirioLabs AI's transcriptions: OpenAI's form, answered as a job that
//! is polled within the call.

use std::time::Duration;

use super::{is_segment, job_path};
use crate::dialect::Spec;
use crate::edge::{bind_provider, sends};
use crate::openai::{OpenAiTranscriptions, read_transcript};
use futures::{FutureExt, SinkExt, TryStreamExt, stream};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_core::{
    Connection, ErrorCode, EventStream, Extensions, Fault, Format, NativeValue, Provider,
};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::Method;
use respan_gateway_http::{Connect, Decoder, ProviderIo, Upstream, Whole};
use respan_gateway_media::transcription::{Input, Output, Task};
use serde_json::Value;

const NAME: &str = "empiriolabs-transcriptions";

/// The first wait before a job is polled, doubled after each poll.
const FIRST_WAIT: Duration = Duration::from_millis(500);

/// The longest wait between two polls.
const LONGEST_WAIT: Duration = Duration::from_secs(4);

/// How long a job may take, polled, before the call fails.
const MOST_WAITED: Duration = Duration::from_secs(10 * 60);

/// EmpirioLabs AI's transcriptions: a provider of the transcription kind.
///
/// A call is OpenAI's, a form to `POST /audio/transcriptions`, which
/// EmpirioLabs AI answers, 202, with a job, however short the audio. The
/// call polls it at `GET /jobs/{id}`, first after half a second, then
/// twice as long after each poll, at most 4 s, until it is `completed`,
/// its `result` a transcript in OpenAI's JSON, or `failed` or `cancelled`,
/// which fails the call `upstream` with the job's error. A job not done
/// within 10 minutes fails the call `upstream` too. The seconds of audio
/// its `result` bills (`usage.duration_seconds`) are [`Output::Seconds`].
///
/// It serves no translations: one is refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmpirioLabsTranscriptions;

impl EmpirioLabsTranscriptions {
    /// The format's name, `empiriolabs-transcriptions`.
    pub const NAME: &'static str = NAME;
}

/// Its spec's: a bearer key, over HTTP, beside its base URL. Its provider
/// keeps the connection, to poll a job with.
impl Upstream for EmpirioLabsTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        let polls = Polled {
            connect: Some(connect.clone()),
        };
        Provider::new(name, polls, move |x| connect(x))
    }
}

/// Without its provider's connection, it cannot poll a job: a call fails
/// `internal`, as only [`Upstream::provider`] makes one that can.
impl Format<ProviderIo, Output, Input> for EmpirioLabsTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Polled { connect: None }.bind(io)
    }
}

/// The format with what makes the calls that poll a job.
struct Polled {
    connect: Option<Connect>,
}

impl Format<ProviderIo, Output, Input> for Polled {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let extensions = io.extensions().clone();
        let connect = self.connect.clone();
        let (opening, sink) = bind_provider(
            io,
            NAME,
            |head, input: &Input| {
                if input.task == Task::Translate {
                    return Err(Fault::bad_request("EmpirioLabs AI cannot translate audio")
                        .with(ErrorCode("unsupported".into())));
                }
                OpenAiTranscriptions.post(head, input, false)
            },
            |_, _| Some(answer()),
        );
        let polled = async move {
            let mut events = opening.await?;
            if events.refusal().is_some() {
                return Ok(events);
            }
            let answered = match one(&mut events).await? {
                Answer::Done(outputs) => Ok(outputs),
                Answer::Job(id) => {
                    let connect = connect.ok_or_else(|| {
                        Fault::internal("an EmpirioLabs AI transcription has no connection to poll")
                    })?;
                    polled(&connect, extensions, &id).await
                }
                Answer::Pending => Err(unreadable("names no job")),
            };
            Ok(match answered {
                Ok(outputs) => EventStream::stream(stream::iter(outputs.into_iter().map(Ok))),
                Err(fault) => EventStream::refused(fault),
            })
        };
        Ok((polled.boxed(), sink))
    }
}

/// What an answer says: the transcript, a job to poll, or that the job
/// polled is not done.
enum Answer {
    Done(Vec<Output>),
    Job(String),
    Pending,
}

/// A provider's answer, read whole: its JSON, to be read as an [`Answer`].
fn answer() -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
        let v = serde_json::from_slice::<Value>(&raw).map_err(|_| unreadable("is not JSON"))?;
        out.push(Output::Native(NativeValue::new(NAME, v)));
        Ok(())
    }))
}

/// The [`Answer`] of the one answer in `events`.
async fn one(events: &mut EventStream<Output>) -> Result<Answer, Fault> {
    let answered = events.try_next().await?;
    let Some(Output::Native(answer)) = answered else {
        return Err(unreadable("is empty"));
    };
    read(&answer.value)
}

/// What `v`, a creation's answer or a polled job, says.
fn read(v: &Value) -> Result<Answer, Fault> {
    let id = v["job_id"].as_str();
    match (v["status"].as_str(), id) {
        (Some("completed"), _) => transcript(&v["result"]).map(Answer::Done),
        (Some("failed" | "cancelled"), _) => Err(failed(v)),
        (Some("processing") | None, Some(id)) if is_segment(id) => Ok(Answer::Job(id.to_owned())),
        (Some("processing"), _) => Ok(Answer::Pending),
        (None, None) if v["text"].is_string() => transcript(v).map(Answer::Done),
        (Some(other), _) => Err(unreadable(&format!(
            "is {other}, a status it has no form for"
        ))),
        (None, _) => Err(unreadable("names no job")),
    }
}

/// The outputs of a finished job's `result`: its transcript, as OpenAI's
/// JSON, and the seconds of audio it bills.
fn transcript(result: &Value) -> Result<Vec<Output>, Fault> {
    let mut result = result.clone();
    let usage = result.as_object_mut().and_then(|r| r.remove("usage"));
    if !result["text"].is_string() {
        return Err(unreadable("finished with no transcript"));
    }
    let raw = serde_json::to_vec(&result)
        .map_err(|e| Fault::internal("a transcript could not be written").with_source(e))?;
    let mut out = Vec::new();
    read_transcript(Bytes::from(raw), &mut out)?;
    let seconds = usage.as_ref().and_then(|u| u["duration_seconds"].as_f64());
    out.extend(seconds.map(Output::Seconds));
    Ok(out)
}

/// The fault of a job that failed or was cancelled: its error's message
/// and code.
fn failed(v: &Value) -> Fault {
    let status = v["status"].as_str().unwrap_or("failed");
    let error = &v["error"];
    let message = error["message"].as_str().or_else(|| error.as_str());
    let message = message.map_or_else(|| format!("the transcription {status}"), str::to_owned);
    let code = error["code"].as_str().unwrap_or(status);
    Fault::upstream(format!(
        "EmpirioLabs AI's transcription job {status}: {message}"
    ))
    .with(ErrorCode(code.to_owned().into()))
}

/// The outputs of job `id`, polled through `connect` until it is done.
async fn polled(connect: &Connect, extensions: Extensions, id: &str) -> Result<Vec<Output>, Fault> {
    let (mut wait, mut waited) = (FIRST_WAIT, Duration::ZERO);
    loop {
        if waited >= MOST_WAITED {
            return Err(Fault::upstream(format!(
                "EmpirioLabs AI's transcription job was not done within {MOST_WAITED:?}"
            )));
        }
        tokio::time::sleep(wait).await;
        waited += wait;
        wait = (wait * 2).min(LONGEST_WAIT);
        let mut events = poll(connect(extensions.clone()), id).await?;
        match one(&mut events).await? {
            Answer::Done(outputs) => return Ok(outputs),
            Answer::Pending | Answer::Job(_) => {}
        }
    }
}

/// One poll of job `id`, over `io`: its answer's events.
async fn poll(io: ProviderIo, id: &str) -> Result<EventStream<Output>, Fault> {
    let path = job_path(id);
    let (opening, mut sink) = bind_provider(
        io,
        NAME,
        move |head, _: &Input| {
            head.method = Method::GET;
            head.uri = path
                .parse()
                .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
            Ok(sends(Bytes::new()))
        },
        |_, _| Some(answer()),
    );
    // A poll has no body: the input it is given is only what starts it.
    let nothing = Media::new("application/octet-stream", MediaSource::Bytes(Bytes::new()));
    let mut start = EventStream::single(Input::new(nothing));
    sink.prepare(&mut start)?;
    let (events, sent) = futures::join!(opening, sink.close());
    let events = events?;
    if let Some(fault) = events.refusal() {
        return Err(fault.clone());
    }
    sent?;
    Ok(events)
}

/// A fault of an answer that is no answer of its: `what` it is.
fn unreadable(what: &str) -> Fault {
    Fault::upstream(format!("the provider's {NAME} answer {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn done(v: Value) -> Vec<Output> {
        match read(&v) {
            Ok(Answer::Done(outputs)) => outputs,
            _ => panic!("not a transcript: {v}"),
        }
    }

    #[test]
    fn a_creation_names_the_job_to_poll() {
        let accepted = json!({"job_id": "576a5588-fcb3", "status": "processing",
            "poll_url": "/v1/jobs/576a5588-fcb3"});
        assert!(matches!(read(&accepted), Ok(Answer::Job(id)) if id == "576a5588-fcb3"));
        let polled = json!({"job_id": "576a5588-fcb3", "status": "processing", "created_at": 5});
        assert!(matches!(read(&polled), Ok(Answer::Job(_))));
        let traversal = json!({"job_id": "../x", "status": "processing"});
        assert!(matches!(read(&traversal), Ok(Answer::Pending)));
    }

    #[test]
    fn a_finished_job_is_its_transcript_and_billed_seconds() {
        let outputs = done(json!({"job_id": "j", "status": "completed", "result": {
            "text": "The quick brown fox.", "language": "en", "duration": 3.168,
            "segments": [{"start": 0.24, "end": 2.94, "text": " The quick brown fox."}],
            "usage": {"duration_seconds": 3.168, "billing_unit": "minute", "cost_usd": 0.000264},
        }}));
        let Some(Output::Transcript(transcript)) = outputs.first() else {
            panic!("no transcript: {outputs:?}")
        };
        assert_eq!(transcript.text, "The quick brown fox.");
        assert_eq!(transcript.language.as_deref(), Some("en"));
        assert_eq!(transcript.segments.len(), 1);
        assert!(matches!(outputs.last(), Some(Output::Seconds(s)) if (*s - 3.168).abs() < 1e-9));
    }

    #[test]
    fn a_failed_job_fails_with_its_error() {
        let failed = json!({"job_id": "j", "status": "failed",
            "error": {"code": "audio_unreadable", "message": "no audio"}});
        let fault = read(&failed).err().expect("fault");
        assert!(fault.message().contains("no audio"), "{}", fault.message());
        assert_eq!(fault.get(), Some(&ErrorCode("audio_unreadable".into())));
        let cancelled = json!({"job_id": "j", "status": "cancelled", "error": null});
        assert!(read(&cancelled).is_err());
    }

    #[test]
    fn an_answer_that_is_no_job_fails() {
        assert!(read(&json!({"status": "completed", "result": {}})).is_err());
        assert!(read(&json!({"job_id": "j", "status": "paused"})).is_err());
        assert!(read(&json!({})).is_err());
        // A transcript answered at once is read as one.
        assert!(matches!(read(&json!({"text": "hi"})), Ok(Answer::Done(_))));
    }
}
