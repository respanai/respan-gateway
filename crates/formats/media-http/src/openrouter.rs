//! OpenRouter's video generation: a format of the
//! [`video`](respan_gateway_media::video) kind, [`OpenRouterVideos`].

use crate::edge::{bind_provider, content, sends};
use crate::openai::{escape, to_data_url};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{HeaderValue, Method, request::Parts as RequestHead};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_http::{Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status, Variant};
use serde_json::{Map, Value, json};
use std::sync::{Arc, OnceLock};

const NAME: &str = "openrouter-videos";

/// OpenRouter's video generation, as its API serves it: a provider of the
/// video kind, at `https://openrouter.ai/api/v1`, a bearer key.
///
/// A creation is `POST /videos`, which needs a model. Its `seconds` go as
/// `duration`, its `size` as `size` (`WIDTHxHEIGHT`, which OpenRouter takes
/// beside `resolution` and `aspect_ratio`), its reference image as the first
/// frame, `frame_images`, and its native fields of this format, such as
/// `generate_audio` or `callback_url`, as they are. A job is polled by `GET
/// /videos/{id}`: `pending` is queued, `in_progress` in progress, `completed`
/// completed, and `failed`, `cancelled` and `expired` failed, saying why. A
/// finished job's `usage` (its cost) and `generation_id` stay native. The
/// content is `GET /videos/{id}/content?index=0`, relayed chunk by chunk as
/// it comes. A job's id is the provider's, `gen-vid-` and a timestamp and
/// letters ([`OpenRouterVideos::JOB_IDS`]), which routes a follow-up to it.
///
/// OpenRouter cannot delete or list videos, nor make a thumbnail or a
/// spritesheet, and continues a job (`previous_job_id`) only for a call that
/// names its model again, which a remix does not: those are refused, 400
/// `unsupported`, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenRouterVideos;

impl OpenRouterVideos {
    /// The format's name, `openrouter-videos`, which tags its native fields.
    pub const NAME: &'static str = NAME;

    /// What the id of a job of OpenRouter's starts with.
    pub const JOB_IDS: &'static str = "gen-vid-";
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenRouterVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenRouterVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        // What the call was, from the input it was made of.
        let sent = Arc::new(OnceLock::new());
        let seen = Arc::clone(&sent);
        Ok(bind_provider(
            io,
            NAME,
            move |head, input| {
                let (encoder, call) = call(head, input)?;
                _ = seen.set(call);
                Ok(encoder)
            },
            move |_, _| Some(read_response(sent.get())),
        ))
    }
}

/// An operation OpenRouter has no form for, refused.
fn unsupported(what: &str) -> Fault {
    Fault::bad_request(format!("OpenRouter cannot {what}")).with(ErrorCode("unsupported".into()))
}

/// What a call asked, which its answer is read by.
#[derive(Debug)]
enum Call {
    /// A creation, of this model, length and size.
    Create(Box<Create>),
    /// A job's state.
    Retrieve,
    /// A video's bytes.
    Content,
}

/// The call of `input`: its method and path, its body's encoder, and what
/// it asks.
fn call(head: &mut RequestHead, input: &Input) -> Result<(Box<dyn Encoder<Input>>, Call), Fault> {
    let (method, path, body, asked) = match input {
        Input::Create(create) => {
            let body = create_body(create)?;
            let asked = Call::Create(Box::new(create.clone()));
            (Method::POST, "/videos".to_owned(), Some(body), asked)
        }
        Input::Retrieve { id } => {
            let path = format!("/videos/{}", escape(id));
            (Method::GET, path, None, Call::Retrieve)
        }
        Input::Content { id, variant, .. } => {
            if *variant != Variant::Video {
                return Err(unsupported("make a thumbnail or a spritesheet"));
            }
            let path = format!("/videos/{}/content?index=0", escape(id));
            (Method::GET, path, None, Call::Content)
        }
        Input::Remix { .. } => return Err(unsupported("remix a video")),
        Input::Delete { .. } => return Err(unsupported("delete a video")),
        Input::List { .. } => return Err(unsupported("list videos")),
        _ => return Err(unsupported("answer this request")),
    };
    head.method = method;
    head.uri = path
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let Some(body) = body else {
        return Ok((sends(Bytes::new()), asked));
    };
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok((sends(body), asked))
}

/// `media` as an `image_url` part: its URL, or its bytes as a data URL.
fn image_part(media: &Media) -> Value {
    let url = match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => to_data_url(media, bytes),
    };
    json!({ "type": "image_url", "image_url": { "url": url } })
}

/// A creation's body.
fn create_body(create: &Create) -> Result<Bytes, Fault> {
    let model = create
        .model
        .as_deref()
        .ok_or_else(|| Fault::bad_request("an OpenRouter video needs a model"))?;
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("prompt".into(), create.prompt.clone().into());
    if let Some(seconds) = create.seconds {
        v.insert("duration".into(), seconds.into());
    }
    if let Some(size) = &create.size {
        v.insert("size".into(), size.clone().into());
    }
    if let Some(reference) = &create.reference {
        let mut frame = image_part(reference);
        frame["frame_type"] = "first_frame".into();
        v.insert("frame_images".into(), json!([frame]));
    }
    for (name, value) in kept(NAME, &create.native)? {
        v.entry(name).or_insert(value);
    }
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body is no JSON").with_source(e))
}

/// A provider's answer to the call `sent`: a job, read whole, or content,
/// chunk by chunk as it comes.
fn read_response(sent: Option<&Call>) -> Box<dyn Decoder<Output>> {
    if let Some(Call::Content) = sent {
        return content();
    }
    let created = match sent {
        Some(Call::Create(create)) => Some((**create).clone()),
        _ => None,
    };
    Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
        out.push(Output::Job(job_of(&raw, created.as_ref())?));
        Ok(())
    }))
}

/// The job of an answer `raw`, with what its creation asked, if this
/// answers one.
fn job_of(raw: &[u8], created: Option<&Create>) -> Result<Job, Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(raw).map_err(|_| unreadable("is not JSON"))?;
    let id = v["id"].as_str().ok_or_else(|| unreadable("has no id"))?;
    let status = v["status"].as_str().unwrap_or_default();
    let mut job = Job::new(id, Status::InProgress);
    if let Some(create) = created {
        job.model = create.model.clone();
        job.seconds = create.seconds;
        job.size = create.size.clone();
    }
    job.expires_at = v["expires_at"].as_u64();
    job.status = match status {
        "pending" => Status::Queued,
        "in_progress" => Status::InProgress,
        "completed" => Status::Completed,
        "failed" | "cancelled" | "expired" => {
            let message = match v["error"].as_str() {
                Some(error) => error.to_owned(),
                None => format!("the video job {status}"),
            };
            job.error = Some(JobError::new(Some(status.to_owned()), message));
            Status::Failed
        }
        other => Status::native(NAME, other),
    };
    let mut native = Map::new();
    for key in ["generation_id", "usage"] {
        if !v[key].is_null() {
            native.insert(key.into(), v[key].clone());
        }
    }
    job.native = native_of(NAME, native);
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http;

    fn sent(input: &Input) -> Result<(RequestHead, Value), Fault> {
        let mut head = http::Request::new(()).into_parts().0;
        let (mut encoder, _) = call(&mut head, input)?;
        let mut out = Vec::new();
        encoder.encode(input, &mut out)?;
        let body = out.concat();
        Ok((head, serde_json::from_slice(&body).unwrap_or_default()))
    }

    #[test]
    fn a_creation_is_posted_with_its_length_size_and_first_frame() {
        let mut create = Create::new("a fox");
        create.model = Some("google/veo-3.1".into());
        create.seconds = Some(8);
        create.size = Some("1280x720".into());
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://x/a.png".into()),
        ));
        let (head, body) = sent(&Input::Create(create)).expect("a call");
        assert_eq!((head.method, head.uri.path()), (Method::POST, "/videos"));
        assert_eq!(body["duration"], 8);
        assert_eq!(body["size"], "1280x720");
        assert_eq!(body["frame_images"][0]["frame_type"], "first_frame");
        assert_eq!(
            body["frame_images"][0]["image_url"]["url"],
            "https://x/a.png"
        );
        assert!(sent(&Input::Create(Create::new("no model"))).is_err());
    }

    #[test]
    fn a_follow_up_goes_to_its_jobs_path_and_the_rest_is_refused() {
        let id = "gen-vid-1-abc".to_owned();
        let (head, _) = sent(&Input::Retrieve { id: id.clone() }).expect("a call");
        assert_eq!(
            (head.method, head.uri.path()),
            (Method::GET, "/videos/gen-vid-1-abc")
        );
        let content = Input::Content {
            id: id.clone(),
            variant: Variant::Video,
            url: None,
        };
        let (head, _) = sent(&content).expect("a call");
        assert_eq!(
            head.uri.to_string(),
            "/videos/gen-vid-1-abc/content?index=0"
        );
        for refused in [
            Input::Delete { id: id.clone() },
            Input::List {
                after: None,
                limit: None,
                order: None,
            },
            Input::Content {
                id,
                variant: Variant::Thumbnail,
                url: None,
            },
        ] {
            let fault = sent(&refused).expect_err("refused");
            assert_eq!(fault.code(), "unsupported");
        }
    }

    #[test]
    fn a_jobs_status_and_cost_are_read() {
        let raw = |status: &str, more: &str| {
            format!(
                r#"{{"id": "gen-vid-1-abc", "status": "{status}", "generation_id": "g" {more}}}"#
            )
        };
        let job = |raw: String| job_of(raw.as_bytes(), None).expect("a job");
        assert_eq!(job(raw("pending", "")).status, Status::Queued);
        assert_eq!(job(raw("in_progress", "")).status, Status::InProgress);
        let done = job(raw("completed", r#", "usage": {"cost": 0.5}"#));
        assert_eq!(done.status, Status::Completed);
        let native = done.native.expect("native");
        assert_eq!(native.fields["usage"]["cost"], 0.5);
        let failed = job(raw("failed", r#", "error": "blocked""#));
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.error.expect("error").message, "blocked");
        let expired = job(raw("expired", ""));
        assert_eq!(expired.status, Status::Failed);
        assert!(job_of(b"{}", None).is_err());
    }
}
