//! The values the conversation formats share: messages and their parts,
//! the fields a value keeps for its own format, tools, usage totals, the
//! faults of what cannot be written, and small JSON helpers.

use crate::fields::{self, base64_bytes, data_url};
use respan_gateway_conversation::{
    Block, Content, Delta, Finish, Input, Message, Output, ProviderTool, Role, Tool, ToolChoice,
};
use respan_gateway_conversation::{Effort, GenerationOptions, MediaSource, Start, Usage};
use respan_gateway_core::{ErrorCode, Fault, Native};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::marker::PhantomData;

use super::Own;
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

/// The refusal of a request whose `field`, as its format names it, holds
/// media no provider would read, as `what` says.
pub(crate) fn unreadable_media(field: &str, what: &str) -> Fault {
    Fault::bad_request(format!("{field} {what}"))
}

/// The string in `value`, the client's `field`; one that is missing is
/// refused.
pub(crate) fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, Fault> {
    value
        .as_str()
        .ok_or_else(|| unreadable_media(field, "is required"))
}

/// The part of a media `holder` that names no source of its own: one that
/// names a file by `file_id` is a form the kind cannot carry, `None`; one
/// that names nothing is refused, as `wanted` says.
pub(crate) fn unless_file_id(holder: &Value, wanted: &str) -> Result<Option<Content>, Fault> {
    match holder["file_id"].is_null() {
        true => Err(unreadable_media(wanted, "is required")),
        false => Ok(None),
    }
}

/// The bytes of the base64 in `value`, the client's `field`; a field that is
/// missing, or whose base64 does not decode, is refused.
pub(crate) fn base64_field(value: &Value, field: &str) -> Result<Vec<u8>, Fault> {
    base64_bytes(required_str(value, field)?)
        .ok_or_else(|| unreadable_media(field, "is not valid base64"))
}

/// The media type and bytes of `url`, the client's `field`, if it is a
/// `data:` URL ([`data_url`]); `None` for any other URL. One with no data,
/// or whose base64 does not decode, is refused.
pub(crate) fn data_media(url: &str, field: &str) -> Result<Option<(String, Vec<u8>)>, Fault> {
    data_url(url).map_err(|e| unreadable_media(field, e.reason()))
}

/// The media type and source of `url`, the client's `field`: the bytes of a
/// `data:` URL ([`data_media`]), else the URL, of type `fallback`.
pub(crate) fn source_url(
    url: &str,
    fallback: &str,
    field: &str,
) -> Result<(String, MediaSource), Fault> {
    Ok(match data_media(url, field)? {
        Some((mime, bytes)) => (mime, MediaSource::Bytes(bytes.into())),
        None => (fallback.into(), MediaSource::Url(url.into())),
    })
}

/// A request body that is not a JSON object.
pub(crate) fn not_an_object() -> Fault {
    Fault::bad_request("request body is not a JSON object")
}

/// Whether `C` writes `part`: another format's shape it cannot.
fn own_part<C: Schema>(part: &mut Content) -> bool {
    if let Content::ToolResult { content, .. } = part {
        content.retain_mut(own_part::<C>);
    }
    match part {
        // WARN: a part in another format's shape is dropped here, silently.
        Content::Native(n) => C::value(n).is_some(),
        _ => true,
    }
}

/// Whether a part, or one in a tool result, is in another format's shape.
fn holds_foreign<C: Schema>(part: &Content) -> bool {
    match part {
        Content::Native(n) => C::value(n).is_none(),
        Content::ToolResult { content, .. } => content.iter().any(holds_foreign::<C>),
        _ => false,
    }
}

/// `chat` without the parts in another format's shape and the messages in
/// another format's role, each dropped.
pub(crate) fn own_shapes<C: Schema>(chat: &Input) -> Cow<'_, Input> {
    let foreign_role = |m: &Message| matches!(&m.role, Role::Native(n) if C::value(n).is_none());
    let holds = |m: &Message| foreign_role(m) || m.content.iter().any(holds_foreign::<C>);
    if !chat.messages.iter().any(holds) {
        return Cow::Borrowed(chat);
    }
    let mut chat = chat.clone();
    chat.messages.retain_mut(|m| {
        // WARN: a message in another format's role is dropped here, silently.
        if foreign_role(m) {
            return false;
        }
        let said = !m.content.is_empty();
        m.content.retain_mut(own_part::<C>);
        !said || !m.content.is_empty()
    });
    Cow::Owned(chat)
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

/// The fields of the native a response's start carries, if they are `C`'s.
pub(crate) fn own_fields<C: Schema>(start: Option<&Start>) -> Map<String, Value> {
    start
        .map(|s| C::fields(&s.native).clone())
        .unwrap_or_default()
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

/// A provider's counts with each part within its whole: some report more
/// reasoning than output, and an answer already given is not failed for it.
pub(crate) fn parts_within_totals(mut usage: Usage) -> Usage {
    usage.reasoning_tokens = usage.reasoning_tokens.min(usage.output_tokens);
    usage.cached_input_tokens = usage.cached_input_tokens.min(usage.input_tokens);
    let uncached = usage.input_tokens - usage.cached_input_tokens;
    usage.cache_write_input_tokens = usage.cache_write_input_tokens.min(uncached);
    usage
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

/// Each part of `parts` with the signature right after it, which seals it.
pub(crate) fn sealed(parts: &[Content]) -> impl Iterator<Item = (&Content, Option<&str>)> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let part = parts.get(at)?;
        at += 1;
        let seal = match parts.get(at) {
            Some(Content::Signature(s)) => {
                at += 1;
                Some(s.as_str())
            }
            _ => None,
        };
        Some((part, seal))
    })
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
/// nothing beside it, and parts otherwise.
pub(crate) fn encode_content<C: Schema>(content: &[Content]) -> Result<Value, Fault> {
    Ok(match content {
        [] => "".into(),
        [Content::Text { text, native, .. }] if C::fields(native).is_empty() => {
            text.as_str().into()
        }
        parts => parts
            .iter()
            .map(encode_any_part::<C>)
            .filter(|p| !matches!(p, Ok(Value::Null)))
            .collect::<Result<_, _>>()?,
    })
}

/// Content of format `C` as parts: a string is one text part.
pub(crate) fn decode_content<C: Schema>(v: &Value) -> Result<Vec<Content>, Fault> {
    Ok(match v {
        Value::String(text) => vec![Content::from(text.as_str())],
        Value::Array(parts) => parts
            .iter()
            .map(decode_any_part::<C>)
            .collect::<Result<_, _>>()?,
        Value::Null => Vec::new(),
        other => vec![Content::Native(C::native(other.clone()))],
    })
}

/// A part as format `C` has it: text, a part in that format only, or what
/// the format writes of the kind's ([`Schema::encode_part`]).
fn encode_any_part<C: Schema>(c: &Content) -> Result<Value, Fault> {
    match c {
        Content::Text { text, native, .. } => {
            let mut part = Map::new();
            part.insert("type".into(), "text".into());
            part.insert("text".into(), text.as_str().into());
            part.extend(C::fields(native).clone());
            Ok(part.into())
        }
        Content::Native(n) => Ok(C::value(n).cloned().unwrap_or_default()),
        other => C::encode_part(other),
    }
}

/// What the format reads of a part as the kind's ([`Schema::decode_part`]); a
/// text part, whose other fields, such as a cache marker, are its `native`;
/// or other content, which only a call in the format it is in can send.
fn decode_any_part<C: Schema>(p: &Value) -> Result<Content, Fault> {
    if let Some(part) = C::decode_part(p)? {
        return Ok(part);
    }
    Ok(match (p["type"].as_str(), p["text"].as_str()) {
        (Some("text"), Some(text)) => {
            let fields = p.as_object().into_iter().flatten();
            let fields = fields.filter(|(k, v)| *k != "type" && *k != "text" && says(v));
            let fields: Map<_, _> = fields.map(|(k, v)| (k.clone(), v.clone())).collect();
            let mut part = Content::from(text);
            if let Content::Text { native, .. } = &mut part {
                *native = C::native(fields);
            }
            part
        }
        _ => Content::Native(C::native(p.clone())),
    })
}

/// A message of format `C` as a conversation keeps it: its other fields in
/// its `native`.
pub(crate) fn decode_message<C: Schema>(m: &Value, roles: &Names<Role>) -> Result<Message, Fault> {
    let name = m["role"].as_str().unwrap_or_default();
    let role = roles
        .value(name)
        .unwrap_or_else(|| Role::Native(C::native(name)));
    let fields = m.as_object().into_iter().flatten();
    let fields = fields.filter(|(k, v)| *k != "role" && *k != "content" && says(v));
    let mut message = Message::new(role, "");
    message.content = decode_content::<C>(&m["content"])?;
    let fields: Map<_, _> = fields.map(|(k, v)| (k.clone(), v.clone())).collect();
    message.native = C::native(fields);
    Ok(message)
}

/// Leave a message no `native` if it says nothing the kind has no form for.
pub(crate) fn tidy<C: Schema>(message: &mut Message) {
    if C::fields(&message.native).is_empty() {
        message.native = Native::default();
    }
}

/// A message of format `C` written anew: role, content and its other fields.
pub(crate) fn encode_message<C: Schema>(m: &Message, roles: &Names<Role>) -> Result<Value, Fault> {
    let mut v = Map::new();
    v.insert("role".into(), role_of::<C>(m, roles));
    v.insert("content".into(), encode_content::<C>(&m.content)?);
    v.extend(C::fields(&m.native).clone());
    Ok(v.into())
}

/// The role to write: a user's for one this format lacks.
fn role_of<C: Schema>(m: &Message, roles: &Names<Role>) -> Value {
    let role = match &m.role {
        Role::Native(n) => C::value(n).and_then(Value::as_str),
        role => roles.name(role),
    };
    // WARN: a role this format lacks, such as a system message after the
    // conversation starts, is written as a user's.
    role.unwrap_or("user").into()
}

/// The tools of a request: the kind's, the provider's, and whether that is
/// all of them.
type ReadTools = (Vec<Tool>, Vec<ProviderTool>, bool);

/// Set `chat`'s tools, tool choice and whether calls may be parallel, each
/// that a format's request says in a form the kind has, taking its field,
/// named in `keys` (none for a format that says it inside another), out of
/// `chat.native`.
pub(crate) fn decode_tools<C: Schema>(
    chat: &mut Input,
    tools: Option<ReadTools>,
    choice: Option<Option<ToolChoice>>,
    parallel: Option<Option<bool>>,
    keys: [&str; 3],
) {
    if let Some((tools, provided, all)) = tools {
        chat.tools = tools;
        chat.provider_tools = provided;
        // Else its format writes the list whole, another what the kind can say.
        if all {
            take_native::<C, _>(&mut chat.native, keys[0], |_| Some(()));
        }
    }
    if let Some(choice) = choice {
        chat.options.tool_choice = choice;
        take_native::<C, _>(&mut chat.native, keys[1], |_| Some(()));
    }
    if let Some(parallel) = parallel {
        chat.options.parallel_tool_calls = parallel;
        if !keys[2].is_empty() {
            take_native::<C, _>(&mut chat.native, keys[2], |_| Some(()));
        }
    }
}

/// The tools of `v` the kind can say, each by `tool` or else by `provided`,
/// and whether that is all of them.
pub(crate) fn each_tool(
    v: &Value,
    tool: fn(&Value) -> Option<Tool>,
    provided: fn(&Value) -> Option<ProviderTool>,
) -> Option<ReadTools> {
    let mut out = (Vec::new(), Vec::new(), true);
    for t in v.as_array().map_or(&[][..], Vec::as_slice) {
        match (tool(t), provided(t)) {
            (Some(t), _) => out.0.push(t),
            (None, Some(p)) => out.1.push(p),
            (None, None) => out.2 = false,
        }
    }
    (v.is_null() || v.is_array()).then_some(out)
}

/// A web search of `C`, `fields` its native.
pub(crate) fn read_search<C: Schema>(fields: Map<String, Value>) -> ProviderTool {
    ProviderTool::WebSearch {
        native: C::native(fields),
    }
}

/// The fields of a provider tool to write in `C`: a web search's own,
/// its hints among them.
pub(crate) fn write_search<C: Schema>(tool: &ProviderTool) -> Result<Map<String, Value>, Fault> {
    let ProviderTool::WebSearch { native } = tool else {
        return Err(untranslatable(
            "a provider tool this format has no form for",
        ));
    };
    Ok(C::fields(native).clone())
}

/// `key` of `input`'s `native`, out of it if it is a string; one of another
/// type stays there.
pub(crate) fn take_text<C: Schema>(input: &mut Input, key: &str) -> Option<String> {
    take_native::<C, _>(&mut input.native, key, |v| v.as_str().map(str::to_owned))
}

/// Moves `tool`'s boolean `strict`, which OpenAI's formats say the same way,
/// out of its `native`; one that is no boolean stays there.
pub(crate) fn take_strict<C: Schema>(tool: &mut Tool) {
    if let Some(strict) = take_native::<C, _>(&mut tool.native, "strict", Value::as_bool) {
        tool.strict = Some(strict);
    }
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
pub(crate) struct Names<T: 'static, C = ()>(pub &'static [(&'static str, T)], pub PhantomData<C>);

impl<T: PartialEq + Clone, C> Names<T, C> {
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

impl<C: Schema> Names<Finish, C> {
    /// A provider's reason, as a [`Finish`].
    pub fn finish(&self, reason: &str) -> Finish {
        self.value(reason)
            .unwrap_or_else(|| Finish::native(C::native(reason), None))
    }

    /// The reason to write: a native one in its own format, else its
    /// fallback, else a stop.
    pub fn reason<'a>(&self, finish: &'a Finish) -> Result<&'a str, Fault> {
        match finish {
            Finish::Native {
                native, fallback, ..
            } => match (C::value(native), fallback) {
                (Some(reason), _) => Ok(reason.as_str().unwrap_or_default()),
                (None, Some(fallback)) => self.reason(fallback),
                // WARN: a native reason without a fallback, such as an
                // OpenAI-compatible vendor's own, is written as a stop.
                (None, None) => self.reason(&Finish::Stop),
            },
            _ => self.name(finish).ok_or_else(unwritable_content),
        }
    }
}

/// An edit of usage this format cannot represent.
pub(crate) fn unwritable_usage() -> Fault {
    Fault::config("a plugin's change cannot be made: this format cannot represent the usage")
}

/// The text a failed tool result leads with in a format that cannot flag it,
/// as Anthropic's `is_error` and Gemini's `error` do.
pub(crate) const FAILED_RESULT_PREFIX: &str = "Error: ";

/// A request carrying `what` cannot be sent in another format.
pub(crate) fn untranslatable(what: &str) -> Fault {
    Fault::bad_request(format!("{what} cannot be translated to another format yet"))
}

/// A 502 for content the client's format cannot write anew, such as a tool call.
pub(crate) fn unwritable_content() -> Fault {
    Fault::new("the response holds content that cannot yet be written anew in this format")
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

/// An effort as vendors name it: `none` (thinking off) to `xhigh`, refused
/// for one this format has no name for.
pub(crate) fn effort_name(effort: Effort) -> Result<&'static str, Fault> {
    Ok(match effort {
        Effort::Off => "none",
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        _ => return Err(untranslatable("a reasoning effort with no name")),
    })
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
        "xhigh" => Effort::XHigh,
        _ => return None,
    })
}

/// The thinking budget an effort buys a Claude that thinks to a budget, and
/// each the nearest of the other for a format that takes only one.
pub(crate) const EFFORT_BUDGETS: &[(&str, u64)] = &[
    ("low", 1024),
    ("medium", 2048),
    ("high", 4096),
    ("xhigh", 8192),
    ("max", 16384),
];

/// The least effort whose budget ([`EFFORT_BUDGETS`]) covers `tokens`, at
/// most `high`, which every model that takes an effort has.
pub(crate) fn effort_for(tokens: u64) -> Effort {
    match tokens {
        0 => Effort::Off,
        1..=1024 => Effort::Low,
        1025..=2048 => Effort::Medium,
        _ => Effort::High,
    }
}

/// The output limit `max`, temperature and top_p of `o` as Claude takes
/// them beside a thinking `budget`.
// WARN: a limit at or under the budget is raised by it, a temperature above
// 1 is 1, and beside a budget another temperature or top_p is dropped.
pub(crate) fn claude_sampling(
    o: &GenerationOptions,
    max: Option<u64>,
    budget: Option<u64>,
) -> (Option<u64>, Option<f64>, Option<f64>) {
    let max = max.map(|max| match budget {
        Some(budget) if max <= budget => budget.saturating_add(max),
        _ => max,
    });
    let temperature = o.temperature.map(|t| t.min(1.0));
    let temperature = temperature.filter(|t| budget.is_none() || *t == 1.0);
    let top_p = o.top_p.filter(|p| budget.is_none() || *p >= 0.95);
    (max, temperature, top_p)
}

/// The value of the request's native field `key` if `parse` reads all of it,
/// which then leaves the native.
pub(crate) fn take_native<C: Schema, T>(
    native: &mut Native,
    key: &str,
    parse: impl FnOnce(&Value) -> Option<T>,
) -> Option<T> {
    let parsed = parse(C::fields(native).get(key)?)?;
    edit_native::<C, _>(native, |fields| fields.shift_remove(key));
    Some(parsed)
}

/// `edit` of `C`'s fields in `native`, which it replaces: none if they end
/// empty; another format's dropped.
pub(crate) fn edit_native<C: Schema, T>(
    native: &mut Native,
    edit: impl FnOnce(&mut Map<String, Value>) -> T,
) -> T {
    let mut fields = C::fields(native).clone();
    let edited = edit(&mut fields);
    *native = C::native(fields);
    edited
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
