//! Google's Veo on Vertex AI: a format of the
//! [`video`](respan_gateway_media::video) kind, [`VertexVeo`].

use super::is_segment;
use super::veo::{aspect_of, size_of};
use crate::edge::{bind_provider, sends};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation_http::fields::kept;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{HeaderValue, Method, request::Parts as RequestHead};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole};
use serde_json::{Map, Value, json};
use std::sync::{Arc, OnceLock};

use respan_gateway_media::video::{Input, Job, JobError, Output, Status, Variant};

const NAME: &str = "vertex-veo";
/// What starts the id of a job of Vertex's Veo.
const PREFIX: &str = "vertex_veo_";

/// Google's Veo, as Vertex AI serves it: a provider of the video kind.
///
/// A creation is `POST /models/{model}:predictLongRunning`, which needs a
/// model; its `size` goes as an `aspectRatio` and a `resolution`, its
/// `seconds` as `durationSeconds`, its reference image as the instance's
/// base64 `image`, and its native fields of this format, such as
/// `sampleCount` or `generateAudio`, as parameters. A `storageUri` is
/// refused, 400, as a video stored in a bucket needs the bucket's own
/// credentials to be read: without one Vertex answers the video's bytes.
///
/// A job is Vertex's long-running operation, its id the operation's name
/// (`projects/{p}/locations/{l}/publishers/google/models/{m}/operations/{o}`)
/// as unpadded base64url after `vertex_veo_` ([`VertexVeo::job_id`]). Unlike
/// Gemini's, an operation is polled with a `POST` of its name to
/// `/models/{model}:fetchPredictOperation`. One not done is `in_progress`;
/// done with a video, `completed`; done with an error, or with no video as
/// when Veo's filters took them all, `failed`. Content is served from the
/// bytes the finished operation holds, so it is that operation fetched
/// again; not done is refused, 400 `video_not_ready`.
///
/// Veo cannot remix, delete or list videos, nor make a thumbnail or a
/// spritesheet: those are refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VertexVeo;

impl VertexVeo {
    /// The format's name, `vertex-veo`, which tags its native fields.
    pub const NAME: &'static str = NAME;

    /// The job id of the operation `name`.
    pub fn job_id(name: &str) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(name))
    }

    /// The operation a job `id` names, if it is one of this format's: a
    /// model's operation, its segments of letters, digits, `-`, `_` and `.`.
    pub fn operation(id: &str) -> Option<String> {
        let name = URL_SAFE_NO_PAD.decode(id.strip_prefix(PREFIX)?).ok()?;
        let name = String::from_utf8(name).ok()?;
        model_of(&name)?;
        Some(name)
    }
}

/// The model of an operation `name`, if it is a model's:
/// `projects/_/locations/_/publishers/_/models/{model}/operations/_`.
fn model_of(name: &str) -> Option<&str> {
    let segments: Vec<&str> = name.split('/').collect();
    let [
        projects,
        _,
        locations,
        _,
        publishers,
        _,
        models,
        model,
        operations,
        _,
    ] = *segments
    else {
        return None;
    };
    let shaped = projects == "projects"
        && locations == "locations"
        && publishers == "publishers"
        && models == "models"
        && operations == "operations";
    (shaped && segments.iter().all(|s| is_segment(s))).then_some(model)
}

/// Its spec's: a bearer key, over HTTP.
impl Upstream for VertexVeo {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for VertexVeo {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        // What the call was, from the input it was made of.
        let sent = Arc::new(OnceLock::new());
        let seen = Arc::clone(&sent);
        Ok(bind_provider(
            io,
            NAME,
            move |head, input| {
                let (encoder, call) = call(head, input)?;
                _ = seen.set(call);
                Ok(encoder)
            },
            move |_, _| Some(read_response(sent.get())),
        ))
    }
}

/// An operation Veo has no form for, refused.
fn unsupported(what: &str) -> Fault {
    Fault::bad_request(format!("Veo cannot {what}")).with(ErrorCode("unsupported".into()))
}

/// The operation a job `id` names, else a 404.
fn operation_of(id: &str) -> Result<String, Fault> {
    VertexVeo::operation(id).ok_or_else(|| Fault::not_found(format!("no Veo video has id {id}")))
}

/// What a call asked of Veo, which its answer is read by.
#[derive(Debug)]
enum Call {
    /// A creation, made with these `parameters`.
    Create(Value),
    /// An operation's state.
    Retrieve,
    /// A video's bytes.
    Content,
}

/// The call of `input`: its method and path, its body's encoder, and what
/// it asks.
fn call(head: &mut RequestHead, input: &Input) -> Result<(Box<dyn Encoder<Input>>, Call), Fault> {
    let (path, body, asked) = match input {
        Input::Create(create) => {
            let model = create.model.as_deref().filter(|m| is_segment(m));
            let model = model.ok_or_else(|| {
                Fault::bad_request("a Veo video needs a model, such as veo-3.0-generate-001")
            })?;
            let (body, parameters) = create_body(create)?;
            let path = format!("/models/{model}:predictLongRunning");
            (path, body, Call::Create(parameters))
        }
        Input::Retrieve { id } => (poll_path(id)?, poll_body(id)?, Call::Retrieve),
        Input::Content { id, variant, .. } => {
            if *variant != Variant::Video {
                return Err(unsupported("make a thumbnail or a spritesheet"));
            }
            (poll_path(id)?, poll_body(id)?, Call::Content)
        }
        Input::Remix { .. } => return Err(unsupported("remix a video")),
        Input::Delete { .. } => return Err(unsupported("delete a video")),
        Input::List { .. } => return Err(unsupported("list videos")),
        _ => return Err(unsupported("answer this request")),
    };
    head.method = Method::POST;
    head.uri = path
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Ok((sends(body), asked))
}

/// Where the operation of job `id` is polled.
fn poll_path(id: &str) -> Result<String, Fault> {
    let name = operation_of(id)?;
    let model = model_of(&name).unwrap_or_default();
    Ok(format!("/models/{model}:fetchPredictOperation"))
}

/// The body that polls the operation of job `id`.
fn poll_body(id: &str) -> Result<Bytes, Fault> {
    let body = json!({ "operationName": operation_of(id)? });
    Ok(Bytes::from(body.to_string()))
}

/// A creation's body: one instance, of its prompt and reference image, and
/// its parameters, which are also given apart.
fn create_body(create: &respan_gateway_media::video::Create) -> Result<(Bytes, Value), Fault> {
    let mut instance = Map::new();
    instance.insert("prompt".into(), create.prompt.clone().into());
    if let Some(reference) = &create.reference {
        let MediaSource::Bytes(bytes) = &reference.source else {
            return Err(Fault::bad_request(
                "Veo takes a reference image's bytes, not its URL",
            ));
        };
        let image =
            json!({"bytesBase64Encoded": STANDARD.encode(bytes), "mimeType": reference.mime_type});
        instance.insert("image".into(), image);
    }
    let mut parameters = Map::new();
    if let Some(size) = &create.size {
        let (aspect, resolution) = aspect_of(size)?;
        parameters.insert("aspectRatio".into(), aspect.into());
        parameters.insert("resolution".into(), resolution.into());
    }
    if let Some(seconds) = create.seconds {
        parameters.insert("durationSeconds".into(), seconds.into());
    }
    for (key, value) in kept(NAME, &create.native)? {
        if key == "storageUri" {
            return Err(Fault::bad_request(
                "a Veo video is answered as bytes: a 'storageUri' is not taken",
            )
            .with(ErrorCode("unsupported".into())));
        }
        parameters.entry(key).or_insert(value);
    }
    let parameters = Value::Object(parameters);
    let mut body = json!({ "instances": [instance] });
    if parameters.as_object().is_some_and(|p| !p.is_empty()) {
        body["parameters"] = parameters.clone();
    }
    let body = serde_json::to_vec(&body)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok((Bytes::from(body), parameters))
}

/// A provider's answer to the call `sent`, read whole.
fn read_response(sent: Option<&Call>) -> Box<dyn Decoder<Output>> {
    match sent {
        Some(Call::Content) => Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
            out.push(Output::Content(content_of(&raw)?));
            Ok(())
        })),
        Some(Call::Create(parameters)) => {
            let parameters = parameters.clone();
            Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
                out.push(Output::Job(job_of(&raw, &parameters)?));
                Ok(())
            }))
        }
        _ => Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
            out.push(Output::Job(job_of(&raw, &Value::Null)?));
            Ok(())
        })),
    }
}

/// An answer that is not what Vertex answers.
fn unreadable(what: &str) -> Fault {
    Fault::upstream(format!("the provider's {NAME} operation {what}"))
}

/// The operation `raw` as JSON.
fn parse(raw: &[u8]) -> Result<Value, Fault> {
    serde_json::from_slice(raw).map_err(|_| unreadable("is not JSON"))
}

/// The job of an operation `raw`, and of the `parameters` it was made with
/// if this answers its creation.
fn job_of(raw: &[u8], parameters: &Value) -> Result<Job, Fault> {
    let v = parse(raw)?;
    let name = v["name"]
        .as_str()
        .ok_or_else(|| unreadable("has no name"))?;
    let model = model_of(name).ok_or_else(|| unreadable("is named as no model's"))?;
    let mut job = Job::new(VertexVeo::job_id(name), Status::InProgress);
    job.model = Some(model.to_owned());
    job.seconds = parameters["durationSeconds"]
        .as_u64()
        .and_then(|s| u32::try_from(s).ok());
    if parameters["aspectRatio"].is_string() || parameters["resolution"].is_string() {
        let (aspect, resolution) = (&parameters["aspectRatio"], &parameters["resolution"]);
        job.size = size_of(aspect.as_str(), resolution.as_str());
    }
    if v["done"] != true {
        return Ok(job);
    }
    if let Some(error) = v["error"].as_object() {
        job.status = Status::Failed;
        let code = match (&error.get("status"), &error.get("code")) {
            (Some(Value::String(status)), _) => Some(status.clone()),
            (_, Some(code)) => Some(code.to_string()),
            _ => None,
        };
        let message = error.get("message").and_then(Value::as_str);
        job.error = Some(JobError::new(code, message.unwrap_or("the video failed")));
        return Ok(job);
    }
    let response = &v["response"];
    let videos = response["videos"].as_array().into_iter().flatten();
    // A video's bytes stay in the operation, which content fetches again.
    let made = videos
        .filter(|v| v["bytesBase64Encoded"].is_string() || v["gcsUri"].is_string())
        .count();
    if made == 0 {
        // Veo's filters took them all, and say why.
        job.status = Status::Failed;
        let reason = response["raiMediaFilteredReasons"][0].as_str();
        let message = reason.unwrap_or("the provider made no video");
        job.error = Some(JobError::new(Some("content_filtered".into()), message));
        return Ok(job);
    }
    job.status = Status::Completed;
    Ok(job)
}

/// The first video's bytes in a finished operation `raw`.
fn content_of(raw: &[u8]) -> Result<Bytes, Fault> {
    let v = parse(raw)?;
    if v["done"] != true {
        return Err(
            Fault::bad_request("the video is not ready").with(ErrorCode("video_not_ready".into()))
        );
    }
    let videos = v["response"]["videos"].as_array().into_iter().flatten();
    let video = videos.into_iter().next();
    let Some(video) = video else {
        return Err(unreadable("holds no video"));
    };
    let Some(data) = video["bytesBase64Encoded"].as_str() else {
        return Err(unreadable("holds the video in storage, not as bytes"));
    };
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| unreadable("holds a video that is not base64"))?;
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Media;
    use respan_gateway_http::http;
    use respan_gateway_media::video::Create;

    const NAME_OF_OP: &str = "projects/p1/locations/us-central1/publishers/google/models/veo-3.0-generate-001/operations/a1b2";

    fn sent(input: &Input) -> Result<(http::request::Parts, Value, Call), Fault> {
        let (mut head, ()) = http::Request::new(()).into_parts();
        let (mut encoder, call) = call(&mut head, input)?;
        let mut out = Vec::new();
        encoder.encode(input, &mut out)?;
        let body = out.concat();
        Ok((
            head,
            serde_json::from_slice(&body).unwrap_or_default(),
            call,
        ))
    }

    fn create(prompt: &str) -> Create {
        let mut create = Create::new(prompt);
        create.model = Some("veo-3.0-generate-001".into());
        create
    }

    #[test]
    fn a_creation_is_a_long_running_prediction_of_the_model() {
        let mut c = create("a fox");
        c.size = Some("1920x1080".into());
        c.seconds = Some(8);
        c.reference = Some(Media::new(
            "image/png",
            MediaSource::Bytes(vec![1, 2].into()),
        ));
        let (head, body, _) = sent(&Input::Create(c)).expect("sent");
        assert_eq!(head.method, Method::POST);
        assert_eq!(
            head.uri.to_string(),
            "/models/veo-3.0-generate-001:predictLongRunning"
        );
        assert_eq!(body["instances"][0]["prompt"], "a fox");
        assert_eq!(body["instances"][0]["image"]["bytesBase64Encoded"], "AQI=");
        assert_eq!(body["parameters"]["aspectRatio"], "16:9");
        assert_eq!(body["parameters"]["resolution"], "1080p");
        assert_eq!(body["parameters"]["durationSeconds"], 8);
    }

    #[test]
    fn a_creation_needs_a_model_and_takes_no_storage() {
        let mut c = create("a fox");
        c.model = None;
        assert!(sent(&Input::Create(c)).is_err());
        let mut c = create("a fox");
        c.native = respan_gateway_conversation_http::fields::native_of(
            NAME,
            serde_json::from_value(json!({"storageUri": "gs://b/o", "sampleCount": 2}))
                .unwrap_or_default(),
        );
        let refused = sent(&Input::Create(c)).expect_err("refused");
        assert!(refused.to_string().contains("storageUri"));
    }

    #[test]
    fn its_native_fields_are_parameters() {
        let mut c = create("a fox");
        c.native = respan_gateway_conversation_http::fields::native_of(
            NAME,
            serde_json::from_value(json!({"sampleCount": 2})).unwrap_or_default(),
        );
        let (_, body, _) = sent(&Input::Create(c)).expect("sent");
        assert_eq!(body["parameters"]["sampleCount"], 2);
    }

    #[test]
    fn a_job_is_polled_by_posting_its_operation_to_the_model() {
        let id = VertexVeo::job_id(NAME_OF_OP);
        assert_eq!(VertexVeo::operation(&id).as_deref(), Some(NAME_OF_OP));
        let (head, body, call) = sent(&Input::Retrieve { id: id.clone() }).expect("sent");
        assert_eq!(head.method, Method::POST);
        assert_eq!(
            head.uri.to_string(),
            "/models/veo-3.0-generate-001:fetchPredictOperation"
        );
        assert_eq!(body, json!({ "operationName": NAME_OF_OP }));
        assert!(matches!(call, Call::Retrieve));
        let content = Input::Content {
            id,
            variant: Variant::Video,
            url: None,
        };
        let (_, _, call) = sent(&content).expect("sent");
        assert!(matches!(call, Call::Content));
    }

    #[test]
    fn only_a_models_operation_is_a_job() {
        for id in [
            "veo_abc".to_owned(),
            "vertex_veo_!!".to_owned(),
            VertexVeo::job_id("models/veo/operations/a"),
            VertexVeo::job_id("projects/p/locations/l/publishers/google/models/../operations/a"),
            VertexVeo::job_id("projects/p/locations/l/publishers/google/models/m/operations"),
        ] {
            assert_eq!(VertexVeo::operation(&id), None, "{id}");
            assert!(sent(&Input::Retrieve { id }).is_err());
        }
    }

    #[test]
    fn what_veo_cannot_do_is_refused_before_a_call() {
        let id = VertexVeo::job_id(NAME_OF_OP);
        let thumb = Input::Content {
            id: id.clone(),
            variant: Variant::Thumbnail,
            url: None,
        };
        for input in [
            thumb,
            Input::Delete { id: id.clone() },
            Input::Remix {
                id,
                prompt: "x".into(),
                native: None,
            },
            Input::List {
                after: None,
                limit: None,
                order: None,
            },
        ] {
            assert!(sent(&input).is_err());
        }
    }

    #[test]
    fn a_creation_answer_is_a_job_in_progress() {
        let raw = json!({ "name": NAME_OF_OP }).to_string();
        let parameters = json!({"durationSeconds": 8, "aspectRatio": "9:16", "resolution": "720p"});
        let job = job_of(raw.as_bytes(), &parameters).expect("job");
        assert_eq!(job.status, Status::InProgress);
        assert_eq!(job.id, VertexVeo::job_id(NAME_OF_OP));
        assert_eq!(job.model.as_deref(), Some("veo-3.0-generate-001"));
        assert_eq!(job.seconds, Some(8));
        assert_eq!(job.size.as_deref(), Some("720x1280"));
        assert!(job_of(b"{}", &Value::Null).is_err());
        assert!(job_of(b"nope", &Value::Null).is_err());
    }

    #[test]
    fn a_finished_operation_with_a_video_is_completed() {
        let raw = json!({"name": NAME_OF_OP, "done": true, "response": {
            "raiMediaFilteredCount": 0,
            "videos": [{"bytesBase64Encoded": "AQI=", "mimeType": "video/mp4"}],
        }})
        .to_string();
        let job = job_of(raw.as_bytes(), &Value::Null).expect("job");
        assert_eq!(job.status, Status::Completed);
        assert!(job.videos.is_empty());
        assert_eq!(
            content_of(raw.as_bytes()).expect("bytes"),
            Bytes::from_static(&[1, 2])
        );
    }

    #[test]
    fn a_failed_or_filtered_operation_is_failed() {
        let error = json!({"name": NAME_OF_OP, "done": true,
            "error": {"code": 3, "message": "bad prompt"}})
        .to_string();
        let job = job_of(error.as_bytes(), &Value::Null).expect("job");
        assert_eq!(job.status, Status::Failed);
        assert_eq!(job.error.map(|e| e.message), Some("bad prompt".into()));
        let filtered = json!({"name": NAME_OF_OP, "done": true, "response": {
            "raiMediaFilteredCount": 1,
            "raiMediaFilteredReasons": ["blocked"],
        }})
        .to_string();
        let job = job_of(filtered.as_bytes(), &Value::Null).expect("job");
        assert_eq!(job.status, Status::Failed);
        assert_eq!(
            job.error.and_then(|e| e.code).as_deref(),
            Some("content_filtered")
        );
    }

    #[test]
    fn content_waits_for_the_operation_and_needs_its_bytes() {
        let pending = json!({"name": NAME_OF_OP}).to_string();
        let refused = content_of(pending.as_bytes()).expect_err("not ready");
        assert!(refused.to_string().contains("not ready"));
        let stored =
            json!({"done": true, "response": {"videos": [{"gcsUri": "gs://b/o.mp4"}]}}).to_string();
        assert!(content_of(stored.as_bytes()).is_err());
        let empty = json!({"done": true, "response": {}}).to_string();
        assert!(content_of(empty.as_bytes()).is_err());
    }
}
