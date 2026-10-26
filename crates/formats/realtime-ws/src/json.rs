//! What the codecs share: a frame's fields taken as the kind's, what is left
//! kept as its format's, audio in base64, and the faults of what a format
//! cannot write.

use base64::Engine;
use base64::alphabet::Alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use respan_gateway_core::{ErrorCode, Fault, NativeFields};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_ws::CloseCode;
use serde_json::{Map, Value};

/// The value at `path` in `v`, removed, if `read` reads it and `write`
/// writes it back as it was; an object the removal leaves empty is removed
/// too. What a frame says is taken as the kind's only where nothing of it
/// is lost.
pub(crate) fn take<T>(
    v: &mut Value,
    path: &[&str],
    read: impl FnOnce(&Value) -> Option<T>,
    write: impl FnOnce(&T) -> Option<Value>,
) -> Option<T> {
    let found = at(v, path)?;
    let value = read(found)?;
    if write(&value).as_ref() != Some(found) {
        return None;
    }
    remove(v, path);
    Some(value)
}

/// The value at `path` in `v`.
pub(crate) fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, key| v.as_object()?.get(*key))
}

/// Set the value at `path` in `v`, making the objects on the way.
pub(crate) fn set(v: &mut Value, path: &[&str], value: Value) {
    match path.split_first() {
        None => *v = value,
        Some((first, rest)) => set(&mut v[*first], rest, value),
    }
}

/// Remove the value at `path` in `v`, and each object that leaves empty.
pub(crate) fn remove(v: &mut Value, path: &[&str]) {
    let (Some(fields), Some((first, rest))) = (v.as_object_mut(), path.split_first()) else {
        return;
    };
    if rest.is_empty() {
        fields.shift_remove(*first);
    } else if let Some(child) = fields.get_mut(*first) {
        remove(child, rest);
        if child.as_object().is_some_and(Map::is_empty) {
            fields.shift_remove(*first);
        }
    }
}

/// A string as itself.
pub(crate) fn string(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// A string written.
pub(crate) fn text(s: &impl AsRef<str>) -> Option<Value> {
    Some(s.as_ref().into())
}

/// A whole number above zero: a zero is the specs' default, kept as it was
/// written.
pub(crate) fn count(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n > 0)
}

/// A number written.
pub(crate) fn number(n: &u64) -> Option<Value> {
    Some((*n).into())
}

/// The fields of `rest`, a frame less what the kind took, as `format`'s;
/// none if it says nothing more.
pub(crate) fn native(format: &str, rest: Value) -> Option<NativeFields> {
    match rest {
        Value::Object(fields) if !fields.is_empty() => Some(NativeFields::new(format, fields)),
        _ => None,
    }
}

/// `v` with the fields of `native` it lacks, objects field by field: what
/// the kind says takes precedence over what its format kept.
pub(crate) fn merge(v: &mut Value, native: Option<&Map<String, Value>>) {
    let (Some(v), Some(native)) = (v.as_object_mut(), native) else {
        return;
    };
    for (key, value) in native {
        match (v.get_mut(key), value) {
            (Some(had), Value::Object(more)) => merge(had, Some(more)),
            (Some(_), _) => {}
            (None, value) => _ = v.insert(key.clone(), value.clone()),
        }
    }
}

/// The fields of `native` to write in `format`: none if there are none, and
/// refused by `refuse` if they are another format's.
pub(crate) fn own<'a>(
    format: &str,
    native: &'a Option<NativeFields>,
    refuse: impl FnOnce() -> Fault,
) -> Result<Option<&'a Map<String, Value>>, Fault> {
    match native {
        None => Ok(None),
        Some(native) if *native.format == *format => Ok(Some(&native.fields)),
        Some(_) => Err(refuse()),
    }
}

/// An input `format` cannot write, which could change the answer: the
/// client's fault, closing its session 1008.
pub(crate) fn untranslatable_input(format: &str, what: &str) -> Fault {
    let message = format!("{what} cannot be translated to {format}");
    Fault::bad_request(message).with(CloseCode(1008))
}

/// An output `format` cannot write, which fails the session.
pub(crate) fn untranslatable_output(format: &str, what: &str) -> Fault {
    let message = format!("{what} cannot be translated to {format}");
    Fault::upstream(message).with(ErrorCode("untranslatable".into()))
}

/// `v` as a frame.
pub(crate) fn frame(v: &Value) -> Result<Bytes, Fault> {
    serde_json::to_vec(v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("an event could not be written").with_source(e))
}

/// Base64 of `alphabet` read with or without its padding.
const fn lenient(alphabet: &'static Alphabet) -> GeneralPurpose {
    GeneralPurpose::new(
        alphabet,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    )
}

const LENIENT: GeneralPurpose = lenient(&base64::alphabet::STANDARD);
const LENIENT_URL_SAFE: GeneralPurpose = lenient(&base64::alphabet::URL_SAFE);

/// The bytes of base64 `v`, standard or URL-safe, padded or not.
pub(crate) fn unbase64(v: &Value) -> Option<Bytes> {
    let text = v.as_str()?;
    LENIENT
        .decode(text)
        .or_else(|_| LENIENT_URL_SAFE.decode(text))
        .ok()
        .map(Bytes::from)
}

/// The audio at `path` in `v`, removed, if it is base64.
pub(crate) fn take_audio(v: &mut Value, path: &[&str]) -> Option<Bytes> {
    let data = unbase64(at(v, path)?)?;
    remove(v, path);
    Some(data)
}

/// `data` as standard, padded base64.
pub(crate) fn base64(data: &[u8]) -> Value {
    STANDARD.encode(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_value_is_taken_only_if_it_writes_back_as_it_came() {
        let mut v = json!({"a": {"b": "x", "c": 1}, "d": {"e": 2}});
        assert_eq!(take(&mut v, &["a", "b"], string, text), Some("x".into()));
        let read_not_written = take(&mut v, &["a", "c"], count, |_| Some(json!(2)));
        assert_eq!(read_not_written, None);
        assert_eq!(take(&mut v, &["d", "e"], count, number), Some(2));
        assert_eq!(v, json!({"a": {"c": 1}}), "the emptied object went too");
        assert_eq!(take(&mut v, &["a", "z"], count, number), None);
    }

    #[test]
    fn what_the_kind_says_wins_over_what_its_format_kept() {
        let mut v = json!({"a": {"b": 1}, "c": null});
        let native = json!({"a": {"b": 2, "d": 3}, "c": {"e": 4}, "f": 5});
        merge(&mut v, native.as_object());
        assert_eq!(v, json!({"a": {"b": 1, "d": 3}, "c": null, "f": 5}));
    }

    #[test]
    fn base64_is_read_padded_or_not_and_written_padded() {
        assert_eq!(unbase64(&json!("AAE=")).as_deref(), Some(&[0, 1][..]));
        assert_eq!(unbase64(&json!("AAE")).as_deref(), Some(&[0, 1][..]));
        assert_eq!(unbase64(&json!("A?")), None);
        assert_eq!(base64(&[0, 1]), json!("AAE="));
    }
}
