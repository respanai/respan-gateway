//! What this crate's formats say of tools' calls only in their own natives,
//! left out for a plugin that runs those tools itself.

use std::collections::HashSet;

use respan_gateway_core::Native;
use serde_json::{Map, Value};

use crate::{gemini, openai};

/// The calls of some tools, which a session's client is not to see: what
/// a format of this crate says of them only in its own natives (OpenAI's
/// items of a call, its argument deltas, the call in `response.done`) is
/// left out of those, for a plugin that cannot read them, such as the tool
/// loop. Any object that names one of the tools, or carries the id of one
/// of their calls, is of one. Another crate's native is left as it is.
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
        if let Value::Object(fields) = &*event {
            let held = std::iter::once(fields).chain(fields.values().filter_map(Value::as_object));
            let ours: Vec<&Map<String, Value>> = held.filter(|o| self.is_ours(o)).collect();
            if !ours.is_empty() {
                let ids: Vec<String> = ours.into_iter().flat_map(ids_of).collect();
                self.ids.extend(ids);
                return true;
            }
        }
        self.prune(event);
        false
    }

    /// Leaves the calls out of an event's own fields.
    pub fn fields(&mut self, native: &mut Native) {
        if let Some(fields) = own(native) {
            self.prune(fields);
        }
    }

    /// Takes the calls out of the arrays in `value`.
    fn prune(&mut self, value: &mut Value) {
        match value {
            Value::Array(items) => {
                items.retain(|item| match item.as_object() {
                    Some(o) if self.is_ours(o) => {
                        self.ids.extend(ids_of(o));
                        false
                    }
                    _ => true,
                });
                items.iter_mut().for_each(|item| self.prune(item));
            }
            Value::Object(fields) => fields.values_mut().for_each(|v| self.prune(v)),
            _ => {}
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
fn own(native: &mut Native) -> Option<&mut Value> {
    if native.value::<openai::Key>().is_some() {
        return native.value_mut::<openai::Key>();
    }
    native.value_mut::<gemini::Key>()
}

/// The keys of the ids a format gives a call, or the item that holds it.
const IDS: [&str; 3] = ["call_id", "id", "item_id"];

/// The ids an object of a call carries.
fn ids_of(object: &Map<String, Value>) -> impl Iterator<Item = String> + '_ {
    IDS.into_iter()
        .filter_map(|key| object.get(key)?.as_str().map(str::to_owned))
}
