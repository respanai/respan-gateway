//! Writing a conversation as a Responses request's input items.

use super::super::RESPONSES;
use super::{ITEM_ROLE, MESSAGE, ROLES};
use crate::fields::media_url;
use crate::schema::{NativeFieldsExt, merge_missing, warn_reasoning};
use respan_gateway_conversation::{Content, Input, Media, MediaSource, Message, Role};
use respan_gateway_core::{Fault, NativeFields};
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
    let instructed =
        chat.native.format() == Some(RESPONSES) && chat.native.fields().contains_key(INSTRUCTIONS);
    let mut rest = &chat.messages[..];
    if instructed
        && let Some((first, after)) = rest.split_first()
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
    let no_role = || {
        Fault::from(crate::Error::Untranslatable(
            "a role this format has no place for",
        ))
    };
    let role = match &m.role {
        Role::Native { format, name, .. } if **format == *RESPONSES && name == ITEM_ROLE => {
            return write_opaque(m, items);
        }
        Role::Native { format, name, .. } if **format == *RESPONSES => name.as_str(),
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
            _ => {
                return Err(Fault::from(crate::Error::Untranslatable(
                    "an item in another shape",
                )));
            }
        }
    }
    Ok(())
}

/// A user's, system's or developer's message: its text and media are one
/// message item, each result an output item of its own.
fn write_other(m: &Message, role: &str, items: &mut Vec<Value>) -> Result<(), Fault> {
    let before = items.len();
    let mut extras = m.native.fields_for(RESPONSES);
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
                return Err(Fault::from(crate::Error::Untranslatable(
                    "an assistant's content in another role",
                )));
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
        false => Err(Fault::from(crate::Error::Untranslatable(
            "a message's fields beside its results",
        ))),
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
            let mut part = Map::new();
            part.insert("type".into(), "input_text".into());
            part.insert("text".into(), text.as_str().into());
            part.extend(native.fields_for(RESPONSES));
            Ok(part.into())
        }
        Content::Image(media) | Content::Document(media) => write_media(c, media),
        Content::Native(n) if *n.format == *RESPONSES => Ok(n.value.as_ref().clone()),
        Content::Audio(_) => Err(Fault::from(crate::Error::Untranslatable(
            "audio in Responses",
        ))),
        _ => Err(Fault::from(crate::Error::Untranslatable(
            "an unsupported kind of part",
        ))),
    }
}

pub(super) fn write_media(c: &Content, media: &Media) -> Result<Value, Fault> {
    let image = matches!(c, Content::Image(_));
    if image && media.name.is_some() {
        return Err(Fault::from(crate::Error::Untranslatable(
            "a display name on image content",
        )));
    }
    let native = media.native.fields_for(RESPONSES);
    let url = || media_url(media);
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
        return Err(Fault::from(crate::Error::Untranslatable(
            "a tool result that is an error",
        )));
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
    v.extend(native.fields_for(RESPONSES));
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
    if !m.native.fields_for(RESPONSES).is_empty() {
        return Err(Fault::from(crate::Error::Untranslatable(
            "an assistant message's fields",
        )));
    }
    let before = items.len();
    let mut run: Vec<&Content> = Vec::new();
    for c in &m.content {
        match c {
            Content::Text { native, .. } => {
                // A text that carries its item's fields starts that item.
                if !run.is_empty() && native.fields().contains_key(MESSAGE) {
                    flush_assistant(&mut run, items);
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
                v.extend(native.fields_for(RESPONSES));
                items.push(v.into());
            }
            // Reasoning is only valid with the id of the item it was.
            Content::Reasoning { native, .. } if native.format() != Some(RESPONSES) => {
                warn_reasoning(RESPONSES, native);
            }
            Content::Reasoning { text, native } => {
                flush_assistant(&mut run, items);
                let summary = match text.is_empty() {
                    true => json!([]),
                    false => json!([{ "type": "summary_text", "text": text }]),
                };
                let mut v = Map::new();
                v.insert("type".into(), "reasoning".into());
                v.insert("summary".into(), summary);
                v.extend(native.fields_for(RESPONSES));
                items.push(v.into());
            }
            _ => {
                return Err(Fault::from(crate::Error::Untranslatable(
                    "an unsupported kind of part",
                )));
            }
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
                let mut fields = native.fields_for(RESPONSES);
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
                written_parts.push(n.value.as_ref().clone());
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
