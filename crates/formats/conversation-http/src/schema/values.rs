//! The values the conversation formats share: messages and their parts,
//! the fields a value keeps for its own format, tools, usage totals, the
//! faults of what cannot be written, and small JSON helpers.

use crate::fields;
use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Block, Content, Delta, Finish, Input, Message, Output, Role, Tool, ToolChoice,
};
use respan_gateway_conversation::{Effort, MediaSource, Reasoning, Start, Usage};
use respan_gateway_core::{ErrorCode, Fault, NativeFields, NativeValue};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;
use serde_json::{Map, Value};

use super::Schema;
use super::errors::says;

/// A tool call's `arguments`, JSON text, as the object they are, `{}` for
/// none.
pub(crate) fn arguments_object(arguments: &str) -> Result<Value, Fault> {
    if arguments.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(args) if args.is_object() => Ok(args),
        _ => Err(Fault::bad_request(
            "a tool call's arguments are not a JSON object",
        )),
    }
}

/// The media type and source of a URL: the data of a `data:` URL, else the
/// URL, of type `fallback`.
pub(crate) fn source_url(url: &str, fallback: &str) -> Option<(String, MediaSource)> {
    match url.strip_prefix("data:") {
        Some(data) => {
            let (mime, data) = data.split_once(";base64,")?;
            let bytes = STANDARD.decode(data).ok()?;
            Some((mime.into(), MediaSource::Bytes(bytes.into())))
        }
        None => Some((fallback.into(), MediaSource::Url(url.into()))),
    }
}

/// A request body that is not a JSON object.
pub(crate) fn not_an_object() -> Fault {
    Fault::bad_request("request body is not a JSON object")
}

/// What a format does with the fields of an `native` it writes: `own` keys it
/// writes itself, which are left out, and `droppable` keys, a field's own
/// spelling or a hint, which another format drops instead of refusing.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Keep {
    /// Keys the format writes itself.
    pub own: &'static [&'static str],
    /// Keys another format drops.
    pub droppable: &'static [&'static str],
}

impl Keep {
    /// Nothing is the format's own, and a cache marker or a thought
    /// signature is dropped by another format: each only hints, at what to
    /// cache or at the thinking behind a part, to the format that wrote it.
    pub const CACHE_MARKER: Keep = Keep {
        own: &[],
        droppable: &["cache_control", "thoughtSignature"],
    };
}

/// The native key under which a format keeps what of a value no other format
/// needs: the id and status of an item a stateful API stored (a `msg_..` or
/// `fc_..` of Responses), or a default the format left unsaid. Any other
/// format drops it instead of refusing.
pub(crate) const INERT: &str = "inert";

/// The fields of `native` to write in `format`: all but the `own` keys if they
/// are in `format`; if they are another's, none if each is droppable (or
/// [`INERT`]), else an `Err` naming the first that is not, by `what`.
pub(crate) fn kept_fields(
    format: &str,
    native: &Option<NativeFields>,
    what: impl FnOnce(&str) -> String,
    keep: Keep,
) -> Result<Map<String, Value>, Fault> {
    let own = native.format() == Some(format);
    let mut fields = Map::new();
    for (key, value) in native.fields() {
        let droppable = keep.droppable.contains(&key.as_str()) || key == INERT;
        match (own, droppable) {
            (true, _) if keep.own.contains(&key.as_str()) => {}
            (true, _) => _ = fields.insert(key.clone(), value.clone()),
            (false, true) => {}
            (false, false) => return Err(untranslatable(&what(key))),
        }
    }
    Ok(fields)
}

/// Write the fields of `native` into `v` as [`kept_fields`] says.
pub(crate) fn encode_native(
    format: &str,
    native: &Option<NativeFields>,
    v: &mut Map<String, Value>,
    what: impl FnOnce(&str) -> String,
    keep: Keep,
) -> Result<(), Fault> {
    v.extend(kept_fields(format, native, what, keep)?);
    Ok(())
}

/// `fields` as the native of a value decoded by `format`: none if empty.
pub(crate) fn native_fields(format: &str, fields: Map<String, Value>) -> Option<NativeFields> {
    match fields.is_empty() {
        true => None,
        false => Some(NativeFields::new(format, fields)),
    }
}

/// Merge the fields of `native` into `target`, an object, objects among them
/// field by field.
pub(crate) fn merge_fields(target: &mut Value, native: &Map<String, Value>) {
    let Some(target) = target.as_object_mut() else {
        return;
    };
    for (key, value) in native {
        match (target.get_mut(key), value) {
            (Some(had), Value::Object(more)) if had.is_object() => merge_fields(had, more),
            _ => _ = target.insert(key.clone(), value.clone()),
        }
    }
}

/// Merge `fields` into `into`, objects among them field by field.
pub(crate) fn merge_into(into: &mut Map<String, Value>, fields: &Map<String, Value>) {
    let mut merged = Value::Object(std::mem::take(into));
    merge_fields(&mut merged, fields);
    if let Value::Object(merged) = merged {
        *into = merged;
    }
}

/// The fields of the native a response's start carries, if it is of
/// `format`.
pub(crate) fn own_fields(start: Option<&Start>, format: &str) -> Map<String, Value> {
    let start = start.filter(|s| s.native.format() == Some(format));
    start.map(|s| s.native.fields().clone()).unwrap_or_default()
}

/// Which counts a format's usage object has a field for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageFields {
    /// Tokens written to a cache, apart from those read from it.
    pub cache_writes: bool,
    /// Reasoning tokens, a part of the output.
    pub reasoning: bool,
}

/// Whether `usage` can be written in a format with these `fields`: its
/// counts must be parts of each other and add up, and each it has must have
/// a field to go in.
pub(crate) fn check_usage(usage: &Usage, fields: UsageFields) -> Result<(), Fault> {
    let cache = usage
        .cached_input_tokens
        .checked_add(usage.cache_write_input_tokens);
    if cache.is_none_or(|both| both > usage.input_tokens)
        || (!fields.cache_writes && usage.cache_write_input_tokens > 0)
        || (fields.reasoning && usage.reasoning_tokens > usage.output_tokens)
        || usage
            .input_tokens
            .checked_add(usage.output_tokens)
            .is_none()
    {
        return Err(unwritable_usage());
    }
    Ok(())
}

/// A provider's usage count at `v`: `None` if it says none (absent or
/// null); a whole number even when written as a float (`12.0`); else a
/// failure, so a count that cannot be one is never metered as zero.
pub(crate) fn count(v: Option<&Value>) -> Result<Option<u64>, Fault> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let whole = v.as_u64().or_else(|| {
        let f = v.as_f64()?;
        (f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64).then_some(f as u64)
    });
    whole
        .map(Some)
        .ok_or_else(|| Fault::upstream(format!("a usage count is not a whole number: {v}")))
}

/// The fields of the usage object `u` the kind has no unit for: not the
/// `known` counters, and of the `details` objects (by name, with the known
/// counters of each) only what else they hold.
pub(crate) fn usage_extra(
    u: &Value,
    known: &[&str],
    details: &[(&str, &[&str])],
) -> Map<String, Value> {
    let mut native = Map::new();
    for (key, value) in u.as_object().into_iter().flatten() {
        match details.iter().find(|(name, _)| name == key) {
            Some((_, counters)) => {
                let rest = value.as_object().into_iter().flatten();
                let rest: Map<String, Value> = rest
                    .filter(|(k, v)| !counters.contains(&k.as_str()) && !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if !rest.is_empty() {
                    native.insert(key.clone(), rest.into());
                }
            }
            None if known.contains(&key.as_str()) || value.is_null() => {}
            None => _ = native.insert(key.clone(), value.clone()),
        }
    }
    native
}

/// Whether `object` says nothing but in the fields `these`.
pub(crate) fn only(object: &Value, these: &[&str]) -> bool {
    let fields = object.as_object();
    fields.is_some_and(|o| {
        o.iter()
            .all(|(k, v)| these.contains(&k.as_str()) || !says(v))
    })
}

/// The fields of `v` but those named `keys`, null ones too: an unknown
/// field's presence is part of what its format said.
pub(crate) fn remainder(v: &Value, keys: &[&str]) -> Map<String, Value> {
    let fields = v.as_object().into_iter().flatten();
    let rest = fields.filter(|(k, _)| !keys.contains(&k.as_str()));
    rest.map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Add to `base`, an object, the fields of `fields` it lacks: the semantic
/// fields `base` was written with take precedence over native edits.
pub(crate) fn merge_missing(base: &mut Value, fields: Map<String, Value>) {
    if let Some(base) = base.as_object_mut() {
        fields::merge_missing(base, &fields);
    }
}

/// Whether `delta` is of the kind of `block`.
pub(crate) fn fits(delta: &Delta, block: &Block) -> bool {
    matches!(
        (delta, block),
        (Delta::Text(_), Block::Text)
            | (Delta::Arguments(_), Block::ToolCall { .. })
            | (Delta::Reasoning(_), Block::Reasoning)
    )
}

/// Whether a reply shows the client something: not a start, usage, or the
/// opening or end of a text block, which says nothing yet.
pub(crate) fn shows(reply: &Output) -> bool {
    match reply {
        Output::Delta { delta, .. } => !delta.as_str().is_empty(),
        Output::BlockStart { kind, .. } => *kind != Block::Text,
        Output::Finish(_) => true,
        _ => false,
    }
}

/// A message's content in format `C`: a string if it is one text with
/// nothing beside it, and parts otherwise; a part in another format is
/// refused.
pub(crate) fn encode_content<C: Schema>(content: &[Content]) -> Result<Value, Fault> {
    Ok(match content {
        [] => "".into(),
        [Content::Text { text, native, .. }] if kept_text_fields(C::NAME, native)?.is_empty() => {
            text.as_str().into()
        }
        parts => parts
            .iter()
            .map(encode_any_part::<C>)
            .collect::<Result<_, _>>()?,
    })
}

/// Content of format `C` as parts: a string is one text part.
pub(crate) fn decode_content<C: Schema>(v: &Value) -> Vec<Content> {
    match v {
        Value::String(text) => vec![Content::from(text.as_str())],
        Value::Array(parts) => parts.iter().map(decode_any_part::<C>).collect(),
        Value::Null => Vec::new(),
        other => vec![Content::native(C::NAME, other.clone())],
    }
}

/// The fields of a text part's `native` to write in `format`: a cache marker
/// is dropped by another format, any other field refused.
fn kept_text_fields(
    format: &str,
    native: &Option<NativeFields>,
) -> Result<Map<String, Value>, Fault> {
    let what = |key: &str| format!("a text part's '{key}'");
    kept_fields(format, native, what, Keep::CACHE_MARKER)
}

/// A part as format `C` has it: text, a part in that format only, or what
/// the format writes of the kind's ([`Schema::encode_part`]).
fn encode_any_part<C: Schema>(c: &Content) -> Result<Value, Fault> {
    match c {
        Content::Text { text, native, .. } => {
            let mut part = Map::new();
            part.insert("type".into(), "text".into());
            part.insert("text".into(), text.as_str().into());
            part.extend(kept_text_fields(C::NAME, native)?);
            Ok(part.into())
        }
        Content::Native(NativeValue { format, value, .. }) if **format == *C::NAME => {
            Ok(Value::clone(value))
        }
        Content::Native(NativeValue { format, .. }) => {
            Err(untranslatable(&format!("a part in {format}'s shape")))
        }
        other => C::encode_part(other),
    }
}

/// What the format reads of a part as the kind's ([`Schema::decode_part`]); a
/// text part, whose other fields, such as a cache marker, are its `native`;
/// or other content, which only a call in the format it is in can send.
fn decode_any_part<C: Schema>(p: &Value) -> Content {
    if let Some(part) = C::decode_part(p) {
        return part;
    }
    match (p["type"].as_str(), p["text"].as_str()) {
        (Some("text"), Some(text)) => {
            let fields = p.as_object().into_iter().flatten();
            let fields = fields.filter(|(k, v)| *k != "type" && *k != "text" && says(v));
            let fields = fields.map(|(k, v)| (k.clone(), v.clone())).collect();
            let mut part = Content::from(text);
            if let Content::Text { native, .. } = &mut part {
                *native = native_fields(C::NAME, fields);
            }
            part
        }
        _ => Content::native(C::NAME, p.clone()),
    }
}

/// A message of format `C` as a conversation keeps it: its other fields in
/// its `native`.
pub(crate) fn decode_message<C: Schema>(m: &Value, roles: &Names<Role>) -> Message {
    let name = m["role"].as_str().unwrap_or_default();
    let role = roles
        .value(name)
        .unwrap_or_else(|| Role::native(C::NAME, name));
    let fields = m.as_object().into_iter().flatten();
    let fields = fields.filter(|(k, v)| *k != "role" && *k != "content" && says(v));
    let mut message = Message::new(role, "");
    message.content = decode_content::<C>(&m["content"]);
    message.native = native_fields(
        C::NAME,
        fields.map(|(k, v)| (k.clone(), v.clone())).collect(),
    );
    message
}

/// Leave a message no `native` if it says nothing the kind has no form for.
pub(crate) fn tidy(message: &mut Message) {
    if message.native.is_empty() {
        message.native = None;
    }
}

/// A message of format `C` written anew: role, content and its other fields.
pub(crate) fn encode_message<C: Schema>(m: &Message, roles: &Names<Role>) -> Result<Value, Fault> {
    let mut v = Map::new();
    v.insert("role".into(), role_of(C::NAME, m, roles)?);
    v.insert("content".into(), encode_content::<C>(&m.content)?);
    let fields = |_: &str| "messages with fields the kind has no place for".to_owned();
    encode_native(C::NAME, &m.native, &mut v, fields, Keep::default())?;
    Ok(v.into())
}

/// The role to write: another format's roles are refused.
fn role_of(format: &str, m: &Message, roles: &Names<Role>) -> Result<Value, Fault> {
    let role = match &m.role {
        Role::Native {
            format: f, name, ..
        } if **f == *format => Some(name.as_str()),
        Role::Native { .. } => None,
        role => roles.name(role),
    };
    let role = role.map(Value::from);
    role.ok_or_else(|| untranslatable("a role this format has no place for"))
}

/// Set `chat`'s tools, tool choice and whether calls may be parallel, each
/// that a format's request says in a form the kind has, taking its field,
/// named in `keys` (none for a format that says it inside another), out of
/// `chat.native`.
pub(crate) fn decode_tools(
    chat: &mut Input,
    tools: Option<Vec<Tool>>,
    choice: Option<Option<ToolChoice>>,
    parallel: Option<Option<bool>>,
    keys: [&str; 3],
) {
    if let Some(tools) = tools {
        chat.tools = tools;
        if let Some(native) = &mut chat.native {
            native.fields.shift_remove(keys[0]);
        }
    }
    if let Some(choice) = choice {
        chat.options.tool_choice = choice;
        if let Some(native) = &mut chat.native {
            native.fields.shift_remove(keys[1]);
        }
    }
    if let Some(parallel) = parallel {
        chat.options.parallel_tool_calls = parallel;
        if !keys[2].is_empty()
            && let Some(native) = &mut chat.native
        {
            native.fields.shift_remove(keys[2]);
        }
    }
}

/// The tools of `v`, if each is one the kind can say, by `tool`.
pub(crate) fn each_tool(v: &Value, tool: fn(&Value) -> Option<Tool>) -> Option<Vec<Tool>> {
    match v {
        Value::Null => Some(Vec::new()),
        Value::Array(tools) => tools.iter().map(tool).collect(),
        _ => None,
    }
}

/// How a refusal names a tool's field `key`.
pub(crate) fn tool_field(key: &str) -> String {
    format!("a tool's '{key}'")
}

/// Turns a provider's running usage totals into increments.
#[derive(Debug, Default)]
pub(crate) struct Totals(Usage);

impl Totals {
    /// What `total` adds to the totals seen so far; usage only grows.
    pub fn increment(&mut self, total: &Usage) -> Usage {
        let step = total.saturating_sub(&self.0);
        self.0 += &step;
        step
    }
}

/// Stop sequences, or `None` for none.
pub(crate) fn stop_json(stop: &[String]) -> Option<Value> {
    (!stop.is_empty()).then(|| stop.into())
}

/// A format's names, under its own name, for a conversation type's values;
/// the first of several is written.
#[derive(Debug)]
pub(crate) struct Names<T: 'static>(pub &'static [(&'static str, T)], pub &'static str);

impl<T: PartialEq + Clone> Names<T> {
    /// The value `name` means, if it is one of these.
    pub fn value(&self, name: &str) -> Option<T> {
        self.0
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
    }

    /// The name written for `value`, if this format has one.
    pub fn name(&self, value: &T) -> Option<&'static str> {
        self.0.iter().find(|(_, v)| v == value).map(|(n, _)| *n)
    }
}

impl Names<Finish> {
    /// A provider's reason, as a [`Finish`].
    pub fn finish(&self, reason: &str) -> Finish {
        self.value(reason)
            .unwrap_or_else(|| Finish::native(self.1, reason))
    }

    /// The reason to write: a native one in its own format, else its
    /// fallback; one without is refused, not invented as a normal completion.
    pub fn reason<'a>(&self, finish: &'a Finish) -> Result<&'a str, Fault> {
        match finish {
            Finish::Native { format, reason, .. } if **format == *self.1 => Ok(reason),
            Finish::Native {
                fallback: Some(fallback),
                ..
            } => self.reason(fallback),
            Finish::Native { format, reason, .. } => Err(unwritable(Some(format!(
                "{format}'s finish reason `{reason}`"
            )))),
            _ => self.name(finish).ok_or_else(unwritable_content),
        }
    }
}

/// An edit of usage this format cannot represent.
pub(crate) fn unwritable_usage() -> Fault {
    Fault::config("a plugin's change cannot be made: this format cannot represent the usage")
}

/// A request carrying `what` cannot be sent in another format.
pub(crate) fn untranslatable(what: &str) -> Fault {
    Fault::bad_request(format!("{what} cannot be translated to another format yet"))
}

/// A 502 for content the client's format cannot write anew, such as a tool call.
pub(crate) fn unwritable_content() -> Fault {
    unwritable(None)
}

fn unwritable(what: Option<String>) -> Fault {
    let message = "the response holds content that cannot yet be written anew in this format";
    let message = match what {
        Some(what) => format!("{message}: {what}"),
        None => message.to_owned(),
    };
    Fault::new(message)
        .with(ErrorCode("untranslatable".into()))
        .with(Status(StatusCode::BAD_GATEWAY))
}

/// A stream that ended before its end event, unless it is `done`.
pub(crate) fn ended(done: bool) -> Result<(), Fault> {
    match done {
        true => Ok(()),
        false => Err(Fault::truncated()),
    }
}

/// The fields of `v` not in `known` that say something other than their
/// `inert` value.
pub(crate) fn unsupported_fields(
    v: &Value,
    known: &[&str],
    inert: &[(&str, f64)],
) -> Map<String, Value> {
    let number = |v: &Value| v.as_f64().or(v.as_bool().map(f64::from));
    let fields = v.as_object().into_iter().flatten();
    fields
        .filter(|(key, value)| {
            let harmless = !says(value)
                || inert
                    .iter()
                    .any(|(k, n)| k == key && number(value) == Some(*n));
            !known.contains(&key.as_str()) && !harmless
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Set `key` to `value`, or remove it for `None`.
pub(crate) fn set(v: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => v.insert(key.into(), value),
        None => v.shift_remove(key),
    };
}

/// Conveniences for optional format-tagged fields.
pub(crate) trait NativeFieldsExt {
    /// The fields, none if there are none.
    fn fields(&self) -> &Map<String, Value>;
    /// The fields to edit, made of `format` if there are none.
    fn fields_mut(&mut self, format: &str) -> &mut Map<String, Value>;
    /// The format the fields are of.
    fn format(&self) -> Option<&str>;
    /// Whether there are no fields.
    fn is_empty(&self) -> bool;
}

impl NativeFieldsExt for Option<NativeFields> {
    fn fields(&self) -> &Map<String, Value> {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        self.as_ref()
            .map(|n| &n.fields)
            .unwrap_or_else(|| EMPTY.get_or_init(Map::new))
    }
    fn fields_mut(&mut self, format: &str) -> &mut Map<String, Value> {
        &mut self
            .get_or_insert_with(|| NativeFields::new(format, Map::new()))
            .fields
    }
    fn format(&self) -> Option<&str> {
        self.as_ref().map(|n| n.format.as_ref())
    }
    fn is_empty(&self) -> bool {
        self.as_ref().is_none_or(NativeFields::is_empty)
    }
}

/// An effort as vendors name it: `none` (thinking off) to `high`.
pub(crate) fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Off => "none",
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        _ => "high",
    }
}

/// The effort a vendor's `name` says, in either case; `None` for any other
/// name, which then stays native to its format.
pub(crate) fn parse_effort(name: &str) -> Option<Effort> {
    Some(match name.to_ascii_lowercase().as_str() {
        "none" => Effort::Off,
        "minimal" => Effort::Minimal,
        "low" => Effort::Low,
        "medium" => Effort::Medium,
        "high" => Effort::High,
        _ => return None,
    })
}

/// The reasoning controls of `input`, refused if they name an effort and a
/// budget at once, as no vendor takes both.
pub(crate) fn reasoning_of(input: &Input) -> Result<Option<&Reasoning>, Fault> {
    match &input.options.reasoning {
        Some(r) if r.effort.is_some() && r.budget_tokens.is_some() => Err(Fault::bad_request(
            "reasoning with both an effort and a token budget cannot be written",
        )),
        r => Ok(r.as_ref()),
    }
}

/// The value of the request's native field `key` if `parse` reads all of it,
/// which then leaves the native.
pub(crate) fn take_native<T>(
    input: &mut Input,
    key: &str,
    parse: impl FnOnce(&Value) -> Option<T>,
) -> Option<T> {
    let native = input.native.as_mut()?;
    let parsed = parse(native.fields.get(key)?)?;
    native.fields.shift_remove(key);
    if native.is_empty() {
        input.native = None;
    }
    Some(parsed)
}

/// The request's own native field `key`, if it is of `format`.
pub(crate) fn own_native(input: &Input, format: &str, key: &str) -> Option<Value> {
    (input.native.format() == Some(format))
        .then(|| input.native.fields().get(key).cloned())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_totals_become_increments() {
        let mut totals = Totals::default();
        let total = |n| Usage::new(0, n);
        assert_eq!(totals.increment(&total(1)).output_tokens, 1);
        assert_eq!(totals.increment(&total(3)).output_tokens, 2);
        assert!(totals.increment(&total(3)).is_empty());
        assert!(totals.increment(&total(2)).is_empty(), "never negative");
    }
}
