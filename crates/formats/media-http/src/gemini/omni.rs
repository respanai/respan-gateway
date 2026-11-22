//! Gemini Omni's videos: a format of the
//! [`video`](respan_gateway_media::video) kind, [`GeminiOmni`].

use super::{is_segment, refusal};
use crate::edge::{bind_provider, content, dimensions, sends, unsupported};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::gemini;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, Native};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::forwarded_path;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole, native::Json};
use respan_gateway_http::{Connect, KeyIn, Reach, Upstream};
use serde_json::{Map, Value, json};
use std::sync::{Arc, OnceLock};

use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status};

const NAME: &str = "gemini-omni";

type Key = crate::Own<GeminiOmni>;
/// What starts the id of a job.
pub(crate) const PREFIX: &str = "omni_";
/// What started a job's id before the format was named for Omni: ids issued
/// so, and kept in logs, still poll and fetch.
pub(crate) const LEGACY_PREFIX: &str = "veo_";

/// Gemini Omni's videos, as the Gemini API serves them: a provider of the
/// video kind.
///
/// A creation is `POST /interactions`, which needs a model (`gemini-omni-*`):
/// its prompt and reference image the `input`, its `size` the
/// `response_format`'s `aspect_ratio` and `resolution` (else its native
/// `aspectRatio` and `resolution`, as for 4k), its `seconds` the `duration`;
/// its other native fields have no place and are dropped. Its video is a file
/// of the Files API, which is its job: the file's name (`files/{id}`) as
/// unpadded base64url after `omni_` ([`GeminiOmni::job_id`]), a path segment,
/// as a client gives an id back. Ids that start `veo_`, as this format's
/// jobs' did before, are read the same ([`GeminiOmni::file`]). Retrieving is
/// `GET /files/{id}`: `in_progress` until the file is `ACTIVE`, then
/// `completed`, its video the file's download URI ([`Job::videos`]). The
/// interaction's `usage`, its tokens by modality, is the created job's native
/// `usage`: a file has none. Content is a `GET` of the URL its job names,
/// needed in the request
/// ([`LocateContent`](respan_gateway_media::video::LocateContent) finds it),
/// at the provider's base: its path after its version segment, so the key
/// never goes to another host. The Gemini API may answer it with a redirect
/// to storage, which a transport follows only if told to
/// (`HttpTransport::follow_redirects`), leaving the key behind.
///
/// Another format's operation, such as OpenAI's remix, delete, list or
/// thumbnail, is refused, 400, before any call.
///
/// A client speaks it too, of any provider of the kind, in the Gemini API's
/// long-running operation dialect: it creates with
/// `POST .../models/{model}:predictLongRunning`, its parameters the kind has
/// no place for kept as this format's own; a job is answered as an operation
/// named `models/{model}/operations/{job id}`, which it polls at
/// `GET .../{name}`, so a follow-up reaches the job's provider; a finished
/// job names its video at the gateway, `GET .../files/{job id}:download`,
/// never the provider's own URL. The operation reports no progress: it is
/// only done or not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiOmni;

impl GeminiOmni {
    /// The format's name, `gemini-omni`.
    pub const NAME: &'static str = NAME;

    /// The job id of the file `name`.
    pub fn job_id(name: &str) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(name))
    }

    /// The file a job `id` names, if it is one of this format's, spelled as
    /// now (`omni_`) or as before (`veo_`): `files/{id}`, its segments of
    /// letters, digits, `-`, `_` and `.`, and no `.` or `..`.
    pub fn file(id: &str) -> Option<String> {
        let encoded = id
            .strip_prefix(PREFIX)
            .or_else(|| id.strip_prefix(LEGACY_PREFIX))?;
        let name = URL_SAFE_NO_PAD.decode(encoded).ok()?;
        let name = String::from_utf8(name).ok()?;
        let mut segments = name.split('/');
        let named = segments.next() == Some("files") && segments.next().is_some_and(is_segment);
        (named && segments.next().is_none()).then_some(name)
    }
}

/// The redirects its calls follow: to Google's hosts over HTTPS alone, as
/// Gemini serves a video's bytes from its storage.
fn google_storage(uri: &Uri) -> bool {
    let host = uri.host().unwrap_or_default();
    let google = [".googleapis.com", ".googleusercontent.com"];
    uri.scheme_str() == Some("https") && google.iter().any(|domain| host.ends_with(domain))
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP, following a
/// redirect to Google's storage, which serves a video's bytes.
impl Upstream for GeminiOmni {
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

impl Format<ProviderIo, Output, Input> for GeminiOmni {
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

/// The file a job `id` names, else a 404.
fn file_of_id(id: &str) -> Result<String, Fault> {
    GeminiOmni::file(id)
        .ok_or_else(|| Fault::not_found(format!("no Gemini Omni video has id {id}")))
}

/// What a call asked, which its answer is read by.
#[derive(Debug)]
enum Call {
    /// A creation.
    Create,
    /// A file's state.
    File,
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
                Fault::bad_request("a video needs a model, such as gemini-omni-1.1-flash")
            })?;
            if !is_omni(model) {
                return Err(Fault::bad_request(format!(
                    "{model} is not a Gemini Omni model, such as gemini-omni-1.1-flash"
                )));
            }
            let body = omni_body(model, create)?;
            (
                Method::POST,
                "/interactions".into(),
                Some(body),
                Call::Create,
            )
        }
        Input::Retrieve { id } => {
            let name = file_of_id(id)?;
            (Method::GET, format!("/{name}"), None, Call::File)
        }
        Input::Content { id, url } => {
            file_of_id(id)?;
            let url = url.as_deref().ok_or_else(|| {
                Fault::config(
                    "a video's content is at the URL its job names, which the request needs",
                )
            })?;
            (Method::GET, content_path(url)?, None, Call::Content)
        }
        _ => return Err(unsupported("Gemini Omni", input)),
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

/// Whether `model` is a Gemini Omni model, created through `/interactions`.
fn is_omni(model: &str) -> bool {
    model.starts_with("gemini-omni")
}

/// A Gemini Omni creation's body: its prompt, after its reference image if
/// any, as the input, and a video as the response, at a URI, as the Files
/// API keeps it, at its `seconds` as a `duration`. Other native
/// parameters, such as `negativePrompt`, Omni has no place for: they are dropped.
fn omni_body(model: &str, create: &Create) -> Result<Bytes, Fault> {
    let input = match &create.reference {
        None => Value::String(create.prompt.clone()),
        Some(reference) => {
            let MediaSource::Bytes(bytes) = &reference.source else {
                return Err(Fault::bad_request(
                    "Gemini Omni takes a reference image's bytes, not its URL",
                ));
            };
            json!([
                {"type": "image", "data": STANDARD.encode(bytes), "mime_type": reference.mime_type},
                {"type": "text", "text": create.prompt},
            ])
        }
    };
    let mut format = json!({ "type": "video", "delivery": "uri" });
    if let Some(size) = &create.size {
        let (aspect, resolution) = aspect_of(size)?;
        format["aspect_ratio"] = aspect.into();
        format["resolution"] = resolution.into();
    }
    let native = create.native.fields::<Key>();
    for (key, said) in [
        ("aspect_ratio", "aspectRatio"),
        ("resolution", "resolution"),
    ] {
        if let Some(value) = native.get(said).filter(|_| format.get(key).is_none()) {
            format[key] = value.clone();
        }
    }
    if let Some(seconds) = create.seconds {
        format["duration"] = format!("{seconds}s").into();
    }
    let body = json!({ "model": model, "input": input, "response_format": format });
    let body = serde_json::to_vec(&body)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(Bytes::from(body))
}

/// The aspect ratio and resolution of a size `WxH`.
fn aspect_of(size: &str) -> Result<(&'static str, &'static str), Fault> {
    let refused = || {
        Fault::bad_request(format!(
            "'size' {size} is not one Gemini Omni makes: it makes 16:9 and 9:16 videos"
        ))
    };
    let (w, h) = dimensions(size).ok_or_else(refused)?;
    let (long, short) = (w.max(h), w.min(h));
    if w == h || (f64::from(long) / f64::from(short) - 16.0 / 9.0).abs() > 0.05 {
        return Err(refused());
    }
    let aspect = if w > h { "16:9" } else { "9:16" };
    let resolution = if short >= 1080 { "1080p" } else { "720p" };
    Ok((aspect, resolution))
}

/// The size, `WxH`, of a video of `aspect` and `resolution`, the defaults
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
    let unusable = || Fault::upstream("the URL of the video is not one it can be fetched at");
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

/// A provider's answer to the call `sent`: a job, read whole, or
/// content, chunk by chunk as it comes.
fn read_response(sent: Option<&Call>) -> Box<dyn Decoder<Output>> {
    match sent {
        Some(Call::Content) => content(),
        Some(Call::Create) => Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
            out.push(Output::Job(job_of_interaction(&raw)?));
            Ok(())
        })),
        _ => Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
            out.push(Output::Job(job_of_file(&raw)?));
            Ok(())
        })),
    }
}

/// The job of a Gemini Omni interaction `raw`: its video's file, not yet
/// known to be ready, and the `usage` it reports, native.
fn job_of_interaction(raw: &[u8]) -> Result<Job, Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's interaction {what}"));
    let v = serde_json::from_slice::<Value>(raw).map_err(|_| unreadable("is not JSON"))?;
    let steps = v["steps"].as_array().into_iter().flatten();
    let contents = steps.flat_map(|s| s["content"].as_array().into_iter().flatten());
    let file = contents
        .filter(|c| c["type"] == "video")
        .find_map(|c| file_of(c["uri"].as_str()?));
    // Its filters took the video, or it answered in words alone.
    let file = file.ok_or_else(|| unreadable("holds no video"))?;
    let mut job = Job::new(GeminiOmni::job_id(&file), Status::InProgress);
    job.model = v["model"].as_str().map(str::to_owned);
    if let Some(usage) = v.get("usage").filter(|usage| usage.is_object()) {
        job.native = Native::json::<Key>(json!({ "usage": usage }));
    }
    Ok(job)
}

/// The name, `files/{id}`, of the file a URI names, such as
/// `https://.../v1beta/files/{id}:download?alt=media`.
fn file_of(uri: &str) -> Option<String> {
    let path = uri.split(['?', '#']).next()?;
    let (_, id) = path.rsplit_once("files/")?;
    let id = id.split(':').next()?;
    is_segment(id).then(|| format!("files/{id}"))
}

/// The job of a file `raw` of the Files API, a Gemini Omni video:
/// `completed` once `ACTIVE`, `failed` once `FAILED`.
fn job_of_file(raw: &[u8]) -> Result<Job, Fault> {
    let unreadable = |what: &str| Fault::upstream(format!("the provider's file {what}"));
    let v = serde_json::from_slice::<Value>(raw).map_err(|_| unreadable("is not JSON"))?;
    let name = v["name"]
        .as_str()
        .ok_or_else(|| unreadable("has no name"))?;
    let mut job = Job::new(GeminiOmni::job_id(name), Status::InProgress);
    match v["state"].as_str() {
        Some("ACTIVE") => {
            let uri = match (v["downloadUri"].as_str(), v["uri"].as_str()) {
                (Some(download), _) => download.to_owned(),
                (None, Some(uri)) => format!("{uri}:download?alt=media"),
                (None, None) => return Err(unreadable("names no URI")),
            };
            let mime = v["mimeType"].as_str().unwrap_or("video/mp4");
            job.videos.push(Media::new(mime, MediaSource::Url(uri)));
            job.status = Status::Completed;
        }
        Some("FAILED") => {
            job.status = Status::Failed;
            let message = v["error"]["message"].as_str();
            job.error = Some(JobError::new(None, message.unwrap_or("the video failed")));
        }
        _ => {}
    }
    Ok(job)
}

// --- A client of the operation dialect: its requests read, the kind's answers written.

impl Format<ClientIo, Input, Output> for GeminiOmni {
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

/// What a client asks, as its method and path say it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// `POST .../models/{model}:predictLongRunning`.
    Create(String),
    /// `GET .../operations/{id}`, as a job's operation is named
    /// ([`issued`]).
    Retrieve { id: String, model: Option<String> },
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
        Method::GET if parent == "operations" && crate::issued(last) => {
            let model = match segments.as_slice() {
                [.., models, model, _, _] if models == "models" && is_segment(model) => {
                    Some(model.clone())
                }
                _ => None,
            };
            Some(Route::Retrieve {
                id: last.clone(),
                model,
            })
        }
        Method::GET if parent == "files" => {
            let id = last
                .strip_suffix(":download")
                .filter(|id| crate::issued(id))?;
            Some(Route::Download(id.to_owned()))
        }
        _ => None,
    }
}

/// What a client asks: a creation's body read into the kind's, the rest by
/// its path alone.
fn read_request(head: &RequestHead, raw: &[u8]) -> Result<Input, Fault> {
    let route = route(&head.method, &head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no Gemini Omni endpoint"))?;
    Ok(match route {
        Route::Create(model) => Input::Create(read_create(model, raw)?),
        Route::Retrieve { id, .. } => Input::Retrieve { id },
        Route::Download(id) => Input::Content { id, url: None },
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
        // A size the kind can say goes as one; another stays this format's own.
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
    create.native = Native::json::<Key>(parameters);
    Ok(create)
}

/// The encoder of an answer to `request`: a video's bytes as they come, or
/// a job as an operation.
fn response_encoder(
    request: &http::Request<Bytes>,
    head: &mut ResponseHead,
) -> Box<dyn Encoder<Output>> {
    let route = route(request.method(), request.uri());
    let content = matches!(route, Some(Route::Download(_)));
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
        // A Files API poll has no model. The matching Gemini client path
        // carries the model this operation was originally named with.
        let model = job.model.as_deref().or_else(|| match &route {
            Some(Route::Retrieve {
                id,
                model: Some(model),
            }) if id == &job.id && is_omni(model) => {
                let inner = id.split_once('~').map_or(id.as_str(), |(_, inner)| inner);
                GeminiOmni::file(inner).map(|_| model.as_str())
            }
            _ => None,
        });
        serde_json::to_vec(&operation_of_job(job, &base, model))
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))
    }))
}

/// Where the gateway serves Gemini Omni to the client of `request`: its origin, as
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

/// A job as a long-running operation: named for the job's id, so a client polls the
/// provider that made it; done once it finished; its video, if made, at the
/// gateway, never the provider's own URL; its failure as an error, or as a
/// filtered response when the provider's filters took the video.
fn operation_of_job(job: &Job, base: &str, model: Option<&str>) -> Value {
    let name = match model {
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
    use crate::Natives;

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
    fn a_job_id_is_a_path_segment_that_names_its_file() {
        let name = "files/a1b2-c3_d4";
        let id = GeminiOmni::job_id(name);
        assert!(id.starts_with("omni_"));
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert_eq!(GeminiOmni::file(&id).as_deref(), Some(name));
        for refused in [
            "video_123".to_owned(),
            "omni_!!".to_owned(),
            "veo_!!".to_owned(),
            GeminiOmni::job_id("files/../x"),
            GeminiOmni::job_id("files//x"),
            GeminiOmni::job_id("files/x?y"),
            GeminiOmni::job_id("files/a/b"),
            GeminiOmni::job_id("models/m/operations/a1"),
        ] {
            assert_eq!(GeminiOmni::file(&refused), None, "{refused}");
        }
    }

    /// Ids issued when this format was named for Veo, `veo_` and the file's
    /// name as base64url, still poll and fetch.
    #[test]
    fn an_id_issued_as_veo_still_names_its_file() {
        let old = format!("veo_{}", URL_SAFE_NO_PAD.encode("files/f1x2"));
        assert_eq!(GeminiOmni::file(&old).as_deref(), Some("files/f1x2"));
        let new = GeminiOmni::job_id("files/f1x2");
        assert_eq!(new, format!("omni_{}", &old["veo_".len()..]));
        let mut head = http::Request::new(()).into_parts().0;
        let polled = call(&mut head, &Input::Retrieve { id: old });
        assert!(matches!(polled, Ok((_, Call::File))));
        assert_eq!(head.uri.path(), "/files/f1x2");
        // An id of the dead Veo operation API names no file: a 404.
        let dead = format!(
            "veo_{}",
            URL_SAFE_NO_PAD.encode("models/veo-3.0/operations/a1")
        );
        assert!(call(&mut head, &Input::Retrieve { id: dead }).is_err());
    }

    /// Regression: every `GET .../operations/{x}` and `.../files/{x}:download`
    /// was read as a job's, so the Files API's downloads and other
    /// operations never reached passthrough.
    #[test]
    fn only_the_gateways_jobs_are_routed() {
        let route = |path: &str| route(&Method::GET, &path.parse().unwrap_or_default());
        let id = GeminiOmni::job_id("files/a1");
        for (path, routed) in [
            (
                format!("/v1beta/models/veo-3.0/operations/{id}"),
                Some(Route::Retrieve {
                    id: id.clone(),
                    model: Some("veo-3.0".into()),
                }),
            ),
            (
                format!("/v1beta/operations/{id}"),
                Some(Route::Retrieve {
                    id: id.clone(),
                    model: None,
                }),
            ),
            (
                "/v1beta/operations/east~video_1".into(),
                Some(Route::Retrieve {
                    id: "east~video_1".into(),
                    model: None,
                }),
            ),
            (
                format!("/v1beta/files/{id}:download"),
                Some(Route::Download(id.clone())),
            ),
            (
                "/v1beta/files/video_1:download".into(),
                Some(Route::Download("video_1".into())),
            ),
            (
                "/v1beta/operations/gen-vid-1".into(),
                Some(Route::Retrieve {
                    id: "gen-vid-1".into(),
                    model: None,
                }),
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
    fn an_omni_files_poll_keeps_the_clients_operation_name() {
        let model = "gemini-omni-1.1-flash";
        let answer = json!({"model": model, "steps": [
            {"type": "model_output", "content": [{"type": "video",
                "uri": "https://generativelanguage.googleapis.com/v1beta/files/f1:download?alt=media"}]},
        ]});
        let encode = |method, path: &str, job| {
            let request = http::Request::builder()
                .method(method)
                .uri(path)
                .body(Bytes::new())
                .unwrap_or_else(|e| panic!("{e}"));
            let mut head = http::Response::new(()).into_parts().0;
            let bytes = response_encoder(&request, &mut head)
                .encode_all(&[Output::Job(job)])
                .unwrap_or_else(|e| panic!("{e}"));
            serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|e| panic!("{e}"))
        };

        let created =
            job_of_interaction(answer.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e}"));
        for provider in ["", "east~"] {
            let mut created = created.clone();
            created.id = format!("{provider}{}", created.id);
            let initial = encode(
                Method::POST,
                &format!("/v1beta/models/{model}:predictLongRunning"),
                created.clone(),
            );
            let name = format!("models/{model}/operations/{}", created.id);
            assert_eq!(initial["name"], name);
            for (state, done) in [("PROCESSING", false), ("ACTIVE", true), ("FAILED", true)] {
                let raw = json!({"name": "files/f1", "state": state,
                    "uri": "https://generativelanguage.googleapis.com/v1beta/files/f1"});
                let mut job =
                    job_of_file(raw.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e}"));
                job.id = format!("{provider}{}", job.id);
                let reply = encode(Method::GET, &format!("/v1beta/{name}"), job.clone());
                assert_eq!(reply["name"], initial["name"], "{state}");
                assert_eq!(reply["done"].as_bool().unwrap_or_default(), done, "{state}");
                let bare = encode(Method::GET, &format!("/v1beta/operations/{}", job.id), job);
                assert_eq!(bare["name"], format!("operations/{}", created.id));
            }
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
    fn a_gemini_omni_video_is_an_interaction_whose_file_is_its_job() {
        let mut create = Create::new("a paper boat");
        create.model = Some("gemini-omni-1.1-flash".into());
        create.seconds = Some(8);
        let own = Map::from_iter([
            ("negativePrompt".to_owned(), json!("rain")),
            ("aspectRatio".to_owned(), json!("9:16")),
            ("resolution".to_owned(), json!("4k")),
        ]);
        create.native = Native::json::<Key>(own);
        let mut head = http::Request::new(()).into_parts().0;
        let input = Input::Create(create);
        let (encoder, asked) = call(&mut head, &input).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(head.uri, "/interactions");
        let mut sent = Vec::new();
        let mut encoder = encoder;
        encoder
            .encode(&input, &mut sent)
            .unwrap_or_else(|e| panic!("{e}"));
        let body: Value = serde_json::from_slice(&sent.concat()).unwrap_or_default();
        assert_eq!(
            body,
            json!({
                "model": "gemini-omni-1.1-flash",
                "input": "a paper boat",
                "response_format": {"type": "video", "delivery": "uri", "aspect_ratio": "9:16",
                    "resolution": "4k", "duration": "8s"},
            })
        );
        assert!(matches!(asked, Call::Create));
        let usage = json!({"total_input_tokens": 12, "total_thought_tokens": 386,
            "output_tokens_by_modality": [{"modality": "video", "tokens": 26064}]});
        let answer = json!({"id": "v1_abc", "model": "gemini-omni-1.1-flash", "usage": usage, "steps": [
            {"type": "model_output", "content": [{"type": "video", "mime_type": "video/mp4",
              "uri": "https://generativelanguage.googleapis.com/v1beta/files/f1x2:download?alt=media"}]},
        ]});
        let job =
            job_of_interaction(answer.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(job.status, Status::InProgress);
        assert_eq!(job.model.as_deref(), Some("gemini-omni-1.1-flash"));
        assert_eq!(GeminiOmni::fields(&job.native).get("usage"), Some(&usage));
        assert_eq!(GeminiOmni::file(&job.id).as_deref(), Some("files/f1x2"));

        let mut head = http::Request::new(()).into_parts().0;
        let retrieve = Input::Retrieve { id: job.id };
        let (_, asked) = call(&mut head, &retrieve).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(head.uri, "/files/f1x2");
        assert!(matches!(asked, Call::File));
        let file = |state: &str| {
            let raw = json!({"name": "files/f1x2", "mimeType": "video/mp4", "state": state,
                "uri": "https://generativelanguage.googleapis.com/v1beta/files/f1x2"});
            job_of_file(raw.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e}"))
        };
        assert_eq!(file("PROCESSING").status, Status::InProgress);
        let done = file("ACTIVE");
        assert_eq!(done.status, Status::Completed);
        assert_eq!(
            done.video_url(),
            Some("https://generativelanguage.googleapis.com/v1beta/files/f1x2:download?alt=media")
        );
        assert_eq!(file("FAILED").status, Status::Failed);
    }

    #[test]
    fn a_gemini_omni_answer_without_a_video_is_the_providers_fault() {
        let answer = json!({"id": "v1_abc", "steps": [{"type": "model_output", "content": [{"type": "text", "text": "no"}]}]});
        assert!(job_of_interaction(answer.to_string().as_bytes()).is_err());
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
