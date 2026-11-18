//! EmpirioLabs AI's video generation, over HTTP: a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`EmpirioLabsVideos`].

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

const NAME: &str = "empiriolabs-videos";

type Key = crate::Own<EmpirioLabsVideos>;

/// What starts the id of a job of EmpirioLabs AI's: its own ids are bare UUIDs.
const PREFIX: &str = "empiriolabs_";

/// The short sides, in pixels, of the resolutions its models document
/// (`480p`, `720p`, ...).
const SHORT_SIDES: [u32; 6] = [360, 480, 540, 720, 768, 1080];

/// The aspect ratios, width to height, its models document.
const ASPECTS: [(u32, u32); 8] = [
    (16, 9),
    (9, 16),
    (1, 1),
    (4, 3),
    (3, 4),
    (3, 2),
    (2, 3),
    (21, 9),
];

/// Where a job is made, at the provider's base.
const GENERATIONS: &str = "/v1/videos/generations";

/// Where a job is polled, at the provider's base.
const JOBS: &str = "/v1/jobs";

/// EmpirioLabs AI's video generation: `POST /v1/videos/generations` makes a
/// job and `GET /v1/jobs/{id}` polls it, at the API's host, its base URL
/// with no version.
///
/// A creation needs a model (`pixverse-v5`); its `seconds` go as its
/// `duration`, its reference image, a URL or a data URL of its bytes, as its
/// `image`, its `size` as a `resolution` and an `aspect_ratio`, and its
/// native fields of this format, such as `generate_audio` or `mode`, as
/// they are, which fill what the `size` does not say (its own wins over a
/// native `resolution` or `aspect_ratio`). A `size` whose short side is not
/// one its models document (360, 480, 540, 720, 768 or 1080) or whose shape
/// is not a documented ratio (16:9, 9:16, 1:1, 4:3, 3:4, 3:2, 2:3 or 21:9) is
/// refused, 400, before any call; a model that lacks the resolution
/// answers with its own error.
///
/// A job's id is the provider's after `empiriolabs_`. It is `processing`,
/// `completed` once its video is made, or `failed` with the error it gives;
/// `cancelled`, a state the kind has no form for, is the format's own. Its
/// videos are the `url`s of its `result`'s `data` ([`Job::videos`]), and the
/// `cost_usd` of its `usage`, what it charged, the job's native `cost_usd`.
/// It serves a video only there, so content, like another format's
/// operation, such as OpenAI's remix, delete, list or thumbnail, is
/// refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmpirioLabsVideos;

impl EmpirioLabsVideos {
    /// The format's name, `empiriolabs-videos`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
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
        Ok(bind_provider(io, NAME, call, |_, _| {
            Some(Box::new(Whole::new(read_job)) as Box<dyn Decoder<Output>>)
        }))
    }
}

/// What EmpirioLabs AI has no form for, refused.
fn unsupported(what: &str) -> Fault {
    Fault::bad_request(format!("EmpirioLabs AI has {what}")).with(ErrorCode("unsupported".into()))
}

/// The provider's id of a job `id`, if it is one of this format's.
fn job_of(id: &str) -> Result<&str, Fault> {
    let job = id.strip_prefix(PREFIX).filter(|job| !job.is_empty());
    job.ok_or_else(|| Fault::not_found(format!("no EmpirioLabs AI video job is {id:?}")))
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let (method, path, body) = match input {
        Input::Create(create) => (
            Method::POST,
            GENERATIONS.to_owned(),
            Some(create_body(create)?),
        ),
        Input::Retrieve { id } => {
            let path = format!("{JOBS}/{}", escape(job_of(id)?));
            (Method::GET, path, None)
        }
        Input::Content { .. } => {
            return Err(unsupported(
                "no video content: it serves a video at the `url` of its job's `result`",
            ));
        }
        _ => return Err(unsupported("no video operation of another format")),
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
        model.ok_or_else(|| Fault::bad_request("a video needs a model, such as pixverse-v5"))?;
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("prompt".into(), create.prompt.clone().into());
    if let Some(seconds) = create.seconds {
        v.insert("duration".into(), seconds.into());
    }
    if let Some(size) = &create.size {
        let (resolution, aspect) = shape_of(size)?;
        v.insert("resolution".into(), resolution.into());
        v.insert("aspect_ratio".into(), aspect.into());
    }
    if let Some(reference) = &create.reference {
        v.insert("image".into(), media_url(reference).into());
    }
    merge_missing(&mut v, create.native.fields::<Key>());
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))
}

/// Its resolution and aspect ratio of a size `WxH`: the resolution is the
/// short side's, the aspect ratio the documented one the size is, to a pixel's
/// rounding (854x480 is 16:9).
fn shape_of(size: &str) -> Result<(String, String), Fault> {
    let parsed = size
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
    let shape = parsed.and_then(|(w, h)| {
        let short = w.min(h);
        let (bw, bh) = *ASPECTS.iter().find(|(bw, bh)| {
            let off = (u64::from(w) * u64::from(*bh)).abs_diff(u64::from(h) * u64::from(*bw));
            off <= u64::from(*bw.max(bh))
        })?;
        SHORT_SIDES
            .contains(&short)
            .then(|| (format!("{short}p"), format!("{bw}:{bh}")))
    });
    shape.ok_or_else(|| {
        unsupported(&format!(
            "no 'size' {size:?}: it takes a width by height of a short side of 360, 480, 540, 720, 768 or 1080 pixels and a shape of 16:9, 9:16, 1:1, 4:3, 3:4, 3:2, 2:3 or 21:9"
        ))
    })
}

/// The job of an answer `raw`.
fn read_job(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let id = v["job_id"]
        .as_str()
        .ok_or_else(|| unreadable("has no id"))?;
    let status = match v["status"].as_str() {
        Some("processing") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = v["model"].as_str().map(str::to_owned);
    let error = &v["error"];
    job.error = error["message"].as_str().map(|message| {
        let code = error["code"].as_str().map(str::to_owned);
        JobError::new(code, message)
    });
    let result = &v["result"];
    let data = result["data"].as_array().map_or(&[][..], Vec::as_slice);
    job.seconds = (data.first()).and_then(|d| u32::try_from(d["duration_seconds"].as_u64()?).ok());
    job.videos = (data.iter())
        .filter_map(|d| d["url"].as_str())
        .map(|url| Media::new("video/mp4", MediaSource::Url(url.to_owned())))
        .collect();
    if let Some(cost) = result["usage"].get("cost_usd").filter(|c| c.is_number()) {
        job.native = Native::json::<Key>(json!({ "cost_usd": cost }));
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
    fn a_creation_is_a_job_of_a_duration_an_image_and_a_size() {
        let mut create = Create::new("a wave");
        create.model = Some("pixverse-v5".into());
        create.seconds = Some(5);
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        ));
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            body,
            json!({ "model": "pixverse-v5", "prompt": "a wave", "duration": 5,
                "image": "https://a.example/cat.png" })
        );
        create.size = Some("854x480".into());
        create.native = EmpirioLabsVideos::with_fields(&create.native, |f| {
            f.insert("resolution".into(), "1080p".into());
            f.insert("mode".into(), "t2v".into());
        });
        let body = create_body(&create).expect("written");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            (&body["resolution"], &body["aspect_ratio"], &body["mode"]),
            (&json!("480p"), &json!("16:9"), &json!("t2v")),
            "the size wins, the native fills"
        );
        for size in ["1000x1000", "1280x721x", "wide"] {
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
            id: "empiriolabs_f040-ec72".into(),
        };
        call(&mut head, &retrieve).expect("a call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/v1/jobs/f040-ec72");
        let content = Input::Content {
            id: "empiriolabs_f040-ec72".into(),
            url: None,
        };
        let refused = call(&mut head, &content).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
        // An id not of this format's is no job of EmpirioLabs AI's.
        let foreign = Input::Retrieve {
            id: "video_1".into(),
        };
        assert!(call(&mut head, &foreign).is_err());
        // Another format's operation, such as OpenAI's remix, is refused before a call.
        let remix = Input::Native {
            id: Some("empiriolabs_1".into()),
            native: Native::json::<()>(json!({"method": "POST"})),
        };
        let refused = call(&mut head, &remix).err().expect("refused");
        assert_eq!(refused.code(), "unsupported");
    }

    #[test]
    fn a_job_is_read_by_its_status() {
        let running = json!({ "job_id": "f040", "status": "processing",
            "endpoint": "/v1/videos/generations", "model": "pixverse-v5" });
        let [Output::Job(job)] = &jobs(running)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.id.as_str(), job.status.clone(), job.model.as_deref()),
            ("empiriolabs_f040", Status::InProgress, Some("pixverse-v5"))
        );

        let done = json!({ "job_id": "f040", "status": "completed", "result": {
            "data": [{ "url": "https://media.example/a.mp4?token=x", "duration_seconds": 5 }],
            "usage": { "cost_usd": 0.23 } } });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(job.video_url(), Some("https://media.example/a.mp4?token=x"));
        assert_eq!(job.seconds, Some(5));
        assert_eq!(
            EmpirioLabsVideos::fields(&job.native).get("cost_usd"),
            Some(&json!(0.23))
        );

        let failed = json!({ "job_id": "f040", "status": "failed",
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
