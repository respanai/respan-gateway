//! Google's Veo: a format of the [`video`](respan_gateway_media::video)
//! kind, [`GeminiVeo`].

use super::{is_segment, refusal};
use crate::edge::{bind_provider, content, sends};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::kept;
use respan_gateway_conversation_http::gemini;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, NativeFields};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::forwarded_path;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, KeyIn, Reach, Upstream};
use serde_json::{Map, Value, json};
use std::sync::{Arc, OnceLock};

use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status, Variant};

const NAME: &str = "gemini-veo";
/// What starts the id of a job of Veo's.
const PREFIX: &str = "veo_";

/// Google's Veo, as the Gemini API serves it: a provider of the video kind.
///
/// A creation is `POST /models/{model}:predictLongRunning`, which needs a
/// model; its `size` goes as an `aspectRatio` (16:9 or 9:16, the only ones
/// Veo makes) and a `resolution` (`1080p` from 1080 pixels on the short
/// side, else `720p`), its `seconds` as `durationSeconds`, its reference
/// image as the instance's base64 `image`, and its native fields of this
/// format, such as `negativePrompt` or `personGeneration`, as parameters.
///
/// A job is Veo's long-running operation, its id the operation's name
/// (`models/{model}/operations/{op}`) as unpadded base64url after `veo_`
/// ([`GeminiVeo::job_id`]): a path segment, as a client gives an id back.
/// An operation not done is `in_progress`; done with a video, `completed`,
/// its videos' URIs [`Job::videos`]; done with an error, or with no video as
/// when Veo's filters took them all, `failed`. Retrieving is `GET
/// /{operation}`. Content is a `GET` of the URL its job names, needed in
/// the request ([`LocateContent`](respan_gateway_media::video::LocateContent) finds it), at
/// the provider's base: its path after its version segment, so the key
/// never goes to another host. The Gemini API may answer it with a redirect
/// to storage, which a transport follows only if told to
/// (`HttpTransport::follow_redirects`), leaving the key behind.
///
/// Veo cannot remix, delete or list videos, nor make a thumbnail or a
/// spritesheet: those are refused, 400, before any call. What Veo answers
/// beyond the kind is dropped.
///
/// A client speaks it too, of any provider of the kind: it creates with
/// `POST .../models/{model}:predictLongRunning`, its parameters the kind
/// has no place for kept as this format's own; a job is answered as an
/// operation named `models/{model}/operations/{job id}`, which it polls at
/// `GET .../{name}`, so a follow-up reaches the job's provider; a finished
/// job names its video at the gateway, `GET .../files/{job id}:download`,
/// never the provider's own URL. Veo reports no progress: an operation is
/// only done or not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiVeo;

impl GeminiVeo {
    /// The format's name, `gemini-veo`, which tags its native fields.
    pub const NAME: &'static str = NAME;

    /// The job id of the operation `name`.
    pub fn job_id(name: &str) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(name))
    }

    /// The operation a job `id` names, if it is one of this format's: its
    /// segments of letters, digits, `-`, `_` and `.`, and no `.` or `..`.
    pub fn operation(id: &str) -> Option<String> {
        let name = URL_SAFE_NO_PAD.decode(id.strip_prefix(PREFIX)?).ok()?;
        let name = String::from_utf8(name).ok()?;
        let mut segments = name.split('/');
        segments.all(is_segment).then_some(name)
    }
}

/// The redirects its calls follow: to Google's hosts over HTTPS alone, as
/// Veo serves a video's bytes from its storage.
fn google_storage(uri: &Uri) -> bool {
    let host = uri.host().unwrap_or_default();
    let google = [".googleapis.com", ".googleusercontent.com"];
    uri.scheme_str() == Some("https") && google.iter().any(|domain| host.ends_with(domain))
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP, following a
/// redirect to Google's storage, which serves a video's bytes.
impl Upstream for GeminiVeo {
    fn name(&self) -> &str {
        NAME
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn reach(&self) -> Reach {
        Reach::Http {
            follow: Some(google_storage),
        }
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiVeo {
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
    GeminiVeo::operation(id).ok_or_else(|| Fault::not_found(format!("no Veo video has id {id}")))
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
    let (method, path, body, asked) = match input {
        Input::Create(create) => {
            let model = create.model.as_deref().filter(|m| is_segment(m));
            let model = model.ok_or_else(|| {
                Fault::bad_request("a Veo video needs a model, such as veo-3.0-generate-001")
            })?;
            let path = format!("/models/{model}:predictLongRunning");
            let (body, parameters) = create_body(create)?;
            (Method::POST, path, Some(body), Call::Create(parameters))
        }
        Input::Retrieve { id } => {
            let path = format!("/{}", operation_of(id)?);
            (Method::GET, path, None, Call::Retrieve)
        }
        Input::Content { id, variant, url } => {
            if *variant != Variant::Video {
                return Err(unsupported("make a thumbnail or a spritesheet"));
            }
            operation_of(id)?;
            let url = url.as_deref().ok_or_else(|| {
                Fault::config("Veo's content is at the URL its job names, which the request needs")
            })?;
            (Method::GET, content_path(url)?, None, Call::Content)
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
        return Ok((sends(Bytes::new()), asked));
    };
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Ok((sends(body), asked))
}

/// A creation's body: one instance, of its prompt and reference image, and
/// its parameters, which are also given apart.
fn create_body(create: &Create) -> Result<(Bytes, Value), Fault> {
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

/// Veo's aspect ratio and resolution of a size `WxH`.
fn aspect_of(size: &str) -> Result<(&'static str, &'static str), Fault> {
    let parsed = size.split_once(['x', 'X']).and_then(|(w, h)| {
        let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
        (w > 0 && h > 0).then_some((w, h))
    });
    let refused = || {
        Fault::bad_request(format!(
            "'size' {size} is not one Veo makes: it makes 16:9 and 9:16 videos"
        ))
    };
    let (w, h) = parsed.ok_or_else(refused)?;
    let (long, short) = (w.max(h), w.min(h));
    if w == h || (f64::from(long) / f64::from(short) - 16.0 / 9.0).abs() > 0.05 {
        return Err(refused());
    }
    let aspect = if w > h { "16:9" } else { "9:16" };
    let resolution = if short >= 1080 { "1080p" } else { "720p" };
    Ok((aspect, resolution))
}

/// The size, `WxH`, of a video of `aspect` and `resolution`, Veo's defaults
/// where they say none.
fn size_of(aspect: Option<&str>, resolution: Option<&str>) -> Option<String> {
    let short = match resolution.unwrap_or("720p") {
        "720p" => 720,
        "1080p" => 1080,
        _ => return None,
    };
    let long = short * 16 / 9;
    match aspect.unwrap_or("16:9") {
        "16:9" => Some(format!("{long}x{short}")),
        "9:16" => Some(format!("{short}x{long}")),
        _ => None,
    }
}

/// The path, relative to the provider's base, of content at `url`: its
/// path after its version segment, such as `v1beta`, and its query.
fn content_path(url: &str) -> Result<String, Fault> {
    let unusable = || Fault::upstream("the URL of Veo's video is not one it can be fetched at");
    let uri = url.parse::<Uri>().map_err(|_| unusable())?;
    let path = uri.path().trim_start_matches('/');
    let path = match path.split_once('/') {
        Some((version, rest)) if is_version(version) => rest,
        None if is_version(path) => "",
        _ => path,
    };
    if path.is_empty() {
        return Err(unusable());
    }
    Ok(match uri.query() {
        Some(query) => format!("/{path}?{query}"),
        None => format!("/{path}"),
    })
}

/// Whether a path segment names an API version, such as `v1` or `v1beta`.
fn is_version(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next() == Some('v') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// A provider's answer to the call `sent`: an operation, read whole, or
/// content, chunk by chunk as it comes.
fn read_response(sent: Option<&Call>) -> Box<dyn Decoder<Output>> {
    match sent {
        Some(Call::Content) => content(),
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

/// The job of an operation `raw`, and of the `parameters` it was made with
/// if this answers its creation.
fn job_of(raw: &[u8], parameters: &Value) -> Result<Job, Fault> {
    let unreadable =
        |what: &str| Fault::upstream(format!("the provider's {NAME} operation {what}"));
    let v = serde_json::from_slice::<Value>(raw).map_err(|_| unreadable("is not JSON"))?;
    let name = v["name"]
        .as_str()
        .ok_or_else(|| unreadable("has no name"))?;
    let mut job = Job::new(GeminiVeo::job_id(name), Status::InProgress);
    let mut segments = name.split('/');
    if segments.next() == Some("models") {
        job.model = segments.next().map(str::to_owned);
    }
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
    let response = &v["response"]["generateVideoResponse"];
    let samples = response["generatedSamples"]
        .as_array()
        .into_iter()
        .flatten();
    for sample in samples {
        let Some(uri) = sample["video"]["uri"].as_str() else {
            continue;
        };
        let mime = sample["video"]["mimeType"].as_str().unwrap_or("video/mp4");
        job.videos
            .push(Media::new(mime, MediaSource::Url(uri.to_owned())));
    }
    if job.videos.is_empty() {
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

// --- A client of Veo's: its requests read, the kind's answers written.

impl Format<ClientIo, Input, Output> for GeminiVeo {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        route(request.method(), request.uri()).is_some()
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(io.with_error(refusal).bind(
            move |head| {
                let head = head.clone();
                Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Input>| {
                    out.push(read_request(&head, &raw)?);
                    Ok(())
                }))
            },
            move |reply| {
                let mut head = reply.head();
                let encoder = response_encoder(reply.request(), &mut head);
                Ok(reply.write(head, encoder))
            },
        ))
    }
}

/// What a client of Veo's asks, as its method and path say it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// `POST .../models/{model}:predictLongRunning`.
    Create(String),
    /// `GET .../operations/{id}`, as a job's operation is named
    /// ([`issued`]).
    Retrieve(String),
    /// `GET .../files/{id}:download`, where a finished job's video is
    /// ([`issued`]).
    Download(String),
}

/// The operation of a request of `method` to `uri`, by the end of its path
/// as [`forwarded_path`] reads it, its segments unescaped.
fn route(method: &Method, uri: &Uri) -> Option<Route> {
    let path = forwarded_path(uri.path());
    let segments: Vec<String> = (path.split('/'))
        .filter(|s| !s.is_empty())
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect();
    let [.., parent, last] = segments.as_slice() else {
        return None;
    };
    match *method {
        Method::POST if parent == "models" => {
            let model = last.strip_suffix(":predictLongRunning")?;
            is_segment(model).then(|| Route::Create(model.to_owned()))
        }
        Method::GET if parent == "operations" && issued(last) => {
            Some(Route::Retrieve(last.clone()))
        }
        Method::GET if parent == "files" => {
            let id = last.strip_suffix(":download").filter(|id| issued(id))?;
            Some(Route::Download(id.to_owned()))
        }
        _ => None,
    }
}

/// Whether `id` is spelled as a job id the gateway gives out, Veo's or
/// OpenAI's, perhaps wrapped in its provider's name (`<provider>~<id>`):
/// none of Google's own files or operations is, so those pass through.
fn issued(id: &str) -> bool {
    let inner = id.split_once('~').map_or(id, |(_, inner)| inner);
    [PREFIX, "video_"].iter().any(|s| inner.starts_with(s))
}

/// What a client asks: a creation's body read into the kind's, the rest by
/// its path alone.
fn read_request(head: &RequestHead, raw: &[u8]) -> Result<Input, Fault> {
    let route = route(&head.method, &head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no Veo endpoint"))?;
    Ok(match route {
        Route::Create(model) => Input::Create(read_create(model, raw)?),
        Route::Retrieve(id) => Input::Retrieve { id },
        Route::Download(id) => Input::Content {
            id,
            variant: Variant::Video,
            url: None,
        },
    })
}

/// A creation: its one instance's prompt and image, its parameters'
/// `aspectRatio` and `resolution` as a size and `durationSeconds` as
/// seconds, and its other parameters as this format's native fields.
fn read_create(model: String, raw: &[u8]) -> Result<Create, Fault> {
    let invalid = |what: &str| Fault::bad_request(what.to_owned());
    let mut v: Map<String, Value> =
        serde_json::from_slice(raw).map_err(|_| invalid("the body is not a JSON object"))?;
    let mut instances = match v.remove("instances") {
        Some(Value::Array(instances)) => instances,
        _ => return Err(invalid("`instances` must be a list of one instance")),
    };
    let (Some(Value::Object(mut instance)), true) = (instances.pop(), instances.is_empty()) else {
        return Err(invalid("`instances` must be a list of one instance"));
    };
    let prompt = match instance.remove("prompt") {
        Some(Value::String(prompt)) => prompt,
        _ => return Err(invalid("`instances[0].prompt` is required")),
    };
    let mut create = Create::new(prompt);
    create.model = Some(model);
    if let Some(image) = instance.remove("image") {
        let bytes = image["bytesBase64Encoded"]
            .as_str()
            .and_then(|b| STANDARD.decode(b).ok());
        let bytes = bytes.ok_or_else(|| invalid("`instances[0].image` must be base64 bytes"))?;
        let mime = image["mimeType"].as_str().unwrap_or("image/png");
        create.reference = Some(Media::new(mime, MediaSource::Bytes(bytes.into())));
    }
    if let Some(key) = instance.keys().next() {
        return Err(invalid(&format!("`instances[0].{key}` is not taken")));
    }
    let mut parameters = match v.remove("parameters") {
        Some(Value::Object(parameters)) => parameters,
        None => Map::new(),
        Some(_) => return Err(invalid("`parameters` must be an object")),
    };
    if let Some(key) = v.keys().next() {
        return Err(invalid(&format!("`{key}` is not taken")));
    }
    let (aspect, resolution) = (parameters.get("aspectRatio"), parameters.get("resolution"));
    if aspect.is_some() || resolution.is_some() {
        let size = size_of(
            aspect.and_then(Value::as_str),
            resolution.and_then(Value::as_str),
        );
        // A size the kind can say goes as one; another stays Veo's own.
        if let Some(size) = size {
            create.size = Some(size);
            parameters.remove("aspectRatio");
            parameters.remove("resolution");
        }
    }
    create.seconds = match parameters.remove("durationSeconds") {
        None => None,
        Some(n) => Some(
            n.as_u64()
                .or_else(|| n.as_str()?.parse().ok())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("`parameters.durationSeconds` must be a whole number"))?,
        ),
    };
    if !parameters.is_empty() {
        create.native = Some(NativeFields::new(NAME, parameters));
    }
    Ok(create)
}

/// The encoder of an answer to `request`: a video's bytes as they come, or
/// a job as an operation.
fn response_encoder(
    request: &http::Request<Bytes>,
    head: &mut ResponseHead,
) -> Box<dyn Encoder<Output>> {
    let content = matches!(
        route(request.method(), request.uri()),
        Some(Route::Download(_))
    );
    let typed = if content {
        "video/mp4"
    } else {
        "application/json"
    };
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static(typed));
    if content {
        return Box::new(|event: &Output, out: &mut Vec<Bytes>| {
            if let Output::Content(bytes) = event {
                out.push(bytes.clone());
            }
            Ok(())
        });
    }
    let base = gateway_base(request);
    Box::new(All::new(move |events: &[Output]| {
        let job = events.iter().rev().find_map(|e| match e {
            Output::Job(job) => Some(job),
            _ => None,
        });
        let job = job.ok_or_else(|| Fault::upstream("the provider's answer said no video"))?;
        serde_json::to_vec(&operation_of_job(job, &base))
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))
    }))
}

/// Where the gateway serves Veo to the client of `request`: its origin, as
/// the request names it, and its path up to the version segment.
fn gateway_base(request: &http::Request<Bytes>) -> String {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let host = header("host").unwrap_or("localhost");
    let scheme = header("x-forwarded-proto").unwrap_or("http");
    let path = forwarded_path(request.uri().path());
    let at = ["/models/", "/operations/", "/files/"]
        .iter()
        .filter_map(|m| path.find(m))
        .min();
    format!("{scheme}://{host}{}", &path[..at.unwrap_or(0)])
}

/// A job as Veo's operation: named for the job's id, so a client polls the
/// provider that made it; done once it finished; its video, if made, at the
/// gateway, never the provider's own URL; its failure as an error, or as a
/// filtered response when the provider's filters took the video.
fn operation_of_job(job: &Job, base: &str) -> Value {
    let name = match &job.model {
        Some(model) => format!("models/{model}/operations/{}", job.id),
        None => format!("operations/{}", job.id),
    };
    let mut operation = json!({ "name": name });
    match &job.status {
        Status::Completed => {
            let uri = format!("{base}/files/{}:download?alt=media", job.id);
            let mime = job
                .videos
                .first()
                .map_or("video/mp4", |v| v.mime_type.as_str());
            operation["done"] = true.into();
            operation["response"] = json!({
                "@type": "type.googleapis.com/google.ai.generativelanguage.v1beta.PredictLongRunningResponse",
                "generateVideoResponse": { "generatedSamples": [{ "video": { "uri": uri, "mimeType": mime } }] },
            });
        }
        Status::Failed => {
            operation["done"] = true.into();
            let error = job
                .error
                .clone()
                .unwrap_or_else(|| JobError::new(None, "the video failed"));
            if error.code.as_deref() == Some("content_filtered") {
                operation["response"] = json!({
                    "@type": "type.googleapis.com/google.ai.generativelanguage.v1beta.PredictLongRunningResponse",
                    "generateVideoResponse": {
                        "raiMediaFilteredCount": 1,
                        "raiMediaFilteredReasons": [error.message],
                    },
                });
            } else {
                let status = error
                    .code
                    .filter(|c| gemini::is_status_name(c))
                    .unwrap_or_else(|| "INTERNAL".into());
                operation["error"] = json!({ "code": gemini::status_code(&status), "message": error.message, "status": status });
            }
        }
        _ => {}
    }
    operation
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: this format's own copy of Google's names lacked 409 and
    /// 499, and read `UNIMPLEMENTED` back as 500.
    #[test]
    fn a_refusal_and_an_operations_error_use_googles_names_both_ways() {
        let request = http::Request::new(Bytes::new());
        for (status, name) in [(409, "ABORTED"), (499, "CANCELLED"), (501, "UNIMPLEMENTED")] {
            let status_code = http::StatusCode::from_u16(status).unwrap_or_default();
            let mut head = http::Response::new(()).into_parts().0;
            head.status = status_code;
            let fault = Fault::upstream("no").with(respan_gateway_http_faults::Status(status_code));
            let body = refusal(&fault, &request, &mut head);
            let v: Value = serde_json::from_slice(&body).unwrap_or_default();
            assert_eq!(v["error"]["status"], name, "{v}");
            assert_eq!(gemini::status_code(name), status);
        }
    }

    #[test]
    fn a_job_id_is_a_path_segment_that_names_its_operation() {
        let name = "models/veo-3.0-generate-001/operations/a1b2-c3_d4";
        let id = GeminiVeo::job_id(name);
        assert!(id.starts_with("veo_"));
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert_eq!(GeminiVeo::operation(&id).as_deref(), Some(name));
        for refused in [
            "video_123".to_owned(),
            "veo_!!".to_owned(),
            GeminiVeo::job_id("models/../operations/x"),
            GeminiVeo::job_id("models//x"),
            GeminiVeo::job_id("models/x?y"),
        ] {
            assert_eq!(GeminiVeo::operation(&refused), None, "{refused}");
        }
    }

    /// Regression: every `GET .../operations/{x}` and `.../files/{x}:download`
    /// was read as a job's, so the Files API's downloads and other
    /// operations never reached passthrough.
    #[test]
    fn only_the_gateways_jobs_are_routed() {
        let route = |path: &str| route(&Method::GET, &path.parse().unwrap_or_default());
        let id = GeminiVeo::job_id("models/veo-3.0-generate-001/operations/a1");
        for (path, routed) in [
            (
                format!("/v1beta/models/veo-3.0/operations/{id}"),
                Some(Route::Retrieve(id.clone())),
            ),
            (
                format!("/v1beta/operations/{id}"),
                Some(Route::Retrieve(id.clone())),
            ),
            (
                "/v1beta/operations/east~video_1".into(),
                Some(Route::Retrieve("east~video_1".into())),
            ),
            (
                format!("/v1beta/files/{id}:download"),
                Some(Route::Download(id.clone())),
            ),
            (
                "/v1beta/files/video_1:download".into(),
                Some(Route::Download("video_1".into())),
            ),
            ("/v1beta/files/abc123:download".into(), None),
            ("/v1beta/operations/abc123".into(), None),
            ("/v1beta/models/gemini-2.5/operations/abc123".into(), None),
            ("/v1beta/tunedModels/t/operations/abc123".into(), None),
        ] {
            assert_eq!(route(&path), routed, "{path}");
        }
    }

    #[test]
    fn a_size_is_an_aspect_ratio_and_a_resolution_and_back() {
        for (size, aspect, resolution) in [
            ("1280x720", "16:9", "720p"),
            ("720x1280", "9:16", "720p"),
            ("1792x1024", "16:9", "720p"),
            ("1920x1080", "16:9", "1080p"),
            ("1080x1920", "9:16", "1080p"),
        ] {
            let read = aspect_of(size).ok();
            assert_eq!(read, Some((aspect, resolution)), "{size}");
        }
        assert_eq!(
            size_of(Some("9:16"), Some("1080p")).as_deref(),
            Some("1080x1920")
        );
        assert_eq!(size_of(None, None).as_deref(), Some("1280x720"));
        for size in ["1024x1024", "1024x768", "big", "0x0"] {
            assert!(aspect_of(size).is_err(), "{size}");
        }
    }

    #[test]
    fn content_is_fetched_at_the_providers_base() {
        let at = |url: &str| content_path(url).ok();
        assert_eq!(
            at("https://generativelanguage.googleapis.com/v1beta/files/abc:download?alt=media")
                .as_deref(),
            Some("/files/abc:download?alt=media")
        );
        assert_eq!(
            at("https://elsewhere.test/files/x").as_deref(),
            Some("/files/x")
        );
        assert_eq!(at("https://h.test/v1beta"), None);
    }

    #[test]
    fn only_googles_hosts_over_https_are_followed() {
        let follows = |uri: &str| google_storage(&uri.parse().unwrap_or_default());
        assert!(follows("https://storage.googleapis.com/v/1"));
        assert!(follows("https://x.googleusercontent.com/v"));
        assert!(!follows("http://storage.googleapis.com/v"));
        assert!(!follows("https://evil.test/storage.googleapis.com"));
        assert!(!follows("https://googleapis.com.evil.test/v"));
    }
}
