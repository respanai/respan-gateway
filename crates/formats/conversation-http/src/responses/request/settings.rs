//! The rest of a Responses request: tools and choice, text format, parts and
//! reasoning.

use super::super::RESPONSES;
use super::{read::read_media, write::write_media};
use crate::schema::{
    INERT, Keep, NativeFieldsExt, decode_tools, each_tool, effort_name, kept_fields, native_fields,
    only, own_native, parse_effort, read_search, reasoning_of, rest_of, set, tool_field,
    untranslatable, write_search,
};
use respan_gateway_conversation::{
    Content, Input, ProviderTool, Reasoning, ResponseFormat, Tool, ToolChoice,
};
use respan_gateway_core::Fault;
use serde_json::{Map, Value, json};

// -------------------------------------------------------------------- tools

/// Function tools, the choice and whether calls may be parallel, as the
/// kind's.
pub(in crate::responses) fn read_tools(v: &Value, chat: &mut Input) {
    let tools = each_tool(&v["tools"], function_tool, web_search);
    let choice = match &v["tool_choice"] {
        Value::Null => Some(None),
        choice => read_choice(choice).map(Some),
    };
    let parallel = match &v["parallel_tool_calls"] {
        Value::Null => Some(None),
        parallel => parallel.as_bool().map(Some),
    };
    let keys = ["tools", "tool_choice", "parallel_tool_calls"];
    decode_tools(chat, tools, choice, parallel, keys);
}

fn function_tool(tool: &Value) -> Option<Tool> {
    if tool["type"] != "function" {
        return None;
    }
    let description = match &tool["description"] {
        Value::Null => None,
        text => Some(text.as_str()?.to_owned()),
    };
    let mut kind = Tool::new(tool["name"].as_str()?, tool["parameters"].clone());
    kind.description = description;
    let mut fields = rest_of(tool, &["type", "name", "description", "parameters"]);
    // Strict is on unless it is said; no other format has that default.
    if !tool.as_object()?.contains_key("strict") {
        fields.insert(INERT.into(), json!({ "strict": "unsaid" }));
    }
    kind.native = native_fields(RESPONSES, fields);
    Some(kind)
}

/// A web search tool, of any dated or preview `type`.
fn web_search(tool: &Value) -> Option<ProviderTool> {
    let kind = tool["type"].as_str()?;
    let search = kind == "web_search" || kind.starts_with("web_search_");
    let hints = ["type", "search_context_size"];
    (search && kind != "web_search_call")
        .then(|| read_search(RESPONSES, rest_of(tool, &[]), &hints))
}

fn read_choice(choice: &Value) -> Option<ToolChoice> {
    Some(match choice {
        Value::String(choice) => match choice.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => return None,
        },
        choice if choice["type"] == "function" && only(choice, &["type", "name"]) => {
            ToolChoice::Tool(choice["name"].as_str()?.into())
        }
        _ => return None,
    })
}

/// The kind's tools, choice and parallelism as the request's fields.
pub(in crate::responses) fn write_tools(
    chat: &Input,
    v: &mut Map<String, Value>,
) -> Result<(), Fault> {
    let tool = |t: &Tool| -> Result<Value, Fault> {
        let mut tool = Map::new();
        tool.insert("type".into(), "function".into());
        tool.insert("name".into(), t.name.as_str().into());
        if let Some(text) = &t.description {
            tool.insert("description".into(), text.as_str().into());
        }
        tool.insert("parameters".into(), t.parameters.clone());
        // Chat Completions' `strict` is Responses' own: OpenAI's, and the same.
        let mut native = t.native.clone();
        if let Some(n) = native
            .as_mut()
            .filter(|n| &*n.format == crate::openai::OPENAI)
            && let Some(strict) = n.fields.shift_remove("strict")
        {
            tool.insert("strict".into(), strict);
        }
        let mut fields = kept_fields(RESPONSES, &native, tool_field, Keep::CACHE_MARKER)?;
        let unsaid = fields
            .shift_remove(INERT)
            .is_some_and(|inert| inert["strict"] == "unsaid");
        tool.extend(fields);
        // Another format's tools are not strict, which is not the default.
        if !unsaid && !tool.contains_key("strict") {
            tool.insert("strict".into(), false.into());
        }
        Ok(Value::Object(tool))
    };
    let mut tools: Vec<Value> = chat.tools.iter().map(tool).collect::<Result<_, Fault>>()?;
    for provided in &chat.provider_tools {
        let mut search = Map::from_iter([("type".into(), "web_search".into())]);
        search.extend(write_search(RESPONSES, provided)?);
        tools.push(search.into());
    }
    set(v, "tools", (!tools.is_empty()).then(|| tools.into()));
    let choice = match &chat.options.tool_choice {
        None => None,
        Some(ToolChoice::Auto) => Some("auto".into()),
        Some(ToolChoice::None) => Some("none".into()),
        Some(ToolChoice::Required) => Some("required".into()),
        Some(ToolChoice::Tool(name)) => Some(json!({ "type": "function", "name": name })),
        Some(_) => return Err(untranslatable("a tool choice this format has no form for")),
    };
    set(v, "tool_choice", choice);
    set(
        v,
        "parallel_tool_calls",
        chat.options.parallel_tool_calls.map(Into::into),
    );
    Ok(())
}

// -------------------------------------------------------------- text format

/// The structured output requirement of `text.format`, if the kind can say
/// all of it: then it leaves the request's `native`.
pub(in crate::responses) fn read_format(v: &Value, input: &mut Input) {
    let raw = &v["text"]["format"];
    let alone = raw.as_object().is_some_and(|m| m.len() == 1);
    let parsed = match raw["type"].as_str() {
        Some("text") if alone => Some(ResponseFormat::Text),
        Some("json_object") if alone => Some(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let known = ["type", "name", "description", "schema", "strict"];
            let clean = raw
                .as_object()
                .is_some_and(|m| m.keys().all(|k| known.contains(&k.as_str())));
            let typed = raw["name"].is_string()
                && raw.get("description").is_none_or(Value::is_string)
                && raw.get("strict").is_none_or(Value::is_boolean)
                && raw["schema"].is_object();
            (clean && typed).then(|| ResponseFormat::JsonSchema {
                name: raw["name"].as_str().unwrap_or("response").into(),
                description: raw["description"].as_str().map(str::to_owned),
                schema: raw["schema"].clone(),
                strict: raw["strict"].as_bool(),
            })
        }
        _ => None,
    };
    let Some(parsed) = parsed else {
        return;
    };
    input.options.response_format = Some(parsed);
    if let Some(native) = &mut input.native
        && let Some(Value::Object(text)) = native.fields.get_mut("text")
    {
        text.shift_remove("format");
        if text.is_empty() {
            native.fields.shift_remove("text");
        }
    }
    if input.native.is_empty() {
        input.native = None;
    }
}

/// The request's `text`: its own fields, with the format the kind's
/// structured output requirement says.
pub(in crate::responses) fn write_format(input: &Input, out: &mut Map<String, Value>) {
    let own = input.native.format() == Some(RESPONSES);
    let mut text = own
        .then(|| input.native.fields().get("text").cloned())
        .flatten()
        .unwrap_or_else(|| json!({}));
    let format = match &input.options.response_format {
        None => None,
        Some(ResponseFormat::Text) => Some(json!({ "type": "text" })),
        Some(ResponseFormat::JsonObject) => Some(json!({ "type": "json_object" })),
        Some(ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        }) => {
            let mut spec = json!({ "type": "json_schema", "name": name, "schema": schema });
            if let Some(description) = description {
                spec["description"] = description.as_str().into();
            }
            if let Some(strict) = strict {
                spec["strict"] = (*strict).into();
            }
            Some(spec)
        }
    };
    if let (Some(format), Some(text)) = (format, text.as_object_mut()) {
        text.insert("format".into(), format);
    }
    let empty = text.as_object().is_none_or(Map::is_empty);
    set(out, "text", (!empty).then_some(text));
}

/// A part the kind has a form for beyond text: an image or a file.
pub(in crate::responses) fn decode_part(p: &Value) -> Option<Content> {
    read_media(p)
}

/// A part of the kind's beyond text, as Responses writes it.
pub(in crate::responses) fn encode_part(c: &Content) -> Result<Value, Fault> {
    match c {
        Content::Image(m) | Content::Document(m) => write_media(c, m),
        Content::Audio(_) => Err(untranslatable("audio in Responses")),
        Content::Reasoning { .. } => Err(untranslatable("reasoning outside an assistant")),
        _ => Err(untranslatable("an unsupported kind of part")),
    }
}

/// The kind's reasoning of `reasoning`: its effort, and a summary of `auto`
/// (the summaries `concise` and `detailed` have no form in the kind). What
/// else it holds stays in the request's `native`.
pub(in crate::responses) fn read_reasoning(input: &mut Input) {
    let Some(native) = input.native.as_mut() else {
        return;
    };
    let Some(Value::Object(config)) = native.fields.get_mut("reasoning") else {
        return;
    };
    let mut r = Reasoning::default();
    if let Some(effort) = config
        .get("effort")
        .and_then(Value::as_str)
        .and_then(parse_effort)
    {
        config.shift_remove("effort");
        r.effort = Some(effort);
    }
    if config.get("summary").and_then(Value::as_str) == Some("auto") {
        config.shift_remove("summary");
        r.summary = Some(true);
    }
    if config.is_empty() {
        native.fields.shift_remove("reasoning");
    }
    if native.is_empty() {
        input.native = None;
    }
    if r != Reasoning::default() {
        input.options.reasoning = Some(r);
    }
}

/// The request's `reasoning`: its own fields, with the kind's effort and
/// summary. Responses has no token budget.
pub(in crate::responses) fn write_reasoning(
    input: &Input,
    out: &mut Map<String, Value>,
) -> Result<(), Fault> {
    let mut config = own_native(input, RESPONSES, "reasoning").unwrap_or_else(|| json!({}));
    if let Some(r) = reasoning_of(input)? {
        if r.budget_tokens.is_some() {
            return Err(untranslatable("a reasoning token budget in Responses"));
        }
        if let Some(config) = config.as_object_mut() {
            if let Some(effort) = r.effort {
                config.insert("effort".into(), effort_name(effort)?.into());
            }
            match r.summary {
                Some(true) => _ = config.insert("summary".into(), "auto".into()),
                Some(false) => _ = config.shift_remove("summary"),
                None => {}
            }
        }
    }
    let empty = config.as_object().is_none_or(Map::is_empty);
    set(out, "reasoning", (!empty).then_some(config));
    Ok(())
}
