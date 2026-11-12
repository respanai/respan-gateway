//! OpenRouter's video generation, over HTTP: a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`OpenRouterVideos`].

use crate::edge::{bind_provider, content, sends};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{media_url, merge_missing};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{Method, request::Parts as RequestHead};
use respan_gateway_http::path::{escape, set_path};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole};
use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "openrouter-videos";

respan_gateway_core::native_key!(Key = NAME);
/// What ends the path of a download, after its job's escaped id.
const CONTENT: &str = "/content";

/// OpenRouter's video generation: `POST /videos` makes a job, `GET
/// /videos/{id}` polls it and `GET /videos/{id}/content` downloads its video.
///
/// A creation needs a model (`x-ai/grok-imagine-video-1.5-lite`); its `seconds` go as the
/// `duration`, its `size` as it is, its reference image, a URL or a data URL
/// of its bytes, as the first of the `frame_images`, and its native fields of
/// this format, such as `resolution`, `aspect_ratio` or `generate_audio`, as
/// they are.
///
/// A job's id starts `gen-vid-`. It is `queued` while `pending`, `in_progress`,
/// `completed` once its video is made, `failed` with the error OpenRouter
/// words, or `cancelled` or `expired` as that; a state it adds is the
/// format's own. Its
/// videos are where it serves them ([`Job::videos`]). What else it answers,
/// such as its `usage` and the signed `urls` of its video, is dropped: it
/// reports no progress and names no model, size or length of a job.
///
/// Another format's operation, such as OpenAI's remix, delete, list or
/// thumbnail, is refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenRouterVideos;

impl OpenRouterVideos {
    /// The format's name, `openrouter-videos`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
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
        Ok(bind_provider(io, NAME, call, |_, sent| {
            Some(match sent.uri().path().ends_with(CONTENT) {
                true => content(),
                false => Box::new(Whole::new(read_job)) as Box<dyn Decoder<Output>>,
            })
        }))
    }
}

/// An operation OpenRouter has no form for, refused.
fn unsupported(input: &Input) -> Fault {
    let what = match input {
        Input::Native { .. } => "operation of another format",
        _ => "operation of this kind",
    };
    Fault::bad_request(format!("OpenRouter has no video {what}"))
        .with(ErrorCode("unsupported".into()))
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let path = |id: &str, rest: &str| format!("/videos/{}{rest}", escape(id));
    let (method, path, body) = match input {
        Input::Create(create) => (
            Method::POST,
            "/videos".to_owned(),
            Some(create_body(create)?),
        ),
        Input::Retrieve { id } => (Method::GET, path(id, ""), None),
        Input::Content { id, .. } => (Method::GET, path(id, CONTENT), None),
        _ => return Err(unsupported(input)),
    };
    head.method = method;
    set_path(head, &path)?;
    let Some(body) = body else {
        return Ok(sends(Bytes::new()));
    };
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(sends(body))
}

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Bytes, Fault> {
    let model = create.model.as_deref();
    let model =
        model.ok_or_else(|| Fault::bad_request("a video needs a model, such as tbc/mischief-1"))?;
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
        let frame = json!({ "type": "image_url", "frame_type": "first_frame",
            "image_url": { "url": media_url(reference) } });
        v.insert("frame_images".into(), vec![frame].into());
    }
    merge_missing(&mut v, create.native.fields::<Key>());
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))
}

/// The job of an answer `raw`.
fn read_job(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let id = v["id"].as_str().ok_or_else(|| unreadable("has no id"))?;
    let mut error = v["error"].as_str();
    let status = match v["status"].as_str() {
        Some("pending") => Status::Queued,
        Some("in_progress") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed") => Status::Failed,
        // A job `cancelled` or `expired` ended without a video: it failed,
        // as that, unless OpenRouter says why.
        Some(ended @ ("cancelled" | "expired")) => {
            error = error.or(Some(ended));
            Status::Failed
        }
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(id, status);
    job.expires_at = v["expires_at"].as_u64();
    job.error = error.map(|m| JobError::new(None, m));
    job.videos = (v["unsigned_urls"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(|url| Media::new("video/mp4", MediaSource::Url(url.to_owned())))
        .collect();
    out.push(Output::Job(job));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http;

    fn head() -> RequestHead {
        http::Request::new(()).into_parts().0
    }

    fn jobs(raw: Value) -> Vec<Output> {
        let mut out = Vec::new();
        read_job(Bytes::from(raw.to_string()), &mut out).expect("read");
        out
    }

    #[test]
    fn a_creation_is_a_job_of_a_duration_and_a_first_frame() {
        let mut create = Create::new("a wave");
        create.model = Some("x-ai/grok-imagine-video-1.5-lite".into());
        create.seconds = Some(2);
        create.size = Some("854x480".into());
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        ));
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            body,
            json!({ "model": "x-ai/grok-imagine-video-1.5-lite", "prompt": "a wave",
                "duration": 2, "size": "854x480",
                "frame_images": [{ "type": "image_url", "frame_type": "first_frame",
                    "image_url": { "url": "https://a.example/cat.png" } }] })
        );
        create.model = None;
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_job_is_polled_and_downloaded_by_its_escaped_id() {
        let mut head = head();
        call(
            &mut head,
            &Input::Retrieve {
                id: "gen-vid-1/a".into(),
            },
        )
        .expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/videos/gen-vid-1%2Fa");
        let content = Input::Content {
            id: "gen-vid-1".into(),
            url: None,
        };
        call(&mut head, &content).expect("a call");
        assert_eq!(head.uri.path(), "/videos/gen-vid-1/content");
        // Another format's operation, such as OpenAI's remix, is refused before a call.
        let remix = Input::Native {
            id: Some("gen-vid-1".into()),
            native: Native::json::<()>(json!({"method": "POST"})),
        };
        let refused = call(&mut head, &remix).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let pending = json!({ "id": "gen-vid-1", "polling_url": "https://o.example/videos/gen-vid-1",
            "status": "pending" });
        assert!(matches!(&jobs(pending)[..], [Output::Job(j)] if j.status == Status::Queued));

        let done = json!({ "id": "gen-vid-1", "generation_id": "gen-vid-1", "status": "completed",
            "unsigned_urls": ["https://o.example/videos/gen-vid-1/content?index=0"],
            "urls": ["https://storage.example/signed"], "usage": { "cost": 0.02, "is_byok": false },
            "expires_at": 1794541809u64 });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.status.clone(), job.expires_at),
            (Status::Completed, Some(1794541809))
        );
        let at = "https://o.example/videos/gen-vid-1/content?index=0";
        assert_eq!(job.video_url(), Some(at));
        assert!(job.native.is_empty());

        let failed = json!({ "id": "gen-vid-1", "status": "failed", "error": "content policy" });
        let [Output::Job(job)] = &jobs(failed)[..] else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Failed);
        assert_eq!(job.error, Some(JobError::new(None, "content policy")));

        // Regression: an OpenAI client polling a cancelled job was answered 502.
        let [Output::Job(job)] = &jobs(json!({ "id": "gen-vid-1", "status": "cancelled" }))[..]
        else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Failed);
        assert_eq!(job.error, Some(JobError::new(None, "cancelled")));
    }
}
