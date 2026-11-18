//! MiniMax's video generation, `/v2/video_generation`: a provider format
//! of the [`video`](respan_gateway_media::video) kind, [`MiniMaxVideos`].

use crate::edge::{bind_provider, sends};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{media_url, merge_missing};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{Method, request::Parts as RequestHead};
use respan_gateway_http::path::{escape, set_path};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "minimax-videos";

type Key = crate::Own<MiniMaxVideos>;

/// What starts the id of a job of MiniMax's: its own ids are bare numbers.
const PREFIX: &str = "minimax_";

/// Where its jobs are, at the provider's base.
const CREATE: &str = "/v2/video_generation";
const QUERY: &str = "/v2/query/video_generation";

/// MiniMax's video generation: `POST /v2/video_generation` makes a job and
/// `GET /v2/query/video_generation/{id}` polls it, at the API's host, its
/// base URL with no version.
///
/// A creation needs a model (`MiniMax-H3-Lite`); its `seconds` go as its
/// `duration`, its prompt as a `text` of its `content`, its reference image
/// as the `first_frame` image there, its `size` as a `ratio` and a
/// `resolution` (`480P` or `768P` by its short side, the ratio the one of
/// MiniMax's it is, to a pixel's rounding), and its native fields of this
/// format, such as `resolution`, `ratio`, `generate_audio` or `extra`, as
/// they are, where the `size` has not said them. Any other `size` is refused,
/// 400, before any call; with none, MiniMax's own answer says what is missing.
///
/// A job's id is MiniMax's after `minimax_`. It is `queued`, `running`,
/// `succeeded` once its video is made, or `failed` with the error MiniMax
/// gives; `cancelled` is the format's own. Its video is the `content.url` of
/// its task ([`Job::videos`]), a time-limited URL, and its `usage`, what it
/// bills, the job's native `usage`. It serves the video only there, so
/// content, like another format's operation, such as OpenAI's remix, delete,
/// list or thumbnail, is refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MiniMaxVideos;

impl MiniMaxVideos {
    /// The format's name, `minimax-videos`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for MiniMaxVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for MiniMaxVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, _| {
            Some(Box::new(Whole::new(read_job)) as Box<dyn Decoder<Output>>)
        }))
    }
}

/// A request MiniMax has no form for, refused.
fn unsupported(what: &str) -> Fault {
    Fault::bad_request(format!("MiniMax has {what}")).with(ErrorCode("unsupported".into()))
}

/// MiniMax's id of a job `id`, if it is one of this format's.
fn job_of(id: &str) -> Result<&str, Fault> {
    let job = id.strip_prefix(PREFIX).filter(|job| !job.is_empty());
    job.ok_or_else(|| Fault::not_found(format!("no MiniMax video job is {id:?}")))
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let (method, path, body) = match input {
        Input::Create(create) => (Method::POST, CREATE.to_owned(), Some(create_body(create)?)),
        Input::Retrieve { id } => (
            Method::GET,
            format!("{QUERY}/{}", escape(job_of(id)?)),
            None,
        ),
        Input::Content { .. } => {
            return Err(unsupported(
                "no video content: it serves a video at the `content.url` of its task",
            ));
        }
        Input::Native { .. } => return Err(unsupported("no video operation of another format")),
        _ => return Err(unsupported("no video operation of this kind")),
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
    let model = model
        .ok_or_else(|| Fault::bad_request("a video needs a model, such as MiniMax-H3-Lite"))?;
    let mut content = vec![json!({ "type": "text", "text": create.prompt })];
    if let Some(reference) = &create.reference {
        let image = json!({ "url": media_url(reference) });
        content.push(json!({ "type": "image_url", "image_url": image, "role": "first_frame" }));
    }
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("content".into(), content.into());
    if let Some(seconds) = create.seconds {
        v.insert("duration".into(), seconds.into());
    }
    if let Some(size) = &create.size {
        let (ratio, resolution) = ratio_of(size)?;
        v.insert("ratio".into(), ratio.into());
        v.insert("resolution".into(), resolution.into());
    }
    merge_missing(&mut v, create.native.fields::<Key>());
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))
}

/// MiniMax's ratio and resolution of a size `WxH`: the short side is 480 or
/// 768, and the ratio one of MiniMax's, within the 2% a rounded pixel makes.
fn ratio_of(size: &str) -> Result<(&'static str, &'static str), Fault> {
    let parsed = size.split_once(['x', 'X']).and_then(|(w, h)| {
        let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
        (w > 0 && h > 0).then_some((f64::from(w), f64::from(h)))
    });
    let (w, h) = parsed.ok_or_else(|| refused(size))?;
    let resolution = match w.min(h) as u32 {
        480 => "480P",
        768 => "768P",
        _ => return Err(refused(size)),
    };
    let ratios = [
        ("21:9", 21.0 / 9.0),
        ("16:9", 16.0 / 9.0),
        ("4:3", 4.0 / 3.0),
    ]
    .into_iter()
    .chain([("1:1", 1.0), ("3:4", 0.75), ("9:16", 9.0 / 16.0)]);
    let ratio = ratios
        .into_iter()
        .find(|(_, r)| (w / h - r).abs() <= r * 0.02);
    ratio
        .map(|(ratio, _)| (ratio, resolution))
        .ok_or_else(|| refused(size))
}

fn refused(size: &str) -> Fault {
    unsupported(&format!(
        "no 'size' {size}: it takes a short side of 480 or 768 pixels and a ratio of 21:9, 16:9, 4:3, 1:1, 3:4 or 9:16"
    ))
}

/// The job of an answer `raw`: a creation's `{task_id}`, or a poll's task,
/// nested in `task` or, as MiniMax's `flat` format has it, not.
fn read_job(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    if let Some(id) = v["task_id"].as_str() {
        out.push(Output::Job(Job::new(
            format!("{PREFIX}{id}"),
            Status::Queued,
        )));
        return Ok(());
    }
    let task = v.get("task").unwrap_or(&v);
    let id = task["id"].as_str().ok_or_else(|| unreadable("has no id"))?;
    let status = match task["status"].as_str() {
        Some("queued") => Status::Queued,
        Some("running") => Status::InProgress,
        Some("succeeded") => Status::Completed,
        Some("failed") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = task["model"].as_str().map(str::to_owned);
    job.seconds = task["duration"]
        .as_u64()
        .and_then(|s| u32::try_from(s).ok());
    let error = &task["error"];
    job.error = error["message"].as_str().map(|message| {
        let code = error["code"].as_str().map(str::to_owned);
        JobError::new(code, message)
    });
    job.videos = (task["content"]["url"].as_str().into_iter())
        .map(|url| Media::new("video/mp4", MediaSource::Url(url.to_owned())))
        .collect();
    if let Some(usage) = task.get("usage").filter(|usage| usage.is_object()) {
        job.native = Native::json::<Key>(json!({ "usage": usage }));
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
    fn a_creation_is_a_prompt_a_duration_a_size_and_a_first_frame() {
        let mut create = Create::new("a wave");
        create.model = Some("MiniMax-H3-Lite".into());
        create.seconds = Some(4);
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        ));
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            body,
            json!({ "model": "MiniMax-H3-Lite", "duration": 4,
                "content": [{ "type": "text", "text": "a wave" },
                    { "type": "image_url", "role": "first_frame",
                        "image_url": { "url": "https://a.example/cat.png" } }] })
        );
        create.size = Some("864x480".into());
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            (&body["ratio"], &body["resolution"]),
            (&json!("16:9"), &json!("480P"))
        );
        for size in ["768x1024", "768x768"] {
            assert!(
                create_body(&{
                    let mut c = create.clone();
                    c.size = Some(size.into());
                    c
                })
                .is_ok()
            );
        }
        // A size that is no combination of MiniMax's is refused, not snapped.
        for size in ["1280x720", "480x1000", "wide"] {
            create.size = Some(size.into());
            let refused = create_body(&create).expect_err("refused");
            assert_eq!(refused.code(), "unsupported", "{size}");
        }
        create.size = None;
        create.model = None;
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_job_is_polled_by_its_id_and_its_content_refused() {
        let mut head = head();
        let retrieve = Input::Retrieve {
            id: "minimax_4240".into(),
        };
        call(&mut head, &retrieve).expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/v2/query/video_generation/4240");
        let content = Input::Content {
            id: "minimax_4240".into(),
            url: None,
        };
        let refused = call(&mut head, &content).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
        // An id not of this format's is no job of MiniMax's.
        let foreign = Input::Retrieve {
            id: "video_1".into(),
        };
        assert!(call(&mut head, &foreign).is_err());
        // Another format's operation, such as OpenAI's remix, is refused before a call.
        let remix = Input::Native {
            id: Some("minimax_1".into()),
            native: Native::json::<()>(json!({"method": "POST"})),
        };
        let refused = call(&mut head, &remix).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let [Output::Job(job)] = &jobs(json!({ "task_id": "4240" }))[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.id.as_str(), job.status.clone()),
            ("minimax_4240", Status::Queued)
        );

        let running = json!({ "task": { "id": "4240", "model": "MiniMax-H3-Lite",
            "status": "running", "duration": 4, "resolution": "480P" } });
        let [Output::Job(job)] = &jobs(running)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.status.clone(), job.model.as_deref(), job.seconds),
            (Status::InProgress, Some("MiniMax-H3-Lite"), Some(4))
        );

        let done = json!({ "task": { "id": "4240", "status": "succeeded", "duration": 4,
            "content": { "url": "https://cdn.example/out.mp4" },
            "usage": { "output_seconds": 4 } } });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(job.video_url(), Some("https://cdn.example/out.mp4"));
        assert_eq!(
            MiniMaxVideos::fields(&job.native).get("usage"),
            Some(&json!({ "output_seconds": 4 }))
        );

        let failed = json!({ "task": { "id": "4240", "status": "failed",
            "error": { "code": "1026", "message": "sensitive content" } } });
        let [Output::Job(job)] = &jobs(failed)[..] else {
            panic!("a job");
        };
        assert_eq!(job.status, Status::Failed);
        assert_eq!(
            job.error,
            Some(JobError::new(Some("1026".into()), "sensitive content"))
        );
    }
}
