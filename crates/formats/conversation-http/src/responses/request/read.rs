//! Reading a Responses request's input items as messages.

use super::super::RESPONSES;
use super::super::decode::SEALED;
use super::{ITEM_ROLE, MESSAGE, ROLES};
use crate::schema::{NativeFieldsExt, native_fields, only, remainder, says, source_url};
use respan_gateway_conversation::{Content, Input, Media, MediaSource, Message, Role};
use serde_json::{Map, Value, json};

/// A text part of `text` with these fields of its own.
fn text_part(text: &str, fields: Map<String, Value>) -> Content {
    let mut part = Content::from(text);
    if let Content::Text { native, .. } = &mut part {
        *native = native_fields(RESPONSES, fields);
    }
    part
}

/// A message content: a string, or parts.
pub(in crate::responses) fn read_content(v: &Value) -> Vec<Content> {
    match v {
        Value::String(text) => vec![Content::from(text.as_str())],
        Value::Array(parts) => parts.iter().map(read_part).collect(),
        Value::Null => Vec::new(),
        other => vec![Content::native(RESPONSES, other.clone())],
    }
}

fn read_part(p: &Value) -> Content {
    let part = match p["type"].as_str() {
        Some("input_text" | "output_text") => read_text(p),
        _ => read_media(p),
    };
    part.unwrap_or_else(|| Content::native(RESPONSES, p.clone()))
}

/// An `input_text` or `output_text` part; its fields that say something,
/// such as annotations, are its native.
fn read_text(p: &Value) -> Option<Content> {
    let text = p["text"].as_str()?;
    let fields = remainder(p, &["type", "text"]);
    let fields = fields.into_iter().filter(|(_, v)| says(v)).collect();
    Some(text_part(text, fields))
}

/// An image or file part the kind can say, whose other fields are its
/// media's native.
pub(super) fn read_media(p: &Value) -> Option<Content> {
    let mut fields = match p["type"].as_str()? {
        "input_image" => remainder(p, &["type", "image_url"]),
        "input_file" => remainder(p, &["type", "file_data", "file_url", "filename"]),
        _ => return None,
    };
    fields.retain(|_, v| !v.is_null());
    let mut media = if p["type"] == "input_image" {
        // The default detail says nothing.
        if fields.get("detail") == Some(&json!("auto")) {
            fields.shift_remove("detail");
        }
        let (mime, source) = source_url(p["image_url"].as_str()?, "image/*")?;
        Media::new(mime, source)
    } else {
        let (mime, source) = match (p["file_data"].as_str(), p["file_url"].as_str()) {
            (Some(data), None) if data.starts_with("data:") => {
                source_url(data, "application/octet-stream")?
            }
            (None, Some(url)) => (
                "application/octet-stream".into(),
                MediaSource::Url(url.into()),
            ),
            _ => return None,
        };
        let mut media = Media::new(mime, source);
        media.name = p["filename"].as_str().map(str::to_owned);
        media
    };
    media.native = native_fields(RESPONSES, fields);
    Some(match p["type"] == "input_image" {
        true => Content::Image(media),
        false => Content::Document(media),
    })
}

/// The request's `instructions` and `input` as messages.
pub(in crate::responses) fn read_input(v: &Value, chat: &mut Input) {
    if says(&v["instructions"]) {
        let mut system = Message::new(Role::System, "");
        system.content = read_content(&v["instructions"]);
        chat.messages.push(system);
    }
    match &v["input"] {
        Value::Null => {}
        Value::String(text) => chat.messages.push(Message::new(Role::User, text.as_str())),
        Value::Array(items) => items.iter().for_each(|i| push_item(&mut chat.messages, i)),
        other => chat.messages.push(opaque(other)),
    }
}

/// An item the kind has no form for.
fn opaque(item: &Value) -> Message {
    let mut message = Message::new(Role::native(RESPONSES, ITEM_ROLE), "");
    message.content = vec![Content::native(RESPONSES, item.clone())];
    message
}

/// The assistant message the next item of an assistant joins.
fn assistant(out: &mut Vec<Message>) -> &mut Message {
    if !matches!(out.last(), Some(m) if m.role == Role::Assistant) {
        let mut message = Message::new(Role::Assistant, "");
        message.content.clear();
        out.push(message);
    }
    let last = out.len() - 1;
    &mut out[last]
}

fn push_item(out: &mut Vec<Message>, item: &Value) {
    let kind = match item.get("type") {
        None | Some(Value::Null) => "message",
        Some(kind) => kind.as_str().unwrap_or_default(),
    };
    let done = match kind {
        "message" => push_message(out, item),
        "function_call" => push_call(out, item),
        "function_call_output" => push_result(out, item),
        "reasoning" => push_reasoning(out, item),
        _ => None,
    };
    if done.is_none() {
        out.push(opaque(item));
    }
}

fn push_message(out: &mut Vec<Message>, item: &Value) -> Option<()> {
    let name = item["role"].as_str()?;
    let mut content = read_content(&item["content"]);
    let extras = remainder(item, &["type", "role", "content"]);
    match ROLES.value(name) {
        Some(Role::Assistant) => {
            // The item's own fields ride on its first text part.
            let first = content.iter_mut().find_map(|c| match c {
                Content::Text { native, .. } => Some(native),
                _ => None,
            })?;
            let mut fields = first.take().map(|n| n.fields).unwrap_or_default();
            if !extras.is_empty() {
                fields.insert(MESSAGE.into(), extras.into());
            }
            *first = native_fields(RESPONSES, fields);
            assistant(out).content.extend(content);
        }
        role => {
            let role = role.unwrap_or_else(|| Role::native(RESPONSES, name));
            let mut message = Message::new(role, "");
            message.content = content;
            message.native = native_fields(RESPONSES, extras);
            out.push(message);
        }
    }
    Some(())
}

fn push_call(out: &mut Vec<Message>, item: &Value) -> Option<()> {
    let (id, name) = (item["call_id"].as_str()?, item["name"].as_str()?);
    let mut call = Content::tool_call(id, name, item["arguments"].as_str()?);
    if let Content::ToolCall { native, .. } = &mut call {
        let taken = ["type", "call_id", "name", "arguments"];
        *native = native_fields(RESPONSES, remainder(item, &taken));
    }
    assistant(out).content.push(call);
    Some(())
}

fn push_result(out: &mut Vec<Message>, item: &Value) -> Option<()> {
    let id = item["call_id"].as_str()?;
    let content = match &item["output"] {
        Value::String(_) | Value::Array(_) => read_content(&item["output"]),
        _ => return None,
    };
    let mut result = Content::tool_result(id, content, false);
    if let Content::ToolResult { native, .. } = &mut result {
        let taken = ["type", "call_id", "output"];
        *native = native_fields(RESPONSES, remainder(item, &taken));
    }
    let results = |m: &Message| {
        m.role == Role::User
            && m.native.is_empty()
            && m.content
                .iter()
                .all(|c| matches!(c, Content::ToolResult { .. }))
    };
    match out.last_mut().filter(|m| results(m)) {
        Some(last) => last.content.push(result),
        None => {
            let mut message = Message::new(Role::User, "");
            message.content = vec![result];
            out.push(message);
        }
    }
    Some(())
}

fn push_reasoning(out: &mut Vec<Message>, item: &Value) -> Option<()> {
    let mut texts = Vec::new();
    for part in item["summary"].as_array()? {
        match (part["type"].as_str()?, part["text"].as_str()) {
            ("summary_text", Some(text)) if only(part, &["type", "text"]) => texts.push(text),
            _ => return None,
        }
    }
    let native = native_fields(RESPONSES, remainder(item, &["type", "summary", SEALED]));
    let text = texts.join("\n\n");
    let content = &mut assistant(out).content;
    content.push(Content::Reasoning { text, native });
    if let Some(data) = item[SEALED].as_str() {
        content.push(Content::Signature(data.into()));
    }
    Some(())
}
