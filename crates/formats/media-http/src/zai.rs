//! Z.ai's video generation, over HTTP: a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`ZaiVideos`].

use crate::edge::{bind_provider, sends};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{media_url, merge_missing};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{Method, request::Parts as RequestHead};
use respan_gateway_http::path::{escape, set_path};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::video::{Create, Input, Job, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "zai-videos";

type Key = crate::Own<ZaiVideos>;

/// What starts the id of a job of Z.ai's: its own ids are bare numbers.
const PREFIX: &str = "zai_";

/// Where its jobs are made, at the provider's base.
const CREATE: &str = "/videos/generations";

/// Where its jobs are polled, at the provider's base.
const RESULT: &str = "/async-result";

/// Z.ai's video generation (CogVideoX): `POST /videos/generations` makes a
/// job and `GET /async-result/{id}` polls it, at the API's version, its base
/// URL `https://api.z.ai/api/paas/v4`.
///
/// A creation needs a model (`cogvideox-3`); its `seconds` go as `duration`,
/// its `size` as `size`, as they are, its reference image, a URL or a data
/// URL of its bytes, as `image_url`, and its native fields of this format,
/// such as `quality`, `with_audio` or `fps`, as they are.
///
/// A job's id is Z.ai's after `zai_`. It is `PROCESSING`, `SUCCESS` once its
/// video is made, or `FAIL`; a state it adds is the format's own. Its video
/// is the `url` of its `video_result` ([`Job::videos`]). It serves the video
/// only there, so content, like another format's operation, such as OpenAI's
/// remix, delete, list or thumbnail, is refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZaiVideos;

impl ZaiVideos {
    /// The format's name, `zai-videos`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for ZaiVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for ZaiVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, _| {
            Some(Box::new(Whole::new(read_job)) as Box<dyn Decoder<Output>>)
        }))
    }
}

/// An operation Z.ai has no form for, refused.
fn unsupported(input: &Input) -> Fault {
    let what = match input {
        Input::Native { .. } => "no video operation of another format",
        Input::Content { .. } => {
            "no video content: it serves a video at the `url` of its job's `video_result`"
        }
        _ => "no video operation of this kind",
    };
    Fault::bad_request(format!("Z.ai has {what}")).with(ErrorCode("unsupported".into()))
}

/// Z.ai's id of a job `id`, if it is one of this format's.
fn job_of(id: &str) -> Result<&str, Fault> {
    let job = id.strip_prefix(PREFIX).filter(|job| !job.is_empty());
    job.ok_or_else(|| Fault::not_found(format!("no Z.ai video job is {id:?}")))
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let (method, path, body) = match input {
        Input::Create(create) => (Method::POST, CREATE.to_owned(), Some(create_body(create)?)),
        Input::Retrieve { id } => {
            let path = format!("{RESULT}/{}", escape(job_of(id)?));
            (Method::GET, path, None)
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

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Bytes, Fault> {
    let model = create.model.as_deref();
    let model =
        model.ok_or_else(|| Fault::bad_request("a video needs a model, such as cogvideox-3"))?;
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("prompt".into(), create.prompt.clone().into());
    if let Some(seconds) = create.seconds {
        v.insert("duration".into(), seconds.into());
    }
    if let Some(size) = &create.size {
        v.insert("size".into(), size.as_str().into());
    }
    if let Some(reference) = &create.reference {
        v.insert("image_url".into(), json!([media_url(reference)]));
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
    let status = match v["task_status"].as_str() {
        Some("PROCESSING") => Status::InProgress,
        Some("SUCCESS") => Status::Completed,
        Some("FAIL") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no task_status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = v["model"].as_str().map(str::to_owned);
    job.videos = (v["video_result"].as_array().into_iter().flatten())
        .filter_map(|video| video["url"].as_str())
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
    fn a_creation_is_a_job_of_a_duration_a_size_and_an_image() {
        let mut create = Create::new("a wave");
        create.model = Some("cogvideox-3".into());
        create.seconds = Some(5);
        create.size = Some("1280x720".into());
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        ));
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            body,
            json!({ "model": "cogvideox-3", "prompt": "a wave", "duration": 5,
                "size": "1280x720", "image_url": ["https://a.example/cat.png"] })
        );
        create.model = None;
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_job_is_polled_by_its_id_and_its_content_refused() {
        let mut head = head();
        let retrieve = Input::Retrieve {
            id: "zai_8868".into(),
        };
        call(&mut head, &retrieve).expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/async-result/8868");
        let content = Input::Content {
            id: "zai_8868".into(),
            url: None,
        };
        let refused = call(&mut head, &content).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
        // An id not of this format's is no job of Z.ai's.
        let foreign = Input::Retrieve {
            id: "video_1".into(),
        };
        assert!(call(&mut head, &foreign).is_err());
        // Another format's operation, such as OpenAI's remix, is refused before a call.
        let remix = Input::Native {
            id: Some("zai_1".into()),
            native: Native::json::<()>(json!({"method": "POST"})),
        };
        let refused = call(&mut head, &remix).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let running = json!({ "model": "cogvideox-3", "id": "8868", "request_id": "r1",
            "task_status": "PROCESSING" });
        let [Output::Job(job)] = &jobs(running)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.id.as_str(), job.status.clone(), job.model.as_deref()),
            ("zai_8868", Status::InProgress, Some("cogvideox-3"))
        );

        let done = json!({ "model": "cogvideox-3", "id": "8868", "task_status": "SUCCESS",
            "usage": { "completion_tokens": 0, "prompt_tokens": 0, "total_tokens": 0 },
            "video_result": [{ "url": "https://a.example/v.mp4",
                "cover_image_url": "https://a.example/c.png" }] });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Completed);
        assert_eq!(job.video_url(), Some("https://a.example/v.mp4"));

        let failed = json!({ "id": "8868", "task_status": "FAIL" });
        let [Output::Job(job)] = &jobs(failed)[..] else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Failed);
    }
}
