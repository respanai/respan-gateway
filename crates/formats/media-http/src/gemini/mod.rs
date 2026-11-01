//! Google's media models as the Gemini API serves them, over HTTP: Veo
//! ([`GeminiVeo`]), and its image models through `generateContent`
//! ([`GeminiImages`]) and transcriptions ([`GeminiTranscriptions`]). A
//! provider's base URL includes the version,
//! `https://generativelanguage.googleapis.com/v1beta`, and its transport the
//! key, `x-goog-api-key`.

mod images;
mod transcriptions;
mod veo;

pub use images::GeminiImages;
pub use transcriptions::GeminiTranscriptions;
pub use veo::GeminiVeo;

use crate::edge::sends;
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::fields::invalid;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_core::Fault;
use respan_gateway_http::Encoder;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::http::{self, HeaderValue, request::Parts as RequestHead};
use serde_json::{Map, Value, json};

/// Whether `s` is a path segment that every reader reads as it is.
fn is_segment(s: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    !s.is_empty() && s != "." && s != ".." && s.chars().all(plain)
}

/// `model` as `generateContent` names it, without its `models/`, if it is
/// a plain path segment; else refused, saying what it `needs`.
fn model_of<'a>(model: Option<&'a str>, needs: &str) -> Result<&'a str, Fault> {
    let model = model.unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    match is_segment(model) {
        true => Ok(model),
        false => Err(Fault::bad_request(needs.to_owned())),
    }
}

/// The `generationConfig` of native fields, taken out of them.
fn config_of(native: &mut Map<String, Value>) -> Result<Map<String, Value>, Fault> {
    match native.remove("generationConfig") {
        Some(Value::Object(config)) => Ok(config),
        None => Ok(Map::new()),
        Some(_) => Err(invalid("generationConfig", "must be an object")),
    }
}

/// A call to `model`'s `generateContent` of one user turn of `parts`, its
/// `config` if it says anything, and its native fields.
fn generate<I: 'static>(
    head: &mut RequestHead,
    model: &str,
    parts: Value,
    config: Map<String, Value>,
    native: Map<String, Value>,
) -> Result<Box<dyn Encoder<I>>, Fault> {
    let mut v = Map::new();
    v.insert(
        "contents".into(),
        json!([{ "role": "user", "parts": parts }]),
    );
    if !config.is_empty() {
        v.insert("generationConfig".into(), config.into());
    }
    for (key, value) in native {
        v.entry(key).or_insert(value);
    }
    head.uri = format!("/models/{model}:generateContent")
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    let body = serde_json::to_vec(&v)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// A client's refusal, as Gemini's `generateContent` writes it:
/// `{"error": {code, message, status}}`.
fn refusal(fault: &Fault, request: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    GeminiGenerate.refusal(fault, request, head)
}
