//! OpenAI Videos: a job made by `POST /videos` (JSON, or a form with an
//! `input_reference` image), polled by `GET /videos/{id}`, downloaded by
//! `GET /videos/{id}/content`, remade by `POST /videos/{id}/remix`, deleted
//! by `DELETE /videos/{id}` and listed by `GET /videos`.

use super::multipart::{self, Writer};
use super::{
    Fields, Form, bind_client, bytes_of, file_name, invalid, json_bytes, merge_missing, missing,
    rest_of, sends, set, take_count, take_string, unreadable, whole_json, write_field,
};
use crate::edge::{bind_provider, content};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, Native};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::{escape, forwarded_path};
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::video::{Create, Input, Job, JobError, Output, Status};
use serde_json::{Map, Value, json};

const NAME: &str = "openai-videos";

/// What only this format reads of a value ([`Native`]).
enum Key {}

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
    /// The format's name, `openai-videos`.
    pub const NAME: &'static str = NAME;

    /// The fields of this format's own in `native`, to edit, in place of
    /// another format's: the door for this format's dialects.
    pub fn fields_mut(native: &mut Native) -> &mut Map<String, Value> {
        native.fields_mut::<Key>()
    }
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

/// A client's request: a creation, a poll or a video's download typed, any
/// other operation (a remix, a deletion, a page, a still) this format's own,
/// as its method, the end of its path after the job's id (none for a page),
/// its query but `after`, and its body.
fn decode_request(head: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let route = route(&head.method, &head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no videos endpoint"))?;
    let native = |id: Option<String>, rest: Option<&str>, body: Option<Map<String, Value>>| {
        let query = query(&head.uri).into_iter().filter(|(k, _)| k != "after");
        let query: Map<String, Value> = query.map(|(k, v)| (k, v.into())).collect();
        let v = json!({"method": head.method.as_str(), "path": rest, "query": query, "body": body});
        let native = Native::json::<Key>(v);
        Input::Native { id, native }
    };
    let video = matches!(param(&head.uri, "variant").as_deref(), None | Some("video"));
    Ok(match route {
        Route::Create => Input::Create(decode_create(form)?),
        Route::Retrieve(id) => Input::Retrieve { id },
        Route::Content(id) if video => Input::Content { id, url: None },
        Route::Content(id) => native(Some(id), Some("/content"), None),
        Route::Delete(id) => native(Some(id), Some(""), None),
        Route::List => native(param(&head.uri, "after"), None, None),
        Route::Remix(id) => {
            let Fields { map, files } = form.fields()?;
            if let Some((name, _)) = files.first() {
                return Err(invalid(name, "is not a file this endpoint takes"));
            }
            native(Some(id), Some("/remix"), Some(map))
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
    create.native = Native::json::<Key>(map);
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
        Input::Retrieve { id } => (Method::GET, path(id, ""), None),
        Input::Content { id, .. } => (Method::GET, path(id, "/content"), None),
        Input::Native { id, native } => {
            let Some(v) = native.value::<Key>() else {
                let what = "OpenAI has no video operation of another format";
                return Err(Fault::bad_request(what));
            };
            let mut query = form_urlencoded::Serializer::new(String::new());
            let rest = v["path"].as_str();
            if let (None, Some(after)) = (rest, id) {
                query.append_pair("after", after);
            }
            for (k, value) in v["query"].as_object().into_iter().flatten() {
                query.append_pair(k, value.as_str().unwrap_or_default());
            }
            let query = query.finish();
            let at = rest.map_or("/videos".to_owned(), |rest| {
                path(id.as_deref().unwrap_or_default(), rest)
            });
            let uri = match query.is_empty() {
                true => at,
                false => format!("{at}?{query}"),
            };
            let method = v["method"].as_str().unwrap_or_default().as_bytes();
            let method = Method::from_bytes(method).unwrap_or(Method::GET);
            let body = v["body"].as_object().map(json_bytes).transpose()?;
            let body = body.map(|b| (b, "application/json".to_owned()));
            (method, uri, body)
        }
        _ => {
            return Err(Fault::bad_request(
                "OpenAI has no video operation of this kind",
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
    merge_missing(&mut v, create.native.fields::<Key>());
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

/// The events of a whole JSON answer: a job, else, such as a page of jobs
/// or a deletion, this format's own, its jobs' ids typed.
fn decode_whole(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let mut v = whole_json(NAME, raw, |v| {
        matches!(
            v["object"].as_str(),
            Some("video" | "list" | "video.deleted")
        ) || v["id"].is_string()
            || v["data"].is_array()
    })?;
    let job = !matches!(v["object"].as_str(), Some("list" | "video.deleted"))
        && !v["data"].is_array()
        && v["deleted"] != true;
    out.push(match job {
        true => Output::Job(job_of(&v)?),
        false => {
            let mut ids = Vec::new();
            each_id(&mut v, &mut |id| ids.push(id.clone()));
            let native = Native::json::<Key>(v);
            Output::Native { ids, native }
        }
    });
    Ok(())
}

/// Each job id a page or a deletion names, in order: its own, then each
/// job's, with the one it was remixed from.
fn each_id(v: &mut Value, f: &mut impl FnMut(&mut String)) {
    let mut visit = |v: &mut Value| {
        for key in ["id", "first_id", "last_id", "remixed_from_video_id"] {
            if let Some(Value::String(id)) = v.get_mut(key) {
                f(id);
            }
        }
    };
    visit(v);
    let data = v.get_mut("data").and_then(Value::as_array_mut);
    data.into_iter().flatten().for_each(visit);
}

fn id_of(v: &Value) -> Result<String, Fault> {
    let id = v["id"].as_str().map(str::to_owned);
    id.ok_or_else(|| unreadable(NAME, "names no video id"))
}

/// A job object: its fields the kind has a form for typed, the rest, and
/// any it could not read, native.
fn job_of(v: &Value) -> Result<Job, Fault> {
    let status = match v["status"].as_str() {
        Some("queued") => Status::Queued,
        Some("in_progress") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed") => Status::Failed,
        Some(other) => Status::Native(Native::json::<Key>(other)),
        None => return Err(unreadable(NAME, "names no video status")),
    };
    let mut job = Job::new(id_of(v)?, status);
    let string = |key: &str| v[key].as_str().map(str::to_owned);
    job.model = string("model");
    job.expires_at = v["expires_at"].as_u64();
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
        ("expires_at", job.expires_at.is_some()),
        ("seconds", job.seconds.is_some()),
        ("size", job.size.is_some()),
        ("error", job.error.is_some()),
    ];
    // A field the kind could not read, such as a fractional length, stays.
    let read = typed
        .iter()
        .filter(|(key, typed)| *typed || v[*key].is_null());
    let mut taken = vec!["id", "object", "status"];
    taken.extend(read.map(|(key, _)| *key));
    job.native = Native::json::<Key>(rest_of(v, &taken));
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
        match param(request.uri(), "variant").as_deref() {
            Some("thumbnail") => "image/webp",
            Some("spritesheet") => "image/jpeg",
            _ => "video/mp4",
        },
    );
    Box::new(|event: &Output, out: &mut Vec<Bytes>| {
        // The content's bytes have no place for anything else.
        if let Output::Content(bytes) = event {
            out.push(bytes.clone());
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
            Output::Native { ids, native } => {
                let Some(v) = native.value::<Key>() else {
                    continue;
                };
                let (mut v, mut ids) = (v.clone(), ids.iter());
                each_id(&mut v, &mut |id| {
                    if let Some(given) = ids.next() {
                        id.clone_from(given);
                    }
                });
                body = v.as_object().cloned();
            }
            // A body has no place for another event.
            _ => {}
        }
    }
    json_bytes(&body.ok_or_else(|| Fault::upstream("the provider's answer said no video"))?)
}

/// A job object, its fields all present, `null` where the job says none.
fn job_json(job: &Job) -> Result<Map<String, Value>, Fault> {
    let status = match &job.status {
        Status::Queued => "queued",
        Status::InProgress => "in_progress",
        Status::Completed => "completed",
        Status::Failed => "failed",
        // WARN: another format's state has no form here: it is written as
        // `in_progress`, so the client polls on.
        Status::Native(native) => {
            (native.value::<Key>().and_then(Value::as_str)).unwrap_or("in_progress")
        }
        _ => "in_progress",
    };
    let error = job
        .error
        .as_ref()
        .map(|e| json!({"code": e.code, "message": e.message}));
    let v = json!({
        "id": job.id,
        "object": "video",
        "status": status,
        "model": job.model,
        "seconds": job.seconds.map(|s| s.to_string()),
        "size": job.size,
        "expires_at": job.expires_at,
        "error": error,
    });
    let mut v = v.as_object().cloned().unwrap_or_default();
    merge_missing(&mut v, job.native.fields::<Key>());
    for key in [
        "created_at",
        "progress",
        "completed_at",
        "remixed_from_video_id",
    ] {
        v.entry(key).or_insert(Value::Null);
    }
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
    fn an_operation_is_called_as_it_was_asked_its_id_escaped() {
        let mut head = http::Request::new(()).into_parts().0;
        let input = Input::Retrieve {
            id: "../a/b?c".into(),
        };
        assert!(call(&mut head, &input).is_ok());
        assert_eq!(head.uri, "/videos/..%2Fa%2Fb%3Fc");
        // The format's own operations go as they came.
        for (method, asked, called) in [
            (
                Method::GET,
                "/v1/videos?after=video_1&limit=2",
                "/videos?after=video_1&limit=2",
            ),
            (Method::DELETE, "/v1/videos/a%2Fb", "/videos/a%2Fb"),
            (
                Method::POST,
                "/v1/videos/video_1/remix",
                "/videos/video_1/remix",
            ),
            (
                Method::GET,
                "/v1/videos/video_1/content?variant=thumbnail",
                "/videos/video_1/content?variant=thumbnail",
            ),
        ] {
            let client = http::Request::builder().method(method.clone()).uri(asked);
            let client = client.body(()).unwrap_or_default().into_parts().0;
            let raw = Bytes::from_static(br#"{"prompt": "slower"}"#);
            let form = Form {
                headers: &client.headers,
                raw: &raw,
                original: None,
            };
            let input = decode_request(&client, &form).expect("read");
            let mut head = http::Request::new(()).into_parts().0;
            call(&mut head, &input).expect("a call");
            assert_eq!(
                (&head.method, head.uri.to_string()),
                (&method, called.to_owned())
            );
        }
        let page = json!({"object": "list", "data": [{"id": "video_1"}], "has_more": false});
        let mut out = Vec::new();
        decode_whole(Bytes::from(page.to_string()), &mut out).expect("read");
        assert!(matches!(&out[..], [Output::Native { ids, .. }] if ids == &["video_1"]));
        let written: Value =
            serde_json::from_slice(&encode_whole(&out).expect("written")).expect("JSON");
        assert_eq!(written, page);
    }

    #[test]
    fn a_job_is_written_back_with_its_times_and_progress() {
        let job = json!({"id": "video_1", "object": "video", "status": "in_progress",
            "model": "sora-2", "seconds": "8", "size": "1280x720", "expires_at": null,
            "error": null, "created_at": 1, "progress": 40.5, "completed_at": null,
            "remixed_from_video_id": null});
        let mut out = Vec::new();
        decode_whole(Bytes::from(job.to_string()), &mut out).expect("read");
        let written = encode_whole(&out).expect("written");
        let written: Value = serde_json::from_slice(&written).expect("JSON");
        assert_eq!(written, job);
        // Another format's job has them all, as null.
        let written = job_json(&Job::new("veo_1", Status::Queued)).expect("written");
        assert!(
            ["created_at", "progress", "completed_at"]
                .iter()
                .all(|k| written[*k].is_null())
        );
    }
}
