//! OpenAI Videos: a job made by `POST /videos` (JSON, or a form with an
//! `input_reference` image), polled by `GET /videos/{id}`, downloaded by
//! `GET /videos/{id}/content`, remade by `POST /videos/{id}/remix`, deleted
//! by `DELETE /videos/{id}` and listed by `GET /videos`.

use super::multipart::{self, Writer};
use super::{
    Fields, Form, bind_client, bytes_of, file_name, foreign, invalid, json_bytes, kept,
    merge_missing, missing, native_of, rest_of, sends, set, take_count, take_string, unreadable,
    whole_json, write_field,
};
use crate::edge::{bind_provider, content};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::forwarded_path;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::video::{Create, Input, Job, JobError, Jobs, Output, Status, Variant};
use serde_json::{Map, Value, json};

const NAME: &str = "openai-videos";

/// OpenAI Videos, as OpenAI or a compatible vendor serves it: a request to
/// a path ending in `/videos`, `/videos/{id}`, `/videos/{id}/content` or
/// `/videos/{id}/remix`, by its method.
///
/// A job's id is the provider's, kept as it came and escaped in a call's
/// path. A job's `seconds` is a string in OpenAI's API, as it is written; a
/// client may send a number. A creation goes as a form if its reference
/// image is bytes, else in JSON; a reference by URL cannot go. Content is
/// relayed chunk by chunk as it comes, the provider read no faster than
/// the client takes it, typed as its variant is: `video/mp4`, `image/webp`
/// for a thumbnail, `image/jpeg` for a spritesheet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiVideos;

impl OpenAiVideos {
    /// The format's name, `openai-videos`, which tags its native fields.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for OpenAiVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        route(request.method(), request.uri()).is_some()
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(bind_client(io, decode_request, response_encoder))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiVideos {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, sent| {
            Some(read_response(sent))
        }))
    }
}

/// An operation, as a request's method and path say it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Create,
    List,
    Retrieve(String),
    Delete(String),
    Content(String),
    Remix(String),
}

/// The operation of a request of `method` to `uri`, if one: matched by the
/// end of its path, read as [`forwarded_path`] reads it, the words in any
/// case and an id as it was spelled, unescaped.
fn route(method: &Method, uri: &Uri) -> Option<Route> {
    let path = forwarded_path(uri.path());
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let name = s.split(';').next().unwrap_or_default();
            percent_encoding::percent_decode_str(name)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect();
    let at = |back: usize| segments.len().checked_sub(back).map(|i| &segments[i]);
    let word = |back: usize, word: &str| at(back).is_some_and(|s| s.eq_ignore_ascii_case(word));
    let id = |back: usize| at(back).cloned();
    match *method {
        Method::POST if word(1, "videos") => Some(Route::Create),
        Method::POST if word(1, "remix") && word(3, "videos") => id(2).map(Route::Remix),
        Method::GET if word(1, "content") && word(3, "videos") => id(2).map(Route::Content),
        Method::GET if word(2, "videos") => id(1).map(Route::Retrieve),
        Method::GET if word(1, "videos") => Some(Route::List),
        Method::DELETE if word(2, "videos") => id(1).map(Route::Delete),
        _ => None,
    }
}

/// A URI's query parameters, unescaped.
fn query(uri: &Uri) -> Vec<(String, String)> {
    let query = uri.query().unwrap_or_default();
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// The query parameter `key` of `uri`.
fn param(uri: &Uri, key: &str) -> Option<String> {
    query(uri)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// The variant a content request asks for.
fn variant(uri: &Uri) -> Result<Variant, Fault> {
    match param(uri, "variant").as_deref() {
        None | Some("video") => Ok(Variant::Video),
        Some("thumbnail") => Ok(Variant::Thumbnail),
        Some("spritesheet") => Ok(Variant::Spritesheet),
        Some(_) => Err(invalid(
            "variant",
            "must be video, thumbnail or spritesheet",
        )),
    }
}

/// `text` escaped as a path segment or query value: all but RFC 3986's
/// unreserved characters.
pub(crate) fn escape(text: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    const UNRESERVED: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(text, UNRESERVED).to_string()
}

/// A client's request.
fn decode_request(head: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let route = route(&head.method, &head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no videos endpoint"))?;
    Ok(match route {
        Route::Create => Input::Create(decode_create(form)?),
        Route::Remix(id) => {
            let Fields { mut map, files } = form.fields()?;
            if let Some((name, _)) = files.first() {
                return Err(invalid(name, "is not a file this endpoint takes"));
            }
            let prompt = take_string(&mut map, "prompt")?.ok_or_else(|| missing("prompt"))?;
            let native = native_of(NAME, map);
            Input::Remix { id, prompt, native }
        }
        Route::Retrieve(id) => Input::Retrieve { id },
        Route::Delete(id) => Input::Delete { id },
        Route::Content(id) => Input::Content {
            id,
            variant: variant(&head.uri)?,
            url: None,
        },
        Route::List => {
            let limit = match param(&head.uri, "limit") {
                None => None,
                Some(limit) => Some(
                    limit
                        .parse()
                        .map_err(|_| invalid("limit", "must be a whole number"))?,
                ),
            };
            Input::List {
                after: param(&head.uri, "after"),
                limit,
                order: param(&head.uri, "order"),
            }
        }
    })
}

/// A client's request for a video.
fn decode_create(form: &Form) -> Result<Create, Fault> {
    let Fields { mut map, files } = form.fields()?;
    let prompt = take_string(&mut map, "prompt")?.ok_or_else(|| missing("prompt"))?;
    let mut create = Create::new(prompt);
    create.model = take_string(&mut map, "model")?;
    create.seconds = take_count(&mut map, "seconds")?;
    create.size = take_string(&mut map, "size")?;
    for (name, media) in files {
        match name.as_str() {
            "input_reference" if create.reference.is_none() => create.reference = Some(media),
            _ => return Err(invalid(&name, "is not a file this endpoint takes")),
        }
    }
    create.native = native_of(NAME, map);
    Ok(create)
}

/// The call of `input`: its method and path, and its body's encoder.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let path = |id: &str, rest: &str| format!("/videos/{}{rest}", escape(id));
    let (method, uri, body) = match input {
        Input::Create(create) => (
            Method::POST,
            "/videos".to_owned(),
            Some(create_body(create)?),
        ),
        Input::Remix { id, prompt, native } => {
            let mut v = Map::new();
            v.insert("prompt".into(), prompt.clone().into());
            merge_missing(&mut v, &kept(NAME, native)?);
            let body = (json_bytes(&v)?, "application/json".to_owned());
            (Method::POST, path(id, "/remix"), Some(body))
        }
        Input::Retrieve { id } => (Method::GET, path(id, ""), None),
        Input::Content { id, variant, .. } => {
            let query = match variant {
                Variant::Video => "",
                Variant::Thumbnail => "?variant=thumbnail",
                Variant::Spritesheet => "?variant=spritesheet",
                _ => return Err(Fault::bad_request("a variant this format has no form for")),
            };
            (Method::GET, path(id, &format!("/content{query}")), None)
        }
        Input::Delete { id } => (Method::DELETE, path(id, ""), None),
        Input::List {
            after,
            limit,
            order,
        } => {
            let limit = limit.map(|l| l.to_string());
            let pairs = [("after", after), ("limit", &limit), ("order", order)];
            let query: Vec<String> = pairs
                .iter()
                .filter_map(|(k, v)| v.as_deref().map(|v| format!("{k}={}", escape(v))))
                .collect();
            let uri = match query.is_empty() {
                true => "/videos".to_owned(),
                false => format!("/videos?{}", query.join("&")),
            };
            (Method::GET, uri, None)
        }
        _ => {
            return Err(Fault::bad_request(
                "a video request this format has no form for",
            ));
        }
    };
    head.method = method;
    head.uri = uri
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let Some((body, content_type)) = body else {
        return Ok(sends(Bytes::new()));
    };
    let content_type = HeaderValue::from_str(&content_type)
        .map_err(|e| Fault::internal("a content type is no header").with_source(e))?;
    head.headers.insert(CONTENT_TYPE, content_type);
    Ok(sends(body))
}

/// The body of a creation and its content type: a form if its reference
/// image is bytes, else JSON.
fn create_body(create: &Create) -> Result<(Bytes, String), Fault> {
    let mut v = Map::new();
    set(&mut v, "model", create.model.clone());
    v.insert("prompt".into(), create.prompt.clone().into());
    set(&mut v, "seconds", create.seconds.map(|s| s.to_string()));
    set(&mut v, "size", create.size.clone());
    merge_missing(&mut v, &kept(NAME, &create.native)?);
    let Some(reference) = &create.reference else {
        return Ok((json_bytes(&v)?, "application/json".to_owned()));
    };
    let Some(bytes) = bytes_of(reference) else {
        return Err(Fault::bad_request(
            "an input_reference by URL cannot be sent: OpenAI takes the image's bytes",
        ));
    };
    let mut form = Writer::with_boundary(multipart::random_boundary());
    let content_type = form.content_type();
    for (name, value) in &v {
        write_field(&mut form, name, value);
    }
    let name = file_name(reference, "reference");
    form.file("input_reference", &name, &reference.mime_type, bytes);
    Ok((form.finish(), content_type))
}

/// A provider's answer to the call `sent`: content, chunk by chunk as it
/// comes, or one JSON body.
fn read_response(sent: &http::Request<Bytes>) -> Box<dyn Decoder<Output>> {
    match route(sent.method(), sent.uri()) {
        Some(Route::Content(_)) => content(),
        _ => Box::new(Whole::new(decode_whole)),
    }
}

/// The events of a whole JSON answer: a job, a page of jobs, or a deletion.
fn decode_whole(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = whole_json(NAME, raw, |v| {
        matches!(
            v["object"].as_str(),
            Some("video" | "list" | "video.deleted")
        ) || v["id"].is_string()
            || v["data"].is_array()
    })?;
    out.push(match v["object"].as_str() {
        Some("list") => Output::Jobs(jobs_of(&v)?),
        _ if v["data"].is_array() => Output::Jobs(jobs_of(&v)?),
        Some("video.deleted") => Output::Deleted(id_of(&v)?),
        _ if v["deleted"] == true => Output::Deleted(id_of(&v)?),
        _ => Output::Job(job_of(&v)?),
    });
    Ok(())
}

fn id_of(v: &Value) -> Result<String, Fault> {
    let id = v["id"].as_str().map(str::to_owned);
    id.ok_or_else(|| unreadable(NAME, "names no video id"))
}

/// A page of jobs.
fn jobs_of(v: &Value) -> Result<Jobs, Fault> {
    let data = v["data"].as_array().into_iter().flatten();
    let jobs = data.map(job_of).collect::<Result<Vec<_>, _>>()?;
    let mut page = Jobs::new(jobs, v["has_more"].as_bool().unwrap_or(false));
    let taken = ["object", "data", "first_id", "last_id", "has_more"];
    page.native = native_of(NAME, rest_of(v, &taken));
    Ok(page)
}

/// A job object: its fields the kind has a form for typed, the rest, and
/// any it could not read, native.
fn job_of(v: &Value) -> Result<Job, Fault> {
    let status = match v["status"].as_str() {
        Some("queued") => Status::Queued,
        Some("in_progress") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed") => Status::Failed,
        Some(other) => Status::native(NAME, other),
        None => return Err(unreadable(NAME, "names no video status")),
    };
    let mut job = Job::new(id_of(v)?, status);
    let string = |key: &str| v[key].as_str().map(str::to_owned);
    let time = |key: &str| v[key].as_u64();
    job.model = string("model");
    job.progress = time("progress").and_then(|p| u32::try_from(p).ok());
    job.created_at = time("created_at");
    job.completed_at = time("completed_at");
    job.expires_at = time("expires_at");
    job.seconds = match &v["seconds"] {
        Value::String(s) => s.parse().ok(),
        n => n.as_u64().and_then(|n| u32::try_from(n).ok()),
    };
    job.size = string("size");
    job.error = v["error"].as_object().map(|e| {
        let code = e.get("code").and_then(Value::as_str).map(str::to_owned);
        let message = e.get("message").and_then(Value::as_str);
        JobError::new(code, message.unwrap_or_default())
    });
    let typed = [
        ("model", job.model.is_some()),
        ("progress", job.progress.is_some()),
        ("created_at", job.created_at.is_some()),
        ("completed_at", job.completed_at.is_some()),
        ("expires_at", job.expires_at.is_some()),
        ("seconds", job.seconds.is_some()),
        ("size", job.size.is_some()),
        ("error", job.error.is_some()),
    ];
    // A field the kind could not read, such as a fractional progress, stays.
    let read = typed
        .iter()
        .filter(|(key, typed)| *typed || v[*key].is_null());
    let mut taken = vec!["id", "object", "status"];
    taken.extend(read.map(|(key, _)| *key));
    job.native = native_of(NAME, rest_of(v, &taken));
    Ok(job)
}

/// The encoder of a response to a client's `request`: content as it comes,
/// else one JSON body.
fn response_encoder(
    request: &http::Request<Bytes>,
    _asked: &Map<String, Value>,
    head: &mut ResponseHead,
) -> Box<dyn Encoder<Output>> {
    if !matches!(
        route(request.method(), request.uri()),
        Some(Route::Content(_))
    ) {
        super::content_type(head, "application/json");
        return Box::new(All::new(encode_whole));
    }
    super::content_type(
        head,
        match variant(request.uri()).unwrap_or_default() {
            Variant::Video => "video/mp4",
            Variant::Thumbnail => "image/webp",
            Variant::Spritesheet => "image/jpeg",
            _ => "application/octet-stream",
        },
    );
    Box::new(|event: &Output, out: &mut Vec<Bytes>| {
        match event {
            Output::Content(bytes) => out.push(bytes.clone()),
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            // The content's bytes have no place for anything else.
            _ => {}
        }
        Ok(())
    })
}

/// A whole answer of `events`.
fn encode_whole(events: &[Output]) -> Result<Bytes, Fault> {
    let mut body = None;
    for event in events {
        match event {
            Output::Job(job) => body = Some(job_json(job)?),
            Output::Jobs(page) => body = Some(jobs_json(page)?),
            Output::Deleted(id) => {
                let v = json!({"id": id, "object": "video.deleted", "deleted": true});
                body = v.as_object().cloned();
            }
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            // A body has no place for another event.
            _ => {}
        }
    }
    json_bytes(&body.ok_or_else(|| Fault::upstream("the provider's answer said no video"))?)
}

/// A 502 for a status `what`, which this format cannot write.
fn unwritable_status(what: &str) -> Fault {
    Fault::new(format!("a video status {what} cannot be written in {NAME}"))
        .with(ErrorCode("untranslatable".into()))
        .with(respan_gateway_http_faults::Status(
            http::StatusCode::BAD_GATEWAY,
        ))
}

/// A job object, its fields all present, `null` where the job says none.
fn job_json(job: &Job) -> Result<Map<String, Value>, Fault> {
    let status = match &job.status {
        Status::Queued => "queued",
        Status::InProgress => "in_progress",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Native { format, status, .. } if &**format == NAME => status,
        Status::Native { format, .. } => {
            return Err(unwritable_status(&format!("of format {format}")));
        }
        _ => return Err(unwritable_status("the format has no form for")),
    };
    let error = job
        .error
        .as_ref()
        .map(|e| json!({"code": e.code, "message": e.message}));
    let v = json!({
        "id": job.id,
        "object": "video",
        "created_at": job.created_at,
        "status": status,
        "model": job.model,
        "progress": job.progress,
        "seconds": job.seconds.map(|s| s.to_string()),
        "size": job.size,
        "completed_at": job.completed_at,
        "expires_at": job.expires_at,
        "error": error,
    });
    let mut v = v.as_object().cloned().unwrap_or_default();
    merge_missing(&mut v, &kept(NAME, &job.native)?);
    v.entry("remixed_from_video_id").or_insert(Value::Null);
    Ok(v)
}

/// A page of jobs.
fn jobs_json(page: &Jobs) -> Result<Map<String, Value>, Fault> {
    let data = page.jobs.iter().map(|j| job_json(j).map(Value::Object));
    let data = data.collect::<Result<Vec<_>, _>>()?;
    let v = json!({
        "object": "list",
        "data": data,
        "first_id": page.jobs.first().map(|j| &j.id),
        "last_id": page.jobs.last().map(|j| &j.id),
        "has_more": page.has_more,
    });
    let mut v = v.as_object().cloned().unwrap_or_default();
    merge_missing(&mut v, &kept(NAME, &page.native)?);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(method: Method, path: &str) -> Option<Route> {
        route(&method, &path.parse().unwrap_or_default())
    }

    #[test]
    fn a_videos_path_is_routed_by_its_end_and_method() {
        let id = || "video_Ab1".to_owned();
        for (method, path, want) in [
            (Method::POST, "/v1/videos", Some(Route::Create)),
            (Method::GET, "/v1/videos?limit=2", Some(Route::List)),
            (
                Method::GET,
                "/v1/videos/video_Ab1",
                Some(Route::Retrieve(id())),
            ),
            (
                Method::DELETE,
                "/v1/videos/video_Ab1/",
                Some(Route::Delete(id())),
            ),
            (
                Method::GET,
                "/v1/videos/video_Ab1/content",
                Some(Route::Content(id())),
            ),
            (
                Method::GET,
                "/openai/v1/Videos/video_Ab1/Content?variant=thumbnail",
                Some(Route::Content(id())),
            ),
            (
                Method::POST,
                "/v1/videos/video_Ab1/remix",
                Some(Route::Remix(id())),
            ),
            (
                Method::GET,
                "/v1/videos/a%2Fb",
                Some(Route::Retrieve("a/b".into())),
            ),
            (Method::GET, "/v1/videos/video_Ab1/remix", None),
            (Method::POST, "/v1/videos/video_Ab1", None),
            (Method::POST, "/v1/videos/video_Ab1/content", None),
            (Method::GET, "/v1/videos/video_Ab1/content/x", None),
            (Method::DELETE, "/v1/videos", None),
            (Method::GET, "/v1/images", None),
        ] {
            assert_eq!(routed(method.clone(), path), want, "{method} {path}");
        }
    }

    #[test]
    fn an_id_is_escaped_in_a_calls_path() {
        let mut head = http::Request::new(()).into_parts().0;
        let input = Input::Retrieve {
            id: "../a/b?c".into(),
        };
        assert!(call(&mut head, &input).is_ok());
        assert_eq!(head.uri, "/videos/..%2Fa%2Fb%3Fc");
    }
}
