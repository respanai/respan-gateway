//! JSON object helpers for formats that read a body's fields and write
//! them back, keeping what they have no place for.

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine, alphabet, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_core::{Details, Fault};
use serde_json::{Map, Value};
use std::borrow::Cow;

/// `media` as a URL: its own, or a data URL of its bytes.
pub fn media_url(media: &Media) -> String {
    match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => {
            format!("data:{};base64,{}", media.mime_type, STANDARD.encode(bytes))
        }
    }
}

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
    /// It has no `,` before its data, or no data after it.
    NoData,
    /// It says it is base64, and its data does not decode.
    NotBase64,
}

impl DataUrlError {
    /// What is wrong with the URL, to follow the name of the field holding it.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoData => "is a `data:` URL with no data",
            Self::NotBase64 => "is not valid base64",
        }
    }
}

/// The media type and bytes of `url` if it is a `data:` URL (RFC 2397):
/// base64 when a parameter of its header says so, in any case, and
/// percent-encoded bytes otherwise, its data percent-decoded either way; its
/// type is `text/plain` when it names none. `Ok(None)` for any other URL.
pub fn data_url(url: &str) -> Result<Option<(String, Vec<u8>)>, DataUrlError> {
    let Some(data_url) = url.strip_prefix("data:") else {
        return Ok(None);
    };
    let (header, data) = data_url.split_once(',').ok_or(DataUrlError::NoData)?;
    let mut params = header.split(';');
    let mime = Some(params.next().unwrap_or_default().trim())
        .filter(|m| !m.is_empty())
        .unwrap_or("text/plain");
    let data = Cow::from(percent_encoding::percent_decode_str(data));
    let bytes = match params.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        true => std::str::from_utf8(&data)
            .ok()
            .and_then(base64_bytes)
            .ok_or(DataUrlError::NotBase64)?,
        false => data.into_owned(),
    };
    if bytes.is_empty() {
        return Err(DataUrlError::NoData);
    }
    Ok(Some((mime.to_owned(), bytes)))
}

/// The seconds since the Unix epoch, as a response's `created` says.
pub fn unix_now() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs())
}

/// The fields of `v` but those named `taken` and null ones.
pub fn rest_of(v: &Value, taken: &[&str]) -> Map<String, Value> {
    let fields = v.as_object().into_iter().flatten();
    let rest = fields.filter(|(k, v)| !taken.contains(&k.as_str()) && !v.is_null());
    rest.map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Add to `base` the fields of `more` it lacks, objects field by field:
/// what a value's typed fields say takes precedence over its native.
pub fn merge_missing(base: &mut Map<String, Value>, more: &Map<String, Value>) {
    for (key, value) in more {
        match (base.get_mut(key), value) {
            (Some(Value::Object(had)), Value::Object(more)) => merge_missing(had, more),
            (Some(_), _) => {}
            (None, value) => _ = base.insert(key.clone(), value.clone()),
        }
    }
}

/// Insert `value` at `key` if it says something.
pub fn set(map: &mut Map<String, Value>, key: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        map.insert(key.into(), value.into());
    }
}

/// A 400 for a request whose field `param` `what`, naming it in its
/// details.
pub fn invalid(param: &str, what: &str) -> Fault {
    let mut details = Map::new();
    details.insert("param".into(), param.into());
    Fault::bad_request(format!("'{param}' {what}")).with(Details(details))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn merging_keeps_what_the_base_says() {
        let mut base = object(json!({"a": 1, "o": {"x": 1}}));
        let more = object(json!({"a": 2, "b": 3, "o": {"x": 2, "y": 3}}));
        merge_missing(&mut base, &more);
        let merged = json!({"a": 1, "b": 3, "o": {"x": 1, "y": 3}});
        assert_eq!(Value::Object(base), merged);
    }

    #[test]
    fn the_rest_leaves_out_taken_and_null_fields() {
        let rest = rest_of(&json!({"a": 1, "b": null, "c": 3}), &["a"]);
        assert_eq!(Value::Object(rest), json!({"c": 3}));
    }

    #[test]
    fn setting_nothing_adds_nothing() {
        let mut map = Map::new();
        set(&mut map, "a", None::<u64>);
        set(&mut map, "b", Some(2));
        assert_eq!(Value::Object(map), json!({"b": 2}));
    }
}
