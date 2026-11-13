//! A body's content, as formats read it: its content type, whether a
//! response is a stream of server-sent events ([`is_stream`]; reading one
//! is [`Sse`](crate::Sse)'s), a body's or an event's JSON, and the bytes of
//! base64 or a `data:` URL a body embeds ([`base64_bytes`], [`data_url`]).
//! The formats
//! themselves are the `respan-gateway-*-http` crates'; traffic of no kind
//! relays through `respan-gateway-passthrough-http`.

use respan_gateway_http::http::{
    self, HeaderValue, header::CONTENT_TYPE, response::Parts as ResponseHead,
};

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde_json::{self, Value};
use std::borrow::Cow;

/// Base64 decoding that accepts the data with or without its padding, and
/// a last symbol whose unused bits are not zero.
const fn lenient(alphabet: &'static alphabet::Alphabet) -> GeneralPurpose {
    let config = GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true);
    GeneralPurpose::new(alphabet, config)
}

/// The base64 alphabets embedded data is read in, the standard one first.
const EMBEDDED_BASE64: [GeneralPurpose; 2] =
    [lenient(&alphabet::STANDARD), lenient(&alphabet::URL_SAFE)];

/// The bytes of base64 `data` a body embeds, read with or without padding,
/// in either alphabet, across the line breaks it may be wrapped at: what
/// any provider reads, and more. `None` if it is not base64.
pub fn base64_bytes(data: &str) -> Option<Vec<u8>> {
    let data = match data.contains(|c: char| c.is_ascii_whitespace()) {
        true => Cow::Owned(data.chars().filter(|c| !c.is_ascii_whitespace()).collect()),
        false => Cow::Borrowed(data),
    };
    EMBEDDED_BASE64
        .iter()
        .find_map(|engine| engine.decode(data.as_bytes()).ok())
}

/// Why the data of a `data:` URL cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataUrlError {
    /// It has no `,` before its data.
    NoData,
    /// It says it is base64, and its data does not decode.
    NotBase64,
}

/// The media type and bytes of `url` if it is a `data:` URL (RFC 2397):
/// base64 when a parameter of its header says so, in any case, and
/// percent-encoded bytes otherwise, its data percent-decoded either way; its
/// type is `text/plain` when it names none. `None` for any other URL.
pub fn data_url(url: &str) -> Option<Result<(String, Vec<u8>), DataUrlError>> {
    let data_url = url.strip_prefix("data:")?;
    let Some((header, data)) = data_url.split_once(',') else {
        return Some(Err(DataUrlError::NoData));
    };
    let mut params = header.split(';');
    let mime = Some(params.next().unwrap_or_default().trim())
        .filter(|m| !m.is_empty())
        .unwrap_or("text/plain");
    let data = Cow::from(percent_encoding::percent_decode_str(data));
    let bytes = match params.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        true => std::str::from_utf8(&data)
            .ok()
            .and_then(base64_bytes)
            .ok_or(DataUrlError::NotBase64),
        false => Ok(data.into_owned()),
    };
    Some(bytes.map(|bytes| (mime.to_owned(), bytes)))
}

/// A provider's message as JSON: a whole body, or an event's `data:`.
pub fn json(raw: impl AsRef<[u8]>) -> Option<Value> {
    let raw = raw.as_ref();
    serde_json::from_slice(raw)
        .ok()
        .or_else(|| respan_gateway_sse::event_json(raw))
}

/// Set a response's content type to `value` if it has none (RFC 9110 8.3).
pub fn default_content_type(head: &mut ResponseHead, value: &'static str) {
    let value = HeaderValue::from_static(value);
    head.headers.entry(CONTENT_TYPE).or_insert(value);
}

/// A response's content type, if it is readable.
pub fn content_type(head: &ResponseHead) -> Option<&str> {
    let value = head.headers.get(http::header::CONTENT_TYPE)?;
    value.to_str().ok()
}

/// Whether a response is a stream: a success labelled `text/event-stream`.
pub fn is_stream(head: &ResponseHead) -> bool {
    let essence = content_type(head).and_then(|c| c.split(';').next());
    let sse = essence.is_some_and(|e| e.trim().eq_ignore_ascii_case("text/event-stream"));
    sse && head.status.is_success()
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_sse::event;

    #[test]
    fn a_stream_is_a_success_labelled_as_sse_in_any_case() {
        let head = |status: u16, content_type: &'static str| {
            let mut head = http::Response::new(()).into_parts().0;
            head.status = http::StatusCode::from_u16(status).unwrap_or_default();
            let value = http::HeaderValue::from_static(content_type);
            head.headers.insert(http::header::CONTENT_TYPE, value);
            head
        };
        assert!(is_stream(&head(200, "Text/Event-Stream; charset=utf-8")));
        assert!(!is_stream(&head(200, "text/event-streamx")));
        assert!(!is_stream(&head(503, "text/event-stream")));
    }

    #[test]
    fn a_body_or_an_events_data_reads_as_json() {
        assert_eq!(json(b"{\"b\":3}"), Some(serde_json::json!({"b": 3})));
        assert_eq!(
            json(event(None, "{\"a\":2}").as_bytes()),
            Some(serde_json::json!({"a": 2}))
        );
    }
}
