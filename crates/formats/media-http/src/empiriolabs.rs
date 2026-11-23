//! EmpirioLabs AI's video generation, over HTTP, a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`EmpirioLabsVideos`]; and
//! its images, OpenAI's but that it may answer one as a job
//! ([`EmpirioLabsImages`]).

use crate::edge::{
    Step, bind_jobs, bind_waited, dimensions, ended_as, gets, job_error, job_in, job_of,
    nearest_ratio, posts, unsupported, video_at,
};
use crate::openai::{OpenAiImages, images};
use respan_gateway_conversation_http::fields::{invalid, media_url, merge_missing, set};
use respan_gateway_conversation_http::read_provider_error;
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::{self, StatusCode};
use respan_gateway_http::path::escape;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, native::Json};
use respan_gateway_media::image;
use respan_gateway_media::video::{Create, Input, Job, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "empiriolabs-videos";

type Key = crate::Own<EmpirioLabsVideos>;

/// What starts the id of a job of EmpirioLabs AI's: its own ids are bare UUIDs.
pub(crate) const PREFIX: &str = "empiriolabs_";

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
/// `cancelled` is failed, as that. Its
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
        Ok(bind_jobs(io, NAME, call, read_job))
    }
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    match input {
        Input::Create(create) => posts(head, "/v1/videos/generations", &create_body(create)?),
        Input::Retrieve { id } => {
            let id = job_of("EmpirioLabs AI", PREFIX, id)?;
            gets(head, &format!("/v1/jobs/{}", escape(id)))
        }
        _ => Err(unsupported("EmpirioLabs AI", input)),
    }
}

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Map<String, Value>, Fault> {
    let model = create.model.as_deref();
    let model =
        model.ok_or_else(|| Fault::bad_request("a video needs a model, such as pixverse-v5"))?;
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("prompt".into(), create.prompt.clone().into());
    set(&mut v, "duration", create.seconds);
    if let Some(size) = &create.size {
        let (resolution, aspect) = shape_of(size)?;
        v.insert("resolution".into(), resolution.into());
        v.insert("aspect_ratio".into(), aspect.into());
    }
    set(&mut v, "image", create.reference.as_ref().map(media_url));
    merge_missing(&mut v, create.native.fields::<Key>());
    Ok(v)
}

/// Its resolution and aspect ratio of a size `WxH`: the resolution is the
/// short side's, the aspect ratio the documented one nearest (854x480 is
/// 16:9).
fn shape_of(size: &str) -> Result<(String, String), Fault> {
    let shape = dimensions(size).and_then(|(w, h)| {
        let (a, b) = nearest_ratio(w, h, &ASPECTS)?;
        let short = w.min(h);
        (SHORT_SIDES.contains(&short)).then(|| (format!("{short}p"), format!("{a}:{b}")))
    });
    shape.ok_or_else(|| {
        invalid(
            "size",
            "is not one EmpirioLabs AI makes: a short side of 360, 480, 540, 720, 768 or 1080 pixels and a shape of 16:9, 9:16, 1:1, 4:3, 3:4, 3:2, 2:3 or 21:9",
        )
    })
}

/// The job of an answer `raw`.
fn read_job(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let id = v["job_id"]
        .as_str()
        .ok_or_else(|| unreadable("has no id"))?;
    let state = v["status"].as_str();
    let status = match state {
        Some("processing") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed" | "cancelled") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable("has no status")),
    };
    let mut job = Job::new(format!("{PREFIX}{id}"), status);
    job.model = v["model"].as_str().map(str::to_owned);
    job.error = match state {
        Some(ended @ "cancelled") => ended_as(job_error(&v["error"]), ended),
        _ => job_error(&v["error"]),
    };
    let result = &v["result"];
    let data = result["data"].as_array().map_or(&[][..], Vec::as_slice);
    job.seconds = (data.first()).and_then(|d| u32::try_from(d["duration_seconds"].as_u64()?).ok());
    job.videos = (data.iter())
        .filter_map(|d| d["url"].as_str())
        .map(video_at)
        .collect();
    if let Some(cost) = result["usage"].get("cost_usd").filter(|c| c.is_number()) {
        job.native = Native::json::<Key>(json!({ "cost_usd": cost }));
    }
    out.push(Output::Job(job));
    Ok(())
}

/// OpenAI's images as EmpirioLabs AI serves them, a provider format of
/// OpenAI's name, `openai-images`: an image it did not finish within its
/// wait, about four minutes, or was not asked to wait for (`sync: false`),
/// it answers as a job, polled at `GET /jobs/{id}`, beside the images'
/// path, until it ends, within the call. The job's `result` is the answer
/// it would have given, and a job that failed a 502 of the error it gives.
/// It streams no images, so its answer is read whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmpirioLabsImages;

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for EmpirioLabsImages {
    fn name(&self) -> &str {
        OpenAiImages::NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, image::Output, image::Input> for EmpirioLabsImages {
    fn name(&self) -> &str {
        OpenAiImages::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<image::Output, image::Input>, Fault> {
        let call = |head: &mut RequestHead, input: &image::Input| images::post(head, input, false);
        Ok(bind_waited(
            io,
            OpenAiImages::NAME,
            call,
            poll_image,
            read_image,
        ))
    }
}

/// The poll of an image's job: beside the images' path, whose base URL has
/// the version.
fn poll_image(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    match input {
        Input::Retrieve { id } => {
            let id = job_of("EmpirioLabs AI", PREFIX, id)?;
            gets(head, &format!("/jobs/{}", escape(id)))
        }
        _ => Err(unsupported("EmpirioLabs AI", input)),
    }
}

/// What an image answer `raw` says: OpenAI's images, or, of a job, its
/// result's once it completed, its failure, or that it has not ended.
fn read_image(raw: Bytes) -> Result<Step<image::Output>, Fault> {
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let mut out = Vec::new();
    if v["job_id"].is_null() {
        images::decode_whole(raw, &mut out)?;
        return Ok(Step::Done(out));
    }
    let job = job_in(read_job, raw)?;
    match job.status {
        Status::InProgress => Ok(Step::Wait(job.id)),
        Status::Completed => {
            images::decode_whole(Bytes::from(v["result"].to_string()), &mut out)?;
            Ok(Step::Done(out))
        }
        Status::Failed => {
            let said = json!({ "error": v["error"] }).to_string();
            let mut answer = http::Response::new(Bytes::from(said));
            *answer.status_mut() = StatusCode::BAD_GATEWAY;
            Err(read_provider_error(answer))
        }
        _ => Err(Fault::upstream(
            "EmpirioLabs AI's job is in no state it documents",
        )),
    }
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
    fn a_creation_is_a_job_of_a_duration_an_image_and_a_size() {
        let mut create = Create::new("a wave");
        create.model = Some("pixverse-v5".into());
        create.seconds = Some(5);
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        ));
        let body = create_body(&create).expect("written");
        assert_eq!(
            Value::Object(body),
            json!({ "model": "pixverse-v5", "prompt": "a wave", "duration": 5,
                "image": "https://a.example/cat.png" })
        );
        create.size = Some("854x480".into());
        create.native = EmpirioLabsVideos::with_fields(&create.native, |f| {
            f.insert("resolution".into(), "1080p".into());
            f.insert("mode".into(), "t2v".into());
        });
        let body = create_body(&create).expect("written");
        assert_eq!(
            (&body["resolution"], &body["aspect_ratio"], &body["mode"]),
            (&json!("480p"), &json!("16:9"), &json!("t2v")),
            "the size wins, the native fills"
        );
        for size in ["1000x1000", "1280x721x", "wide"] {
            create.size = Some(size.into());
            assert!(create_body(&create).is_err(), "{size}");
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
