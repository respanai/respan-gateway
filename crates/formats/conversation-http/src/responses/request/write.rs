//! Writing a conversation as a Responses request's input items.

use super::super::RESPONSES;
use super::{ITEM_ROLE, MESSAGE, ROLES};
use crate::schema::{
    INERT, Keep, NativeFieldsExt, encode_native, kept_fields, merge_missing, untranslatable,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{Content, Input, Media, MediaSource, Message, Role};
use respan_gateway_core::{Fault, NativeFields};
use serde_json::{Map, Value, json};

fn written(native: &Option<NativeFields>, what: &str) -> Result<Map<String, Value>, Fault> {
    let what = |key: &str| format!("{what} '{key}'");
    let mut fields = kept_fields(RESPONSES, native, what, Keep::CACHE_MARKER)?;
    if let Some(Value::Object(inert)) = fields.shift_remove(INERT) {
        fields.extend(inert);
    }
    Ok(fields)
}

/// The request's `instructions` and `input`: a leading system message of
/// one plain text is the instructions.
pub(in crate::responses) fn write_input(
    chat: &Input,
    v: &mut Map<String, Value>,
) -> Result<(), Fault> {
    let mut rest = &chat.messages[..];
    if let Some((first, after)) = rest.split_first()
        && first.role == Role::System
        && first.native.is_empty()
        && let [
            Content::Text {
                text, native: None, ..
            },
        ] = &first.content[..]
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
    let no_role = || untranslatable("a role this format has no place for");
    let role = match &m.role {
        Role::Native { format, name, .. } if **format == *RESPONSES && name == ITEM_ROLE => {
            return write_opaque(m, items);
        }
        Role::Native { format, name, .. } if **format == *RESPONSES => name.as_str(),
        Role::Native { .. } => return Err(no_role()),
        role => ROLES.name(role).ok_or_else(no_role)?,
    };
    match m.role {
        Role::Assistant => write_assistant(m, items),
        _ => write_other(m, role, items),
    }
}

fn write_opaque(m: &Message, items: &mut Vec<Value>) -> Result<(), Fault> {
    for part in &m.content {
        match part {
            Content::Native(n) if *n.format == *RESPONSES => items.push(n.value.as_ref().clone()),
            _ => return Err(untranslatable("an item in another shape")),
        }
    }
    Ok(())
}

/// A user's, system's or developer's message: its text and media are one
/// message item, each result an output item of its own.
fn write_other(m: &Message, role: &str, items: &mut Vec<Value>) -> Result<(), Fault> {
    let before = items.len();
    let mut extras = written(&m.native, "a message's")?;
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
                items.push(write_result(id, content, *is_error, native)?);
            }
            Content::Reasoning { .. } | Content::ToolCall { .. } => {
                return Err(untranslatable("an assistant's content in another role"));
            }
            part => parts.push(write_input_part(part)?),
        }
    }
    flush_other(role, &mut parts, &mut extras, items);
    if items.len() == before {
        items.push(json!({ "role": role, "content": "" }));
    }
    match extras.is_empty() {
        true => Ok(()),
        false => Err(untranslatable("a message's fields beside its results")),
    }
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

/// A part of an input message or of a result's output.
fn write_input_part(c: &Content) -> Result<Value, Fault> {
    match c {
        Content::Text { text, native, .. } => {
            let what = |k: &str| format!("a text part's '{k}'");
            let mut part = Map::new();
            part.insert("type".into(), "input_text".into());
            part.insert("text".into(), text.as_str().into());
            encode_native(RESPONSES, native, &mut part, what, Keep::CACHE_MARKER)?;
            Ok(part.into())
        }
        Content::Image(media) | Content::Document(media) => write_media(c, media),
        Content::Native(n) if *n.format == *RESPONSES => Ok(n.value.as_ref().clone()),
        Content::Native(n) => Err(untranslatable(&format!("a part in {}'s shape", n.format))),
        Content::Audio(_) => Err(untranslatable("audio in Responses")),
        _ => Err(untranslatable("an unsupported kind of part")),
    }
}

pub(super) fn write_media(c: &Content, media: &Media) -> Result<Value, Fault> {
    let image = matches!(c, Content::Image(_));
    if image && media.name.is_some() {
        return Err(untranslatable("a display name on image content"));
    }
    let what = |k: &str| format!("media field '{k}'");
    let native = kept_fields(RESPONSES, &media.native, what, Keep::CACHE_MARKER)?;
    let url = || match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(data) => {
            format!("data:{};base64,{}", media.mime_type, STANDARD.encode(data))
        }
    };
    let mut v = match (image, &media.source) {
        (true, _) => json!({ "type": "input_image", "image_url": url() }),
        (false, MediaSource::Url(url)) => json!({ "type": "input_file", "file_url": url }),
        (false, MediaSource::Bytes(_)) => json!({ "type": "input_file", "file_data": url() }),
    };
    if let Some(name) = &media.name {
        v["filename"] = name.as_str().into();
    }
    merge_missing(&mut v, native);
    if image && v.get("detail").is_none() {
        v["detail"] = "auto".into();
    }
    Ok(v)
}

fn write_result(
    id: &str,
    content: &[Content],
    is_error: bool,
    native: &Option<NativeFields>,
) -> Result<Value, Fault> {
    // A function call output cannot say that the call failed.
    if is_error {
        return Err(untranslatable("a tool result that is an error"));
    }
    let plain = |c: &Content| matches!(c, Content::Text { native: None, .. });
    let output: Value = match content.iter().all(plain) {
        true => content
            .iter()
            .map(|c| c.as_text())
            .collect::<String>()
            .into(),
        false => content
            .iter()
            .map(write_input_part)
            .collect::<Result<Vec<_>, _>>()?
            .into(),
    };
    let mut v = Map::new();
    v.insert("type".into(), "function_call_output".into());
    v.insert("call_id".into(), id.into());
    v.insert("output".into(), output);
    v.extend(written(native, "a tool part's")?);
    Ok(v.into())
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
fn write_assistant(m: &Message, items: &mut Vec<Value>) -> Result<(), Fault> {
    // Another format's inert fields are dropped; any other, or its own,
    // have no place here.
    let what = |_: &str| "an assistant message's fields".to_owned();
    if !kept_fields(RESPONSES, &m.native, what, Keep::CACHE_MARKER)?.is_empty() {
        return Err(untranslatable("an assistant message's fields"));
    }
    let before = items.len();
    let mut run: Vec<&Content> = Vec::new();
    for c in &m.content {
        match c {
            Content::Text { native, .. } => {
                // A text that carries its item's fields starts that item.
                if !run.is_empty() && native.fields().contains_key(INERT) {
                    flush_assistant(&mut run, items)?;
                }
                run.push(c);
            }
            Content::Native(n) if *n.format == *RESPONSES => run.push(c),
            Content::ToolCall {
                id,
                name,
                arguments,
                native,
                ..
            } => {
                flush_assistant(&mut run, items)?;
                let arguments = match arguments.trim().is_empty() {
                    true => "{}",
                    false => arguments.as_str(),
                };
                let mut v = Map::new();
                v.insert("type".into(), "function_call".into());
                v.insert("call_id".into(), id.as_str().into());
                v.insert("name".into(), name.as_str().into());
                v.insert("arguments".into(), arguments.into());
                v.extend(written(native, "a tool part's")?);
                items.push(v.into());
            }
            Content::Reasoning { text, native } => {
                flush_assistant(&mut run, items)?;
                let summary = match text.is_empty() {
                    true => json!([]),
                    false => json!([{ "type": "summary_text", "text": text }]),
                };
                let mut v = Map::new();
                v.insert("type".into(), "reasoning".into());
                v.insert("summary".into(), summary);
                // Reasoning is only valid with the id of the item it was.
                if native.format() != Some(RESPONSES) {
                    return Err(untranslatable("reasoning from another format"));
                }
                v.extend(written(native, "reasoning field")?);
                items.push(v.into());
            }
            _ => return Err(untranslatable("an unsupported kind of part")),
        }
    }
    flush_assistant(&mut run, items)?;
    if items.len() == before {
        items.push(json!({ "role": "assistant", "content": "" }));
    }
    Ok(())
}

/// The message item of a run of an assistant's text: by its text alone if
/// it keeps nothing of an item, else in full.
fn flush_assistant(run: &mut Vec<&Content>, items: &mut Vec<Value>) -> Result<(), Fault> {
    let parts = std::mem::take(run);
    if parts.is_empty() {
        return Ok(());
    }
    let mut item = Map::new();
    let mut plain = true;
    let mut texts = String::new();
    let mut written_parts = Vec::new();
    for (i, c) in parts.iter().enumerate() {
        match c {
            Content::Text { text, native, .. } => {
                let what = |k: &str| format!("a text part's '{k}'");
                let mut fields = kept_fields(RESPONSES, native, what, Keep::CACHE_MARKER)?;
                if i == 0 {
                    if let Some(Value::Object(inert)) = fields.shift_remove(INERT) {
                        item.extend(inert);
                    }
                    if let Some(Value::Object(more)) = fields.shift_remove(MESSAGE) {
                        item.extend(more);
                    }
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
                written_parts.push(n.value.as_ref().clone());
            }
            _ => {}
        }
    }
    if plain && item.is_empty() {
        items.push(json!({ "role": "assistant", "content": texts }));
        return Ok(());
    }
    let mut v = Map::new();
    v.insert("type".into(), "message".into());
    v.insert("role".into(), "assistant".into());
    v.insert("content".into(), written_parts.into());
    v.entry("status").or_insert_with(|| "completed".into());
    v.extend(item);
    items.push(v.into());
    Ok(())
}
