//! A request in Converse's terms: the `system` blocks, `messages` and their
//! content blocks, `toolConfig`, `inferenceConfig` and the reasoning of
//! `additionalModelRequestFields`. What the kind has but Converse has no
//! field for is dropped, or written as its nearest.

use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Effort, Input, Media, MediaSource, Message, ResponseFormat, Role, Tool, ToolChoice,
};
use respan_gateway_core::Fault;
use serde_json::{Map, Value, json};

use super::{AUDIO, DOCUMENTS, IMAGES, ROLES, format_of};
use crate::schema::{
    arguments_object, budget_for, claude_sampling, sealed, set, stop_json, untranslatable,
};

/// The leading system messages as `system` blocks, and the rest, where a
/// later system message is a user's ([`write_messages`]).
pub(super) fn split_system(chat: &Input) -> (Vec<Value>, &[Message]) {
    let is_system = |m: &Message| matches!(m.role, Role::System | Role::Developer);
    let lead = chat.messages.iter().take_while(|m| is_system(m)).count();
    let (system, rest) = chat.messages.split_at(lead);
    // WARN: Anthropic's `cache_control`, on a part or tool, is no `cachePoint`;
    // dropped here, silently, and the prompt cache with it.
    // WARN: a system prompt's media and other parts are dropped.
    let parts = system.iter().flat_map(|m| &m.content);
    let blocks = parts.filter_map(|part| match part {
        Content::Text { text, .. } if !text.trim().is_empty() => Some(json!({ "text": text })),
        _ => None,
    });
    (blocks.collect(), rest)
}

/// The messages as Converse's: its content blocks, and, as it needs the
/// roles to alternate, a message of the role before it joined to that one.
/// A message with nothing to say is left out.
pub(super) fn write_messages(messages: &[Message]) -> Result<Vec<Value>, Fault> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        // WARN: a role Converse lacks, such as a later system message's, is a user's.
        let role = ROLES.name(&m.role).unwrap_or("user");
        let mut blocks: Vec<Value> = Vec::new();
        for (part, signature) in sealed(&m.content) {
            match (part, signature) {
                (Content::Reasoning { text, .. }, Some(signature)) => {
                    let text = json!({ "text": text, "signature": signature });
                    blocks.push(json!({ "reasoningContent": { "reasoningText": text } }));
                    continue;
                }
                (Content::Withheld(data), _) => {
                    blocks.push(json!({ "reasoningContent": { "redactedContent": data } }));
                    continue;
                }
                // Reasoning Bedrock did not sign it refuses, and an earlier turn may leave out.
                // WARN: Bedrock signs only reasoning; another part's signature is dropped here, silently.
                (Content::Reasoning { .. } | Content::Signature(_), _) => continue,
                _ => {}
            }
            blocks.extend(block(part)?);
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

/// A part as a content block, if Converse has one: `None` for a text that
/// says nothing, too.
fn block(part: &Content) -> Result<Option<Value>, Fault> {
    Ok(Some(match part {
        Content::Text { text, .. } if text.trim().is_empty() => return Ok(None),
        Content::Text { text, .. } => json!({ "text": text }),
        Content::Image(media) => return Ok(media_block("image", media)),
        Content::Audio(media) => return Ok(media_block("audio", media)),
        Content::Document(media) => return Ok(media_block("document", media)),
        Content::ToolCall {
            id,
            name,
            arguments,
            ..
        } => {
            let input = arguments_object(arguments)?;
            json!({ "toolUse": { "toolUseId": id, "name": name, "input": input } })
        }
        Content::ToolResult {
            id,
            content: content_of,
            is_error,
            ..
        } => {
            // WARN: a result's audio and other parts are dropped.
            let said = |c: &&Content| {
                matches!(
                    c,
                    Content::Text { .. } | Content::Image(_) | Content::Document(_)
                )
            };
            let mut content = Vec::new();
            for c in content_of.iter().filter(said) {
                content.extend(block(c)?);
            }
            if content.is_empty() {
                content.push(json!({ "text": EMPTY_RESULT }));
            }
            let mut result = json!({ "toolUseId": id, "content": content });
            if *is_error {
                result["status"] = "error".into();
            }
            json!({ "toolResult": result })
        }
        _ => return Ok(None),
    }))
}

/// What a tool result with nothing to say holds, as Converse takes no blank
/// text and needs at least one block.
const EMPTY_RESULT: &str = "(no output)";

/// An `image`, `audio` or `document` block of `media`.
// WARN: Converse takes media as bytes of the types it names, and an image
// has no name; media by URL or of another type is dropped, as is the name.
fn media_block(kind: &str, media: &Media) -> Option<Value> {
    let MediaSource::Bytes(bytes) = &media.source else {
        return None;
    };
    let mime = match media.mime_type.as_str() {
        "image/jpg" => "image/jpeg",
        mime => mime,
    };
    let table = match kind {
        "image" => IMAGES,
        "audio" => AUDIO,
        _ => DOCUMENTS,
    };
    let source = json!({ "bytes": STANDARD.encode(bytes) });
    let mut block = json!({ "format": format_of(table, mime)?, "source": source });
    if kind == "document" {
        block["name"] = document_name(media.name.as_deref().unwrap_or("document")).into();
    }
    Some(json!({ kind: block }))
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
    // WARN: Converse cannot turn off parallel tool calls; the ask is dropped.
    let tool = |t: &Tool| {
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
        json!({ "toolSpec": spec })
    };
    let choice = match &chat.options.tool_choice {
        None | Some(ToolChoice::Auto) => None,
        Some(ToolChoice::None) => return Ok(()),
        Some(ToolChoice::Required) => Some(json!({ "any": {} })),
        Some(ToolChoice::Tool(name)) => Some(json!({ "tool": { "name": name } })),
        Some(_) => return Err(untranslatable("a tool choice this format has no form for")),
    };
    let tools: Vec<Value> = chat.tools.iter().map(tool).collect();
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
    // WARN: an end user's id has no field in Converse; dropped silently.
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
        // WARN: JSON without a schema has no field; the prompt asks for it.
        Some(_) => {}
    }
    // Converse's `thinking` is Claude's: another model refuses it.
    // WARN: another model's reasoning, which it names its own way, is dropped,
    // as is an ask to withhold the thinking.
    let claude = chat.model.as_deref().is_some_and(|m| m.contains("claude"));
    let reasoning = o.reasoning.as_ref().filter(|_| claude);
    let enabled = |budget: u64| json!({ "type": "enabled", "budget_tokens": budget });
    let thinking = reasoning.and_then(|r| match (r.effort, r.budget_tokens) {
        (Some(Effort::Off), _) => Some(json!({ "type": "disabled" })),
        // WARN: a budget under Claude's least, 1024, is raised to it.
        (_, Some(budget)) => Some(enabled(budget.max(1024))),
        // The thinking budget nearest the effort.
        (effort, None) => effort.map(budget_for).map(enabled),
    });
    let budget = thinking.as_ref().and_then(|t| t["budget_tokens"].as_u64());
    if let Some(thinking) = thinking {
        v.insert(
            "additionalModelRequestFields".into(),
            json!({ "thinking": thinking }),
        );
    }
    // Converse's temperature is at most 1, too.
    let (max, temperature, top_p) = claude_sampling(o, o.max_output_tokens, budget);
    let mut config = Map::new();
    set(&mut config, "maxTokens", max.map(Into::into));
    set(&mut config, "temperature", temperature.map(Into::into));
    set(&mut config, "topP", top_p.map(Into::into));
    set(&mut config, "stopSequences", stop_json(&o.stop));
    if !config.is_empty() {
        v.insert("inferenceConfig".into(), config.into());
    }
    Ok(())
}
