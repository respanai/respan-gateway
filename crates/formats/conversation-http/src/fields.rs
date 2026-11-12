//! JSON object helpers for formats that read a body's fields and write
//! them back, keeping what they have no place for.

use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_core::{Details, Fault};
use serde_json::{Map, Value};

/// `media` as a URL: its own, or a data URL of its bytes.
pub fn media_url(media: &Media) -> String {
    match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => {
            format!("data:{};base64,{}", media.mime_type, STANDARD.encode(bytes))
        }
    }
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
