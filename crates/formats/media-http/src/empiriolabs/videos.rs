//! EmpirioLabs AI's video API: `POST /videos/generations`, answered as a
//! job.

use super::{is_segment, job_path};
use crate::edge::{bind_provider, sends};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::kept;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{HeaderValue, Method, request::Parts as RequestHead};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole};
use serde_json::{Map, Value};

use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status, Variant};

const NAME: &str = "empiriolabs-videos";
/// What starts the id of a job of EmpirioLabs AI's.
const PREFIX: &str = "empiriolabs_";

/// EmpirioLabs AI's video API: a provider of the video kind.
///
/// A creation is `POST /videos/generations`, which answers a job at once; a
/// job is polled at `GET /jobs/{id}`, an endpoint shared with its other
/// asynchronous kinds. A job's id is the provider's after `empiriolabs_`
/// ([`EmpirioLabsVideos::job_id`]). `processing` is `in_progress`; `completed`
/// with a video is `completed`, its videos' URLs [`Job::videos`]; `failed`,
/// `cancelled`, and `completed` with no video are `failed`.
///
/// A creation's `seconds` goes as `duration`, its `size` as an
/// `aspect_ratio` and a `resolution` (the tier nearest its short side: `480p`,
/// `720p`, `1080p` or `4k`), its
/// reference image as `image`, a URL or a data URL, and its native fields of
/// this format as they are.
///
/// A video is only at a signed URL of `media.empiriolabs.ai`, which no path
/// of its API serves, and a provider's calls all go beside its base URL:
/// its content is refused, 400, naming that it is at the URL its job names. EmpirioLabs AI cannot remix, delete or
/// list videos, nor make a thumbnail or a spritesheet: those are refused,
/// 400, before any call. What it answers beyond the kind is dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmpirioLabsVideos;

impl EmpirioLabsVideos {
    /// The format's name, `empiriolabs-videos`, which tags its native
    /// fields.
    pub const NAME: &'static str = NAME;

    /// The job id of the provider's job `id`.
    pub fn job_id(id: &str) -> String {
        format!("{PREFIX}{id}")
    }

    /// The provider's id of a job `id` of this format's, if it is one: of
    /// letters, digits, `-`, `_` and `.`, and no `.` or `..`.
    pub fn provider_id(id: &str) -> Option<&str> {
        id.strip_prefix(PREFIX).filter(|id| is_segment(id))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for EmpirioLabsVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for EmpirioLabsVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, _| Some(read_response())))
    }
}

/// An operation EmpirioLabs AI has no form for, refused.
fn unsupported(what: &str) -> Fault {
    Fault::bad_request(format!("EmpirioLabs AI cannot {what}"))
        .with(ErrorCode("unsupported".into()))
}

/// The provider's id of a job `id`, else a 404.
fn provider_id(id: &str) -> Result<&str, Fault> {
    EmpirioLabsVideos::provider_id(id)
        .ok_or_else(|| Fault::not_found(format!("no EmpirioLabs AI video has id {id}")))
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let (method, path, body) = match input {
        Input::Create(create) => (
            Method::POST,
            "/videos/generations".to_owned(),
            Some(create_body(create)?),
        ),
        Input::Retrieve { id } => (Method::GET, job_path(provider_id(id)?), None),
        Input::Content { id, variant, .. } => {
            if *variant != Variant::Video {
                return Err(unsupported("make a thumbnail or a spritesheet"));
            }
            provider_id(id)?;
            return Err(unsupported(
                "serve a video's bytes: it is at the URL its job names",
            ));
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
        return Ok(sends(Bytes::new()));
    };
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(sends(body))
}

/// A creation's body.
fn create_body(create: &Create) -> Result<Bytes, Fault> {
    let mut v = Map::new();
    if let Some(model) = &create.model {
        v.insert("model".into(), model.clone().into());
    }
    v.insert("prompt".into(), create.prompt.clone().into());
    if let Some(seconds) = create.seconds {
        v.insert("duration".into(), seconds.into());
    }
    if let Some(size) = &create.size {
        let (aspect, resolution) = aspect_of(size)?;
        v.insert("aspect_ratio".into(), aspect.into());
        v.insert("resolution".into(), resolution.into());
    }
    if let Some(reference) = &create.reference {
        v.insert("image".into(), image_of(reference).into());
    }
    for (key, value) in kept(NAME, &create.native)? {
        v.entry(key).or_insert(value);
    }
    serde_json::to_vec(&Value::Object(v))
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))
}

/// A reference image as the provider takes it: its URL, or a data URL of
/// its bytes.
fn image_of(media: &Media) -> String {
    match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => {
            format!("data:{};base64,{}", media.mime_type, STANDARD.encode(bytes))
        }
    }
}

/// The resolution tiers it takes, by the short side they are named for.
const TIERS: [(u32, &str); 4] = [(480, "480p"), (720, "720p"), (1080, "1080p"), (2160, "4k")];

/// The aspect ratio and resolution of a size `WxH`: its sides' ratio in
/// lowest terms and the tier nearest its short side.
fn aspect_of(size: &str) -> Result<(String, String), Fault> {
    let parsed = size.split_once(['x', 'X']).and_then(|(w, h)| {
        let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
        (w > 0 && h > 0).then_some((w, h))
    });
    let (w, h) = parsed.ok_or_else(|| {
        Fault::bad_request(format!("'size' {size} is not a width and a height, WxH"))
    })?;
    let divisor = gcd(w, h);
    Ok((
        format!("{}:{}", w / divisor, h / divisor),
        tier_of(w.min(h)).to_owned(),
    ))
}

/// The tier whose short side is nearest `short`, the lower on a tie.
fn tier_of(short: u32) -> &'static str {
    let nearest = TIERS.iter().min_by_key(|(side, _)| side.abs_diff(short));
    nearest.map_or("720p", |(_, tier)| tier)
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A provider's answer: a job, read whole.
fn read_response() -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
        out.push(Output::Job(job_of(&raw)?));
        Ok(())
    }))
}

/// The job of an answer `raw`, a creation's or a poll's.
fn job_of(raw: &[u8]) -> Result<Job, Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(raw).map_err(|_| unreadable("is not JSON"))?;
    let id = v["job_id"]
        .as_str()
        .ok_or_else(|| unreadable("has no job_id"))?;
    let mut job = Job::new(EmpirioLabsVideos::job_id(id), Status::InProgress);
    job.model = v["model"].as_str().map(str::to_owned);
    job.created_at = v["created_at"].as_u64();
    job.completed_at = v["completed_at"].as_u64();
    job.progress = v["progress"].as_u64().and_then(|p| u32::try_from(p).ok());
    match v["status"].as_str() {
        Some("processing") | None => {}
        Some("completed") => {
            let items = v["result"]["data"].as_array().into_iter().flatten();
            for item in items {
                if let Some(url) = item["url"].as_str() {
                    let mime = item["mime_type"].as_str().unwrap_or("video/mp4");
                    job.videos
                        .push(Media::new(mime, MediaSource::Url(url.to_owned())));
                }
            }
            if job.videos.is_empty() {
                job.status = Status::Failed;
                job.error = Some(JobError::new(None, "the provider made no video"));
            } else {
                job.status = Status::Completed;
            }
        }
        Some(status @ ("failed" | "cancelled")) => {
            job.status = Status::Failed;
            job.error = Some(error_of(&v["error"], status));
        }
        Some(other) => {
            return Err(unreadable(&format!(
                "is {other}, a status it has no form for"
            )));
        }
    }
    Ok(job)
}

/// Why a job failed: its `error` object's code and message, else its
/// `status`.
fn error_of(error: &Value, status: &str) -> JobError {
    let text = |key: &str| error[key].as_str().map(str::to_owned);
    let message = text("message")
        .or_else(|| error.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("the video {status}"));
    JobError::new(text("code").or_else(|| Some(status.to_owned())), message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::NativeFields;
    use respan_gateway_http::http;
    use serde_json::json;

    fn job(v: Value) -> Job {
        job_of(v.to_string().as_bytes()).expect("job")
    }

    fn body(create: &Create) -> Value {
        serde_json::from_slice(&create_body(create).expect("body")).expect("json")
    }

    #[test]
    fn a_creation_is_written_in_its_fields() {
        let mut create = Create::new("a fox");
        create.model = Some("seedance-1-5-pro".into());
        create.seconds = Some(6);
        create.size = Some("1280x720".into());
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Bytes(Bytes::from("x")),
        ));
        let mut native = Map::new();
        native.insert("generate_audio".into(), false.into());
        native.insert("duration".into(), 9.into());
        create.native = Some(NativeFields::new(NAME, native));
        assert_eq!(
            body(&create),
            json!({
                "model": "seedance-1-5-pro", "prompt": "a fox", "duration": 6,
                "aspect_ratio": "16:9", "resolution": "720p",
                "image": "data:image/png;base64,eA==", "generate_audio": false,
            })
        );
        create.size = Some("wide".into());
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_sizes_short_side_is_the_nearest_tier() {
        let tier = |size| aspect_of(size).expect("size").1;
        assert_eq!(tier("1280x720"), "720p");
        assert_eq!(tier("1024x576"), "480p");
        assert_eq!(tier("640x360"), "480p");
        assert_eq!(tier("1920x1080"), "1080p");
        assert_eq!(tier("3840x2160"), "4k");
        assert_eq!(tier("8192x4320"), "4k");
    }

    #[test]
    fn a_sizes_aspect_is_in_lowest_terms() {
        let aspect = |size| aspect_of(size).expect("size");
        assert_eq!(aspect("720x1280"), ("9:16".to_owned(), "720p".to_owned()));
        assert_eq!(aspect("1024x1024"), ("1:1".to_owned(), "1080p".to_owned()));
    }

    #[test]
    fn a_follow_up_goes_to_the_jobs_endpoint() {
        let mut head = http::Request::new(()).into_parts().0;
        let input = Input::Retrieve {
            id: "empiriolabs_job_01HV".into(),
        };
        assert!(call(&mut head, &input).is_ok());
        assert_eq!(
            (head.method.clone(), head.uri.to_string()),
            (Method::GET, "/jobs/job_01HV".into())
        );
        for id in ["job_01HV", "empiriolabs_../x", "empiriolabs_"] {
            let input = Input::Retrieve { id: id.into() };
            assert!(call(&mut head, &input).is_err(), "{id}");
        }
    }

    #[test]
    fn what_the_provider_cannot_do_is_refused() {
        let mut head = http::Request::new(()).into_parts().0;
        let id = || "empiriolabs_job_1".to_owned();
        let content = |variant| Input::Content {
            id: id(),
            variant,
            url: None,
        };
        for input in [
            content(Variant::Video),
            content(Variant::Thumbnail),
            Input::Delete { id: id() },
            Input::Remix {
                id: id(),
                prompt: "p".into(),
                native: None,
            },
        ] {
            assert!(call(&mut head, &input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let made = job(
            json!({"job_id": "job_1", "status": "processing", "created_at": 5,
            "model": "kling-v3", "progress": 40, "poll_url": "/v1/jobs/job_1"}),
        );
        assert_eq!(made.id, "empiriolabs_job_1");
        assert_eq!(made.status, Status::InProgress);
        assert_eq!((made.created_at, made.progress), (Some(5), Some(40)));
        assert_eq!(made.model.as_deref(), Some("kling-v3"));
        // A creation's `created_at` is a date, which the kind has no number for.
        let created = job(json!({"job_id": "job_1", "status": "processing",
            "created_at": "2026-05-02T17:11:32Z"}));
        assert_eq!(created.created_at, None);

        let done = job(
            json!({"job_id": "job_1", "status": "completed", "completed_at": 9,
            "result": {"data": [{"url": "https://media.example/v.mp4"}]}}),
        );
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.video_url(), Some("https://media.example/v.mp4"));
        assert_eq!(done.completed_at, Some(9));

        let empty = job(json!({"job_id": "job_1", "status": "completed", "result": null}));
        assert_eq!(empty.status, Status::Failed);

        let failed = job(json!({"job_id": "job_1", "status": "failed",
            "error": {"code": "content_filtered", "message": "refused"}}));
        assert_eq!(failed.status, Status::Failed);
        let error = failed.error.expect("error");
        assert_eq!(error.code.as_deref(), Some("content_filtered"));
        assert_eq!(error.message, "refused");

        let cancelled = job(json!({"job_id": "job_1", "status": "cancelled", "error": null}));
        assert_eq!(cancelled.status, Status::Failed);
        assert_eq!(
            cancelled.error.expect("error").code.as_deref(),
            Some("cancelled")
        );
    }

    #[test]
    fn an_answer_that_is_no_job_fails() {
        assert!(job_of(b"[]").is_err());
        assert!(job_of(br#"{"status": "processing"}"#).is_err());
        assert!(job_of(br#"{"job_id": "j", "status": "paused"}"#).is_err());
    }
}
