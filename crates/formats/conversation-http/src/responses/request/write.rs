//! Writing a conversation as a Responses request's input items.

use super::super::decode::SEALED;
use super::{ITEM_ROLE, MESSAGE, ROLES};
use crate::Schema;
use crate::fields::media_url;
use crate::responses::OpenAiResponses;
use crate::schema::{FAILED_RESULT_PREFIX, Own, merge_missing, sealed};
use respan_gateway_conversation::{Content, Input, Media, MediaSource, Message, Role};
use respan_gateway_core::Fault;
use respan_gateway_core::Native;
use serde_json::{Map, Value, json};

/// The key in a request's native that says its leading system message was
/// its `instructions`.
pub(in crate::responses) const INSTRUCTIONS: &str = "instructions";

/// The request's `instructions` and `input`: a leading system message of
/// one plain text is the instructions if it came so; another's system
/// message, such as Chat Completions', is an input item, where OpenAI reads
/// it as it would have there (its check that a JSON answer is asked for in
/// words reads only the input).
pub(in crate::responses) fn write_input(
    chat: &Input,
    v: &mut Map<String, Value>,
) -> Result<(), Fault> {
    let instructed = OpenAiResponses::fields(&chat.native).contains_key(INSTRUCTIONS);
    let mut rest = &chat.messages[..];
    if instructed
        && let Some((first, after)) = rest.split_first()
        && first.role == Role::System
        && first.native.is_empty()
        && let [Content::Text { text, native, .. }] = &first.content[..]
        && native.is_empty()
        && !text.is_empty()
    {
        v.insert("instructions".into(), text.as_str().into());
        rest = after;
    }
    let mut items = Vec::new();
    for m in rest {
        write_message(m, &mut items)?;
    }
    v.insert("input".into(), items.into());
    Ok(())
}

fn write_message(m: &Message, items: &mut Vec<Value>) -> Result<(), Fault> {
    let role = match &m.role {
        Role::Native(n) => match OpenAiResponses::value(n).and_then(Value::as_str) {
            Some(ITEM_ROLE) => {
                write_opaque(m, items);
                return Ok(());
            }
            // WARN: a role Responses lacks is a user's.
            name => name.unwrap_or("user"),
        },
        role => ROLES.name(role).unwrap_or("user"),
    };
    match m.role {
        Role::Assistant => write_assistant(m, items),
        _ => write_other(m, role, items),
    }
}

fn write_opaque(m: &Message, items: &mut Vec<Value>) {
    for part in &m.content {
        // WARN: what else a plugin put among the items is dropped.
        if let Content::Native(n) = part
            && let Some(item) = OpenAiResponses::value(n)
        {
            items.push(item.clone());
        }
    }
}

/// A user's, system's or developer's message: its text and media are one
/// message item, each result an output item of its own.
fn write_other(m: &Message, role: &str, items: &mut Vec<Value>) -> Result<(), Fault> {
    let before = items.len();
    let mut extras = OpenAiResponses::fields(&m.native).clone();
    let mut parts: Vec<Value> = Vec::new();
    for c in &m.content {
        match c {
            Content::ToolResult {
                id,
                content,
                is_error,
                native,
                ..
            } => {
                flush_other(role, &mut parts, &mut extras, items);
                items.push(write_result(id, content, *is_error, native));
            }
            // WARN: an assistant's content, or a signature, in another role is dropped.
            Content::Reasoning { .. }
            | Content::ToolCall { .. }
            | Content::Signature(_)
            | Content::Withheld(_) => {}
            part => parts.extend(write_input_part(part)),
        }
    }
    flush_other(role, &mut parts, &mut extras, items);
    if items.len() == before {
        items.push(json!({ "role": role, "content": "" }));
    }
    // WARN: a message's fields beside its results alone have no item; dropped.
    Ok(())
}

/// The message item of `parts`, if there are any.
fn flush_other(
    role: &str,
    parts: &mut Vec<Value>,
    extras: &mut Map<String, Value>,
    items: &mut Vec<Value>,
) {
    if parts.is_empty() {
        return;
    }
    let plain = match &parts[..] {
        [one] if only_keys(one, &["type", "text"]) => one["text"].as_str().map(str::to_owned),
        _ => None,
    };
    let mut item = Map::new();
    if !extras.is_empty() {
        item.insert("type".into(), "message".into());
    }
    item.insert("role".into(), role.into());
    let content = match plain.filter(|_| extras.is_empty()) {
        Some(text) => text.into(),
        None => std::mem::take(parts).into(),
    };
    item.insert("content".into(), content);
    item.extend(std::mem::take(extras));
    parts.clear();
    items.push(item.into());
}

fn only_keys(v: &Value, keys: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)))
}

/// A part of an input message or of a result's output, if Responses has one.
fn write_input_part(c: &Content) -> Option<Value> {
    match c {
        Content::Text { text, native, .. } => {
            let mut part = Map::new();
            part.insert("type".into(), "input_text".into());
            part.insert("text".into(), text.as_str().into());
            part.extend(OpenAiResponses::fields(native).clone());
            Some(part.into())
        }
        Content::Image(media) | Content::Document(media) => Some(write_media(c, media)),
        Content::Native(n) => OpenAiResponses::value(n).cloned(),
        // WARN: audio and other parts have no form in an input; dropped.
        _ => None,
    }
}

/// An image or a file; an image has no display name, which is dropped.
pub(super) fn write_media(c: &Content, media: &Media) -> Value {
    let image = matches!(c, Content::Image(_));
    let native = OpenAiResponses::fields(&media.native).clone();
    let url = || media_url(media);
    let mut v = match (image, &media.source) {
        (true, _) => json!({ "type": "input_image", "image_url": url() }),
        (false, MediaSource::Url(url)) => json!({ "type": "input_file", "file_url": url }),
        (false, MediaSource::Bytes(_)) => json!({ "type": "input_file", "file_data": url() }),
    };
    if let Some(name) = media.name.as_ref().filter(|_| !image) {
        v["filename"] = name.as_str().into();
    }
    merge_missing(&mut v, native);
    if image && v.get("detail").is_none() {
        v["detail"] = "auto".into();
    }
    v
}

fn write_result(id: &str, content: &[Content], is_error: bool, native: &Native) -> Value {
    // A function call output cannot say that the call failed: its text does.
    let failed = is_error.then(|| Content::from(FAILED_RESULT_PREFIX));
    let content: Vec<Content> = failed.into_iter().chain(content.iter().cloned()).collect();
    let plain = |c: &Content| matches!(c, Content::Text { native, .. } if native.is_empty());
    let output: Value = match content.iter().all(plain) {
        true => content
            .iter()
            .map(|c| c.as_text())
            .collect::<String>()
            .into(),
        false => content
            .iter()
            .filter_map(write_input_part)
            .collect::<Vec<_>>()
            .into(),
    };
    let mut v = Map::new();
    v.insert("type".into(), "function_call_output".into());
    v.insert("call_id".into(), id.into());
    v.insert("output".into(), output);
    v.extend(OpenAiResponses::fields(native).clone());
    v.into()
}

/// The text of a plain text part; others have none.
trait AsText {
    fn as_text(&self) -> &str;
}

impl AsText for Content {
    fn as_text(&self) -> &str {
        match self {
            Content::Text { text, .. } => text,
            _ => "",
        }
    }
}

/// An assistant message: its text runs are message items, its calls and
/// reasoning items of their own.
// WARN: an assistant message's own fields have no item of their own; dropped.
fn write_assistant(m: &Message, items: &mut Vec<Value>) -> Result<(), Fault> {
    let before = items.len();
    let mut run: Vec<&Content> = Vec::new();
    for (c, signature) in sealed(&m.content) {
        match c {
            Content::Text { native, .. } => {
                // A text that carries its item's fields starts that item.
                if !run.is_empty() && OpenAiResponses::fields(native).contains_key(MESSAGE) {
                    flush_assistant(&mut run, items);
                }
                run.push(c);
            }
            Content::Native(n) if OpenAiResponses::value(n).is_some() => run.push(c),
            Content::ToolCall {
                id,
                name,
                arguments,
                native,
                ..
            } => {
                flush_assistant(&mut run, items);
                let arguments = match arguments.trim().is_empty() {
                    true => "{}",
                    false => arguments.as_str(),
                };
                let mut v = Map::new();
                v.insert("type".into(), "function_call".into());
                v.insert("call_id".into(), id.as_str().into());
                v.insert("name".into(), name.as_str().into());
                v.insert("arguments".into(), arguments.into());
                v.extend(OpenAiResponses::fields(native).clone());
                items.push(v.into());
            }
            // Reasoning is Responses' by its item's id or its encrypted content.
            // WARN: unsigned reasoning without an id, another format's, is dropped here, silently.
            // WARN: a signature is written as Responses' own; gateway-next strips another family's.
            Content::Reasoning { native, .. }
                if signature.is_none() && OpenAiResponses::fields(native).is_empty() => {}
            Content::Reasoning { text, native } => {
                flush_assistant(&mut run, items);
                let summary = match text.is_empty() {
                    true => json!([]),
                    false => json!([{ "type": "summary_text", "text": text }]),
                };
                let mut v = Map::new();
                v.insert("type".into(), "reasoning".into());
                v.insert("summary".into(), summary);
                v.extend(OpenAiResponses::fields(native).clone());
                if let Some(s) = signature {
                    v.insert(SEALED.into(), s.into());
                }
                items.push(v.into());
            }
            // WARN: Responses signs only reasoning, and has no form for reasoning withheld; another signature is dropped here, silently.
            // WARN: media and other parts have no form in an assistant's item; dropped.
            _ => {}
        }
    }
    flush_assistant(&mut run, items);
    if items.len() == before {
        items.push(json!({ "role": "assistant", "content": "" }));
    }
    Ok(())
}

/// The message item of a run of an assistant's text: by its text alone if
/// it keeps nothing of an item, else in full.
fn flush_assistant(run: &mut Vec<&Content>, items: &mut Vec<Value>) {
    let parts = std::mem::take(run);
    if parts.is_empty() {
        return;
    }
    let mut item = Map::new();
    let mut plain = true;
    let mut texts = String::new();
    let mut written_parts = Vec::new();
    for (i, c) in parts.iter().enumerate() {
        match c {
            Content::Text { text, native, .. } => {
                let mut fields = OpenAiResponses::fields(native).clone();
                if i == 0
                    && let Some(Value::Object(more)) = fields.shift_remove(MESSAGE)
                {
                    item.extend(more);
                }
                plain &= fields.is_empty();
                texts.push_str(text);
                let mut part = Map::new();
                part.insert("type".into(), "output_text".into());
                part.insert("text".into(), text.as_str().into());
                part.extend(fields);
                part.entry("annotations").or_insert_with(|| json!([]));
                written_parts.push(Value::Object(part));
            }
            Content::Native(n) => {
                plain = false;
                written_parts.extend(OpenAiResponses::value(n).cloned());
            }
            _ => {}
        }
    }
    if plain && item.is_empty() {
        items.push(json!({ "role": "assistant", "content": texts }));
        return;
    }
    let mut v = Map::new();
    v.insert("type".into(), "message".into());
    v.insert("role".into(), "assistant".into());
    v.insert("content".into(), written_parts.into());
    v.entry("status").or_insert_with(|| "completed".into());
    v.extend(item);
    items.push(v.into());
}
