//! What a request says beyond text, in OpenAI's terms: tools and their
//! choice, an assistant's calls and their results, media, and structured
//! output. What the kind has no form
//! for, such as a tool the provider runs, stays in the `native` of the
//! request, the message or the tool.

use crate::schema::{
    INERT, Keep, Names, NativeFieldsExt, decode_tools, each_tool, effort_name, encode_content,
    encode_message, encode_native, kept_fields, merge_missing, message_fields, native_fields, only,
    own_native, reasoning_of, remainder, rest_of, set, source_url, tool_field, untranslatable,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Input, Media, MediaSource, Message, ResponseFormat, Role, Tool, ToolChoice,
};
use respan_gateway_core::{Fault, NativeFields};
use serde_json::{Map, Value, json};

use crate::openai::{OPENAI, OpenAiChat};

/// OpenAI's function tools, choice and `parallel_tool_calls`, as the kind's.
pub(crate) fn read_openai_tools(v: &Value, chat: &mut Input) {
    let tools = each_tool(&v["tools"], openai_tool);
    let choice = match &v["tool_choice"] {
        Value::Null => Some(None),
        choice => openai_choice(choice).map(Some),
    };
    let parallel = match &v["parallel_tool_calls"] {
        Value::Null => Some(None),
        parallel => parallel.as_bool().map(Some),
    };
    let keys = ["tools", "tool_choice", "parallel_tool_calls"];
    decode_tools(chat, tools, choice, parallel, keys);
}

fn openai_tool(tool: &Value) -> Option<Tool> {
    let function = &tool["function"];
    if tool["type"] != "function" || !only(tool, &["type", "function"]) {
        return None;
    }
    let description = match &function["description"] {
        Value::Null => None,
        text => Some(text.as_str()?.to_owned()),
    };
    let mut kind = Tool::new(function["name"].as_str()?, function["parameters"].clone());
    kind.description = description;
    kind.native = native_fields(
        OPENAI,
        rest_of(function, &["name", "description", "parameters"]),
    );
    Some(kind)
}

fn openai_choice(choice: &Value) -> Option<ToolChoice> {
    Some(match choice {
        Value::String(choice) => match choice.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => return None,
        },
        choice if choice["type"] == "function" && only(choice, &["type", "function"]) => {
            if !only(&choice["function"], &["name"]) {
                return None;
            }
            ToolChoice::Tool(choice["function"]["name"].as_str()?.into())
        }
        _ => return None,
    })
}

/// How a refusal names a message's fields.
fn refused_fields(_: &str) -> String {
    "messages with fields the kind has no place for".into()
}

/// The kind's tools as OpenAI's fields of a request `v`.
pub(crate) fn write_openai_tools(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    let tool = |t: &Tool| -> Result<Value, Fault> {
        let mut function = Map::new();
        function.insert("name".into(), t.name.as_str().into());
        if let Some(text) = &t.description {
            function.insert("description".into(), text.as_str().into());
        }
        if !t.parameters.is_null() {
            function.insert("parameters".into(), t.parameters.clone());
        }
        encode_native(
            OPENAI,
            &t.native,
            &mut function,
            tool_field,
            Keep::CACHE_MARKER,
        )?;
        Ok(json!({ "type": "function", "function": function }))
    };
    let tools: Vec<Value> = chat.tools.iter().map(tool).collect::<Result<_, Fault>>()?;
    set(v, "tools", (!tools.is_empty()).then(|| tools.into()));
    let choice = match &chat.options.tool_choice {
        None => None,
        Some(ToolChoice::Auto) => Some("auto".into()),
        Some(ToolChoice::None) => Some("none".into()),
        Some(ToolChoice::Required) => Some("required".into()),
        Some(ToolChoice::Tool(name)) => {
            Some(json!({ "type": "function", "function": { "name": name } }))
        }
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

/// A message's fields of Chat Completions' vendors that no other format has
/// a place for and that it can leave out: a participant's `name`, the
/// reasoning an assistant sends back (`reasoning_content`, `reasoning`,
/// `reasoning_details`, `thinking_blocks`), which a provider takes back only
/// from the format it came in. A message's `cache_control` stays a field of
/// its own, a cache marker ([`Keep::CACHE_MARKER`]).
const INERT_FIELDS: &[&str] = &[
    "name",
    "reasoning_content",
    "reasoning",
    "reasoning_details",
    "thinking_blocks",
];

/// An OpenAI message read as the kind's
/// ([`decode_message`](crate::schema::decode_message)) with what it says
/// beside its content: a tool message as a user message of one result; an
/// assistant's `tool_calls` as calls after its content. Its
/// [`INERT_FIELDS`] are [`INERT`]: written back as they came by this format,
/// dropped by another. What else the kind cannot say of them stays in the
/// message's `native`.
pub(crate) fn read_openai_message(m: &Value, message: &mut Message) {
    if let Some(native) = &mut message.native {
        let mut inert = Map::new();
        for key in INERT_FIELDS {
            if let Some(value) = native.fields.shift_remove(*key) {
                inert.insert((*key).into(), value);
            }
        }
        if !inert.is_empty() {
            native.fields.insert(INERT.into(), inert.into());
        }
    }
    if matches!(&message.role, Role::Native { name, .. } if name == "tool") {
        let Some(id) = m["tool_call_id"].as_str() else {
            return;
        };
        let content = std::mem::take(&mut message.content);
        message.content = vec![Content::tool_result(id, content, false)];
        message.role = Role::User;
        if let Some(native) = &mut message.native {
            native.fields.shift_remove("tool_call_id");
        }
        return;
    }
    let calls = m["tool_calls"].as_array().into_iter().flatten();
    let calls: Option<Vec<Content>> = calls.map(openai_call).collect();
    if let Some(calls) = calls.filter(|calls| !calls.is_empty()) {
        message.content.extend(calls);
        if let Some(native) = &mut message.native {
            native.fields.shift_remove("tool_calls");
        }
    }
}

/// A function call of `tool_calls`, its own fields beyond the spec's, such
/// as a provider's signature of it, kept as its `native`.
fn openai_call(call: &Value) -> Option<Content> {
    let function = &call["function"];
    let function_call = call["type"].is_null() || call["type"] == "function";
    if !function_call || !only(function, &["name", "arguments"]) {
        return None;
    }
    let (id, name) = (call["id"].as_str()?, function["name"].as_str()?);
    let mut part = Content::tool_call(id, name, function["arguments"].as_str()?);
    if let Content::ToolCall { native, .. } = &mut part {
        *native = native_fields(OPENAI, rest_of(call, &["id", "type", "function"]));
    }
    Some(part)
}

/// The kind's calls as OpenAI's `tool_calls`.
fn openai_calls(calls: &[&Content]) -> Result<Vec<Value>, Fault> {
    let mut out = Vec::new();
    for c in calls {
        if let Content::ToolCall {
            id,
            name,
            arguments,
            native,
            ..
        } = c
        {
            let what = |key: &str| format!("a tool call's '{key}'");
            let own = kept_fields(OPENAI, native, what, Keep::CACHE_MARKER)?;
            let function = json!({ "name": name, "arguments": arguments });
            let mut call = json!({ "id": id, "type": "function", "function": function });
            merge_missing(&mut call, own);
            out.push(call);
        }
    }
    Ok(out)
}

/// OpenAI has no place for the fields of a tool result, but drops a cache
/// marker.
fn refuse_part_fields(native: &Option<NativeFields>) -> Result<(), Fault> {
    let what = |key: &str| format!("a tool part's '{key}'");
    kept_fields(OPENAI, native, what, Keep::CACHE_MARKER).map(drop)
}

/// A tool message answering call `id` with `content`.
fn openai_result(
    id: &str,
    content: &[Content],
    is_error: bool,
    native: &Option<NativeFields>,
) -> Result<Map<String, Value>, Fault> {
    refuse_part_fields(native)?;
    // OpenAI's tool messages cannot say the call failed.
    if is_error {
        return Err(untranslatable("a tool result that is an error"));
    }
    let media = |c: &Content| {
        matches!(
            c,
            Content::Image(_) | Content::Audio(_) | Content::Document(_)
        )
    };
    if content.iter().any(media) {
        return Err(untranslatable("media in an OpenAI tool message"));
    }
    let mut v = Map::new();
    v.insert("role".into(), "tool".into());
    v.insert("tool_call_id".into(), id.into());
    v.insert("content".into(), encode_content::<OpenAiChat>(content)?);
    Ok(v)
}

/// A message as OpenAI's: each result a tool message of its own, then, if
/// it says more, the message and its calls in `tool_calls`. It has no field
/// for reasoning.
pub(crate) fn write_openai_message(m: &Message, roles: &Names<Role>) -> Result<Vec<Value>, Fault> {
    let is = |p: &&Content, result: bool| matches!(p, Content::ToolResult { .. }) == result;
    let results: Vec<&Content> = m.content.iter().filter(|p| is(p, true)).collect();
    let rest: Vec<&Content> = m.content.iter().filter(|p| is(p, false)).collect();
    let mut out = Vec::new();
    for result in &results {
        if let Content::ToolResult {
            id,
            content,
            is_error,
            native,
            ..
        } = result
        {
            out.push(openai_result(id, content, *is_error, native)?);
        }
    }
    // A message of one result keeps its other fields, such as a name.
    if let ([only], true) = (&mut out[..], rest.is_empty()) {
        only.extend(message_fields(OPENAI, &m.native, refused_fields)?);
        return Ok(out.into_iter().map(Value::Object).collect());
    }
    let mut out: Vec<Value> = out.into_iter().map(Value::Object).collect();
    if !rest.is_empty() || results.is_empty() || !m.native.is_empty() {
        let call = |p: &&Content| matches!(p, Content::ToolCall { .. });
        let calls: Vec<&Content> = rest.iter().copied().filter(call).collect();
        let mut said = m.clone();
        said.content = rest.into_iter().filter(|p| !call(p)).cloned().collect();
        if said
            .content
            .iter()
            .any(|p| matches!(p, Content::Reasoning { .. }))
        {
            return Err(untranslatable("reasoning in OpenAI Chat"));
        }
        let mut v = encode_message::<OpenAiChat>(&said, roles)?;
        if !calls.is_empty() {
            v["tool_calls"] = openai_calls(&calls)?.into();
            if said.content.is_empty() {
                v["content"] = Value::Null;
            }
        }
        out.push(v);
    }
    Ok(out)
}

/// A part the kind has a form for: an image, audio, or a file of data.
pub(crate) fn decode_part(v: &Value) -> Option<Content> {
    let (kind, mime, source, mut fields, name) = match v["type"].as_str()? {
        "image_url" => {
            let image = &v["image_url"];
            let (mime, src) = source_url(image["url"].as_str()?, "image/*")?;
            let mut fields = remainder(v, &["type", "image_url"]);
            let nested = remainder(image, &["url"]);
            if !nested.is_empty() {
                fields.insert("image_url".into(), nested.into());
            }
            (0, mime, src, fields, None)
        }
        "input_audio" => {
            let audio = &v["input_audio"];
            let mime = match audio["format"].as_str()? {
                "wav" => "audio/wav",
                "mp3" => "audio/mpeg",
                _ => return None,
            };
            let src = MediaSource::Bytes(STANDARD.decode(audio["data"].as_str()?).ok()?.into());
            let mut fields = remainder(v, &["type", "input_audio"]);
            let nested = remainder(audio, &["data", "format"]);
            if !nested.is_empty() {
                fields.insert("input_audio".into(), nested.into());
            }
            (1, mime.into(), src, fields, None)
        }
        "file" => {
            let file = &v["file"];
            if file.get("filename").is_some_and(|v| !v.is_string()) {
                return None;
            }
            let data = file["file_data"].as_str()?;
            if !data.starts_with("data:") {
                return None;
            }
            let (mime, src) = source_url(data, "application/octet-stream")?;
            let mut fields = remainder(v, &["type", "file"]);
            let nested = remainder(file, &["file_data", "filename"]);
            if !nested.is_empty() {
                fields.insert("file".into(), nested.into());
            }
            let name = file["filename"].as_str().map(str::to_owned);
            (2, mime, src, fields, name)
        }
        _ => return None,
    };
    let mut media = Media::new(mime, source);
    media.name = name;
    media.native = native_fields(OPENAI, std::mem::take(&mut fields));
    Some(match kind {
        0 => Content::Image(media),
        1 => Content::Audio(media),
        _ => Content::Document(media),
    })
}

/// A part of the kind's as OpenAI writes it, refusing what it cannot say.
pub(crate) fn encode_part(c: &Content) -> Result<Value, Fault> {
    let (kind, media) = match c {
        Content::Image(m) => ("image", m),
        Content::Audio(m) => ("audio", m),
        Content::Document(m) => ("document", m),
        Content::Reasoning { .. } => {
            return Err(untranslatable("reasoning outside an assistant message"));
        }
        _ => return Err(untranslatable("an unsupported kind of part")),
    };
    if media.name.is_some() && kind != "document" {
        return Err(untranslatable("a display name on image or audio content"));
    }
    let what = |k: &str| format!("media field '{k}'");
    let native = kept_fields(OPENAI, &media.native, what, Keep::CACHE_MARKER)?;
    let url = || match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(data) => {
            format!("data:{};base64,{}", media.mime_type, STANDARD.encode(data))
        }
    };
    let mut v = match kind {
        "image" => json!({"type":"image_url","image_url":{"url":url()}}),
        "document" => {
            if matches!(media.source, MediaSource::Url(_)) {
                return Err(untranslatable("a document URL in OpenAI Chat"));
            }
            let mut v = json!({"type":"file","file":{"file_data":url()}});
            if let Some(name) = &media.name {
                v["file"]["filename"] = name.clone().into();
            }
            v
        }
        _ => {
            let MediaSource::Bytes(bytes) = &media.source else {
                return Err(untranslatable("an audio URL in OpenAI Chat"));
            };
            let encoding = match media.mime_type.as_str() {
                "audio/wav" | "audio/x-wav" => "wav",
                "audio/mpeg" => "mp3",
                _ => return Err(untranslatable("this audio MIME type")),
            };
            json!({"type":"input_audio","input_audio":{"data":STANDARD.encode(bytes),"format":encoding}})
        }
    };
    merge_missing(&mut v, native);
    Ok(v)
}

/// The structured output requirement of `response_format`, if the kind can
/// say all of it: then it leaves the request's `native`.
pub(crate) fn read_response_format(v: &Value, input: &mut Input) {
    let raw = &v["response_format"];
    let alone = raw.as_object().is_some_and(|m| m.len() == 1);
    let parsed = match raw["type"].as_str() {
        Some("text") if alone => Some(ResponseFormat::Text),
        Some("json_object") if alone => Some(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let spec = &raw["json_schema"];
            let known = ["name", "description", "schema", "strict"];
            let clean = spec
                .as_object()
                .is_some_and(|m| m.keys().all(|k| known.contains(&k.as_str())));
            let wrapped = raw
                .as_object()
                .is_some_and(|m| m.keys().all(|k| k == "type" || k == "json_schema"));
            let typed = spec["name"].is_string()
                && spec.get("description").is_none_or(Value::is_string)
                && spec.get("strict").is_none_or(Value::is_boolean)
                && spec["schema"].is_object();
            (clean && wrapped && typed).then(|| ResponseFormat::JsonSchema {
                name: spec["name"].as_str().unwrap_or("response").into(),
                description: spec["description"].as_str().map(str::to_owned),
                schema: spec["schema"].clone(),
                strict: spec["strict"].as_bool(),
            })
        }
        _ => None,
    };
    if let Some(parsed) = parsed {
        input.options.response_format = Some(parsed);
        if let Some(native) = &mut input.native {
            native.fields.remove("response_format");
        }
        if input.native.is_empty() {
            input.native = None;
        }
    }
}

/// The request's `response_format`: the kind's structured output
/// requirement, else the request's own.
pub(crate) fn write_response_format(input: &Input) -> Option<Value> {
    let Some(requirement) = &input.options.response_format else {
        let own = input.native.format() == Some(OPENAI);
        return own
            .then(|| input.native.fields().get("response_format").cloned())
            .flatten();
    };
    Some(match requirement {
        ResponseFormat::Text => json!({"type":"text"}),
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            let mut spec = json!({"name":name,"schema":schema});
            if let Some(description) = description {
                spec["description"] = description.clone().into();
            }
            if let Some(strict) = strict {
                spec["strict"] = (*strict).into();
            }
            json!({"type":"json_schema","json_schema":spec})
        }
    })
}

/// The request's `reasoning_effort`: the kind's effort, else the request's
/// own. Chat has no token budget, and no field that returns the thinking.
pub(crate) fn write_reasoning(input: &Input) -> Result<Option<Value>, Fault> {
    let Some(r) = reasoning_of(input)? else {
        return Ok(own_native(input, OPENAI, "reasoning_effort"));
    };
    if r.budget_tokens.is_some() {
        return Err(untranslatable("a reasoning token budget in OpenAI Chat"));
    }
    if r.summary == Some(true) {
        return Err(untranslatable("returned reasoning in OpenAI Chat"));
    }
    Ok(match r.effort {
        Some(effort) => Some(effort_name(effort)?.into()),
        None => own_native(input, OPENAI, "reasoning_effort"),
    })
}
