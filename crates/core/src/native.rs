//! Format-specific data, opaque to every format but its own.

use std::borrow::Cow;

use serde_json::{Map, Value};

/// A type a format keeps private, which keys its [`Native`]: code that
/// cannot name it cannot read what the format keeps.
pub trait Key: 'static {
    /// The format's name, stable across builds: a native is kept under it,
    /// and read back only under it.
    const NAME: &'static str;
}

/// No format's: another format's, as a test has one.
impl Key for () {
    const NAME: &'static str = "";
}

/// What a value holds that only its own format reads: fields the kind has no
/// place for, or a whole event or part it has no form for, as that format's
/// JSON.
///
/// It is keyed by a type `K` the format keeps private ([`Key`]), which no
/// other format can name: another's reads as none, and is replaced when a
/// format writes its own. That is the drop: what has meaning across formats
/// belongs in the kind. One value per slot. The key is named, not proven: a
/// guard against reading another's by accident.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Native(Option<(Cow<'static, str>, Value)>);

impl Native {
    /// JSON `value` as the native of the format `K` keys: none if it says
    /// nothing (null, or an empty object).
    pub fn json<K: Key>(value: impl Into<Value>) -> Self {
        match value.into() {
            Value::Null => Native::default(),
            Value::Object(fields) if fields.is_empty() => Native::default(),
            value => Native(Some((Cow::Borrowed(K::NAME), value))),
        }
    }

    /// Whether it holds nothing, of any format.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// The JSON value of the format `K` keys, if it holds one.
    pub fn value<K: Key>(&self) -> Option<&Value> {
        match &self.0 {
            Some((name, value)) if name == K::NAME => Some(value),
            _ => None,
        }
    }

    /// The JSON value of the format `K` keys, to edit, if it holds one.
    pub fn value_mut<K: Key>(&mut self) -> Option<&mut Value> {
        match &mut self.0 {
            Some((name, value)) if name == K::NAME => Some(value),
            _ => None,
        }
    }

    /// The fields of the format `K` keys: none if it holds none.
    pub fn fields<K: Key>(&self) -> &Map<String, Value> {
        static NONE: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        let fields = self.value::<K>().and_then(Value::as_object);
        fields.unwrap_or_else(|| NONE.get_or_init(Map::new))
    }

    /// The fields of the format `K` keys, to edit, in place of what it holds
    /// of another.
    pub fn fields_mut<K: Key>(&mut self) -> &mut Map<String, Value> {
        if self.value::<K>().is_none_or(|v| !v.is_object()) {
            // WARN: another format's native is dropped here, silently.
            self.0 = Some((Cow::Borrowed(K::NAME), Value::Object(Map::new())));
        }
        let value = self.0.as_mut().and_then(|(_, v)| v.as_object_mut());
        value.expect("an object was just put in")
    }

    /// The length of its JSON, as its format writes it, written to nothing:
    /// a bound for what keeps or meters a native it cannot read.
    pub fn len(&self) -> usize {
        struct Count(usize);
        impl std::fmt::Write for Count {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                self.0 += s.len();
                Ok(())
            }
        }
        let mut count = Count(0);
        if let Some((_, value)) = &self.0 {
            let _ = std::fmt::Write::write_fmt(&mut count, format_args!("{value}"));
        }
        count.0
    }
}

/// How a native is kept: its format's name and its value, as
/// `{"format", "value"}`; one kept as `{"format", "fields"}`, or a role as
/// `{"format", "name"}`, reads too.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept<'a> {
    format: Cow<'a, str>,
    #[serde(alias = "fields", alias = "name")]
    value: Cow<'a, Value>,
}

#[cfg(feature = "serde")]
impl serde::Serialize for Native {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let kept = self.0.as_ref().map(|(format, value)| Kept {
            format: Cow::Borrowed(format),
            value: Cow::Borrowed(value),
        });
        kept.serialize(s)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Native {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let kept = Option::<Kept>::deserialize(d)?;
        Ok(Native(kept.map(|k| {
            (Cow::Owned(k.format.into_owned()), k.value.into_owned())
        })))
    }
}

/// A format's [`Key`], `$key`, under its name `$name`: a type to keep
/// private to it.
#[macro_export]
macro_rules! native_key {
    ($vis:vis $key:ident = $name:expr) => {
        /// What only this format reads of a value ([`Native`]($crate::Native)).
        $vis enum $key {}
        impl $crate::Key for $key {
            const NAME: &'static str = $name;
        }
    };
}

/// A format's doors to its own [`Native`], keyed by `$key`, a type private
/// to it: associated functions in an `impl` of the format's type, through
/// which only code that names the format, such as a vendor's dialect of it
/// or a host, reaches what it keeps.
#[macro_export]
macro_rules! native_doors {
    ($key:ty) => {
        /// The fields of this format's own in `native`: none if it holds
        /// another's.
        pub fn fields(native: &$crate::Native) -> &::serde_json::Map<String, ::serde_json::Value> {
            native.fields::<$key>()
        }

        /// The fields of this format's own in `native`, to edit, in place of
        /// another's.
        pub fn fields_mut(
            native: &mut $crate::Native,
        ) -> &mut ::serde_json::Map<String, ::serde_json::Value> {
            native.fields_mut::<$key>()
        }

        /// The value of this format's own in `native`, if it holds one.
        pub fn value(native: &$crate::Native) -> Option<&::serde_json::Value> {
            native.value::<$key>()
        }

        /// The value of this format's own in `native`, to edit, if it holds
        /// one.
        pub fn value_mut(native: &mut $crate::Native) -> Option<&mut ::serde_json::Value> {
            native.value_mut::<$key>()
        }

        /// `value` as this format's own: none if it says nothing.
        pub fn native(value: impl Into<::serde_json::Value>) -> $crate::Native {
            $crate::Native::json::<$key>(value)
        }
    };
}
