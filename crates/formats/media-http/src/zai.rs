//! Z.ai's video generation, over HTTP: a provider format of the
//! [`video`](respan_gateway_media::video) kind, [`ZaiVideos`].

use crate::edge::{bind_provider, gets, job_of, posts, unsupported, video_at};
use percent_encoding::percent_decode_str;
use respan_gateway_conversation_http::fields::{media_url, merge_missing, set};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::escape;
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::video::{Create, Input, Job, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "zai-videos";

type Key = crate::Own<ZaiVideos>;

/// What starts the id of a job of Z.ai's: its own ids are bare numbers.
pub(crate) const PREFIX: &str = "zai_";

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
        Ok(bind_provider(io, NAME, call, |_, sent| {
            // A poll's answer need not repeat its id: it is the one asked for.
            let asked = (sent.uri().path().rsplit_once("/async-result/"))
                .map(|(_, id)| percent_decode_str(id).decode_utf8_lossy().into_owned());
            let read = move |raw, out: &mut Vec<Output>| read_job(raw, asked.as_deref(), out);
            Some(Box::new(Whole::new(read)) as Box<dyn Decoder<Output>>)
        }))
    }
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    match input {
        Input::Create(create) => posts(head, "/videos/generations", &create_body(create)?),
        Input::Retrieve { id } => {
            let id = job_of("Z.ai", PREFIX, id)?;
            gets(head, &format!("/async-result/{}", escape(id)))
        }
        _ => Err(unsupported("Z.ai", input)),
    }
}

/// A creation's JSON body.
fn create_body(create: &Create) -> Result<Map<String, Value>, Fault> {
    let model = create.model.as_deref();
    let model =
        model.ok_or_else(|| Fault::bad_request("a video needs a model, such as cogvideox-3"))?;
    let mut v = Map::new();
    v.insert("model".into(), model.into());
    v.insert("prompt".into(), create.prompt.clone().into());
    set(&mut v, "duration", create.seconds);
    set(&mut v, "size", create.size.clone());
    if let Some(reference) = &create.reference {
        v.insert("image_url".into(), json!([media_url(reference)]));
    }
    merge_missing(&mut v, create.native.fields::<Key>());
    Ok(v)
}

/// The job of an answer `raw`, its id the one `asked` for if it says none.
fn read_job(raw: Bytes, asked: Option<&str>, out: &mut Vec<Output>) -> Result<(), Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's {NAME} job {what}"));
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let id = v["id"].as_str().or(asked);
    let id = id.ok_or_else(|| unreadable("has no id"))?;
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
        .map(video_at)
        .collect();
    out.push(Output::Job(job));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Media, MediaSource};
    use respan_gateway_http::http::{self, Method};

    fn jobs(raw: Value) -> Vec<Output> {
        let mut out = Vec::new();
        read_job(Bytes::from(raw.to_string()), Some("8868"), &mut out).expect("read");
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
        assert_eq!(
            Value::Object(body),
            json!({ "model": "cogvideox-3", "prompt": "a wave", "duration": 5,
                "size": "1280x720", "image_url": ["https://a.example/cat.png"] })
        );
        create.model = None;
        assert!(create_body(&create).is_err());
    }

    #[test]
    fn a_job_is_polled_by_its_id_and_its_content_refused() {
        let mut head = http::Request::new(()).into_parts().0;
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

        // A poll's answer is documented with no id: the job keeps the one asked for.
        let done = json!({ "model": "cogvideox-3", "request_id": "r1", "task_status": "SUCCESS",
            "video_result": [{ "url": "https://a.example/v.mp4",
                "cover_image_url": "https://a.example/c.png" }] });
        let [Output::Job(job)] = &jobs(done)[..] else {
            panic!("a job");
        };
        assert_eq!(
            (job.id.as_str(), &job.status),
            ("zai_8868", &Status::Completed)
        );
        assert_eq!(job.video_url(), Some("https://a.example/v.mp4"));
    }
}
