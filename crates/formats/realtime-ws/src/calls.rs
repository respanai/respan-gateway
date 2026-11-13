//! Tools' calls, left out of what this crate's formats say only natively.

use respan_gateway_http::native::Json;
use std::collections::HashSet;

use respan_gateway_core::Native;
use serde_json::{Map, Value};

use crate::{gemini, openai};

/// The calls of some tools a session's client is not to see, left out of
/// what a format of this crate says of them only in its own natives
/// (OpenAI's items of a call, its argument deltas, the call in
/// `response.done`), for a plugin that cannot read those, such as the tool
/// loop: any object that names one of the tools, or carries the id of one
/// of their calls.
#[derive(Debug, Default)]
pub struct Calls {
    /// The tools' names.
    pub names: HashSet<String>,
    /// The ids of their calls, and of the items that hold them, so far.
    pub ids: HashSet<String>,
}

impl Calls {
    /// Leaves the calls out of a format's own event: `true` if the event is
    /// of one of them (it or an object it holds is), and is left out whole.
    pub fn event(&mut self, native: &mut Native) -> bool {
        let Some(event) = own(native) else {
            return false;
        };
        if let Value::Object(fields) = event {
            let held = std::iter::once(fields).chain(fields.values().filter_map(Value::as_object));
            let ours: Vec<&Map<String, Value>> = held.filter(|o| self.is_ours(o)).collect();
            if !ours.is_empty() {
                self.ids
                    .extend(ours.into_iter().flat_map(ids_of).collect::<Vec<_>>());
                return true;
            }
        }
        self.fields(native);
        false
    }

    /// Leaves the calls out of an event's own fields.
    pub fn fields(&mut self, native: &mut Native) {
        let Some(mut value) = own(native).cloned() else {
            return;
        };
        if self.prune(&mut value) {
            // Its format's own, written anew without them.
            *native = match native.value::<openai::Key>() {
                Some(_) => Native::json::<openai::Key>(value),
                None => Native::json::<gemini::Key>(value),
            };
        }
    }

    /// Takes the calls out of the arrays in `value`: whether it held any.
    fn prune(&mut self, value: &mut Value) -> bool {
        match value {
            Value::Array(items) => {
                let held = items.len();
                items.retain(|item| match item.as_object() {
                    Some(o) if self.is_ours(o) => {
                        self.ids.extend(ids_of(o));
                        false
                    }
                    _ => true,
                });
                let inner = items
                    .iter_mut()
                    .fold(false, |any, item| self.prune(item) | any);
                inner || items.len() != held
            }
            Value::Object(fields) => fields
                .values_mut()
                .fold(false, |any, v| self.prune(v) | any),
            _ => false,
        }
    }

    /// Whether `object` is one of the calls, or holds one's answer.
    fn is_ours(&self, object: &Map<String, Value>) -> bool {
        let str_of = |key: &str| object.get(key).and_then(Value::as_str);
        str_of("name").is_some_and(|name| self.names.contains(name))
            || IDS
                .iter()
                .any(|key| str_of(key).is_some_and(|id| self.ids.contains(id)))
    }
}

/// The value of a format of this crate's with tools, if `native` is one's.
fn own(native: &Native) -> Option<&Value> {
    (native.value::<openai::Key>()).or_else(|| native.value::<gemini::Key>())
}

/// The keys of the ids a format gives a call, or the item that holds it.
const IDS: [&str; 3] = ["call_id", "id", "item_id"];

/// The ids an object of a call carries.
fn ids_of(object: &Map<String, Value>) -> impl Iterator<Item = String> + '_ {
    IDS.into_iter()
        .filter_map(|key| object.get(key)?.as_str().map(str::to_owned))
}
