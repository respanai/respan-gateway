//! Together's video generation, over HTTP: a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`TogetherVideos`].

use crate::edge::{
    bind_jobs, dimensions, ended_as, fetches, gets, job_error, job_of, on_hosts, posts,
    unsupported, video_at,
};
use respan_gateway_conversation_http::fields::{media_url, merge_missing};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::Uri;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::escape;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Reach, Upstream, native::Json};
use respan_gateway_media::video::{Create, Input, Job, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "together-videos";

type Key = crate::Own<TogetherVideos>;

/// What starts the id of a job of Together's: its own ids are bare UUIDs.
pub(crate) const PREFIX: &str = "together_";

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
/// A job's id is Together's after `together_`. It is `queued`, `in_progress`,
/// `completed` once its video is made, or `failed` with the error Together
/// gives; `cancelled` is failed, as that, and a state it adds the format's
/// own. Its video is the `video_url`
/// of its `outputs` ([`Job::videos`]), and the `cost` there, what it
/// charged, the job's native `cost`. Content is a `GET` of that URL, needed in
/// the request ([`LocateContent`](respan_gateway_media::video::LocateContent)
/// finds it), with no key, on Together's own hosts (`together.ai`, `together.xyz`) alone.
/// Another format's operation, such as OpenAI's remix, delete, list or
/// thumbnail, is refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TogetherVideos;

impl TogetherVideos {
    /// The format's name, `together-videos`.
    pub const NAME: &'static str = NAME;
}

/// Where it serves its jobs' videos: Together's own hosts, over HTTPS.
fn video_host(uri: &Uri) -> bool {
    on_hosts(uri, &["together.ai", "together.xyz"])
}

/// A bearer key, over HTTP, beside its base URL; a video is fetched where
/// its job names it, with no key.
impl Upstream for TogetherVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn reach(&self) -> Reach {
        Reach::Http {
            follow: Some(video_host),
        }
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
        Ok(bind_jobs(io, NAME, call, read_job))
    }
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    match input {
        Input::Create(create) => posts(head, VIDEOS, &create_body(create)?),
        Input::Retrieve { id } => {
            let id = job_of("Together", PREFIX, id)?;
            gets(head, &format!("{VIDEOS}/{}", escape(id)))
        }
        Input::Content { url, .. } => fetches(head, "Together", url.as_deref(), video_host),
        _ => Err(unsupported("Together", input)),
    }
}

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Map<String, Value>, Fault> {
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
        let (width, height) = dimensions(size).ok_or_else(|| {
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
    Ok(v)
}

/// The job of an answer `raw`.
fn read_job(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let id = v["id"].as_str().ok_or_else(|| unreadable("has no id"))?;
    let state = v["status"].as_str();
    let status = match state {
        Some("queued") => Status::Queued,
        Some("in_progress") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed" | "cancelled") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = v["model"].as_str().map(str::to_owned);
    job.seconds = v["seconds"].as_str().and_then(|s| s.parse().ok());
    job.size = v["size"].as_str().map(str::to_owned);
    job.error = match state {
        Some(ended @ "cancelled") => ended_as(job_error(&v["error"]), ended),
        _ => job_error(&v["error"]),
    };
    let outputs = &v["outputs"];
    job.videos = (outputs["video_url"].as_str().map(video_at).into_iter()).collect();
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
    use respan_gateway_conversation::{Media, MediaSource};
    use respan_gateway_http::http::{self, Method};
    use respan_gateway_media::video::JobError;

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
        assert_eq!(
            Value::Object(body),
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
    fn a_job_is_polled_by_its_id_and_its_content_fetched_at_its_url() {
        let mut head = head();
        let retrieve = Input::Retrieve {
            id: "together_01a1-ecb7".into(),
        };
        call(&mut head, &retrieve).expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/v2/videos/01a1-ecb7");
        // Its content is fetched where its job names it, on its own hosts alone.
        let content = Input::Content {
            id: "together_01a1-ecb7".into(),
            url: Some("https://api.together.ai/v.mp4?sig=1".into()),
        };
        call(&mut head, &content).expect("a call");
        assert_eq!(head.uri, "https://api.together.ai/v.mp4?sig=1");
        assert!(video_host(&head.uri));
        assert!(!video_host(&Uri::from_static(
            "https://evil.example/api.together.ai"
        )));
        assert!(!video_host(&Uri::from_static(
            "http://api.together.ai/v.mp4"
        )));
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
