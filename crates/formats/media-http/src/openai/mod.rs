//! OpenAI's media endpoints over HTTP, as OpenAI and compatible vendors
//! serve them, one format per kind: [`OpenAiImages`], [`OpenAiTranscriptions`],
//! [`OpenAiSpeech`] and [`OpenAiVideos`]. Each is a client format over
//! `ClientIo` and a provider format over `ProviderIo`; a client's request is
//! recognised by its method and a path ending in its endpoint (a `POST` but
//! for a video job's), and a provider's base URL
//! includes the version, as OpenAI's SDKs take it
//! (`https://api.openai.com/v1`).
//!
//! A request is decoded into its kind's `Input`, its fields the kind has no
//! place for kept native, and written anew from it; a response, whole or
//! streamed, is decoded into the kind's events and written anew from them,
//! in the framing the client asked for. Whether a provider is asked to
//! stream follows the call's [`StreamResponse`], else the client's request
//! if it was to the same endpoint, else not.
//!
//! A provider's error is read leniently, as compatible servers write it,
//! and a client's in OpenAI's schema, `{"error": {"message", "type",
//! "param", "code"}}`, under its status.

mod embeddings;
mod images;
mod multipart;
mod speech;
mod transcriptions;
mod videos;

pub use embeddings::OpenAiEmbeddings;
pub use images::OpenAiImages;
pub use respan_gateway_conversation_http::StreamResponse;
pub use speech::OpenAiSpeech;
pub use transcriptions::OpenAiTranscriptions;
pub use videos::OpenAiVideos;

use crate::edge::{bind_provider, sends};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_conversation_http::fields::{
    self, kept, merge_missing, native_of, rest_of, set, unix_now,
};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::{Schema, read_provider_error};
use respan_gateway_core::{Connection, ErrorCode, Fault, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderMap, HeaderValue, StatusCode, request::Parts as RequestHead,
    response::Parts as ResponseHead,
};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{ClientIo, Encoder, ProviderIo, raw::Original};
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

use multipart::Writer;

/// A request's fields, from a JSON object or a form: a form's text fields
/// as strings, those named `name[]` as an array under `name`, and its files
/// apart, by name, `[]` dropped.
#[derive(Debug, Default, Clone)]
pub(crate) struct Fields {
    pub(crate) map: Map<String, Value>,
    pub(crate) files: Vec<(String, Media)>,
}

/// A client's request as its decoder reads it: the head's headers and the
/// body, whose fields are parsed once for every reader of the original.
pub(crate) struct Form<'a> {
    pub(crate) headers: &'a HeaderMap,
    pub(crate) raw: &'a Bytes,
    pub(crate) original: Option<&'a Original>,
}

impl Form<'_> {
    /// The fields of the body, a form if the content type says so, else a
    /// JSON object. The first reader parses them; the response's framing
    /// reads the same ([`asked`]).
    pub(crate) fn fields(&self) -> Result<Fields, Fault> {
        let parsed = self
            .original
            .and_then(|w| w.memo(|r| parse_fields(r.headers(), r.body())));
        match parsed {
            Some(parsed) => parsed.clone(),
            None => parse_fields(self.headers, self.raw),
        }
    }
}

/// The fields of a request of `headers` and body `raw`.
fn parse_fields(headers: &HeaderMap, raw: &Bytes) -> Result<Fields, Fault> {
    let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let Some(boundary) = content_type.and_then(multipart::boundary) else {
        return match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(map)) => Ok(Fields {
                map,
                files: Vec::new(),
            }),
            _ => Err(Fault::bad_request(
                "the request body is neither a JSON object nor multipart/form-data",
            )),
        };
    };
    let mut fields = Fields::default();
    for part in multipart::parse(&boundary, raw)? {
        let (name, listed) = match part.name.strip_suffix("[]") {
            Some(name) => (name.to_owned(), true),
            None => (part.name.clone(), false),
        };
        if let Some(filename) = part.filename {
            let mime = mime_of(part.content_type.as_deref(), &filename, &part.body);
            let mut media = Media::new(mime, MediaSource::Bytes(part.body));
            media.name = Some(filename);
            fields.files.push((name, media));
            continue;
        }
        let text = String::from_utf8(part.body.to_vec())
            .map_err(|_| invalid(&name, "must be UTF-8 text"))?;
        match (fields.map.get_mut(&name), listed) {
            (Some(Value::Array(all)), true) => all.push(text.into()),
            (None, true) => _ = fields.map.insert(name, vec![Value::from(text)].into()),
            (None, false) => _ = fields.map.insert(name, text.into()),
            _ => return Err(invalid(&name, "is given twice")),
        }
    }
    Ok(fields)
}

/// The MIME type of a file sent as `content_type` and `filename`: its own,
/// unless it says none or `application/octet-stream` (which some vendors
/// refuse for an image), then its file name's, then what its first bytes
/// say.
fn mime_of(content_type: Option<&str>, filename: &str, content: &[u8]) -> String {
    let said = content_type
        .and_then(|c| c.split(';').next())
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty() && c != "application/octet-stream");
    let extension = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase());
    let named = extension.and_then(|e| {
        let found = MIME.iter().find(|(ext, _)| *ext == e);
        found.map(|(_, mime)| (*mime).to_owned())
    });
    said.or(named)
        .or_else(|| sniff(content).map(str::to_owned))
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

/// File name extensions and their MIME types; the first of a type is its
/// own extension.
const MIME: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("mp3", "audio/mpeg"),
    ("mpga", "audio/mpeg"),
    ("mpeg", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("m4a", "audio/mp4"),
    ("mp4", "audio/mp4"),
    ("webm", "audio/webm"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("flac", "audio/flac"),
    ("opus", "audio/opus"),
    ("aac", "audio/aac"),
    ("pcm", "audio/pcm"),
];

/// The type a file's first bytes say, for the formats these endpoints take.
fn sniff(content: &[u8]) -> Option<&'static str> {
    let at = |from: usize, magic: &[u8]| content.get(from..from + magic.len()) == Some(magic);
    Some(match () {
        _ if at(0, b"\x89PNG") => "image/png",
        _ if at(0, b"\xFF\xD8\xFF") => "image/jpeg",
        _ if at(0, b"GIF8") => "image/gif",
        _ if at(0, b"RIFF") && at(8, b"WEBP") => "image/webp",
        _ if at(0, b"RIFF") && at(8, b"WAVE") => "audio/wav",
        _ if at(0, b"ID3") || content.first() == Some(&0xFF) => "audio/mpeg",
        _ if at(0, b"OggS") => "audio/ogg",
        _ if at(0, b"fLaC") => "audio/flac",
        _ if at(0, b"\x1A\x45\xDF\xA3") => "audio/webm",
        _ if at(4, b"ftyp") => "audio/mp4",
        _ => return None,
    })
}

/// A file name for media of type `mime` that has none, such as `image.png`.
fn file_name(media: &Media, stem: &str) -> String {
    if let Some(name) = &media.name {
        return name.clone();
    }
    let found = MIME.iter().find(|(_, m)| *m == media.mime_type);
    format!("{stem}.{}", found.map_or("bin", |(ext, _)| ext))
}

/// The bytes of `media`, or `None` for a URL.
fn bytes_of(media: &Media) -> Option<&Bytes> {
    match &media.source {
        MediaSource::Bytes(bytes) => Some(bytes),
        MediaSource::Url(_) => None,
    }
}

/// A 400 for a request whose field `param` `what`, as OpenAI words one.
pub(crate) fn invalid(param: &str, what: &str) -> Fault {
    fields::invalid(param, what).with(ErrorCode("invalid_request_error".into()))
}

/// A 400 for a request without the field `param`.
pub(crate) fn missing(param: &str) -> Fault {
    invalid(param, "is required")
}

/// The string field `key`, taken from `map`.
pub(crate) fn take_string(
    map: &mut Map<String, Value>,
    key: &str,
) -> Result<Option<String>, Fault> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(invalid(key, "must be a string")),
    }
}

/// The number field `key`, taken from `map`: a JSON number, or a form's
/// text of one.
fn take_number(map: &mut Map<String, Value>, key: &str) -> Result<Option<f64>, Fault> {
    let number = match map.remove(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        Some(_) => None,
    };
    number
        .filter(|n: &f64| n.is_finite())
        .map(Some)
        .ok_or_else(|| invalid(key, "must be a number"))
}

/// The whole number field `key`, taken from `map`.
pub(crate) fn take_count(map: &mut Map<String, Value>, key: &str) -> Result<Option<u32>, Fault> {
    let count = match map.remove(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        Some(_) => None,
    };
    count
        .and_then(|n| u32::try_from(n).ok())
        .map(Some)
        .ok_or_else(|| invalid(key, "must be a whole number"))
}

/// Whether a field says true: JSON's `true`, or a form's `true`.
fn is_true(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// `seconds` as JSON: a whole number as one, as OpenAI writes it.
fn seconds_json(seconds: f64) -> Value {
    match seconds.fract() == 0.0 && (0.0..9e15).contains(&seconds) {
        true => (seconds as u64).into(),
        false => seconds.into(),
    }
}

/// The token usage of a usage object `u`, and its fields the kind has no
/// place for but `known`, such as a split by modality.
fn read_usage(u: &Value, known: &[&str]) -> Result<(Usage, Map<String, Value>), Fault> {
    let count = |key: &str| match &u[key] {
        Value::Null => Ok(0),
        v => v
            .as_u64()
            .ok_or_else(|| Fault::upstream(format!("a usage count is not a whole number: {v}"))),
    };
    let usage = Usage::new(count("input_tokens")?, count("output_tokens")?);
    let mut taken = vec!["input_tokens", "output_tokens", "total_tokens"];
    taken.extend_from_slice(known);
    Ok((usage, rest_of(u, &taken)))
}

/// `usage` as OpenAI's media endpoints write it.
fn write_usage(usage: &Usage) -> Map<String, Value> {
    let mut u = Map::new();
    u.insert("input_tokens".into(), usage.input_tokens.into());
    u.insert("output_tokens".into(), usage.output_tokens.into());
    let total = usage.input_tokens.saturating_add(usage.output_tokens);
    u.insert("total_tokens".into(), total.into());
    u
}

/// The fields of a usage object the kind has no place for, as an event of
/// `format`, `{"usage": {..}}`, which its encoders write into the usage they
/// write; none if there are none.
pub(crate) fn usage_extras(format: &str, extras: Map<String, Value>) -> Option<NativeValue> {
    (!extras.is_empty()).then(|| NativeValue::new(format, json!({ "usage": extras })))
}

/// The usage fields of an event [`usage_extras`] made.
pub(crate) fn as_usage_extras(v: &Value) -> Option<&Map<String, Value>> {
    let o = v.as_object().filter(|o| o.len() == 1)?;
    o.get("usage")?.as_object()
}

/// An event of another format than `format`, which it cannot write.
pub(crate) fn foreign(format: &str, native: &NativeValue) -> Fault {
    Fault::new(format!(
        "an event of format {} cannot be written in {format}",
        native.format
    ))
    .with(ErrorCode("untranslatable".into()))
    .with(respan_gateway_http_faults::Status(StatusCode::BAD_GATEWAY))
}

/// Bytes as base64.
fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Base64 `text` as bytes.
fn unb64(text: &str) -> Option<Bytes> {
    STANDARD.decode(text.trim()).ok().map(Bytes::from)
}

/// `v` as JSON bytes.
pub(crate) fn json_bytes(v: &Map<String, Value>) -> Result<Bytes, Fault> {
    serde_json::to_vec(v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))
}

/// One server-sent event of `v`, without a name, as OpenAI's SDKs read it
/// by its `type`.
fn sse_event(v: &Value) -> String {
    respan_gateway_sse::event(None, v)
}

/// The JSON of a provider's stream event `raw` of `format`: `None` for one
/// with no data or `[DONE]`; data that is no JSON as a string; an error
/// event as its fault.
fn event_json(_format: &str, raw: &[u8]) -> Result<Option<Value>, Fault> {
    let Some(data) = respan_gateway_sse::event_data(raw) else {
        return Ok(None);
    };
    if data.trim() == "[DONE]" {
        return Ok(None);
    }
    let Ok(v) = serde_json::from_str::<Value>(&data) else {
        return Ok(Some(Value::String(data)));
    };
    match says_error(&v) {
        true => Err(said_error(Bytes::from(data))),
        false => Ok(Some(v)),
    }
}

/// Whether a provider's JSON says an error: an `error` type, or an `error`
/// that says something.
fn says_error(v: &Value) -> bool {
    v["type"] == "error"
        || match &v["error"] {
            Value::Object(o) => !o.is_empty(),
            Value::String(s) => !s.is_empty(),
            _ => false,
        }
}

/// The fault of an error a provider said in a success: read
/// as its error answers are, a 502 unless it says another status.
fn said_error(raw: Bytes) -> Fault {
    let mut answer = http::Response::new(raw);
    let json = HeaderValue::from_static("application/json");
    answer.headers_mut().insert(CONTENT_TYPE, json);
    read_provider_error(answer)
}

/// A provider's whole JSON body `raw` of `format`, if `is_response` says it
/// is one; else the error it says, or an `upstream` fault.
pub(crate) fn whole_json(
    format: &str,
    raw: Bytes,
    is_response: fn(&Value) -> bool,
) -> Result<Value, Fault> {
    let v = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    if is_response(&v) {
        return Ok(v);
    }
    Err(match says_error(&v) {
        true => said_error(raw),
        false => Fault::upstream(format!(
            "the provider's success is not a response of {format}"
        )),
    })
}

/// A provider's answer of `format` that cannot be read, such as a
/// malformed image.
pub(crate) fn unreadable(format: &str, what: &str) -> Fault {
    Fault::upstream(format!("the provider's {format} response {what}"))
}

/// A refusal in OpenAI's error schema, as Chat Completions writes it.
fn refusal(fault: &Fault, request: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    OpenAiChat.refusal(fault, request, head)
}

/// Chooses the encoder of a response from the client's request, the text
/// fields it asked ([`asked`]) and the response's head, whose content type
/// it sets.
pub(crate) type ChooseEncoder<O> =
    fn(&http::Request<Bytes>, &Map<String, Value>, &mut ResponseHead) -> Box<dyn Encoder<O>>;

/// The client edge of a format: a request decoded by `decode`, and its
/// response written by the encoder `encoder` chooses for the client's
/// request, with the head's content type it sets.
pub(crate) fn bind_client<I, O>(
    io: ClientIo,
    decode: fn(&RequestHead, &Form) -> Result<I, Fault>,
    encoder: ChooseEncoder<O>,
) -> Connection<I, O>
where
    I: Clone + Send + Sync + 'static,
    O: Clone + Send + Sync + 'static,
{
    let original = io.extensions().get::<Original>().cloned();
    let read = original.clone();
    io.with_error(refusal).bind(
        move |head| {
            let head = head.clone();
            Box::new(move |raw: Bytes, out: &mut Vec<I>| {
                let form = Form {
                    headers: &head.headers,
                    raw: &raw,
                    original: read.as_ref(),
                };
                out.push(decode(&head, &form)?);
                Ok(())
            })
        },
        move |reply| {
            let mut head = reply.head();
            let fields = asked(original.as_ref());
            let encoder = encoder(reply.request(), fields, &mut head);
            Ok(reply.write(head, encoder))
        },
    )
}

/// Set a call's path and content type.
pub(crate) fn post_to(head: &mut RequestHead, path: &str, content_type: &str) -> Result<(), Fault> {
    respan_gateway_http::path::set_path(head, path)?;
    let content_type = HeaderValue::from_str(content_type)
        .map_err(|e| Fault::internal("a content type is no header").with_source(e))?;
    head.headers.insert(CONTENT_TYPE, content_type);
    Ok(())
}

/// Whether a provider is asked to stream: as the call's [`StreamResponse`]
/// says, else as the client's request did, if to one of `paths`, by
/// `wants`, else not.
fn streams(io: &ProviderIo, paths: &[&str], wants: fn(&Map<String, Value>) -> bool) -> bool {
    if let Some(StreamResponse(stream)) = io.extensions().get::<StreamResponse>() {
        return *stream;
    }
    let original = io.extensions().get::<Original>();
    let paths_match = original
        .and_then(Original::get)
        .is_some_and(|r| paths.iter().any(|p| path_ends_with(r.uri(), p)));
    paths_match && wants(asked(original))
}

/// The text fields of a client's request, for its response's framing, as
/// its decoder parsed them ([`Form::fields`]); none if it cannot be read, as
/// it then was refused.
fn asked(original: Option<&Original>) -> &Map<String, Value> {
    static NONE: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    let parsed = original.and_then(|w| w.memo(|r| parse_fields(r.headers(), r.body())));
    parsed
        .and_then(|p| p.as_ref().ok())
        .map_or(&NONE, |f| &f.map)
}

/// Write the field `name` of JSON `value` into a form, as OpenAI's SDKs
/// do: text as it is, numbers and booleans as JSON, an array as one
/// `name[]` field per item, an object as JSON text.
fn write_field(form: &mut Writer, name: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::String(s) => form.text(name, s),
        Value::Array(items) => {
            let listed = format!("{name}[]");
            for item in items {
                write_field(form, &listed, item);
            }
        }
        v => form.text(name, &v.to_string()),
    }
}

/// Set a response's content type.
pub(crate) fn content_type(head: &mut ResponseHead, value: &'static str) {
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_type_is_its_own_else_its_names_else_its_bytes() {
        let png = b"\x89PNG\r\n";
        assert_eq!(mime_of(Some("image/webp"), "a.png", png), "image/webp");
        assert_eq!(
            mime_of(Some("application/octet-stream"), "a.JPG", png),
            "image/jpeg"
        );
        assert_eq!(mime_of(None, "upload", png), "image/png");
        assert_eq!(mime_of(None, "upload", b"RIFF\0\0\0\0WAVEfmt"), "audio/wav");
        assert_eq!(mime_of(None, "x", b"?"), "application/octet-stream");
    }

    #[test]
    fn a_fault_is_written_in_openais_error_schema() {
        let mut head = http::Response::new(()).into_parts().0;
        head.status = StatusCode::BAD_REQUEST;
        let request = http::Request::new(Bytes::new());
        let body = refusal(&missing("prompt"), &request, &mut head);
        let v: Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(
            v,
            json!({"error": {
                "message": "'prompt' is required",
                "type": "invalid_request_error",
                "param": "prompt",
                "code": "invalid_request_error",
            }})
        );
    }

    #[test]
    fn an_array_is_written_as_a_field_per_item() {
        let mut form = Writer::with_boundary("b".into());
        write_field(&mut form, "include", &json!(["logprobs", 1]));
        write_field(&mut form, "chunking", &json!({"type": "auto"}));
        write_field(&mut form, "none", &Value::Null);
        let parts = multipart::parse("b", &form.finish()).unwrap_or_default();
        let read: Vec<_> = parts
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    String::from_utf8_lossy(&p.body).into_owned(),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ("include[]", "logprobs".to_owned()),
                ("include[]", "1".to_owned()),
                ("chunking", "{\"type\":\"auto\"}".to_owned()),
            ]
        );
    }

    #[test]
    fn a_form_is_parsed_once_for_the_decoder_and_the_framing() {
        let mut form = Writer::with_boundary("b".into());
        form.text("stream", "true");
        form.file("file", "a.wav", "audio/wav", b"RIFF\0\0\0\0WAVEfmt");
        let body = form.finish();
        let request = http::Request::post("/v1/audio/transcriptions")
            .header(CONTENT_TYPE, "multipart/form-data; boundary=b")
            .body(body)
            .unwrap_or_default();
        let original = Original::new(request);
        let request = original.get().cloned().unwrap_or_default();
        let (head, raw) = request.into_parts();
        let decoding = Form {
            headers: &head.headers,
            raw: &raw,
            original: Some(&original),
        };
        let read = decoding.fields().expect("fields");
        assert_eq!(read.files.len(), 1, "the decoder gets the files");
        let framing = asked(Some(&original));
        assert_eq!(framing["stream"], "true", "the framing gets the text");
        let parsed = original.memo(|_| -> Result<Fields, Fault> { unreachable!("parsed again") });
        assert!(parsed.is_some_and(Result::is_ok), "both read the one parse");
    }
}
