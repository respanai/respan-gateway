//! What a request says beyond text, in Anthropic's terms: tools and their
//! choice, tool blocks, media and reasoning, and structured output. What
//! the kind has no form for, such as a tool the provider runs, stays in the
//! `native` of the request, the tool or the part.

use crate::schema::{
    EFFORT_BUDGETS, arguments_object, base64_field, decode_content, decode_tools, each_tool,
    edit_native, effort_for, encode_content, merge_missing, only, read_search, remainder,
    required_str, rest_of, set, take_native, untranslatable, write_search,
};
use crate::schema::{Own, Schema};
use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Effort, Input, Media, MediaSource, Message, ProviderTool, Reasoning, ResponseFormat,
    Tool, ToolChoice,
};
use respan_gateway_core::Fault;
use respan_gateway_core::Native;
use serde_json::{Map, Value, json};

use crate::anthropic::AnthropicMessages;
use crate::anthropic::output_schema::output_schema;

/// Anthropic's custom tools and choice, with whether calls may be
/// parallel, as the kind's.
pub(crate) fn read_tools_of(v: &Value, chat: &mut Input) {
    let tools = each_tool(&v["tools"], anthropic_tool, web_search);
    let choice = match &v["tool_choice"] {
        Value::Null => Some((None, None)),
        choice => anthropic_choice(choice),
    };
    let (choice, parallel) = choice.unzip();
    decode_tools::<AnthropicMessages>(chat, tools, choice, parallel, ["tools", "tool_choice", ""]);
}

fn anthropic_tool(tool: &Value) -> Option<Tool> {
    let custom = tool["type"].is_null() || tool["type"] == "custom";
    if !custom {
        return None;
    }
    let description = match &tool["description"] {
        Value::Null => None,
        text => Some(text.as_str()?.to_owned()),
    };
    let mut kind = Tool::new(tool["name"].as_str()?, tool["input_schema"].clone());
    kind.description = description;
    let rest = rest_of(tool, &["type", "name", "description", "input_schema"]);
    kind.native = AnthropicMessages::native(rest);
    Some(kind)
}

/// Anthropic's web search tool, of any dated `type`.
fn web_search(tool: &Value) -> Option<ProviderTool> {
    let dated = tool["type"].as_str()?.starts_with("web_search_");
    let fields = rest_of(tool, &["name"]);
    (dated && tool["name"] == "web_search").then(|| read_search::<AnthropicMessages>(fields))
}

/// Anthropic's `tool_choice`, and whether calls may be parallel if it says.
fn anthropic_choice(choice: &Value) -> Option<(Option<ToolChoice>, Option<bool>)> {
    if !only(choice, &["type", "name", "disable_parallel_tool_use"]) {
        return None;
    }
    let kind = match (choice["type"].as_str()?, choice["name"].as_str()) {
        ("auto", None) => ToolChoice::Auto,
        ("none", None) => ToolChoice::None,
        ("any", None) => ToolChoice::Required,
        ("tool", Some(name)) => ToolChoice::Tool(name.into()),
        _ => return None,
    };
    let parallel = match &choice["disable_parallel_tool_use"] {
        Value::Null => None,
        disable => Some(!disable.as_bool()?),
    };
    Some((Some(kind), parallel))
}

/// The kind's tools as Anthropic's fields of a request `v`. Calls one at a
/// time are said inside `tool_choice`.
pub(crate) fn write_tools(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    let tool = |t: &Tool| {
        let mut tool = Map::new();
        tool.insert("name".into(), t.name.as_str().into());
        if let Some(text) = &t.description {
            tool.insert("description".into(), text.as_str().into());
        }
        let schema = match t.parameters.is_object() {
            true => t.parameters.clone(),
            false => json!({ "type": "object" }),
        };
        tool.insert("input_schema".into(), schema);
        tool.extend(AnthropicMessages::fields(&t.native).clone());
        Value::Object(tool)
    };
    let mut tools: Vec<Value> = chat.tools.iter().map(tool).collect();
    for provided in &chat.provider_tools {
        let mut search = Map::new();
        search.insert("type".into(), "web_search_20250305".into());
        search.insert("name".into(), "web_search".into());
        search.extend(write_search::<AnthropicMessages>(provided)?);
        tools.push(search.into());
    }
    set(v, "tools", (!tools.is_empty()).then(|| tools.into()));
    let one_at_a_time = chat.options.parallel_tool_calls == Some(false);
    let mut choice = match &chat.options.tool_choice {
        Some(ToolChoice::Auto) => Some(json!({ "type": "auto" })),
        Some(ToolChoice::None) => Some(json!({ "type": "none" })),
        Some(ToolChoice::Required) => Some(json!({ "type": "any" })),
        Some(ToolChoice::Tool(name)) => Some(json!({ "type": "tool", "name": name })),
        None => one_at_a_time.then(|| json!({ "type": "auto" })),
        Some(_) => return Err(untranslatable("a tool choice this format has no form for")),
    };
    if let Some(c) = choice
        .as_mut()
        .filter(|c| one_at_a_time && c["type"] != "none")
    {
        c["disable_parallel_tool_use"] = true.into();
    }
    set(v, "tool_choice", choice);
    Ok(())
}

/// `m`, read from `content`, with each signed thinking's signature after it.
pub(crate) fn signed(m: &mut Message, content: &Value) {
    let Some(blocks) = content.as_array() else {
        return;
    };
    let parts = std::mem::take(&mut m.content);
    for (part, b) in parts.into_iter().zip(blocks) {
        m.content.push(part);
        if let Some(s) = b["signature"].as_str().filter(|_| b["type"] == "thinking") {
            m.content.push(Content::Signature(s.into()));
        }
    }
}

/// A part the kind has a form for: an image or a document, reasoning, or a
/// tool block, `None` for one it has no form for.
pub(crate) fn decode_part(p: &Value) -> Result<Option<Content>, Fault> {
    Ok(match p["type"].as_str() {
        Some("image" | "document") => read_media(p)?,
        Some("thinking") => p["thinking"].as_str().map(|text| Content::Reasoning {
            text: text.into(),
            native: AnthropicMessages::native(remainder(p, &["type", "thinking", "signature"])),
        }),
        Some("redacted_thinking") => p["data"]
            .as_str()
            .map(|data| Content::Withheld(data.into())),
        _ => tool_part(p).transpose()?,
    })
}

/// A part of the kind's as Anthropic writes it.
pub(crate) fn encode_part(c: &Content) -> Result<Value, Fault> {
    match c {
        Content::Image(media) => write_media("image", media),
        Content::Document(media) => write_media("document", media),
        Content::ToolCall { .. } | Content::ToolResult { .. } => tool_block(c),
        Content::Withheld(data) => Ok(json!({ "type": "redacted_thinking", "data": data })),
        // WARN: audio and other parts have no form in Anthropic; dropped.
        _ => Ok(Value::Null),
    }
}

/// The one media type Anthropic takes a document's plain text in.
const PLAIN_TEXT: &str = "text/plain";

/// An image or document block, if the kind can say its source: its other
/// fields are its media's `native`. One whose source misses its data, or
/// whose base64 does not decode, is refused.
fn read_media(v: &Value) -> Result<Option<Content>, Fault> {
    if v.get("title").is_some_and(|v| !v.is_string()) {
        return Ok(None);
    }
    let source = &v["source"];
    let (mut mime, src, known) = match source["type"].as_str() {
        Some("base64") => {
            let mime = required_str(&source["media_type"], "`source.media_type`")?;
            let bytes = base64_field(&source["data"], "`source.data`")?;
            let src = MediaSource::Bytes(bytes.into());
            (mime.to_owned(), src, &["type", "media_type", "data"][..])
        }
        Some("url") => {
            let url = required_str(&source["url"], "`source.url`")?;
            let src = MediaSource::Url(url.into());
            ("image/*".to_owned(), src, &["type", "url"][..])
        }
        Some("text") if v["type"] == "document" => {
            let mime = required_str(&source["media_type"], "`source.media_type`")?;
            let text = required_str(&source["data"], "`source.data`")?;
            let src = MediaSource::Bytes(text.as_bytes().to_vec().into());
            (mime.to_owned(), src, &["type", "media_type", "data"][..])
        }
        _ => return Ok(None),
    };
    let image = v["type"] == "image";
    if !image && mime == "image/*" {
        mime = "application/pdf".into();
    }
    let mut fields = remainder(v, &["type", "source", "title"]);
    let nested = remainder(source, known);
    if !nested.is_empty() {
        fields.insert("source".into(), nested.into());
    }
    let mut media = Media::new(mime, src);
    media.name = v["title"].as_str().map(str::to_owned);
    media.native = AnthropicMessages::native(fields);
    Ok(Some(match image {
        true => Content::Image(media),
        false => Content::Document(media),
    }))
}

/// An image or document block of `media`; an image's display name, which
/// Anthropic has no field for, is dropped.
fn write_media(kind: &str, media: &Media) -> Result<Value, Fault> {
    let native = AnthropicMessages::fields(&media.native).clone();
    let source = match &media.source {
        MediaSource::Url(url) => json!({"type":"url","url":url}),
        MediaSource::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) if kind == "document" && media.mime_type.starts_with("text/") => {
                json!({"type":"text","media_type":PLAIN_TEXT,"data":text})
            }
            _ => {
                json!({"type":"base64","media_type":media.mime_type,"data":STANDARD.encode(bytes)})
            }
        },
    };
    let mut v = json!({"type":kind,"source":source});
    if let Some(name) = media.name.as_ref().filter(|_| kind == "document") {
        v["title"] = name.clone().into();
    }
    merge_missing(&mut v, native);
    Ok(v)
}

/// An Anthropic `tool_use` or `tool_result` block as the kind's part, if it
/// can say all of it. A field it has no place for, other than a cache marker,
/// on the block or on a text block of a result, keeps it the format's own:
/// translating it is refused, not dropped.
fn tool_part(block: &Value) -> Option<Result<Content, Fault>> {
    match block["type"].as_str()? {
        "tool_use" => {
            let input = block["input"].as_object()?;
            let arguments = Value::Object(input.clone()).to_string();
            let (id, name) = (block["id"].as_str()?, block["name"].as_str()?);
            let mut part = Content::tool_call(id, name, arguments);
            if let Content::ToolCall { native, .. } = &mut part {
                let rest = rest_of(block, &["type", "id", "name", "input"]);
                *native = AnthropicMessages::native(rest);
            }
            Some(Ok(part))
        }
        "tool_result" => {
            let is_error = match &block["is_error"] {
                Value::Null => false,
                is_error => is_error.as_bool()?,
            };
            let blocks = block["content"].as_array().into_iter().flatten();
            let marked =
                |b: &Value| b["type"] == "text" && !only(b, &["type", "text", "cache_control"]);
            if blocks.into_iter().any(marked) {
                return None;
            }
            let id = block["tool_use_id"].as_str()?;
            let content = match decode_content::<AnthropicMessages>(&block["content"]) {
                Ok(content) => content,
                Err(fault) => return Some(Err(fault)),
            };
            let mut part = Content::tool_result(id, content, is_error);
            if let Content::ToolResult { native, .. } = &mut part {
                let rest = rest_of(block, &["type", "tool_use_id", "content", "is_error"]);
                *native = AnthropicMessages::native(rest);
            }
            Some(Ok(part))
        }
        _ => None,
    }
}

/// Write the fields of a tool part's `native` into its Anthropic `block`:
/// another format's are dropped.
fn block_fields(block: &mut Value, native: &Native) {
    let fields = AnthropicMessages::fields(native).clone();
    if let Some(block) = block.as_object_mut() {
        block.extend(fields);
    }
}

/// The kind's tool call or result as an Anthropic block.
fn tool_block(part: &Content) -> Result<Value, Fault> {
    match part {
        Content::ToolCall {
            id,
            name,
            arguments,
            native,
            ..
        } => {
            let input = arguments_object(arguments)?;
            let mut block = json!({ "type": "tool_use", "id": id, "name": name, "input": input });
            block_fields(&mut block, native);
            Ok(block)
        }
        Content::ToolResult {
            id,
            content,
            is_error,
            native,
            ..
        } => {
            let mut block = json!({ "type": "tool_result", "tool_use_id": id });
            block_fields(&mut block, native);
            if !content.is_empty() {
                block["content"] = encode_content::<AnthropicMessages>(content)?;
            }
            if *is_error {
                block["is_error"] = true.into();
            }
            Ok(block)
        }
        _ => Err(untranslatable("an unsupported kind of part")),
    }
}

/// The structured output requirement of `output_config.format`, if the
/// kind can say all of it: then it leaves the request's `native`.
pub(crate) fn read_output(v: &Value, input: &mut Input) {
    let spec = &v["output_config"]["format"];
    let clean = spec
        .as_object()
        .is_some_and(|m| m.keys().all(|k| ["type", "schema"].contains(&k.as_str())));
    if spec["type"] != "json_schema" || !clean || !spec["schema"].is_object() {
        return;
    }
    // Anthropic always constrains the result to the schema.
    input.options.response_format = Some(ResponseFormat::JsonSchema {
        name: "response".into(),
        description: None,
        schema: spec["schema"].clone(),
    });
    edit_native::<AnthropicMessages, _>(&mut input.native, |native| {
        if let Some(Value::Object(config)) = native.get_mut("output_config") {
            config.remove("format");
            if config.is_empty() {
                native.remove("output_config");
            }
        }
    });
}

/// The kind's effort of `output_config.effort`, if it is one the kind names:
/// then it leaves the request's `native`.
pub(crate) fn read_effort(input: &mut Input) {
    let named = |effort: &Value| match effort.as_str()? {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::XHigh),
        _ => None,
    };
    let config = AnthropicMessages::fields(&input.native).get("output_config");
    let Some(effort) = config.and_then(|c| c.get("effort")).and_then(named) else {
        return;
    };
    edit_native::<AnthropicMessages, _>(&mut input.native, |native| {
        let config = native
            .get_mut("output_config")
            .and_then(Value::as_object_mut);
        config.map(|c| c.remove("effort"));
        let config = native.get("output_config").and_then(Value::as_object);
        if config.is_some_and(Map::is_empty) {
            native.shift_remove("output_config");
        }
    });
    // Beside a thinking budget, as Claude Opus 4.5 takes both.
    let reasoning = input
        .options
        .reasoning
        .get_or_insert_with(Reasoning::default);
    reasoning.effort = Some(effort);
}

/// Anthropic's name of a named `effort`: its `output_config.effort`. `Off`
/// is `thinking`'s, not an effort.
pub(crate) fn effort_name(effort: Effort) -> Result<Option<&'static str>, Fault> {
    Ok(match effort {
        Effort::Off => None,
        Effort::Minimal | Effort::Low => Some("low"),
        Effort::Medium => Some("medium"),
        Effort::High => Some("high"),
        Effort::XHigh => Some("xhigh"),
        _ => {
            return Err(untranslatable(
                "a reasoning effort Anthropic has no level for",
            ));
        }
    })
}

/// The request's own `output_config`, with the format the kind's structured
/// output requirement says and the effort its reasoning names.
pub(crate) fn write_output(input: &Input, out: &mut Map<String, Value>) -> Result<(), Fault> {
    // WARN: an `output_config` that is no object is dropped.
    let config = AnthropicMessages::fields(&input.native)
        .get("output_config")
        .cloned();
    let mut config = config.filter(Value::is_object);
    let effort = input.options.reasoning.as_ref().and_then(|r| r.effort);
    if let Some(effort) = effort.map(effort_name).transpose()?.flatten() {
        config.get_or_insert_with(|| json!({}))["effort"] = effort.into();
    }
    // A JSON object without a schema has no format here: the prompt asks
    // for JSON. A schema wrapper's description and strictness have no field,
    // and Anthropic always holds the answer to the schema.
    let schema = match &input.options.response_format {
        None | Some(ResponseFormat::Text | ResponseFormat::JsonObject) => None,
        Some(ResponseFormat::JsonSchema { schema, .. }) => Some(output_schema(schema)),
    };
    if let Some(schema) = schema {
        config.get_or_insert_with(|| json!({}))["format"] =
            json!({"type":"json_schema","schema":schema});
    }
    set(out, "output_config", config);
    Ok(())
}

/// The kind's reasoning of `thinking`, if it is exactly an enabled budget or
/// disabled: then it leaves the request's `native`.
pub(crate) fn read_thinking(input: &mut Input) {
    input.options.reasoning =
        take_native::<AnthropicMessages, _>(&mut input.native, "thinking", |t| {
            let only_keys = |keys: &[&str]| {
                t.as_object()
                    .is_some_and(|m| m.keys().all(|k| keys.contains(&k.as_str())))
            };
            match t["type"].as_str()? {
                "enabled" if only_keys(&["type", "budget_tokens"]) => {
                    Some(Reasoning::budget(t["budget_tokens"].as_u64()?))
                }
                "disabled" if only_keys(&["type"]) => Some(Reasoning::effort(Effort::Off)),
                _ => None,
            }
        });
}

/// How a Claude model thinks.
enum Thinks {
    /// Adaptively, to an effort: Claude 4.6 and later.
    Adaptively,
    /// To a token budget: Claude 3.7 to 4.5.
    ToBudget,
    /// Not at all: Claude 3 but 3.7.
    Never,
}

fn thinks(model: &str) -> Thinks {
    let budgeted = [
        "-4-0", "-4.0", "-4-1", "-4.1", "-4-5", "-4.5", "-4-2025", "-4@",
    ];
    let family = |family: &str| {
        model.ends_with(&format!("{family}-4"))
            || budgeted
                .iter()
                .any(|v| model.contains(&format!("{family}{v}")))
    };
    if model.contains("claude-3-7") || model.contains("3-7-sonnet") {
        Thinks::ToBudget
    } else if model.contains("claude-3") {
        Thinks::Never
    } else if ["opus", "sonnet", "haiku"].iter().any(|f| family(f)) {
        Thinks::ToBudget
    } else {
        Thinks::Adaptively
    }
}

/// Whether Claude `model` is of a `family` at `from` or later, as
/// `claude-opus-5-5` is Opus 5.5; a dated model is of its major's `.0`.
fn at_least(model: &str, (family, from): &(&str, (u32, u32))) -> bool {
    fn number(s: &str) -> Option<(u32, &str)> {
        let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let n = s[..digits].parse::<u32>().ok().filter(|_| digits < 3)?;
        Some((n, &s[digits..]))
    }
    let Some((major, rest)) = model
        .split_once(&format!("{family}-"))
        .and_then(|(_, r)| number(r))
    else {
        return false;
    };
    let minor = rest.strip_prefix(['-', '.']).and_then(number);
    (major, minor.map_or(0, |(minor, _)| minor)) >= *from
}

/// Whether Claude `model` takes a forced tool choice: Opus and Sonnet 5.5,
/// and Fable and Mythos 5.1, and later refuse one.
fn forces_tools(model: &str) -> bool {
    let refusing = [
        ("opus", (5, 5)),
        ("sonnet", (5, 5)),
        ("fable", (5, 1)),
        ("mythos", (5, 1)),
    ];
    !refusing.iter().any(|f| at_least(model, f))
}

/// Whether Claude `model` takes `thinking: {type: "disabled"}`: Opus and
/// Sonnet 5.5, Fable and Mythos, and later refuse it.
fn disables_thinking(model: &str) -> bool {
    let refusing = [
        ("opus", (5, 5)),
        ("sonnet", (5, 5)),
        ("fable", (5, 0)),
        ("mythos", (5, 0)),
    ];
    !refusing.iter().any(|f| at_least(model, f))
}

/// The kind of a block or tool choice in Anthropic's terms (`type`) or
/// Converse's (its one key).
fn kind(b: &Value) -> Option<&str> {
    let mut keys = b.as_object()?.keys();
    b["type"]
        .as_str()
        .or_else(|| keys.next().map(String::as_str))
}

/// Whether the call `v` can think: Anthropic refuses thinking beside a
/// temperature but 1, a `top_k`, a `top_p` under 0.95, a forced tool
/// (`any` or `tool`), a last turn of the assistant's to continue, and a
/// tool loop's turn of the assistant's that does not open with its thinking.
/// Its messages may be Anthropic's or Converse's.
fn can_think(v: &Map<String, Value>) -> bool {
    let number = |key: &str| v.get(key).and_then(Value::as_f64);
    let forced = matches!(v.get("tool_choice").and_then(kind), Some("any" | "tool"));
    if number("temperature").is_some_and(|t| t != 1.0)
        || number("top_p").is_some_and(|p| p < 0.95)
        || v.contains_key("top_k")
        || forced
    {
        return false;
    }
    let messages = v.get("messages").and_then(Value::as_array);
    let messages = messages.map(Vec::as_slice).unwrap_or_default();
    let blocks = |m: &Value| m["content"].as_array().cloned().unwrap_or_default();
    let result = |b: &Value| matches!(kind(b), Some("tool_result" | "toolResult"));
    // The turn in progress: what follows the user's last message of its own.
    let of_user = |m: &Value| m["role"] == "user" && !blocks(m).iter().any(result);
    let turn = messages.iter().rposition(of_user).map_or(0, |at| at + 1);
    let turn = &messages[turn..];
    let assistant = |m: &&Value| m["role"] == "assistant";
    let opens_with_thinking = |m: &Value| {
        let first = blocks(m).first().cloned().unwrap_or_default();
        matches!(
            kind(&first),
            Some("thinking" | "redacted_thinking" | "reasoningContent")
        )
    };
    turn.last().is_none_or(|last| last["role"] != "assistant")
        && turn.iter().filter(assistant).all(opens_with_thinking)
}

/// A Claude call whose reasoning is an effort alone (`output_config.effort`)
/// and no `thinking`, as a client of another format names it: beside the
/// effort, adaptive thinking on Claude 4.6 and later; in its place, a
/// thinking budget on Claude 3.7 to 4.5, `max_tokens` above it; on another
/// Claude 3, neither. A call that cannot think ([`can_think`]) keeps the
/// effort alone on Claude 4.6 and later, and has none on the others. Another
/// model's call stays as written.
pub(crate) fn effort_thinking(v: &mut Map<String, Value>) {
    let model = v.get("model").and_then(Value::as_str).unwrap_or_default();
    let model = model.to_ascii_lowercase();
    if !model.contains("claude") || v.contains_key("thinking") {
        return;
    }
    let effort = v.get("output_config").and_then(|c| c.get("effort"));
    let Some(effort) = effort.and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let thinks = thinks(&model);
    let thinking = match (&thinks, can_think(v)) {
        (Thinks::Adaptively, true) => Some(json!({ "type": "adaptive" })),
        (Thinks::ToBudget, true) => {
            let Some(&(_, budget)) = EFFORT_BUDGETS.iter().find(|(e, _)| *e == effort) else {
                return;
            };
            let limit = v.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
            if limit <= budget {
                let limit = budget.saturating_add(crate::anthropic::DEFAULT_MAX_TOKENS);
                v.insert("max_tokens".into(), limit.into());
            }
            Some(json!({ "type": "enabled", "budget_tokens": budget }))
        }
        _ => None,
    };
    if !matches!(thinks, Thinks::Adaptively)
        && let Some(Value::Object(config)) = v.get_mut("output_config")
    {
        config.remove("effort");
        if config.is_empty() {
            v.remove("output_config");
        }
    }
    if let Some(thinking) = thinking {
        v.insert("thinking".into(), thinking);
    }
}

/// The Claude call `v` with only the thinking and tool choice its model
/// takes, however they were set. Claude 4.6 and later think adaptively, to
/// an effort: a budget there is adaptive thinking at the nearest effort
/// ([`effort_for`]), unless the call names one, and `disabled`, where the
/// model refuses it, no `thinking` at the least effort. A model that refuses
/// to force a tool chooses `auto`. Beside a forced tool, on a model that
/// thinks to a budget in a call that cannot think ([`can_think`]), and on a
/// model that never thinks, there is no `thinking`. Another model's call
/// stays as written.
pub(crate) fn fit_thinking(v: &mut Map<String, Value>) {
    let model = v.get("model").and_then(Value::as_str).unwrap_or_default();
    let model = model.to_ascii_lowercase();
    if !model.contains("claude") {
        return;
    }
    let choice = v.get_mut("tool_choice").and_then(Value::as_object_mut);
    let forced = choice.filter(|c| matches!(c["type"].as_str(), Some("any" | "tool")));
    let forced = match forced {
        Some(choice) if !forces_tools(&model) => {
            // WARN: a forced tool choice this Claude refuses is `auto`.
            choice.insert("type".into(), "auto".into());
            choice.remove("name");
            false
        }
        forced => forced.is_some(),
    };
    let thinks = thinks(&model);
    let thinking = v.get("thinking").cloned().unwrap_or_default();
    // WARN: a thinking budget, and `disabled` where it is refused, are the
    // nearest effort on Claude 4.6 and later, thinking adaptively for a budget.
    let nearest = match (&thinks, thinking["type"].as_str()) {
        (Thinks::Adaptively, Some("enabled")) => {
            v.insert("thinking".into(), json!({ "type": "adaptive" }));
            Some(effort_for(thinking["budget_tokens"].as_u64().unwrap_or(0)))
        }
        (Thinks::Adaptively, Some("disabled")) if !disables_thinking(&model) => {
            v.remove("thinking");
            Some(Effort::Low)
        }
        _ => None,
    };
    if let Some(effort) = nearest.and_then(|e| effort_name(e).ok().flatten()) {
        let config = v.entry("output_config").or_insert_with(|| json!({}));
        if let Some(config) = config.as_object_mut() {
            config.entry("effort").or_insert(effort.into());
        }
    }
    let refused = match thinks {
        Thinks::Adaptively => forced,
        Thinks::ToBudget => !can_think(v),
        Thinks::Never => true,
    };
    if refused && v.get("thinking").is_some_and(|t| t["type"] != "disabled") {
        // WARN: thinking Claude refuses in this call is dropped.
        v.remove("thinking");
    }
}

/// The request's `thinking`: the kind's reasoning, else the request's own.
/// Anthropic thinks to a token budget here, its effort is `output_config`'s
/// ([`write_output`]), and it returns its thinking whenever it thinks; what
/// a model refuses of it [`fit_thinking`] fits.
pub(crate) fn write_thinking(input: &Input) -> Option<Value> {
    let own = || {
        AnthropicMessages::fields(&input.native)
            .get("thinking")
            .cloned()
    };
    // WARN: an ask to withhold the thinking is dropped.
    let Some(r) = &input.options.reasoning else {
        return own();
    };
    match (r.effort, r.budget_tokens) {
        (Some(Effort::Off), _) => Some(json!({ "type": "disabled" })),
        // WARN: a budget under Anthropic's least, 1024, is raised to it.
        (_, Some(budget)) => Some(json!({ "type": "enabled", "budget_tokens": budget.max(1024) })),
        (_, None) => own(),
    }
}
