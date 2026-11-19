//! DashScope's video synthesis, a task made and polled: a provider format
//! of the [`video`](respan_gateway_media::video) kind, [`DashScopeVideos`].

use super::says_error;
use crate::edge::{bind_jobs, ended_as, gets, job_error, job_of, posts, unsupported, video_at};
use respan_gateway_conversation_http::fields::{media_url, merge_missing, set};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{HeaderName, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::escape;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, native::Json};
use respan_gateway_media::video::{Create, Input, Job, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "dashscope-videos";

type Key = crate::Own<DashScopeVideos>;

/// What starts the id of a job of DashScope's: its own ids are bare UUIDs.
pub(crate) const PREFIX: &str = "dashscope_";

/// Where a job is made, below the base URL.
const SYNTHESIS: &str = "/services/aigc/video-generation/video-synthesis";

/// Alibaba Cloud Model Studio's video generation (Wan) on DashScope's own
/// API: `POST` to the synthesis service, asked to run as a task
/// (`X-DashScope-Async: enable`), and `GET /tasks/{id}` polls it. A
/// provider's base URL is its region's API, with the version,
/// `https://dashscope-intl.aliyuncs.com/api/v1`.
///
/// A creation needs a model (`wan2.2-t2v-plus`). Its prompt and reference
/// image, a URL or a data URL of its bytes, are the `input`'s `prompt` and
/// `img_url`; its `seconds` and `size` (written `832*480`) the
/// `parameters`' `duration` and `size`. Its native fields of this format
/// are the body's, an `input` or `parameters` object laid into those
/// field by field, such as `negative_prompt` or `resolution`.
///
/// A job's id is DashScope's task after `dashscope_`. `PENDING` is queued,
/// `RUNNING` in progress, `SUCCEEDED` completed and `FAILED` failed with
/// the `code` and `message` it gives; `CANCELED` and `UNKNOWN`, a task
/// gone, are failed, as that. Its video is the `video_url` of its output, valid for 24
/// hours, and the `usage` it reports the job's native `usage`. It serves
/// the video only there, so content, like another format's operation, such
/// as OpenAI's remix, delete, list or thumbnail, is refused, 400, before
/// any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DashScopeVideos;

impl DashScopeVideos {
    /// The format's name, `dashscope-videos`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for DashScopeVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for DashScopeVideos {
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
        Input::Create(create) => {
            let call = posts(head, SYNTHESIS, &create_body(create)?)?;
            let asynchronous = HeaderValue::from_static("enable");
            (head.headers).insert(HeaderName::from_static("x-dashscope-async"), asynchronous);
            Ok(call)
        }
        Input::Retrieve { id } => {
            let id = job_of("DashScope", PREFIX, id)?;
            gets(head, &format!("/tasks/{}", escape(id)))
        }
        _ => Err(unsupported("DashScope", input)),
    }
}

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Map<String, Value>, Fault> {
    let model = create.model.as_deref();
    let model = model
        .ok_or_else(|| Fault::bad_request("a video needs a model, such as wan2.2-t2v-plus"))?;
    let mut input = Map::new();
    input.insert("prompt".into(), create.prompt.clone().into());
    set(
        &mut input,
        "img_url",
        create.reference.as_ref().map(media_url),
    );
    let mut parameters = Map::new();
    set(&mut parameters, "duration", create.seconds);
    let size = create.size.as_ref().map(|size| size.replace('x', "*"));
    set(&mut parameters, "size", size);
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("input".into(), input.into());
    if !parameters.is_empty() {
        v.insert("parameters".into(), parameters.into());
    }
    merge_missing(&mut v, create.native.fields::<Key>());
    Ok(v)
}

/// The job of an answer `raw`: a task made or polled.
fn read_job(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    if says_error(&v) {
        return Err(super::said_error(raw, &v));
    }
    let task = &v["output"];
    let id = task["task_id"]
        .as_str()
        .ok_or_else(|| unreadable("has no id"))?;
    let state = task["task_status"].as_str();
    let status = match state {
        Some("PENDING") => Status::Queued,
        Some("RUNNING") => Status::InProgress,
        Some("SUCCEEDED") => Status::Completed,
        Some("FAILED" | "CANCELED" | "UNKNOWN") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = v["model"].as_str().map(str::to_owned);
    let usage = &v["usage"];
    job.seconds = usage["video_duration"]
        .as_u64()
        .and_then(|s| s.try_into().ok());
    job.size = usage["video_ratio"]
        .as_str()
        .map(|size| size.replace('*', "x"));
    job.error = match state {
        Some(ended @ ("CANCELED" | "UNKNOWN")) => ended_as(job_error(task), ended),
        _ => job_error(task),
    };
    job.videos = (task["video_url"].as_str().map(video_at).into_iter()).collect();
    if usage.is_object() {
        job.native = Native::json::<Key>(json!({ "usage": usage }));
    }
    out.push(Output::Job(job));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Natives;
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
    fn a_creation_is_an_async_task_of_an_input_and_its_parameters() {
        let mut create = Create::new("a wave");
        create.model = Some("wan2.2-t2v-plus".into());
        create.seconds = Some(5);
        create.size = Some("832x480".into());
        create.native = DashScopeVideos::with_fields(&Native::default(), |fields| {
            fields.insert("input".into(), json!({ "negative_prompt": "blur" }));
            fields.insert("parameters".into(), json!({ "seed": 7, "duration": 9 }));
        });
        let mut head = head();
        call(&mut head, &Input::Create(create.clone())).expect("a call");
        assert_eq!(head.method, Method::POST);
        assert_eq!(head.uri.path(), SYNTHESIS);
        assert_eq!(head.headers["x-dashscope-async"], "enable");
        let body = create_body(&create).expect("written");
        assert_eq!(
            Value::Object(body),
            json!({ "model": "wan2.2-t2v-plus",
                "input": { "prompt": "a wave", "negative_prompt": "blur" },
                "parameters": { "duration": 5, "size": "832*480", "seed": 7 } })
        );
        create.model = None;
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_job_is_polled_by_its_id_and_its_content_refused() {
        let mut head = head();
        let retrieve = Input::Retrieve {
            id: "dashscope_1e8f-7dff".into(),
        };
        call(&mut head, &retrieve).expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/tasks/1e8f-7dff");
        let content = Input::Content {
            id: "dashscope_1e8f-7dff".into(),
            url: None,
        };
        let refused = call(&mut head, &content).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let made = json!({ "output": { "task_id": "1e8f", "task_status": "PENDING" } });
        let [Output::Job(job)] = &jobs(made)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.id.as_str(), job.status.clone()),
            ("dashscope_1e8f", Status::Queued)
        );

        let done = json!({ "model": "wan2.2-t2v-plus",
            "output": { "task_id": "1e8f", "task_status": "SUCCEEDED",
                "video_url": "https://a.example/v.mp4?Expires=1" },
            "usage": { "video_duration": 5, "video_ratio": "832*480", "video_count": 1 } });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(job.video_url(), Some("https://a.example/v.mp4?Expires=1"));
        assert_eq!(
            (job.seconds, job.size.as_deref()),
            (Some(5), Some("832x480"))
        );
        assert_eq!(
            DashScopeVideos::fields(&job.native)
                .get("usage")
                .map(|u| u["video_count"].clone()),
            Some(json!(1))
        );

        let failed = json!({ "output": { "task_id": "1e8f", "task_status": "FAILED",
            "code": "InvalidParameter", "message": "size is not in the correct format." } });
        let [Output::Job(job)] = &jobs(failed)[..] else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Failed);
        assert_eq!(
            job.error,
            Some(JobError::new(
                Some("InvalidParameter".into()),
                "size is not in the correct format."
            ))
        );
    }
}
