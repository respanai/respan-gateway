//! A request in Converse's terms: the `system` blocks, `messages` and their
//! content blocks, `toolConfig`, `inferenceConfig` and the reasoning of
//! `additionalModelRequestFields`. What the kind has but Converse has no
//! field for, such as `top_k`, an end user's id or an effort, is refused.

use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Effort, Input, Media, MediaSource, Message, ResponseFormat, Role, Tool, ToolChoice,
};
use respan_gateway_core::{Fault, NativeFields};
use serde_json::{Map, Value, json};

use super::read::CACHE_MARKER;
use super::{BEDROCK, ROLES};
use crate::schema::{
    Keep, NativeFieldsExt, arguments_object, encode_native, kept_fields, reasoning_of, set,
    stop_json, tool_field, untranslatable, unwritable_content,
};

/// The leading system messages as `system` blocks, and the rest; a later
/// system message is refused.
pub(super) fn split_system(chat: &Input) -> Result<(Vec<Value>, &[Message]), Fault> {
    let is_system = |m: &Message| matches!(m.role, Role::System | Role::Developer);
    let lead = chat.messages.iter().take_while(|m| is_system(m)).count();
    let (system, rest) = chat.messages.split_at(lead);
    if rest.iter().any(is_system) {
        return Err(Fault::bad_request(
            "Bedrock has no place for a system message after the conversation starts",
        ));
    }
    let mut blocks = Vec::new();
    for message in system {
        check(&message.native, "a system message's fields")?;
        for part in &message.content {
            match part {
                Content::Text { text, native, .. } => {
                    check(native, "a text part's fields")?;
                    if !text.trim().is_empty() {
                        blocks.push(json!({ "text": text }));
                        if cached(native) {
                            blocks.push(cache_point());
                        }
                    }
                }
                Content::Native(raw) if *raw.format == *BEDROCK => {
                    blocks.push(Value::clone(&raw.value));
                }
                _ => return Err(untranslatable("a system prompt that is not text")),
            }
        }
    }
    Ok((blocks, rest))
}

/// Refuses the fields of another format's `native`, but a cache marker,
/// which Converse has no block for here.
fn check(native: &Option<NativeFields>, what: &str) -> Result<(), Fault> {
    let what = |_: &str| what.to_owned();
    kept_fields(BEDROCK, native, what, Keep::CACHE_MARKER).map(drop)
}

/// The messages as Converse's: its content blocks, and, as it needs the
/// roles to alternate, a message of the role before it joined to that one.
/// A message with nothing to say is left out.
pub(super) fn write_messages(messages: &[Message]) -> Result<Vec<Value>, Fault> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let role = match &m.role {
            Role::Native { format, name, .. } if **format == *BEDROCK => name.as_str(),
            role => ROLES
                .name(role)
                .ok_or_else(|| untranslatable("a role this format has no place for"))?,
        };
        let fields = |_: &str| "messages with fields the kind has no place for".to_owned();
        encode_native(BEDROCK, &m.native, &mut Map::new(), fields, Keep::default())?;
        let mut blocks: Vec<Value> = Vec::new();
        for part in m.content.iter().filter(|c| !is_unsigned(c)) {
            if let Some(written) = block(part)? {
                blocks.push(written);
                if marked(part) {
                    blocks.push(cache_point());
                }
            }
        }
        if blocks.is_empty() {
            continue;
        }
        match out.last_mut().filter(|last| last["role"] == role) {
            Some(last) => {
                if let Some(content) = last["content"].as_array_mut() {
                    content.extend(blocks);
                }
            }
            None => out.push(json!({ "role": role, "content": blocks })),
        }
    }
    Ok(out)
}

/// What Converse caches up to: a `cachePoint` after the block.
fn cache_point() -> Value {
    json!({ "cachePoint": { "type": "default" } })
}

/// Whether fields hold a cache marker, Anthropic's `cache_control` or one
/// a `cachePoint` of this format's put there: the block they are on is
/// cached up to, whichever format marked it.
fn cached(native: &Option<NativeFields>) -> bool {
    native.fields().contains_key(CACHE_MARKER)
}

/// Whether a part is marked to be cached up to.
fn marked(part: &Content) -> bool {
    match part {
        Content::Text { native, .. }
        | Content::ToolCall { native, .. }
        | Content::ToolResult { native, .. } => cached(native),
        Content::Image(media) | Content::Document(media) => cached(&media.native),
        _ => false,
    }
}

/// A part as a content block; `None` for a text that says nothing.
fn block(part: &Content) -> Result<Option<Value>, Fault> {
    match part {
        Content::Text { text, .. } if text.trim().is_empty() => Ok(None),
        part => content_block(part).map(Some),
    }
}

/// A part as a content block.
fn content_block(part: &Content) -> Result<Value, Fault> {
    match part {
        Content::Text { text, native, .. } => {
            check(native, "a text part's fields")?;
            Ok(json!({ "text": text }))
        }
        Content::Image(media) => image(media),
        Content::Document(media) => document(media),
        Content::Reasoning { text, native } => reasoning(text, native),
        Content::ToolCall {
            id,
            name,
            arguments,
            native,
            ..
        } => {
            check(native, "a tool call's fields")?;
            let input = arguments_object(arguments)?;
            Ok(json!({ "toolUse": { "toolUseId": id, "name": name, "input": input } }))
        }
        Content::ToolResult {
            id,
            content,
            is_error,
            native,
            ..
        } => {
            check(native, "a tool result's fields")?;
            let said = content.iter().filter(|c| !is_blank(c));
            let mut content: Vec<Value> = said.map(result_block).collect::<Result<_, _>>()?;
            if content.is_empty() {
                content.push(json!({ "text": EMPTY_RESULT }));
            }
            let mut result = json!({ "toolUseId": id, "content": content });
            if *is_error {
                result["status"] = "error".into();
            }
            Ok(json!({ "toolResult": result }))
        }
        Content::Native(raw) if *raw.format == *BEDROCK => Ok(Value::clone(&raw.value)),
        Content::Native(raw) => Err(untranslatable(&format!("a part in {}'s shape", raw.format))),
        Content::Audio(_) => Err(untranslatable("audio")),
        _ => Err(untranslatable("an unsupported kind of part")),
    }
}

/// What a tool result with nothing to say holds, as Converse takes no blank
/// text and needs at least one block.
const EMPTY_RESULT: &str = "(no output)";

/// Whether a part is a text that says nothing.
fn is_blank(part: &Content) -> bool {
    matches!(part, Content::Text { text, .. } if text.trim().is_empty())
}

/// What a tool result holds: text, an image or a document.
fn result_block(part: &Content) -> Result<Value, Fault> {
    match part {
        Content::Text { .. } | Content::Image(_) | Content::Document(_) => content_block(part),
        _ => Err(untranslatable("a tool result that is not text or media")),
    }
}

/// Reasoning kept in the history, signed or redacted by Bedrock.
fn reasoning(text: &str, native: &Option<NativeFields>) -> Result<Value, Fault> {
    let own = native.format() == Some(BEDROCK);
    let field = |key: &str| own.then(|| native.fields().get(key)).flatten();
    if let Some(redacted) = field("redactedContent") {
        return Ok(json!({ "reasoningContent": { "redactedContent": redacted } }));
    }
    match field("signature") {
        Some(signature) => Ok(json!({
            "reasoningContent": { "reasoningText": { "text": text, "signature": signature } }
        })),
        None => Err(unwritable_content()),
    }
}

/// Whether a part is reasoning Bedrock cannot take back, which a message
/// leaves out as Anthropic's does.
pub(super) fn is_unsigned(part: &Content) -> bool {
    match part {
        Content::Reasoning { native, .. } => {
            let signed = ["signature", "redactedContent"]
                .iter()
                .any(|k| native.fields().contains_key(*k));
            native.format() != Some(BEDROCK) || !signed
        }
        _ => false,
    }
}

/// The bytes of `media`, base64; a URL is refused, as Converse takes bytes
/// only.
fn bytes_of(media: &Media) -> Result<Value, Fault> {
    let what = |_: &str| "media fields the kind has no place for".to_owned();
    kept_fields(BEDROCK, &media.native, what, Keep::CACHE_MARKER)?;
    match &media.source {
        MediaSource::Bytes(bytes) => Ok(json!({ "bytes": STANDARD.encode(bytes) })),
        MediaSource::Url(_) => Err(Fault::bad_request(
            "Bedrock takes media as bytes, not a URL: send it as a data URL",
        )),
    }
}

fn image(media: &Media) -> Result<Value, Fault> {
    let format = match media.mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => {
            return Err(Fault::bad_request(format!(
                "Bedrock takes png, jpeg, gif and webp images, not {other}"
            )));
        }
    };
    if media.name.is_some() {
        return Err(untranslatable("a display name on an image"));
    }
    Ok(json!({ "image": { "format": format, "source": bytes_of(media)? } }))
}

fn document(media: &Media) -> Result<Value, Fault> {
    let format = match media.mime_type.as_str() {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        other => {
            return Err(Fault::bad_request(format!(
                "Bedrock does not take {other} documents"
            )));
        }
    };
    let name = document_name(media.name.as_deref().unwrap_or("document"));
    let source = bytes_of(media)?;
    Ok(json!({ "document": { "format": format, "name": name, "source": source } }))
}

/// A document's name as Bedrock allows it: letters, digits, single spaces,
/// hyphens, parentheses and square brackets.
fn document_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = match c {
            c if c.is_alphanumeric() || "-()[]".contains(c) => c,
            c if c.is_whitespace() => ' ',
            _ => '-',
        };
        if c != ' ' || !out.ends_with(' ') {
            out.push(c);
        }
    }
    match out.trim() {
        "" => "document".to_owned(),
        name => name.to_owned(),
    }
}

/// `toolConfig`: the tools and their choice. No tool choice of the kind's
/// `None` offers none.
pub(super) fn write_tools(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    if chat.options.parallel_tool_calls == Some(false) {
        return Err(untranslatable("one tool call at a time"));
    }
    let tool = |t: &Tool| -> Result<Value, Fault> {
        let mut spec = Map::new();
        spec.insert("name".into(), t.name.as_str().into());
        if let Some(text) = &t.description {
            spec.insert("description".into(), text.as_str().into());
        }
        let schema = match t.parameters.is_object() {
            true => t.parameters.clone(),
            false => json!({ "type": "object" }),
        };
        spec.insert("inputSchema".into(), json!({ "json": schema }));
        let mut fields = kept_fields(BEDROCK, &t.native, tool_field, Keep::CACHE_MARKER)?;
        fields.shift_remove(CACHE_MARKER);
        spec.extend(fields);
        Ok(json!({ "toolSpec": spec }))
    };
    let choice = match &chat.options.tool_choice {
        None | Some(ToolChoice::Auto) => None,
        Some(ToolChoice::None) => return Ok(()),
        Some(ToolChoice::Required) => Some(json!({ "any": {} })),
        Some(ToolChoice::Tool(name)) => Some(json!({ "tool": { "name": name } })),
        Some(_) => return Err(untranslatable("a tool choice this format has no form for")),
    };
    let mut tools: Vec<Value> = Vec::new();
    for t in &chat.tools {
        tools.push(tool(t)?);
        if cached(&t.native) {
            tools.push(cache_point());
        }
    }
    if tools.is_empty() {
        return Ok(());
    }
    let mut config = json!({ "tools": tools });
    if let Some(choice) = choice {
        config["toolChoice"] = choice;
    }
    v.insert("toolConfig".into(), config);
    Ok(())
}

/// `inferenceConfig` and the reasoning in `additionalModelRequestFields`.
pub(super) fn write_config(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    let o = &chat.options;
    if o.top_k.is_some() {
        return Err(untranslatable("top_k, which Bedrock takes per model"));
    }
    if chat.user.is_some() {
        return Err(untranslatable("an end user's id"));
    }
    match &o.response_format {
        None | Some(ResponseFormat::Text) => {}
        Some(ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            ..
        }) => {
            let mut json_schema = json!({ "schema": schema.to_string(), "name": name });
            if let Some(description) = description {
                json_schema["description"] = description.as_str().into();
            }
            let structure = json!({ "jsonSchema": json_schema });
            let format = json!({ "type": "json_schema", "structure": structure });
            v.insert("outputConfig".into(), json!({ "textFormat": format }));
        }
        Some(_) => return Err(untranslatable("JSON output without a schema")),
    }
    let mut config = Map::new();
    set(
        &mut config,
        "maxTokens",
        o.max_output_tokens.map(Into::into),
    );
    set(&mut config, "temperature", o.temperature.map(Into::into));
    set(&mut config, "topP", o.top_p.map(Into::into));
    set(&mut config, "stopSequences", stop_json(&o.stop));
    if !config.is_empty() {
        v.insert("inferenceConfig".into(), config.into());
    }
    if let Some(r) = reasoning_of(chat)? {
        if r.summary == Some(false) {
            return Err(untranslatable("withholding the thinking"));
        }
        let thinking = match (r.effort, r.budget_tokens) {
            (Some(Effort::Off), _) => json!({ "type": "disabled" }),
            (Some(_), _) => {
                return Err(untranslatable(
                    "a reasoning effort, which Bedrock names per model: give a token budget",
                ));
            }
            (None, Some(budget)) => json!({ "type": "enabled", "budget_tokens": budget }),
            (None, None) => return Ok(()),
        };
        v.insert(
            "additionalModelRequestFields".into(),
            json!({ "thinking": thinking }),
        );
    }
    Ok(())
}
