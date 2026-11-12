//! What a request says, in Gemini's terms: `contents` and their parts, the
//! system instruction, tools and their choice, `generationConfig` and
//! structured output. What the kind has no form for, such as a tool Google
//! runs or a sampling field, stays in the `native` of the request, the
//! message or the part.

use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Effort, Input, Media, MediaSource, Message, ProviderTool, Reasoning, ResponseFormat,
    Role, Tool, ToolChoice,
};
use respan_gateway_core::{Fault, NativeFields};
use serde_json::{Map, Value, json};

use super::{GEMINI, ROLES, SIGNATURE, SYNTHETIC};
use crate::schema::{
    NativeFieldsExt, arguments_object, effort_name, merge_fields, merge_missing, native_fields,
    parse_effort, remainder, sealed, set, untranslatable, write_search,
};

/// The id a call of Gemini's without one is given: its position, so that its
/// answer, which names the tool, finds it.
fn synthetic(message: usize, part: usize) -> String {
    format!("{SYNTHETIC}{message}_{part}")
}

/// Whether `id` was made by [`synthetic`] (or the decoder's), so Gemini never
/// sees it.
pub(super) fn is_synthetic(id: &str) -> bool {
    id.starts_with(SYNTHETIC)
}

/// The calls a conversation has made so far, by id, and whether each is
/// answered: how a response that names only a tool finds its call.
#[derive(Default)]
struct Calls(Vec<(String, String, bool)>);

impl Calls {
    fn id_of(&mut self, id: Option<&str>, name: &str) -> String {
        let found = match id {
            Some(id) => self.0.iter_mut().find(|c| c.0 == id),
            None => self.0.iter_mut().find(|c| c.1 == name && !c.2),
        };
        match found {
            Some(call) => {
                call.2 = true;
                call.0.clone()
            }
            None => id.map_or_else(|| format!("{SYNTHETIC}{name}"), str::to_owned),
        }
    }
}

/// A request body's `contents` and `systemInstruction` as messages.
pub(super) fn read_messages(v: &Value, chat: &mut Input) {
    if let Some(system) = v.get("systemInstruction").filter(|s| !s.is_null()) {
        let mut message = Message::new(Role::System, "");
        message.content = Vec::new();
        let mut calls = Calls::default();
        match system {
            Value::String(text) => message.content.push(Content::from(text.as_str())),
            system => {
                message.content = read_parts(&system["parts"], 0, &mut calls);
                let rest = remainder(system, &["parts", "role"]);
                message.native = native_fields(GEMINI, rest);
            }
        }
        chat.messages.push(message);
    }
    let mut calls = Calls::default();
    for (at, m) in v["contents"].as_array().into_iter().flatten().enumerate() {
        let name = m["role"].as_str().unwrap_or("user");
        let role = ROLES
            .value(name)
            .unwrap_or_else(|| Role::native(GEMINI, name));
        let mut message = Message::new(role, "");
        message.content = read_parts(&m["parts"], at, &mut calls);
        message.native = native_fields(GEMINI, remainder(m, &["role", "parts"]));
        chat.messages.push(message);
    }
}

fn read_parts(parts: &Value, message: usize, calls: &mut Calls) -> Vec<Content> {
    let parts = parts.as_array().into_iter().flatten();
    let mut out = Vec::new();
    for (at, p) in parts.enumerate() {
        let part = read_part(p, message, at, calls);
        let signed = !matches!(part, Content::Native(_));
        out.push(part);
        // A part's signature is a signature after it.
        if let Some(s) = p[SIGNATURE].as_str().filter(|_| signed) {
            out.push(Content::Signature(s.into()));
        }
    }
    out
}

/// A part as the kind's, or, if the kind has no form for all of it, as
/// this format's own.
fn read_part(p: &Value, message: usize, at: usize, calls: &mut Calls) -> Content {
    let own = || Content::native(GEMINI, p.clone());
    let payloads = [
        "text",
        "inlineData",
        "fileData",
        "functionCall",
        "functionResponse",
    ];
    let which: Vec<&str> = payloads
        .iter()
        .copied()
        .filter(|k| p.get(*k).is_some())
        .collect();
    let [which] = which[..] else { return own() };
    let part_fields = |known: &[&str]| native_fields(GEMINI, remainder(p, known));
    match which {
        "text" => {
            let Some(text) = p["text"].as_str() else {
                return own();
            };
            let thought = match &p["thought"] {
                Value::Null | Value::Bool(false) => false,
                Value::Bool(true) => true,
                _ => return own(),
            };
            let native = part_fields(&["text", "thought", SIGNATURE]);
            match thought {
                true => Content::Reasoning {
                    text: text.into(),
                    native,
                },
                false => {
                    let mut part = Content::from(text);
                    if let Content::Text { native: n, .. } = &mut part {
                        *n = native;
                    }
                    part
                }
            }
        }
        "inlineData" | "fileData" => read_media(p, which).unwrap_or_else(own),
        "functionCall" => {
            let call = &p["functionCall"];
            let clean = call["args"].is_object() || call["args"].is_null();
            let (Some(name), true) = (call["name"].as_str(), clean) else {
                return own();
            };
            if !call.as_object().is_some_and(|o| {
                o.keys()
                    .all(|k| matches!(k.as_str(), "name" | "args" | "id"))
            }) {
                return own();
            }
            let id = match call["id"].as_str() {
                Some(id) => id.to_owned(),
                None => synthetic(message, at),
            };
            calls.0.push((id.clone(), name.into(), false));
            let args = match call["args"].is_null() {
                true => "{}".to_owned(),
                false => call["args"].to_string(),
            };
            let mut part = Content::tool_call(id, name, args);
            if let Content::ToolCall { native, .. } = &mut part {
                *native = part_fields(&["functionCall", SIGNATURE]);
            }
            part
        }
        _ => {
            let response = &p["functionResponse"];
            let clean = response.as_object().is_some_and(|o| {
                o.keys()
                    .all(|k| matches!(k.as_str(), "name" | "response" | "id"))
            });
            let name = response["name"].as_str();
            let (Some(name), true, true) = (name, clean, response["response"].is_object()) else {
                return own();
            };
            let id = calls.id_of(response["id"].as_str(), name);
            let said = Content::from(response["response"].to_string());
            let mut part = Content::tool_result(id, vec![said], false);
            if let Content::ToolResult { native, .. } = &mut part {
                *native = part_fields(&["functionResponse"]);
            }
            part
        }
    }
}

/// An `inlineData` or `fileData` part, if the kind can say it.
fn read_media(p: &Value, which: &str) -> Option<Content> {
    let data = &p[which];
    let mime = data["mimeType"].as_str()?;
    let (source, known): (MediaSource, &[&str]) = match which {
        "inlineData" => (
            MediaSource::Bytes(STANDARD.decode(data["data"].as_str()?).ok()?.into()),
            &["mimeType", "data"],
        ),
        _ => (
            MediaSource::Url(data["fileUri"].as_str()?.into()),
            &["mimeType", "fileUri"],
        ),
    };
    let mut fields = remainder(p, &[which]);
    let nested = remainder(data, known);
    if !nested.is_empty() {
        fields.insert(which.into(), nested.into());
    }
    let mut media = Media::new(mime, source);
    media.native = native_fields(GEMINI, fields);
    match mime.split('/').next()? {
        "image" => Some(Content::Image(media)),
        "audio" => Some(Content::Audio(media)),
        "video" => None,
        _ => Some(Content::Document(media)),
    }
}

/// The messages `chat` has after its leading system ones, and the
/// instruction they make: a later system message is a user's
/// ([`write_messages`]).
pub(super) fn split_system(chat: &Input) -> Result<(Option<Value>, &[Message]), Fault> {
    let is_system = |m: &Message| matches!(m.role, Role::System | Role::Developer);
    let lead = chat.messages.iter().take_while(|m| is_system(m)).count();
    let (system, rest) = chat.messages.split_at(lead);
    let mut names = Vec::new();
    let mut parts = Vec::new();
    // WARN: a system message's own fields have no place in the instruction; dropped.
    for m in system {
        for c in &m.content {
            parts.push(write_part(c, &mut names)?);
        }
    }
    parts.retain(|p| !p.is_null());
    let instruction = (!parts.is_empty()).then(|| json!({ "parts": parts }));
    Ok((instruction, rest))
}

/// Whether a message's fields are none Gemini writes: none, or another
/// format's it drops.
fn unsaid(m: &Message) -> bool {
    m.native.format() != Some(GEMINI)
}

/// The messages as `contents`: the results of one turn's calls are one user
/// content, as Gemini needs a response for each call in the turn after it.
pub(super) fn write_messages(messages: &[Message]) -> Result<Vec<Value>, Fault> {
    let mut names: Vec<(String, String)> = Vec::new();
    let mut contents: Vec<(Map<String, Value>, bool)> = Vec::new();
    for m in messages {
        let role = match &m.role {
            Role::Native { format, name, .. } if **format == *GEMINI => name.as_str(),
            // WARN: a role Gemini lacks, such as a later system message's, is a user's.
            role => ROLES.name(role).unwrap_or("user"),
        };
        let mut parts: Vec<Value> = Vec::new();
        for (c, signature) in sealed(&m.content) {
            // WARN: Gemini has no part for reasoning withheld; it is dropped here, silently.
            if matches!(c, Content::Signature(_) | Content::Withheld(_)) {
                continue;
            }
            let mut part = write_part(c, &mut names)?;
            if part.is_null() {
                continue;
            }
            if let Some(s) = signature {
                part[SIGNATURE] = s.into();
            }
            parts.push(part);
        }
        let mut content = Map::new();
        content.insert("role".into(), role.into());
        content.insert("parts".into(), parts.into());
        content.extend(m.native.fields_for(GEMINI));
        let answers = !m.content.is_empty()
            && unsaid(m)
            && m.content
                .iter()
                .all(|c| matches!(c, Content::ToolResult { .. }));
        match contents.last_mut() {
            Some((last, true)) if answers && last["role"] == role => {
                if let Some(Value::Array(more)) = content.get_mut("parts").map(Value::take)
                    && let Some(Value::Array(parts)) = last.get_mut("parts")
                {
                    parts.extend(more);
                }
            }
            _ => contents.push((content, answers)),
        }
    }
    Ok(contents.into_iter().map(|(c, _)| c.into()).collect())
}

/// Whether a call to `model` signs the function calls in its history: one
/// to any model but those before Gemini 3.
pub fn needs_signatures(model: Option<&str>) -> bool {
    let model = model.unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    !(model.starts_with("gemini-1") || model.starts_with("gemini-2"))
}

/// The first call of each `model` content in request `v` signed with the
/// placeholder, unless it has a signature of its own.
pub(super) fn sign_calls(v: &mut Map<String, Value>) {
    let Some(Value::Array(contents)) = v.get_mut("contents") else {
        return;
    };
    for content in contents.iter_mut().filter(|c| c["role"] == "model") {
        let Some(parts) = content["parts"].as_array_mut() else {
            continue;
        };
        let first = parts.iter_mut().find(|p| p.get("functionCall").is_some());
        if let Some(Value::Object(call)) = first {
            call.entry("thoughtSignature")
                .or_insert_with(|| super::SIGNATURE_PLACEHOLDER.into());
        }
    }
}

/// A part of the kind's as Gemini writes it, `Null` for one it drops;
/// `names` has the calls made so far, by id, for the results that answer
/// them.
fn write_part(c: &Content, names: &mut Vec<(String, String)>) -> Result<Value, Fault> {
    let fields = |native: &Option<NativeFields>| native.fields_for(GEMINI);
    match c {
        Content::Text { text, native, .. } => {
            let mut part = Map::new();
            part.insert("text".into(), text.as_str().into());
            part.extend(fields(native));
            Ok(part.into())
        }
        Content::Reasoning { text, native, .. } => {
            let mut part = json!({ "text": text, "thought": true });
            merge_missing(&mut part, fields(native));
            Ok(part)
        }
        Content::Image(media) | Content::Audio(media) | Content::Document(media) => {
            Ok(write_media(media, false))
        }
        Content::ToolCall {
            id,
            name,
            arguments,
            native,
            ..
        } => {
            names.push((id.clone(), name.clone()));
            let mut call = json!({ "name": name, "args": arguments_object(arguments)? });
            if !is_synthetic(id) {
                call["id"] = id.as_str().into();
            }
            let mut part = json!({ "functionCall": call });
            merge_missing(&mut part, fields(native));
            Ok(part)
        }
        Content::ToolResult {
            id,
            content,
            is_error,
            native,
            ..
        } => {
            // WARN: a result of a call no message makes is named by its id.
            let name = names
                .iter()
                .rev()
                .find(|(i, _)| i == id)
                .map_or(id, |(_, n)| n);
            let mut response = json!({ "name": name });
            write_result(content, *is_error, &mut response);
            if !is_synthetic(id) {
                response["id"] = id.as_str().into();
            }
            let mut part = json!({ "functionResponse": response });
            merge_missing(&mut part, fields(native));
            Ok(part)
        }
        Content::Native(raw) if *raw.format == *GEMINI => Ok(Value::clone(&raw.value)),
        _ => Ok(Value::Null),
    }
}

/// What a tool said as a `functionResponse`'s `response`: a JSON object as
/// it is, else the text under `output`, or under `error` for a failure, as
/// Gemini's examples do; and its images and documents as its `parts`.
fn write_result(content: &[Content], is_error: bool, response: &mut Value) {
    let mut text = String::new();
    let mut parts = Vec::new();
    for c in content {
        match c {
            // WARN: a text part's native fields are dropped here, silently.
            Content::Text { text: t, .. } => text.push_str(t),
            Content::Image(m) | Content::Document(m) => parts.push(write_media(m, true)),
            // WARN: audio and other parts have no place in a result; dropped.
            _ => {}
        }
    }
    response["response"] = match serde_json::from_str::<Value>(&text) {
        Ok(object @ Value::Object(_)) => object,
        _ => json!({ if is_error { "error" } else { "output" }: text }),
    };
    if !parts.is_empty() {
        response["parts"] = parts.into();
    }
}

/// `media` as an `inlineData` or `fileData` part. Gemini refuses a wildcard
/// type, and finds a URL's type itself.
pub fn media_part(media: &Media) -> Value {
    match &media.source {
        MediaSource::Url(uri) if media.mime_type == "image/*" => {
            json!({ "fileData": { "fileUri": uri } })
        }
        MediaSource::Url(uri) => {
            json!({ "fileData": { "mimeType": media.mime_type, "fileUri": uri } })
        }
        MediaSource::Bytes(bytes) => {
            json!({ "inlineData": { "mimeType": media.mime_type, "data": STANDARD.encode(bytes) } })
        }
    }
}

/// An `inlineData` or `fileData` part of `media`, with its native fields; only
/// a tool result's part takes a display name, which another drops.
fn write_media(media: &Media, in_result: bool) -> Value {
    let mut part = media_part(media);
    let data = part.as_object_mut().and_then(|p| p.values_mut().next());
    if let (Some(name), Some(Value::Object(data)), true) = (&media.name, data, in_result) {
        data.insert("displayName".into(), name.as_str().into());
    }
    merge_fields(&mut part, &media.native.fields_for(GEMINI));
    part
}

/// Gemini's `Schema` (OpenAPI's subset, with upper-case type names) as JSON
/// Schema, which is what the kind holds: type names in lower case, and
/// `nullable` as a type that may be null.
pub(super) fn json_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => items.iter().map(json_schema).collect(),
        Value::Object(fields) => {
            let mut out = Map::new();
            for (key, value) in fields {
                match key.as_str() {
                    "type" if value.is_string() => {
                        let name = value.as_str().unwrap_or_default().to_ascii_lowercase();
                        out.insert(key.clone(), name.into());
                    }
                    "nullable" | "propertyOrdering" => {}
                    "properties" if value.is_object() => {
                        let props = value.as_object().into_iter().flatten();
                        let props = props.map(|(k, v)| (k.clone(), json_schema(v)));
                        out.insert(key.clone(), Value::Object(props.collect()));
                    }
                    _ => _ = out.insert(key.clone(), json_schema(value)),
                }
            }
            if fields.get("nullable") == Some(&Value::Bool(true))
                && let Some(Value::String(t)) = out.get("type").cloned()
            {
                out.insert("type".into(), json!([t, "null"]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// The function declarations of `tools` and an empty `googleSearch`, if all
/// of it is that; else the whole `tools` stays native.
fn declarations(tools: &Value) -> Option<(Vec<Tool>, Vec<ProviderTool>)> {
    let mut out = Vec::new();
    let mut provided = Vec::new();
    for tool in tools.as_array()? {
        if *tool == json!({ "googleSearch": {} }) {
            provided.push(ProviderTool::WebSearch { native: None });
            continue;
        }
        let clean = tool
            .as_object()
            .is_some_and(|o| o.keys().all(|k| k == "functionDeclarations"));
        if !clean {
            return None;
        }
        for d in tool["functionDeclarations"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let function = d.as_object()?;
            let known = ["name", "description", "parameters", "parametersJsonSchema"];
            let rest = remainder(d, &known);
            let schema = match (
                function.get("parameters"),
                function.get("parametersJsonSchema"),
            ) {
                (Some(_), Some(_)) => return None,
                (Some(p), None) => json_schema(p),
                (None, Some(p)) => p.clone(),
                (None, None) => Value::Null,
            };
            let mut tool = Tool::new(d["name"].as_str()?, schema);
            tool.description = match &d["description"] {
                Value::Null => None,
                text => Some(text.as_str()?.to_owned()),
            };
            tool.native = native_fields(GEMINI, rest);
            out.push(tool);
        }
    }
    Some((out, provided))
}

/// The tool choice `toolConfig` says, if the kind has it all.
fn choice_of(config: &Value) -> Option<ToolChoice> {
    let fc = config.get("functionCallingConfig")?;
    if !fc
        .as_object()?
        .keys()
        .all(|k| k == "mode" || k == "allowedFunctionNames")
    {
        return None;
    }
    let names: Vec<&str> = fc["allowedFunctionNames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match (fc["mode"].as_str()?, &names[..]) {
        ("AUTO", []) => Some(ToolChoice::Auto),
        ("NONE", []) => Some(ToolChoice::None),
        ("ANY", []) => Some(ToolChoice::Required),
        ("ANY", [name]) => Some(ToolChoice::Tool((*name).into())),
        _ => None,
    }
}

/// The request's tools and their choice, if the kind can say them: then they
/// leave the request's `native`.
pub(super) fn read_tools(v: &Value, chat: &mut Input) {
    if let Some((tools, provided)) = declarations(&v["tools"]) {
        chat.tools = tools;
        chat.provider_tools = provided;
    } else if says(&v["tools"]) {
        chat.native
            .fields_mut(GEMINI)
            .insert("tools".into(), v["tools"].clone());
    }
    let config = &v["toolConfig"];
    if !config.is_object() || config.as_object().is_some_and(Map::is_empty) {
        return;
    }
    let choice = choice_of(config);
    let mut rest = remainder(config, &["functionCallingConfig"]);
    match choice {
        Some(choice) => chat.options.tool_choice = Some(choice),
        None => {
            _ = rest.insert(
                "functionCallingConfig".into(),
                config["functionCallingConfig"].clone(),
            )
        }
    }
    if !rest.is_empty() {
        chat.native
            .fields_mut(GEMINI)
            .insert("toolConfig".into(), rest.into());
    }
}

fn says(v: &Value) -> bool {
    crate::schema::says(v)
}

/// The request's tools and choice as Gemini's `tools` and `toolConfig`,
/// beside any the request's own `native` holds.
pub(super) fn write_tools(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    let own = chat.native.format() == Some(GEMINI);
    let native = |key: &str| {
        own.then(|| chat.native.fields().get(key).cloned())
            .flatten()
    };
    let mut declarations = Vec::new();
    for t in &chat.tools {
        let mut d = Map::new();
        d.insert("name".into(), t.name.as_str().into());
        if let Some(text) = &t.description {
            d.insert("description".into(), text.as_str().into());
        }
        if t.parameters.is_object() {
            d.insert("parametersJsonSchema".into(), t.parameters.clone());
        }
        d.extend(t.native.fields_for(GEMINI));
        declarations.push(Value::Object(d));
    }
    let mut tools = Vec::new();
    if !declarations.is_empty() {
        tools.push(json!({ "functionDeclarations": declarations }));
    }
    for provided in &chat.provider_tools {
        write_search(GEMINI, provided)?;
        tools.push(json!({ "googleSearch": {} }));
    }
    let tools = (!tools.is_empty())
        .then(|| tools.into())
        .or_else(|| native("tools"));
    set(v, "tools", tools);
    // WARN: Gemini cannot turn off parallel tool calls; the ask is dropped.
    let mode = match &chat.options.tool_choice {
        None => None,
        Some(ToolChoice::Auto) => Some(json!({ "mode": "AUTO" })),
        Some(ToolChoice::None) => Some(json!({ "mode": "NONE" })),
        Some(ToolChoice::Required) => Some(json!({ "mode": "ANY" })),
        Some(ToolChoice::Tool(name)) => {
            Some(json!({ "mode": "ANY", "allowedFunctionNames": [name] }))
        }
        Some(_) => return Err(untranslatable("a tool choice this format has no form for")),
    };
    let mut config = native("toolConfig").unwrap_or_else(|| json!({}));
    if let Some(mode) = mode {
        config["functionCallingConfig"] = mode;
    }
    set(
        v,
        "toolConfig",
        config
            .as_object()
            .is_some_and(|c| !c.is_empty())
            .then_some(config),
    );
    Ok(())
}

/// The structured output `generationConfig` asks for, if the kind can say
/// all of it: then its fields leave `config`.
fn read_format(config: &mut Map<String, Value>) -> Option<ResponseFormat> {
    let mime = config.get("responseMimeType")?.as_str()?;
    let schema = ["responseJsonSchema", "responseSchema"]
        .into_iter()
        .filter(|k| config.get(*k).is_some())
        .collect::<Vec<_>>();
    let format = match (mime, &schema[..]) {
        ("text/plain", []) => ResponseFormat::Text,
        ("application/json", []) => ResponseFormat::JsonObject,
        ("application/json", [key]) => {
            let schema = config.get(*key)?;
            ResponseFormat::JsonSchema {
                name: "response".into(),
                description: None,
                schema: match *key {
                    "responseSchema" => json_schema(schema),
                    _ => schema.clone(),
                },
            }
        }
        _ => return None,
    };
    for key in ["responseMimeType", "responseSchema", "responseJsonSchema"] {
        config.shift_remove(key);
    }
    Some(format)
}

/// The sampling and output fields of `generationConfig` the kind has; the
/// rest stay in the request's `native`.
pub(super) fn read_config(v: &Value, chat: &mut Input) {
    let Some(config) = v["generationConfig"].as_object() else {
        return;
    };
    let mut rest = config.clone();
    let mut take = |key: &str| rest.shift_remove(key).filter(|v| !v.is_null());
    chat.options.max_output_tokens = take("maxOutputTokens").and_then(|v| v.as_u64());
    chat.options.temperature = take("temperature").and_then(|v| v.as_f64());
    chat.options.top_p = take("topP").and_then(|v| v.as_f64());
    if let Some(Value::Array(stops)) = take("stopSequences") {
        chat.options.stop = stops
            .iter()
            .filter_map(|s| s.as_str().map(Into::into))
            .collect();
    }
    chat.options.reasoning = read_thinking(&mut rest);
    // One candidate is what the kind writes; asking for it says nothing.
    if rest.get("candidateCount") == Some(&json!(1)) {
        rest.shift_remove("candidateCount");
    }
    chat.options.response_format = read_format(&mut rest);
    rest.retain(|_, v| says(v) || v.is_boolean() || v.is_number());
    if !rest.is_empty() {
        chat.native
            .fields_mut(GEMINI)
            .insert("generationConfig".into(), rest.into());
    }
}

/// `generationConfig`: the request's own fields beside the kind's.
pub(super) fn write_config(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    let own = chat.native.format() == Some(GEMINI);
    let mut config = own
        .then(|| chat.native.fields().get("generationConfig").cloned())
        .flatten()
        .unwrap_or_else(|| json!({}));
    let o = &chat.options;
    let mut set_field = |key: &str, value: Option<Value>| {
        if let (Some(value), Some(config)) = (value, config.as_object_mut()) {
            config.insert(key.into(), value);
        }
    };
    set_field("maxOutputTokens", o.max_output_tokens.map(Into::into));
    set_field("temperature", o.temperature.map(Into::into));
    set_field("topP", o.top_p.map(Into::into));
    set_field("stopSequences", crate::schema::stop_json(&o.stop));
    match &o.response_format {
        None | Some(ResponseFormat::Text) => {}
        Some(ResponseFormat::JsonObject) => {
            set_field("responseMimeType", Some("application/json".into()));
        }
        // WARN: the wrapper's description has no field; dropped.
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            set_field("responseMimeType", Some("application/json".into()));
            set_field("responseJsonSchema", Some(schema.clone()));
        }
    }
    // WARN: an end user's id has no field in Gemini; dropped silently.
    if let Some(r) = &o.reasoning {
        write_thinking(r, &mut config);
    }
    set(
        v,
        "generationConfig",
        config
            .as_object()
            .is_some_and(|c| !c.is_empty())
            .then_some(config),
    );
    Ok(())
}

/// The kind's reasoning of `thinkingConfig`: a budget (0 is off) or a level,
/// and `includeThoughts`. What else it holds, and a budget beside a level,
/// stays in `config`.
fn read_thinking(config: &mut Map<String, Value>) -> Option<Reasoning> {
    let Some(Value::Object(thinking)) = config.get_mut("thinkingConfig") else {
        return None;
    };
    let mut r = Reasoning::default();
    let both = thinking.contains_key("thinkingBudget") && thinking.contains_key("thinkingLevel");
    if !both {
        if let Some(budget) = thinking.get("thinkingBudget").and_then(Value::as_u64) {
            thinking.shift_remove("thinkingBudget");
            match budget {
                0 => r.effort = Some(Effort::Off),
                n => r.budget_tokens = Some(n),
            }
        } else if let Some(level) = thinking
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .and_then(parse_effort)
            .filter(|e| level_name(*e).is_some())
        {
            thinking.shift_remove("thinkingLevel");
            r.effort = Some(level);
        }
    }
    if let Some(shown) = thinking.get("includeThoughts").and_then(Value::as_bool) {
        thinking.shift_remove("includeThoughts");
        r.summary = Some(shown);
    }
    if thinking.is_empty() {
        config.shift_remove("thinkingConfig");
    }
    (r != Reasoning::default()).then_some(r)
}

/// Thinking budgets per effort on Gemini 2.5; `minimal`'s is per model.
const EFFORT_BUDGETS: &[(&str, u64)] = &[("low", 1024), ("medium", 2048), ("high", 4096)];

/// The least thinking budget of a Gemini 2.5 `model`.
fn minimal_budget(model: &str) -> u64 {
    if model.contains("flash-lite") {
        512
    } else if model.contains("2.5-pro") || model.contains("2-5-pro") {
        128
    } else if model.contains("flash") {
        1
    } else {
        128
    }
}

/// The major version of a Gemini `model`, if its name says one.
fn gemini_version(model: &str) -> Option<u32> {
    let (_, after) = model.split_once("gemini-")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// A call whose `thinkingConfig` is an effort the kind wrote, its level or
/// a budget of 0 for none, as a client of another format names it: on
/// Gemini 3 and later, the level the model has for it (the flash line has
/// `minimal` and `medium`, Gemini 3.1 Pro `medium`; others round to `low` or
/// `high`, and none is the least level without thoughts); on Gemini 2.5 a
/// thinking budget for it (none is 0); before 2.5, which does not think,
/// none. Thoughts are included but for none.
pub(crate) fn effort_thinking(model: &str, v: &mut Map<String, Value>) {
    let model = model.to_ascii_lowercase();
    let Some(version) = gemini_version(&model) else {
        return;
    };
    let Some(Value::Object(config)) = v.get_mut("generationConfig") else {
        return;
    };
    let Some(Value::Object(thinking)) = config.get_mut("thinkingConfig") else {
        return;
    };
    let effort = match (
        thinking.get("thinkingLevel"),
        thinking.get("thinkingBudget"),
    ) {
        (Some(level), None) => level.as_str().unwrap_or_default().to_owned(),
        (None, Some(budget)) if budget.as_u64() == Some(0) => "none".to_owned(),
        _ => return,
    };
    let none = effort == "none";
    let older = version < 2 || model.contains("gemini-2.0") || model.contains("gemini-2-0");
    if older {
        thinking.shift_remove("thinkingLevel");
        thinking.shift_remove("thinkingBudget");
    } else if version >= 3 {
        let flash = model.contains("gemini-3-flash") || model.contains("gemini-3.1-flash");
        let medium = flash || model.contains("gemini-3.1-pro-preview");
        let floor = if flash { "minimal" } else { "low" };
        let level = match effort.as_str() {
            "minimal" | "none" => floor,
            "low" => "low",
            "medium" if medium => "medium",
            _ => "high",
        };
        thinking.shift_remove("thinkingBudget");
        thinking.insert("thinkingLevel".into(), level.into());
    } else {
        let budget = match effort.as_str() {
            "none" => 0,
            "minimal" => minimal_budget(&model),
            effort => EFFORT_BUDGETS
                .iter()
                .find(|(e, _)| *e == effort)
                .map_or(EFFORT_BUDGETS[2].1, |(_, budget)| *budget),
        };
        thinking.shift_remove("thinkingLevel");
        thinking.insert("thinkingBudget".into(), budget.into());
    }
    if !older {
        thinking.entry("includeThoughts").or_insert((!none).into());
    }
    if thinking.is_empty() {
        config.shift_remove("thinkingConfig");
    }
    if config.is_empty() {
        v.shift_remove("generationConfig");
    }
}

/// Gemini's `thinkingLevel` of a named `effort`, which has none for `Off`
/// (a budget of 0) or above `high`.
fn level_name(effort: Effort) -> Option<&'static str> {
    match effort {
        Effort::Minimal | Effort::Low | Effort::Medium | Effort::High => effort_name(effort).ok(),
        _ => None,
    }
}

/// `thinkingConfig` of `config` with the kind's reasoning over its own: an
/// effort above Gemini's levels is its highest, `high`.
fn write_thinking(r: &Reasoning, config: &mut Value) {
    let Some(config) = config.as_object_mut() else {
        return;
    };
    let thinking = config.entry("thinkingConfig").or_insert_with(|| json!({}));
    let Some(thinking) = thinking.as_object_mut() else {
        return;
    };
    if r.effort.is_some() || r.budget_tokens.is_some() {
        thinking.shift_remove("thinkingBudget");
        thinking.shift_remove("thinkingLevel");
    }
    match (r.effort, r.budget_tokens) {
        (Some(Effort::Off), _) => _ = thinking.insert("thinkingBudget".into(), 0.into()),
        (Some(effort), _) => {
            let level = level_name(effort).unwrap_or("high");
            thinking.insert("thinkingLevel".into(), level.into());
        }
        (None, Some(n)) => _ = thinking.insert("thinkingBudget".into(), n.into()),
        (None, None) => {}
    }
    if let Some(shown) = r.summary {
        thinking.insert("includeThoughts".into(), shown.into());
    }
    if thinking.is_empty() {
        config.shift_remove("thinkingConfig");
    }
}
