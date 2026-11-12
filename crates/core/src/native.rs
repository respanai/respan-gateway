//! Format-specific data, opaque to every format but its own.

use std::any::TypeId;

use serde_json::{Map, Value};

/// What a value holds that only its own format reads: fields the kind has no
/// place for, or a whole event or part it has no form for, as that format's
/// JSON.
///
/// It is keyed by a type `K` the format keeps private, which no other format
/// can name: another's reads as none, and is replaced when a format writes
/// its own. That is the drop: what has meaning across formats belongs in the
/// kind. One value per slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Native(Option<(TypeId, Value)>);

impl Native {
    /// JSON `value` as the native of the format that `K`, a type private to
    /// it, names: none if it says nothing (null, or an empty object).
    pub fn json<K: 'static>(value: impl Into<Value>) -> Self {
        match value.into() {
            Value::Null => Native::default(),
            Value::Object(fields) if fields.is_empty() => Native::default(),
            value => Native(Some((TypeId::of::<K>(), value))),
        }
    }

    /// Whether it holds nothing, of any format.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// The JSON value of the format `K` names, if it holds one.
    pub fn value<K: 'static>(&self) -> Option<&Value> {
        match &self.0 {
            Some((key, value)) if *key == TypeId::of::<K>() => Some(value),
            _ => None,
        }
    }

    /// The fields of the format `K` names: none if it holds none.
    pub fn fields<K: 'static>(&self) -> &Map<String, Value> {
        static NONE: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        let fields = self.value::<K>().and_then(Value::as_object);
        fields.unwrap_or_else(|| NONE.get_or_init(Map::new))
    }

    /// The fields of the format `K` names, to edit, in place of what it
    /// holds of another.
    pub fn fields_mut<K: 'static>(&mut self) -> &mut Map<String, Value> {
        if self.value::<K>().is_none_or(|v| !v.is_object()) {
            // WARN: another format's native is dropped here, silently.
            self.0 = Some((TypeId::of::<K>(), Value::Object(Map::new())));
        }
        let value = self.0.as_mut().and_then(|(_, v)| v.as_object_mut());
        value.expect("an object was just put in")
    }
}

/// A native is kept by its format alone: it is written as nothing, and read
/// back as none.
#[cfg(feature = "serde")]
impl serde::Serialize for Native {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // WARN: a native is not kept, and is dropped here, silently.
        s.serialize_none()
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Native {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(d)?;
        Ok(Native::default())
    }
}
