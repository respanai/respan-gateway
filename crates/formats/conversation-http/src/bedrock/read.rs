//! A client's Converse request as a conversation: its `system` blocks,
//! `messages`, `toolConfig`, `inferenceConfig`, `outputConfig` and a
//! reasoning budget in `additionalModelRequestFields`. What the kind has
//! no form for stays native: a field of the request, a block (a guard, a
//! video, media in S3) or a part's field. A `cachePoint` marks the part,
//! tool or system block before it, as a cache marker.

use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Effort, Input, Media, MediaSource, Message, Reasoning, ResponseFormat, Role, Tool,
    ToolChoice,
};
use respan_gateway_core::NativeFields;
use respan_gateway_http::bytes::Bytes;
use serde_json::{Map, Value, json};

use super::{BEDROCK, ROLES};
use crate::schema::{native_fields, rest_of, unsupported_fields};

/// The request fields read into the conversation.
const KNOWN: &[&str] = &[
    "messages",
    "system",
    "inferenceConfig",
    "toolConfig",
    "outputConfig",
    "additionalModelRequestFields",
];

/// The cache marker a `cachePoint` puts on what it follows, as a part's
/// field: the one every format drops but the one that wrote it, and this
/// format writes back as a `cachePoint`.
pub(super) const CACHE_MARKER: &str = "cache_control";

/// A Converse request `v` as a conversation.
pub(super) fn read(v: &Value) -> Input {
    let mut chat = Input::default();
    let mut native = unsupported_fields(v, KNOWN, &[]);
    let system = blocks(&v["system"]);
    if !system.is_empty() {
        let mut message = Message::new(Role::System, "");
        message.content = system;
        chat.messages.push(message);
    }
    for m in v["messages"].as_array().into_iter().flatten() {
        let name = m["role"].as_str().unwrap_or_default();
        let role = ROLES
            .value(name)
            .unwrap_or_else(|| Role::native(BEDROCK, name));
        let mut message = Message::new(role, "");
        message.content = blocks(&m["content"]);
        message.native = native_fields(BEDROCK, rest_of(m, &["role", "content"]));
        chat.messages.push(message);
    }
    if !read_config(&v["inferenceConfig"], &mut chat) {
        native.insert("inferenceConfig".into(), v["inferenceConfig"].clone());
    }
    if !read_tools(&v["toolConfig"], &mut chat) {
        native.insert("toolConfig".into(), v["toolConfig"].clone());
    }
    match read_format(&v["outputConfig"]) {
        Some(format) => chat.options.response_format = format,
        None => _ = native.insert("outputConfig".into(), v["outputConfig"].clone()),
    }
    let mut fields = v["additionalModelRequestFields"].clone();
    if let Some(reasoning) = fields.as_object_mut().and_then(take_thinking) {
        chat.options.reasoning = Some(reasoning);
    }
    if fields.as_object().is_some_and(|f| !f.is_empty()) {
        native.insert("additionalModelRequestFields".into(), fields);
    }
    native.retain(|_, v| !v.is_null());
    chat.native = native_fields(BEDROCK, native);
    chat
}

/// The kind's reasoning of Anthropic's `thinking`, as Converse carries it
/// for Claude, if it is exactly an enabled budget or disabled: then it
/// leaves the fields.
fn take_thinking(fields: &mut Map<String, Value>) -> Option<Reasoning> {
    let t = fields.get("thinking")?;
    let keys = |keys: &[&str]| {
        t.as_object()
            .is_some_and(|m| m.keys().all(|k| keys.contains(&k.as_str())))
    };
    let reasoning = match t["type"].as_str()? {
        "enabled" if keys(&["type", "budget_tokens"]) => {
            Reasoning::budget(t["budget_tokens"].as_u64()?)
        }
        "disabled" if keys(&["type"]) => Reasoning::effort(Effort::Off),
        _ => return None,
    };
    fields.shift_remove("thinking");
    Some(reasoning)
}

/// `inferenceConfig` as the kind's options; `false` if it says what they
/// cannot.
fn read_config(v: &Value, chat: &mut Input) -> bool {
    if v.is_null() {
        return true;
    }
    let known = ["maxTokens", "temperature", "topP", "stopSequences"];
    if !unsupported_fields(v, &known, &[]).is_empty() {
        return false;
    }
    let o = &mut chat.options;
    o.max_output_tokens = v["maxTokens"].as_u64();
    o.temperature = v["temperature"].as_f64();
    o.top_p = v["topP"].as_f64();
    let stops = v["stopSequences"].as_array().into_iter().flatten();
    o.stop = stops.filter_map(|s| s.as_str().map(Into::into)).collect();
    true
}

/// `toolConfig` as the kind's tools and choice; `false` if it says what
/// they cannot, such as a tool of another type.
fn read_tools(v: &Value, chat: &mut Input) -> bool {
    if v.is_null() {
        return true;
    }
    let mut tools: Vec<Tool> = Vec::new();
    for entry in v["tools"].as_array().into_iter().flatten() {
        if entry.get("cachePoint").is_some() {
            match tools.last_mut() {
                Some(tool) => mark(&mut tool.native),
                None => return false,
            }
            continue;
        }
        let spec = &entry["toolSpec"];
        let (Some(name), Some(schema)) = (spec["name"].as_str(), spec["inputSchema"].get("json"))
        else {
            return false;
        };
        let mut tool = Tool::new(name, schema.clone());
        tool.description = spec["description"].as_str().map(str::to_owned);
        let rest = rest_of(spec, &["name", "description", "inputSchema"]);
        tool.native = native_fields(BEDROCK, rest);
        tools.push(tool);
    }
    let choice = &v["toolChoice"];
    let choice = match () {
        _ if choice.is_null() || choice.get("auto").is_some() => ToolChoice::Auto,
        _ if choice.get("any").is_some() => ToolChoice::Required,
        _ => match choice["tool"]["name"].as_str() {
            Some(name) => ToolChoice::Tool(name.into()),
            None => return false,
        },
    };
    if !unsupported_fields(v, &["tools", "toolChoice"], &[]).is_empty() {
        return false;
    }
    chat.tools = tools;
    chat.options.tool_choice = Some(choice);
    true
}

/// `outputConfig` as the kind's response format: none, or a JSON schema;
/// `None` if it says what the kind cannot.
fn read_format(v: &Value) -> Option<Option<ResponseFormat>> {
    if v.is_null() {
        return Some(None);
    }
    let format = &v["textFormat"];
    let schema = &format["structure"]["jsonSchema"];
    let only = |v: &Value, keys: &[&str]| unsupported_fields(v, keys, &[]).is_empty();
    let plain = only(v, &["textFormat"])
        && only(format, &["type", "structure"])
        && format["type"] == "json_schema";
    if !plain {
        return None;
    }
    let parsed: Value = serde_json::from_str(schema["schema"].as_str()?).ok()?;
    Some(Some(ResponseFormat::JsonSchema {
        name: schema["name"].as_str().unwrap_or("response").to_owned(),
        description: schema["description"].as_str().map(str::to_owned),
        schema: parsed,
        strict: None,
    }))
}

/// Content blocks as the kind's parts, a `cachePoint` marking the one
/// before it.
fn blocks(v: &Value) -> Vec<Content> {
    let mut parts: Vec<Content> = Vec::new();
    for b in v.as_array().into_iter().flatten() {
        if b.get("cachePoint").is_some()
            && let Some(native) = parts.last_mut().and_then(native_of)
        {
            mark(native);
            continue;
        }
        parts.push(block(b).unwrap_or_else(|| Content::native(BEDROCK, b.clone())));
    }
    parts
}

/// Marks a part's fields with a cache marker.
fn mark(native: &mut Option<NativeFields>) {
    let fields = native.get_or_insert_with(|| NativeFields::new(BEDROCK, Map::new()));
    if *fields.format == *BEDROCK {
        let marker = json!({ "type": "ephemeral" });
        fields.fields.insert(CACHE_MARKER.into(), marker);
    }
}

/// The fields of a part a cache marker can go on.
fn native_of(part: &mut Content) -> Option<&mut Option<NativeFields>> {
    match part {
        Content::Text { native, .. }
        | Content::ToolCall { native, .. }
        | Content::ToolResult { native, .. } => Some(native),
        Content::Image(media) | Content::Document(media) => Some(&mut media.native),
        _ => None,
    }
}

/// A content block as the kind's part, if it can say all of it.
fn block(b: &Value) -> Option<Content> {
    let only = |keys: &[&str]| unsupported_fields(b, keys, &[]).is_empty();
    if let Some(text) = b["text"].as_str() {
        return only(&["text"]).then(|| Content::from(text));
    }
    if !b.as_object().is_some_and(|o| o.len() == 1) {
        return None;
    }
    if let Some(image) = b.get("image") {
        let mime = match image["format"].as_str()? {
            "jpeg" | "jpg" => "image/jpeg".to_owned(),
            format => format!("image/{format}"),
        };
        return Some(Content::Image(media(image, mime)?));
    }
    if let Some(document) = b.get("document") {
        let mime = document_mime(document["format"].as_str()?)?;
        let mut media = media(document, mime.to_owned())?;
        media.name = document["name"].as_str().map(str::to_owned);
        return Some(Content::Document(media));
    }
    if let Some(call) = b.get("toolUse") {
        let (id, name) = (call["toolUseId"].as_str()?, call["name"].as_str()?);
        let input = call["input"].as_object()?;
        let mut part = Content::tool_call(id, name, Value::Object(input.clone()).to_string());
        if let Content::ToolCall { native, .. } = &mut part {
            *native = native_fields(BEDROCK, rest_of(call, &["toolUseId", "name", "input"]));
        }
        return Some(part);
    }
    if let Some(result) = b.get("toolResult") {
        let id = result["toolUseId"].as_str()?;
        let is_error = match result["status"].as_str() {
            None | Some("success") => false,
            Some("error") => true,
            Some(_) => return None,
        };
        let said = result["content"].as_array().into_iter().flatten();
        let content = said
            .map(|c| match c.get("json") {
                Some(json) if c.as_object().is_some_and(|o| o.len() == 1) => {
                    Some(Content::from(json.to_string().as_str()))
                }
                _ => block(c),
            })
            .collect::<Option<Vec<_>>>()?;
        let mut part = Content::tool_result(id, content, is_error);
        if let Content::ToolResult { native, .. } = &mut part {
            let rest = rest_of(result, &["toolUseId", "content", "status"]);
            *native = native_fields(BEDROCK, rest);
        }
        return Some(part);
    }
    if let Some(r) = b.get("reasoningContent") {
        let mut fields = Map::new();
        let text = match (&r["reasoningText"], &r["redactedContent"]) {
            (said @ Value::Object(_), Value::Null) => {
                if let Some(signature) = said.get("signature") {
                    fields.insert("signature".into(), signature.clone());
                }
                said["text"].as_str()?.to_owned()
            }
            (Value::Null, redacted @ Value::String(_)) => {
                fields.insert("redactedContent".into(), redacted.clone());
                String::new()
            }
            _ => return None,
        };
        return Some(Content::Reasoning {
            text,
            native: native_fields(BEDROCK, fields),
        });
    }
    None
}

/// Media of a block in bytes; `None` for one in S3, which stays native.
fn media(v: &Value, mime: String) -> Option<Media> {
    let bytes = STANDARD.decode(v["source"]["bytes"].as_str()?).ok()?;
    let known = ["format", "source", "name"];
    if !unsupported_fields(v, &known, &[]).is_empty() {
        return None;
    }
    Some(Media::new(mime, MediaSource::Bytes(Bytes::from(bytes))))
}

/// A document format's media type.
fn document_mime(format: &str) -> Option<&'static str> {
    Some(match format {
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "html" => "text/html",
        "txt" => "text/plain",
        "md" => "text/markdown",
        _ => return None,
    })
}
