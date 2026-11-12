//! A format's natives as its JSON, under a type the format keeps private:
//! code that cannot name the type cannot read them.

use std::marker::PhantomData;

use respan_gateway_core::Native;
use serde_json::{Map, Value};

/// The JSON of the format the private type `K` names.
struct Own<K>(Value, PhantomData<fn() -> K>);

/// A native as the JSON of the format a private type `K` names.
pub trait Json {
    /// `value` as `K`'s: none if it says nothing (null, or an empty object).
    fn json<K: 'static>(value: impl Into<Value>) -> Native;
    /// `K`'s, saying nothing: whose a value is, as its fields are none.
    fn own<K: 'static>() -> Native;
    /// `K`'s value, if it holds one.
    fn value<K: 'static>(&self) -> Option<&Value>;
    /// `K`'s fields: none if it holds none.
    fn fields<K: 'static>(&self) -> &Map<String, Value>;
    /// `K`'s fields edited by `edit`, as a new native: another's dropped,
    /// none if no field is left.
    fn with_fields<K: 'static>(&self, edit: impl FnOnce(&mut Map<String, Value>)) -> Native;
}

impl Json for Native {
    fn json<K: 'static>(value: impl Into<Value>) -> Native {
        match value.into() {
            Value::Null => Native::default(),
            Value::Object(fields) if fields.is_empty() => Native::default(),
            value => Native::new(Own::<K>(value, PhantomData)),
        }
    }

    fn own<K: 'static>() -> Native {
        Native::new(Own::<K>(Value::Object(Map::new()), PhantomData))
    }

    fn value<K: 'static>(&self) -> Option<&Value> {
        self.get::<Own<K>>().map(|own| &own.0)
    }

    fn fields<K: 'static>(&self) -> &Map<String, Value> {
        static NONE: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        let fields = self.value::<K>().and_then(Value::as_object);
        fields.unwrap_or_else(|| NONE.get_or_init(Map::new))
    }

    fn with_fields<K: 'static>(&self, edit: impl FnOnce(&mut Map<String, Value>)) -> Native {
        let mut fields = self.fields::<K>().clone();
        edit(&mut fields);
        Native::json::<K>(fields)
    }
}
