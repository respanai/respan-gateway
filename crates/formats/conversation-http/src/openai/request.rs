//! What a request says beyond text, in OpenAI's terms: tools and their
//! choice, an assistant's calls and their results, media, and structured
//! output. What the kind has no form
//! for, such as a tool the provider runs, stays in the `native` of the
//! request, the message or the tool.

use crate::Schema;
use crate::fields::media_url;
use crate::schema::{
    FAILED_RESULT_PREFIX, Names, decode_tools, each_tool, edit_native, effort_for, effort_name,
    encode_content, encode_message, merge_fields, merge_missing, only, read_search, remainder,
    rest_of, set, source_url, take_strict, untranslatable, write_search,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{
    Content, Input, Media, MediaSource, Message, ProviderTool, ResponseFormat, Role, Tool,
    ToolChoice,
};
use respan_gateway_core::Fault;
use serde_json::{Map, Value, json};

use crate::openai::{OpenAiChat, take_signature};

/// OpenAI's function tools, choice, `parallel_tool_calls` and
/// `web_search_options`, as the kind's.
pub(crate) fn read_openai_tools(v: &Value, chat: &mut Input) {
    let tools = each_tool(&v["tools"], openai_tool, |_| None);
    let choice = match &v["tool_choice"] {
        Value::Null => Some(None),
        choice => openai_choice(choice).map(Some),
    };
    let parallel = match &v["parallel_tool_calls"] {
        Value::Null => Some(None),
        parallel => parallel.as_bool().map(Some),
    };
    let keys = ["tools", "tool_choice", "parallel_tool_calls"];
    decode_tools::<OpenAiChat>(chat, tools, choice, parallel, keys);
    // Any object asks for a search, an empty one too.
    if let Some(Value::Object(options)) = v.get("web_search_options") {
        let search = read_search::<OpenAiChat>(options.clone());
        chat.provider_tools.push(search);
        edit_native::<OpenAiChat, _>(&mut chat.native, |n| n.shift_remove("web_search_options"));
    }
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
    kind.native = OpenAiChat::native(rest_of(function, &["name", "description", "parameters"]));
    take_strict::<OpenAiChat>(&mut kind);
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

/// The kind's tools as OpenAI's fields of a request `v`.
pub(crate) fn write_openai_tools(chat: &Input, v: &mut Map<String, Value>) -> Result<(), Fault> {
    let tool = |t: &Tool| {
        let mut function = Map::new();
        function.insert("name".into(), t.name.as_str().into());
        if let Some(text) = &t.description {
            function.insert("description".into(), text.as_str().into());
        }
        if !t.parameters.is_null() {
            function.insert("parameters".into(), t.parameters.clone());
        }
        if t.strict == Some(true) {
            function.insert("strict".into(), true.into());
        }
        function.extend(OpenAiChat::fields(&t.native).clone());
        json!({ "type": "function", "function": function })
    };
    let tools: Vec<Value> = chat.tools.iter().map(tool).collect();
    set(v, "tools", (!tools.is_empty()).then(|| tools.into()));
    // WARN: Chat has one search; the others are dropped.
    if let Some(search @ ProviderTool::WebSearch { .. }) = chat.provider_tools.first() {
        v.insert(
            "web_search_options".into(),
            write_search::<OpenAiChat>(search)?.into(),
        );
    }
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

/// An OpenAI message read as the kind's
/// ([`decode_message`](crate::schema::decode_message)) with what it says
/// beside its content: a tool message as a user message of one result; an
/// assistant's `tool_calls` as calls after its content. What else the kind
/// cannot say of them stays in the message's `native`.
pub(crate) fn read_openai_message(m: &Value, message: &mut Message) {
    if matches!(&message.role, Role::Native(n) if OpenAiChat::value(n) == Some(&"tool".into())) {
        let Some(id) = m["tool_call_id"].as_str() else {
            return;
        };
        let content = std::mem::take(&mut message.content);
        message.content = vec![Content::tool_result(id, content, false)];
        message.role = Role::User;
        edit_native::<OpenAiChat, _>(&mut message.native, |n| n.shift_remove("tool_call_id"));
        return;
    }
    let calls = m["tool_calls"].as_array().into_iter().flatten();
    let calls: Option<Vec<Vec<Content>>> = calls.map(openai_call).collect();
    if let Some(calls) = calls.filter(|calls| !calls.is_empty()) {
        message.content.extend(calls.concat());
        edit_native::<OpenAiChat, _>(&mut message.native, |n| n.shift_remove("tool_calls"));
    }
    // A text's signature is the message's: its last text's, even an empty one.
    let signature = edit_native::<OpenAiChat, _>(&mut message.native, take_signature);
    if let Some(s) = signature {
        let text = |c: &Content| matches!(c, Content::Text { .. });
        let content = &mut message.content;
        if !content.iter().any(text) {
            content.insert(0, Content::from(""));
        }
        let at = content.iter().rposition(text).map_or(0, |at| at + 1);
        content.insert(at, Content::Signature(s));
    }
}

/// A function call of `tool_calls`, its own fields beyond the spec's kept
/// as its `native`, and its signature after it.
fn openai_call(call: &Value) -> Option<Vec<Content>> {
    let function = &call["function"];
    let function_call = call["type"].is_null() || call["type"] == "function";
    if !function_call || !only(function, &["name", "arguments"]) {
        return None;
    }
    let (id, name) = (call["id"].as_str()?, function["name"].as_str()?);
    let mut part = Content::tool_call(id, name, function["arguments"].as_str()?);
    let mut fields = rest_of(call, &["id", "type", "function"]);
    let signature = take_signature(&mut fields).map(Content::Signature);
    if let Content::ToolCall { native, .. } = &mut part {
        *native = OpenAiChat::native(fields);
    }
    Some([part].into_iter().chain(signature).collect())
}

/// The kind's calls as OpenAI's `tool_calls`.
fn openai_calls(calls: &[&Content]) -> Vec<Value> {
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
            let own = OpenAiChat::fields(native).clone();
            let function = json!({ "name": name, "arguments": arguments });
            let mut call = json!({ "id": id, "type": "function", "function": function });
            merge_missing(&mut call, own);
            out.push(call);
        }
    }
    out
}

/// A tool message answering call `id` with `content`.
fn openai_result(
    id: &str,
    content: &[Content],
    is_error: bool,
) -> Result<Map<String, Value>, Fault> {
    // OpenAI's tool messages cannot say the call failed: their text does.
    let failed = is_error.then(|| Content::from(FAILED_RESULT_PREFIX));
    let content: Vec<Content> = failed.into_iter().chain(content.iter().cloned()).collect();
    let mut v = Map::new();
    v.insert("role".into(), "tool".into());
    v.insert("tool_call_id".into(), id.into());
    v.insert("content".into(), encode_content::<OpenAiChat>(&content)?);
    Ok(v)
}

/// A message as OpenAI's: each result a tool message of its own, then, if
/// it says more, the message and its calls in `tool_calls`. A tool message
/// holds text alone, so the results' media leads that message. It has no
/// field for reasoning, which is dropped.
pub(crate) fn write_openai_message(m: &Message, roles: &Names<Role>) -> Result<Vec<Value>, Fault> {
    let is = |p: &&Content, result: bool| matches!(p, Content::ToolResult { .. }) == result;
    let results: Vec<&Content> = m.content.iter().filter(|p| is(p, true)).collect();
    let rest: Vec<&Content> = m.content.iter().filter(|p| is(p, false)).collect();
    let mut out = Vec::new();
    let mut shown = Vec::new();
    for result in &results {
        if let Content::ToolResult {
            id,
            content,
            is_error,
            ..
        } = result
        {
            let (media, mut text): (Vec<_>, Vec<_>) = content.iter().cloned().partition(|c| {
                matches!(
                    c,
                    Content::Image(_) | Content::Audio(_) | Content::Document(_)
                )
            });
            if !media.is_empty() {
                if text.is_empty() {
                    text.push("The result is in the next message.".into());
                }
                shown.push(format!("Media returned by tool call {id}:").into());
                shown.extend(media);
            }
            out.push(openai_result(id, &text, *is_error)?);
        }
    }
    let rest: Vec<Content> = shown.into_iter().chain(rest.into_iter().cloned()).collect();
    // A message of one result keeps its other fields, such as a name.
    if let ([only], true) = (&mut out[..], rest.is_empty()) {
        only.extend(OpenAiChat::fields(&m.native).clone());
        return Ok(out.into_iter().map(Value::Object).collect());
    }
    let mut out: Vec<Value> = out.into_iter().map(Value::Object).collect();
    if !rest.is_empty() || results.is_empty() || !m.native.is_empty() {
        let call = |p: &&Content| matches!(p, Content::ToolCall { .. });
        let calls: Vec<&Content> = rest.iter().filter(call).collect();
        let mut said = m.clone();
        said.content = rest.iter().filter(|p| !call(p)).cloned().collect();
        said.content.retain(|p| match p {
            // WARN: Chat has no field for a past turn's reasoning; it is dropped silently.
            Content::Reasoning { .. } => false,
            // WARN: OpenAI's Chat has no field for a signature, and a provider may refuse one; it is dropped silently.
            Content::Signature(_) | Content::Withheld(_) => false,
            _ => true,
        });
        let mut v = encode_message::<OpenAiChat>(&said, roles)?;
        // WARN: a message whose every part was dropped is left out.
        let dropped = v["content"] == json!([]);
        if !calls.is_empty() {
            v["tool_calls"] = openai_calls(&calls).into();
            if said.content.is_empty() || dropped {
                v["content"] = Value::Null;
            }
        } else if dropped {
            return Ok(out);
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
    media.native = OpenAiChat::native(std::mem::take(&mut fields));
    Some(match kind {
        0 => Content::Image(media),
        1 => Content::Audio(media),
        _ => Content::Document(media),
    })
}

/// A part of the kind's as OpenAI writes it, `Null` for one it cannot say.
// WARN: another part, a document or audio by URL, audio but WAV or MP3, and
// a display name on an image or audio, have no form in Chat; dropped.
pub(crate) fn encode_part(c: &Content) -> Result<Value, Fault> {
    let (kind, media) = match c {
        Content::Image(m) => ("image", m),
        Content::Audio(m) => ("audio", m),
        Content::Document(m) => ("document", m),
        _ => return Ok(Value::Null),
    };
    let native = OpenAiChat::fields(&media.native).clone();
    let url = || media_url(media);
    let mut v = match kind {
        "image" => json!({"type":"image_url","image_url":{"url":url()}}),
        "document" => {
            if matches!(media.source, MediaSource::Url(_)) {
                return Ok(Value::Null);
            }
            let mut v = json!({"type":"file","file":{"file_data":url()}});
            if let Some(name) = &media.name {
                v["file"]["filename"] = name.clone().into();
            }
            v
        }
        _ => {
            let MediaSource::Bytes(bytes) = &media.source else {
                return Ok(Value::Null);
            };
            let encoding = match media.mime_type.as_str() {
                "audio/wav" | "audio/x-wav" => "wav",
                "audio/mpeg" => "mp3",
                _ => return Ok(Value::Null),
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
            })
        }
        _ => None,
    };
    if let Some(parsed) = parsed {
        input.options.response_format = Some(parsed);
        // Its `strict` is Chat's own, which only Chat writes back.
        edit_native::<OpenAiChat, _>(&mut input.native, |native| {
            match raw["json_schema"].get("strict") {
                Some(strict) => {
                    _ = native.insert(
                        "response_format".into(),
                        json!({"json_schema": {"strict": strict}}),
                    )
                }
                None => _ = native.remove("response_format"),
            }
        });
    }
}

/// The request's `response_format`: the kind's structured output
/// requirement, else the request's own.
pub(crate) fn write_response_format(input: &Input) -> Option<Value> {
    let own = OpenAiChat::fields(&input.native)
        .get("response_format")
        .cloned();
    let Some(requirement) = &input.options.response_format else {
        // Not a `strict` alone, of a format no longer asked for.
        return own.filter(|own| own["type"].is_string());
    };
    Some(match requirement {
        ResponseFormat::Text => json!({"type":"text"}),
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
        } => {
            let mut spec = json!({"name":name,"schema":schema});
            if let Some(description) = description {
                spec["description"] = description.clone().into();
            }
            let mut format = json!({"type":"json_schema","json_schema":spec});
            if let Some(Value::Object(own)) = own {
                merge_fields(&mut format, &own);
            }
            format
        }
    })
}

/// The request's `reasoning_effort`: the kind's effort, or the one nearest
/// its token budget, else the request's own. Chat drops an ask for a summary.
pub(crate) fn write_reasoning(input: &Input) -> Result<Option<Value>, Fault> {
    let r = input.options.reasoning.as_ref();
    // WARN: Chat has no field for a reasoning summary; the ask is dropped silently.
    Ok(
        match r.and_then(|r| r.effort.or(r.budget_tokens.map(effort_for))) {
            Some(effort) => Some(effort_name(effort)?.into()),
            None => OpenAiChat::fields(&input.native)
                .get("reasoning_effort")
                .cloned(),
        },
    )
}
