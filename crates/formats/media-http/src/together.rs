//! Together's video generation, over HTTP: a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`TogetherVideos`].

use crate::edge::{bind_provider, content, sends};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{media_url, merge_missing};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{Method, Uri, request::Parts as RequestHead};
use respan_gateway_http::path::{escape, set_path};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "together-videos";

type Key = crate::Own<TogetherVideos>;

/// What starts the id of a job of Together's: its own ids are bare UUIDs.
const PREFIX: &str = "together_";

/// Where its jobs are, at the provider's base.
const VIDEOS: &str = "/v2/videos";

/// Together's video generation: `POST /v2/videos` makes a job and `GET
/// /v2/videos/{id}` polls it, at the API's host, its base URL with no
/// version.
///
/// A creation needs a model (`google/veo-3.1-lite`); its `seconds` go as
/// they are, as text, its `size` as its `width` and `height`, its reference
/// image, a URL or a data URL of its bytes, as the first of the media's
/// `frame_images`, and its native fields of this format, such as
/// `resolution`, `ratio`, `generate_audio` or `negative_prompt`, as they
/// are.
///
/// A job's id is Together's after `together_`. It is `in_progress`,
/// `completed` once its video is made, or `failed` with the error Together
/// gives; a state it adds is the format's own. Its video is the `video_url`
/// of its `outputs` ([`Job::videos`]), and the `cost` there, what it
/// charged, the job's native `cost`. Content is a `GET` of that URL's path
/// at the provider's host, so the key never goes to another host;
/// [`LocateContent`](respan_gateway_media::video::LocateContent) finds it.
///
/// Another format's operation, such as OpenAI's remix, delete, list or
/// thumbnail, is refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TogetherVideos;

impl TogetherVideos {
    /// The format's name, `together-videos`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for TogetherVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for TogetherVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, sent| {
            Some(match sent.uri().path().starts_with(VIDEOS) {
                true => Box::new(Whole::new(read_job)) as Box<dyn Decoder<Output>>,
                false => content(),
            })
        }))
    }
}

/// An operation Together has no form for, refused.
fn unsupported(input: &Input) -> Fault {
    let what = match input {
        Input::Native { .. } => "operation of another format",
        _ => "operation of this kind",
    };
    Fault::bad_request(format!("Together has no video {what}"))
        .with(ErrorCode("unsupported".into()))
}

/// Together's id of a job `id`, if it is one of this format's.
fn job_of(id: &str) -> Result<&str, Fault> {
    let job = id.strip_prefix(PREFIX).filter(|job| !job.is_empty());
    job.ok_or_else(|| Fault::not_found(format!("no Together video job is {id:?}")))
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let (method, path, body) = match input {
        Input::Create(create) => (Method::POST, VIDEOS.to_owned(), Some(create_body(create)?)),
        Input::Retrieve { id } => {
            let path = format!("{VIDEOS}/{}", escape(job_of(id)?));
            (Method::GET, path, None)
        }
        Input::Content { id, url } => {
            job_of(id)?;
            let url = url.as_deref().ok_or_else(|| {
                Fault::config(
                    "Together's content is at the URL its job names, which the request needs",
                )
            })?;
            (Method::GET, content_path(url)?, None)
        }
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

/// The path and query of the URL `url` of a job's video, at the provider's
/// host; one under its jobs' is not a video's.
fn content_path(url: &str) -> Result<String, Fault> {
    let unusable =
        || Fault::upstream("the URL of Together's video is not one it can be fetched at");
    let uri = url.parse::<Uri>().map_err(|_| unusable())?;
    let path = uri.path();
    if path.trim_start_matches('/').is_empty() || path.starts_with(VIDEOS) {
        return Err(unusable());
    }
    Ok(match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    })
}

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Bytes, Fault> {
    let model = create.model.as_deref();
    let model = model
        .ok_or_else(|| Fault::bad_request("a video needs a model, such as google/veo-3.1-lite"))?;
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("prompt".into(), create.prompt.clone().into());
    if let Some(seconds) = create.seconds {
        v.insert("seconds".into(), seconds.to_string().into());
    }
    if let Some(size) = &create.size {
        let (width, height) = (size.split_once('x'))
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            .ok_or_else(|| {
                Fault::bad_request(format!(
                    "'size' {size:?} is not a width by height, such as 1280x720"
                ))
            })?;
        v.insert("width".into(), width.into());
        v.insert("height".into(), height.into());
    }
    if let Some(reference) = &create.reference {
        let frame = json!({ "input_image": media_url(reference), "frame": "first" });
        v.insert("media".into(), json!({ "frame_images": [frame] }));
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
    let status = match v["status"].as_str() {
        Some("in_progress") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = v["model"].as_str().map(str::to_owned);
    job.seconds = v["seconds"].as_str().and_then(|s| s.parse().ok());
    job.size = v["size"].as_str().map(str::to_owned);
    let error = &v["error"];
    job.error = error["message"].as_str().map(|message| {
        let code = error["code"].as_str().map(str::to_owned);
        JobError::new(code, message)
    });
    let outputs = &v["outputs"];
    job.videos = (outputs["video_url"].as_str().into_iter())
        .map(|url| Media::new("video/mp4", MediaSource::Url(url.to_owned())))
        .collect();
    if let Some(cost) = outputs.get("cost").filter(|cost| cost.is_number()) {
        job.native = Native::json::<Key>(json!({ "cost": cost }));
    }
    out.push(Output::Job(job));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Natives;
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
    fn a_creation_is_a_job_of_a_width_a_height_and_a_first_frame() {
        let mut create = Create::new("a wave");
        create.model = Some("google/veo-3.1-lite".into());
        create.seconds = Some(4);
        create.size = Some("1280x720".into());
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        ));
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            body,
            json!({ "model": "google/veo-3.1-lite", "prompt": "a wave", "seconds": "4",
                "width": 1280, "height": 720,
                "media": { "frame_images": [{ "input_image": "https://a.example/cat.png",
                    "frame": "first" }] } })
        );
        create.size = Some("wide".into());
        assert!(create_body(&create).is_err());
        create.model = None;
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_job_is_polled_by_its_id_and_downloaded_where_it_says() {
        let mut head = head();
        let retrieve = Input::Retrieve {
            id: "together_01a1-ecb7".into(),
        };
        call(&mut head, &retrieve).expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/v2/videos/01a1-ecb7");
        let content = Input::Content {
            id: "together_01a1-ecb7".into(),
            url: Some("https://api.together.ai/shrt/5juV?x=1".into()),
        };
        call(&mut head, &content).expect("a call");
        assert_eq!(head.uri.to_string(), "/shrt/5juV?x=1");
        // An id not of this format's is no job of Together's.
        let foreign = Input::Retrieve {
            id: "video_1".into(),
        };
        assert!(call(&mut head, &foreign).is_err());
        // Another format's operation, such as OpenAI's remix, is refused before a call.
        let remix = Input::Native {
            id: Some("together_1".into()),
            native: Native::json::<()>(json!({"method": "POST"})),
        };
        let refused = call(&mut head, &remix).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let running = json!({ "object": "video", "id": "01a1", "model": "google/veo-3.1-lite",
            "status": "in_progress", "created_at": 1794972795u64, "seconds": "4",
            "size": "1920x1080" });
        let [Output::Job(job)] = &jobs(running)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (
                job.id.as_str(),
                job.status.clone(),
                job.seconds,
                job.size.as_deref()
            ),
            (
                "together_01a1",
                Status::InProgress,
                Some(4),
                Some("1920x1080")
            )
        );

        let done = json!({ "id": "01a1", "status": "completed", "seconds": "4",
            "outputs": { "cost": 1, "video_url": "https://api.together.ai/shrt/5juV" } });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(job.video_url(), Some("https://api.together.ai/shrt/5juV"));
        assert_eq!(
            TogetherVideos::fields(&job.native).get("cost"),
            Some(&json!(1))
        );

        let failed = json!({ "id": "01a1", "status": "failed",
            "error": { "code": "content_policy", "message": "refused" } });
        let [Output::Job(job)] = &jobs(failed)[..] else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Failed);
        assert_eq!(
            job.error,
            Some(JobError::new(Some("content_policy".into()), "refused"))
        );
    }
}
