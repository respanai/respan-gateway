//! OpenAI's Conversations API, of the Responses format: the routes a
//! conversation's state is reached by, read as [`Operation`]s, and the JSON
//! of what they answer. No model is called: whoever keeps conversations,
//! such as the gateway's `plugins.responses`, answers each with
//! [`answer`], which the format writes as the body whole.
//!
//! A conversation's items are Responses items, each with an `id` the API
//! mints: what a client adds is read as OpenAI reads it, refused as it
//! refuses it, and given the type, content parts, `status` and `id` the API
//! lists it with ([`items`]); a response's output is its items as the format
//! writes them ([`output_items`]). [`messages`] reads them back as the
//! conversation they are. A reasoning item's encrypted content is kept, and
//! shown only to a request that includes it ([`shown`]).
//!
//! The routes, under any prefix, such as `/v1`:
//!
//! - `POST /conversations`, `GET`, `POST` (its metadata, merged) and
//!   `DELETE` `/conversations/{id}`;
//! - `GET /conversations/{id}/items` (`limit`, 0 to 100, 20 by default;
//!   `order`, `desc` by default; `after`, an item's id; `include[]`) and
//!   `POST` (1 to 20 items);
//! - `GET` and `DELETE /conversations/{id}/items/{item_id}`.
//!
//! A request said wrong is refused as OpenAI refuses it, an
//! `invalid_request_error` naming its `param` and `code` ([`invalid`]).

use std::borrow::Cow;

use respan_gateway_conversation::{Assembled, Input, Message, Output};
use respan_gateway_core::{Connection, Details, Fault};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderValue, Method, header::CONTENT_TYPE};
use respan_gateway_http::{ClientIo, Encoder};
use serde_json::{Map, Value, json};

use super::{OpenAiResponses, request};
use crate::Schema;
use crate::openai::ErrorType;
use crate::schema::Own;

/// The most items a request adds at once.
pub const MAX_ITEMS: usize = 20;
/// The most metadata pairs; a key's most characters, and a value's.
pub const METADATA: (usize, usize, usize) = (16, 64, 512);
/// What a request may include of its items.
const INCLUDABLE: &[&str] = &[
    "file_search_call.results",
    "web_search_call.action.sources",
    "message.input_image.image_url",
    "computer_call_output.output.image_url",
    "code_interpreter_call.outputs",
    ENCRYPTED,
    "message.output_text.logprobs",
];
/// What includes a reasoning item's encrypted content.
const ENCRYPTED: &str = "reasoning.encrypted_content";

/// One request of the Conversations API.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Operation {
    /// Make a conversation of these items ([`items`]'s) and metadata.
    Create {
        /// Its first items.
        items: Vec<Value>,
        /// Its metadata.
        metadata: Map<String, Value>,
    },
    /// A conversation.
    Retrieve {
        /// The conversation.
        id: String,
    },
    /// Merge metadata into a conversation's.
    Update {
        /// The conversation.
        id: String,
        /// The keys it sets.
        metadata: Map<String, Value>,
    },
    /// Delete a conversation.
    Delete {
        /// The conversation.
        id: String,
    },
    /// A page of a conversation's items.
    List {
        /// The conversation.
        id: String,
        /// Which page.
        page: Page,
    },
    /// Add items ([`items`]'s) to a conversation's end.
    Add {
        /// The conversation.
        id: String,
        /// What it adds.
        items: Vec<Value>,
        /// Whether its answer shows reasoning's encrypted content.
        encrypted: bool,
    },
    /// One of a conversation's items.
    Item {
        /// The conversation.
        id: String,
        /// The item.
        item: String,
        /// Whether it shows reasoning's encrypted content.
        encrypted: bool,
    },
    /// Delete one of a conversation's items.
    DeleteItem {
        /// The conversation.
        id: String,
        /// The item.
        item: String,
    },
}

/// A page of items: up to `limit` after the item `after`, in the order
/// asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The item it starts after, if any.
    pub after: Option<String>,
    /// The most items it holds.
    pub limit: usize,
    /// Oldest first, else newest.
    pub ascending: bool,
    /// Whether it shows reasoning's encrypted content.
    pub encrypted: bool,
}

impl Operation {
    /// The operation `request` asks for, if it is one of the API's routes;
    /// `Err` if it is, but says it wrong.
    pub fn read(request: &http::Request<Bytes>) -> Option<Result<Self, Fault>> {
        let route = route(request.method(), request.uri().path())?;
        Some(route.read(request.uri().query(), request.body()))
    }

    /// The conversation it is on, `None` for `Create`.
    pub fn conversation(&self) -> Option<&str> {
        match self {
            Self::Create { .. } => None,
            Self::Retrieve { id }
            | Self::Update { id, .. }
            | Self::Delete { id }
            | Self::List { id, .. }
            | Self::Add { id, .. }
            | Self::Item { id, .. }
            | Self::DeleteItem { id, .. } => Some(id),
        }
    }
}

/// A 400 as OpenAI's API refuses: an `invalid_request_error` naming the
/// `param` it is of and its `code`, if any.
pub fn invalid(param: Option<&str>, code: Option<&str>, message: impl Into<String>) -> Fault {
    let fault = Fault::bad_request(message);
    described(fault, param, code)
}

/// A 404 as OpenAI's API answers one, naming `param` if it is of one.
fn missing(param: Option<&str>, message: impl Into<String>) -> Fault {
    described(Fault::not_found(message), param, None)
}

fn described(fault: Fault, param: Option<&str>, code: Option<&str>) -> Fault {
    let mut details = Map::new();
    details.insert("param".into(), param.into());
    details.insert("code".into(), code.into());
    let error_type = ErrorType(Cow::Borrowed("invalid_request_error"));
    fault.with(error_type).with(Details(details))
}

/// No conversation `id`.
pub fn no_conversation(id: &str) -> Fault {
    missing(None, format!("Conversation with id '{id}' not found."))
}

/// No item `item` in the conversation, found to be read (`read`) or deleted.
pub fn no_item(item: &str, read: bool) -> Fault {
    let place = if read { " in conversation" } else { "" };
    missing(None, format!("Item with id '{item}' not found{place}."))
}

/// No item `after` for a page to start after.
pub fn no_after(after: &str) -> Fault {
    missing(Some("after"), format!("No item found with id '{after}'"))
}

/// A request naming both a `previous_response_id` and a `conversation`.
pub fn both_named() -> Fault {
    let message = "Mutually exclusive parameters: Ensure you are only providing one of: \
                   'previous_response_id' or 'conversation'.";
    invalid(None, Some("mutually_exclusive_parameters"), message)
}

/// Metadata merged past its most pairs, `had` before.
pub fn too_much_metadata(merged: usize, had: usize) -> Fault {
    let (pairs, _, _) = METADATA;
    let message = format!(
        "Invalid 'metadata': too many properties after update. Metadata can have at most \
         {pairs} properties, but your update resulted in an object with {merged} properties \
         (up from {had})."
    );
    invalid(
        Some("metadata"),
        Some("metadata_max_properties_exceeded"),
        message,
    )
}

/// Which route, and the ids its path names.
enum Route {
    Create,
    Retrieve(String),
    Update(String),
    Delete(String),
    List(String),
    Add(String),
    Item(String, String),
    DeleteItem(String, String),
}

/// The route of `method` at `path`, ending in one of the API's.
fn route(method: &Method, path: &str) -> Option<Route> {
    let segments: Vec<String> = (path.split('/').filter(|s| !s.is_empty()))
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect();
    let from_end = |n: usize| {
        segments
            .len()
            .checked_sub(n + 1)
            .map(|i| segments[i].as_str())
    };
    let id = |n: usize| from_end(n).unwrap_or_default().to_owned();
    let tail = if from_end(0) == Some("conversations") {
        0
    } else if from_end(1) == Some("conversations") {
        1
    } else if from_end(2) == Some("conversations") && from_end(0) == Some("items") {
        2
    } else if from_end(3) == Some("conversations") && from_end(1) == Some("items") {
        3
    } else {
        return None;
    };
    Some(match (tail, method) {
        (0, &Method::POST) => Route::Create,
        (1, &Method::GET) => Route::Retrieve(id(0)),
        (1, &Method::POST) => Route::Update(id(0)),
        (1, &Method::DELETE) => Route::Delete(id(0)),
        (2, &Method::GET) => Route::List(id(1)),
        (2, &Method::POST) => Route::Add(id(1)),
        (3, &Method::GET) => Route::Item(id(2), id(0)),
        (3, &Method::DELETE) => Route::DeleteItem(id(2), id(0)),
        _ => return None,
    })
}

impl Route {
    fn read(self, query: Option<&str>, body: &[u8]) -> Result<Operation, Fault> {
        Ok(match self {
            Self::Create => {
                let body = object(body, true, &["items", "metadata"])?;
                let items = match body.get("items") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(raw) => items(list(raw, false)?)?,
                };
                let metadata = match body.get("metadata") {
                    None | Some(Value::Null) => Map::new(),
                    Some(raw) => metadata(raw)?,
                };
                Operation::Create { items, metadata }
            }
            Self::Retrieve(id) => Operation::Retrieve {
                id: conversation_id(id)?,
            },
            Self::Update(id) => {
                let id = conversation_id(id)?;
                let body = object(body, false, &["metadata"])?;
                let metadata = match body.get("metadata") {
                    None => return Err(required("metadata")),
                    Some(Value::Null) => {
                        let message = "Invalid type for 'metadata': expected a metadata object, but got null instead.";
                        return Err(invalid(Some("metadata"), Some("invalid_type"), message));
                    }
                    Some(raw) => metadata(raw)?,
                };
                Operation::Update { id, metadata }
            }
            Self::Delete(id) => Operation::Delete {
                id: conversation_id(id)?,
            },
            Self::List(id) => Operation::List {
                id: conversation_id(id)?,
                page: page(query)?,
            },
            Self::Add(id) => {
                let id = conversation_id(id)?;
                let body = object(body, false, &["items"])?;
                let raw = match body.get("items") {
                    None => return Err(required("items")),
                    Some(raw) => list(raw, true)?,
                };
                Operation::Add {
                    id,
                    items: items(raw)?,
                    encrypted: page(query)?.encrypted,
                }
            }
            Self::Item(id, item) => Operation::Item {
                id: conversation_id(id)?,
                item: item_id(item)?,
                encrypted: page(query)?.encrypted,
            },
            Self::DeleteItem(id, item) => Operation::DeleteItem {
                id: conversation_id(id)?,
                item: item_id(item)?,
            },
        })
    }
}

fn required(param: &str) -> Fault {
    let message = format!("Missing required parameter: '{param}'.");
    invalid(Some(param), Some("missing_required_parameter"), message)
}

/// `id` if it is a conversation's, beginning with `conv`.
fn conversation_id(id: String) -> Result<String, Fault> {
    if id.starts_with("conv") && is_id(&id) {
        return Ok(id);
    }
    let message =
        format!("Invalid 'conversation_id': '{id}'. Expected an ID that begins with 'conv'.");
    Err(invalid(
        Some("conversation_id"),
        Some("invalid_value"),
        message,
    ))
}

/// `item` if it is an id: letters, numbers, underscores or dashes.
fn item_id(item: String) -> Result<String, Fault> {
    if is_id(&item) {
        return Ok(item);
    }
    let message = format!(
        "Invalid 'item_id': '{item}'. Expected an ID that contains letters, numbers, \
         underscores, or dashes, but this value contained additional characters."
    );
    Err(invalid(Some("item_id"), Some("invalid_value"), message))
}

fn is_id(id: &str) -> bool {
    !id.is_empty() && (id.chars()).all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The name of `v`'s JSON type, as OpenAI's refusals say it.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "an integer",
        Value::Number(_) => "a decimal",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A body's JSON object of no keys but `known`; with `empty`, no body is
/// an empty one.
fn object(body: &[u8], empty: bool, known: &[&str]) -> Result<Map<String, Value>, Fault> {
    if empty && body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let body = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(body)) => body,
        Ok(other) => {
            let message = format!(
                "Invalid body: expected an object, but got {} instead.",
                type_name(&other)
            );
            return Err(invalid(None, Some("invalid_type"), message));
        }
        Err(_) => {
            return Err(invalid(
                None,
                Some("invalid_json"),
                "The body is not valid JSON.",
            ));
        }
    };
    if let Some(unknown) = body.keys().find(|k| !known.contains(&k.as_str())) {
        let message = format!("Unknown parameter: '{unknown}'.");
        return Err(invalid(Some(unknown), Some("unknown_parameter"), message));
    }
    Ok(body)
}

/// `raw` as up to [`MAX_ITEMS`] items, at least one if `some`.
fn list(raw: &Value, some: bool) -> Result<&[Value], Fault> {
    let Some(items) = raw.as_array() else {
        let message = format!(
            "Invalid type for 'items': expected an array of input items, but got {} instead.",
            type_name(raw)
        );
        return Err(invalid(Some("items"), Some("invalid_type"), message));
    };
    if some && items.is_empty() {
        let message = "Invalid 'items': empty array. Expected an array with minimum length 1, but got an empty array instead.";
        return Err(invalid(Some("items"), Some("empty_array"), message));
    }
    if items.len() > MAX_ITEMS {
        let message = format!(
            "Invalid 'items': array too long. Expected an array with maximum length {MAX_ITEMS}, \
             but got an array with length {} instead.",
            items.len()
        );
        return Err(invalid(
            Some("items"),
            Some("array_above_max_length"),
            message,
        ));
    }
    Ok(items)
}

/// Metadata: up to 16 pairs of strings, a key up to 64 characters, a value
/// up to 512.
fn metadata(raw: &Value) -> Result<Map<String, Value>, Fault> {
    let (pairs, key, value) = METADATA;
    let Value::Object(metadata) = raw else {
        let message = format!(
            "Invalid type for 'metadata': expected a metadata object, but got {} instead.",
            type_name(raw)
        );
        return Err(invalid(Some("metadata"), Some("invalid_type"), message));
    };
    if metadata.len() > pairs {
        let message = format!(
            "Invalid 'metadata': too many properties. Expected an object with at most {pairs} \
             properties, but got an object with {} properties instead.",
            metadata.len()
        );
        return Err(invalid(
            Some("metadata"),
            Some("object_above_max_properties"),
            message,
        ));
    }
    for (k, v) in metadata {
        let param = format!("metadata.{k}");
        if k.chars().count() > key {
            let message = format!("Invalid 'metadata': a key is longer than {key} characters.");
            return Err(invalid(Some("metadata"), Some("invalid_value"), message));
        }
        let Some(text) = v.as_str() else {
            let message = format!(
                "Invalid type for '{param}': expected a string, but got {} instead.",
                type_name(v)
            );
            return Err(invalid(Some(&param), Some("invalid_type"), message));
        };
        let length = text.chars().count();
        if length > value {
            let message = format!(
                "Invalid '{param}': string too long. Expected a string with maximum length \
                 {value}, but got a string with length {length} instead."
            );
            return Err(invalid(
                Some(&param),
                Some("string_above_max_length"),
                message,
            ));
        }
    }
    Ok(metadata.clone())
}

/// A page from `query`'s `limit`, `order`, `after` and `include[]`.
fn page(query: Option<&str>) -> Result<Page, Fault> {
    let mut page = Page {
        after: None,
        limit: 20,
        ascending: false,
        encrypted: false,
    };
    let pairs = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|p| p.split_once('='));
    let mut included = 0;
    for (key, value) in pairs {
        let key = percent_encoding::percent_decode_str(key).decode_utf8_lossy();
        let value = percent_encoding::percent_decode_str(value).decode_utf8_lossy();
        match &*key {
            "limit" => {
                let limit = value.parse::<i64>().map_err(|_| {
                    let message = "Invalid type for 'limit': expected an integer, but got a \
                                   string value that could not be converted into an integer.";
                    invalid(Some("limit"), Some("invalid_type"), message)
                })?;
                page.limit = match limit {
                    ..0 => {
                        let message = format!(
                            "Invalid 'limit': integer below minimum value. Expected a value >= 0, but got {limit} instead."
                        );
                        return Err(invalid(
                            Some("limit"),
                            Some("integer_below_min_value"),
                            message,
                        ));
                    }
                    101.. => {
                        let message = format!(
                            "Invalid 'limit': integer above maximum value. Expected a value <= 100, but got {limit} instead."
                        );
                        return Err(invalid(
                            Some("limit"),
                            Some("integer_above_max_value"),
                            message,
                        ));
                    }
                    _ => usize::try_from(limit).unwrap_or_default(),
                };
            }
            "order" => {
                page.ascending = match &*value {
                    "asc" => true,
                    "desc" => false,
                    _ => {
                        let message = format!(
                            "Invalid value: '{value}'. Supported values are: 'asc' and 'desc'."
                        );
                        return Err(invalid(Some("order"), Some("invalid_value"), message));
                    }
                };
            }
            "after" => page.after = Some(value.into_owned()),
            "include[]" => {
                if !INCLUDABLE.contains(&&*value) {
                    let supported = INCLUDABLE
                        .iter()
                        .map(|v| format!("'{v}'"))
                        .collect::<Vec<_>>();
                    let message = format!(
                        "Invalid value: '{value}'. Supported values are: {}.",
                        supported.join(", ")
                    );
                    let param = format!("include[{included}]");
                    return Err(invalid(Some(&param), Some("invalid_value"), message));
                }
                page.encrypted |= value == ENCRYPTED;
                included += 1;
            }
            "include" => {
                let message =
                    "Invalid type for 'include': expected an array, but got a string instead.";
                return Err(invalid(Some("include"), Some("invalid_type"), message));
            }
            _ => {}
        }
    }
    Ok(page)
}

/// Items a client adds, as a conversation lists them ([`listed`]), each
/// refused as OpenAI refuses it: a message of a role it has, of its content
/// parts; a function's call or output whole; reasoning only with its
/// encrypted content.
pub fn items(raw: &[Value]) -> Result<Vec<Value>, Fault> {
    for (i, item) in raw.iter().enumerate() {
        check(i, item)?;
    }
    let items: Vec<Value> = raw.iter().cloned().map(listed).collect();
    let mut read = Input::default();
    let at = |e: Fault| invalid(Some("items"), Some("invalid_value"), e.message().to_owned());
    request::read_input(&json!({ "input": items }), &mut read).map_err(at)?;
    Ok(items)
}

/// Refuses item `i` of a request, `item`, as OpenAI does.
fn check(i: usize, item: &Value) -> Result<(), Fault> {
    let param = format!("items[{i}]");
    let Some(fields) = item.as_object() else {
        let message = format!(
            "Invalid type for '{param}': expected an input item, but got {} instead.",
            type_name(item)
        );
        return Err(invalid(Some(&param), Some("invalid_type"), message));
    };
    let text = |key: &str| fields.get(key).and_then(Value::as_str);
    let need = |key: &str| match fields.get(key) {
        Some(Value::String(_)) => Ok(()),
        None | Some(Value::Null) => Err(required(&format!("{param}.{key}"))),
        Some(v) => {
            let message = format!(
                "Invalid type for '{param}.{key}': expected a string, but got {} instead.",
                type_name(v)
            );
            Err(invalid(
                Some(&format!("{param}.{key}")),
                Some("invalid_type"),
                message,
            ))
        }
    };
    match fields.get("type").map_or(Some("message"), Value::as_str) {
        Some("message") => {
            let role = text("role").unwrap_or_default();
            let parts: &[&str] = match role {
                "assistant" => &["output_text", "refusal", "input_text"],
                "user" | "system" | "developer" => &["input_text", "input_image", "input_file"],
                _ => {
                    let message = format!(
                        "Invalid value: '{role}'. Supported values are: 'assistant', 'system', 'developer', and 'user'."
                    );
                    return Err(invalid(Some(&param), Some("invalid_value"), message));
                }
            };
            match fields.get("content") {
                Some(Value::String(_)) => Ok(()),
                Some(Value::Array(content)) => {
                    for (j, part) in content.iter().enumerate() {
                        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
                        if !parts.contains(&kind) {
                            let supported =
                                parts.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>();
                            let message = format!(
                                "Invalid value: '{kind}'. Supported values are: {}.",
                                supported.join(", ")
                            );
                            let param = format!("{param}.content[{j}]");
                            return Err(invalid(Some(&param), Some("invalid_value"), message));
                        }
                    }
                    Ok(())
                }
                None | Some(Value::Null) => Err(required(&format!("{param}.content"))),
                Some(v) => {
                    let message = format!(
                        "Invalid type for '{param}.content': expected one of an array of objects or string, but got {} instead.",
                        type_name(v)
                    );
                    Err(invalid(
                        Some(&format!("{param}.content")),
                        Some("invalid_type"),
                        message,
                    ))
                }
            }
        }
        Some("function_call") => ["call_id", "name", "arguments"]
            .into_iter()
            .try_for_each(need),
        Some("function_call_output") => {
            need("call_id")?;
            match fields.get("output") {
                Some(Value::String(_) | Value::Array(_)) => Ok(()),
                _ => Err(required(&format!("{param}.output"))),
            }
        }
        Some("reasoning")
            if text("encrypted_content").is_some()
                && fields.get("summary").is_some_and(Value::is_array) =>
        {
            Ok(())
        }
        Some("reasoning") => Err(invalid(Some("item"), Some("invalid_item"), "Invalid item")),
        kind => {
            let message = format!(
                "Invalid value: '{}'. Supported values are: 'function_call', 'function_call_output', 'message', and 'reasoning'.",
                kind.unwrap_or_default()
            );
            Err(invalid(Some(&param), Some("invalid_value"), message))
        }
    }
}

/// `item` as a conversation lists it: an `id` of its own, whatever it said;
/// a message's `type`, its text as a part and its `status`; a function
/// call's `status` and an output's. What else it says is kept.
fn listed(item: Value) -> Value {
    let Value::Object(mut item) = item else {
        return item;
    };
    if !item.contains_key("type") && item.contains_key("role") {
        item.insert("type".into(), "message".into());
    }
    let (prefix, status) = match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            match item.get_mut("content") {
                Some(Value::String(text)) => {
                    let text = std::mem::take(text);
                    item.insert(
                        "content".into(),
                        json!([{ "type": "input_text", "text": text }]),
                    );
                }
                Some(Value::Array(parts)) => {
                    for part in parts.iter_mut().filter(|p| p["type"] == "output_text") {
                        if let Some(part) = part.as_object_mut() {
                            part.entry("annotations").or_insert_with(|| json!([]));
                            part.entry("logprobs").or_insert_with(|| json!([]));
                        }
                    }
                }
                _ => {}
            }
            ("msg", "completed")
        }
        // As OpenAI lists a call it is given.
        Some("function_call") => ("fc", "in_progress"),
        Some("function_call_output") => ("fc", "completed"),
        Some("reasoning") => ("rs", "completed"),
        _ => ("item", "completed"),
    };
    if prefix != "rs" {
        item.entry("status").or_insert(status.into());
    }
    item.insert("id".into(), new_id(prefix).into());
    item.into()
}

/// The items `messages` are, as a request writes them, then as a
/// conversation lists them: what a response's request added to it.
pub fn input_items(messages: &[Message]) -> Result<Vec<Value>, Fault> {
    let mut written = Map::new();
    request::write_input(&Input::new(messages.iter().cloned()), &mut written)?;
    let written = written.remove("input").unwrap_or_default();
    let written = written.as_array().cloned().unwrap_or_default();
    Ok(written.into_iter().map(listed).collect())
}

/// The items of a response, `said`, as its body's `output` has them.
pub fn output_items(said: &Assembled) -> Result<Vec<Value>, Fault> {
    let mut body = OpenAiResponses.encode_whole(said)?;
    Ok(match body["output"].take() {
        Value::Array(items) => items,
        _ => Vec::new(),
    })
}

/// `items` as a request is shown them: a reasoning item's encrypted content
/// only if it includes it (`encrypted`).
pub fn shown(mut items: Vec<Value>, encrypted: bool) -> Vec<Value> {
    if !encrypted {
        for item in items.iter_mut().filter(|i| i["type"] == "reasoning") {
            if let Some(item) = item.as_object_mut() {
                item.remove("encrypted_content");
            }
        }
    }
    items
}

/// The conversation `items` are, to send as a request's input: without
/// their `id` and `status`, but for a reasoning item's that carries its
/// encrypted content, as it is sent back.
pub fn messages(items: &[Value]) -> Result<Vec<Message>, Fault> {
    let sent: Vec<Value> = (items.iter().cloned())
        .map(|mut item| {
            let sealed = item["type"] == "reasoning" && item["encrypted_content"].is_string();
            if let Some(fields) = item.as_object_mut().filter(|_| !sealed) {
                fields.remove("id");
                fields.remove("status");
            }
            item
        })
        .collect();
    let mut read = Input::default();
    request::read_input(&json!({ "input": sent }), &mut read)?;
    Ok(read.messages)
}

/// The `conversation` a Responses request names, by its id or as
/// `{"id": ..}`; `Err` as OpenAI refuses one named wrong.
pub fn conversation_of(input: &Input) -> Result<Option<String>, Fault> {
    let id = match OpenAiResponses::fields(&input.native).get("conversation") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(id)) => (id, "conversation"),
        Some(Value::Object(c)) => match c.get("id") {
            Some(Value::String(id)) => (id, "conversation.id"),
            None | Some(Value::Null) => return Err(required("conversation.id")),
            Some(other) => {
                let message = format!(
                    "Invalid type for 'conversation.id': expected a string, but got {} instead.",
                    type_name(other)
                );
                return Err(invalid(
                    Some("conversation.id"),
                    Some("invalid_type"),
                    message,
                ));
            }
        },
        Some(other) => {
            let message = format!(
                "Invalid type for 'conversation': expected one of a string or object, but got {} instead.",
                type_name(other)
            );
            return Err(invalid(Some("conversation"), Some("invalid_type"), message));
        }
    };
    match id {
        (id, param) if id.is_empty() => {
            let message = format!(
                "Invalid '{param}': empty string. Expected a string with minimum length 1, but got an empty string instead."
            );
            Err(invalid(Some(param), Some("empty_string"), message))
        }
        (id, param) if !id.starts_with("conv") || !is_id(id) => {
            let message =
                format!("Invalid '{param}': '{id}'. Expected an ID that begins with 'conv'.");
            Err(invalid(Some(param), Some("invalid_value"), message))
        }
        (id, _) => Ok(Some(id.clone())),
    }
}

/// A conversation object.
pub fn conversation(id: &str, created_at: u64, metadata: &Map<String, Value>) -> Value {
    json!({ "id": id, "object": "conversation", "created_at": created_at, "metadata": metadata })
}

/// What a conversation's deletion answers.
pub fn deleted(id: &str) -> Value {
    json!({ "id": id, "object": "conversation.deleted", "deleted": true })
}

/// A list of `items`, of which there are more if `has_more`.
pub fn item_list(items: Vec<Value>, has_more: bool) -> Value {
    let id = |item: Option<&Value>| item.map_or(Value::Null, |i| i["id"].clone());
    let (first, last) = (id(items.first()), id(items.last()));
    json!({ "object": "list", "data": items, "first_id": first, "last_id": last, "has_more": has_more })
}

/// The page `page` of `items`, oldest first; `Err` if it starts after an
/// item they lack.
pub fn page_of(items: &[Value], page: &Page) -> Result<Value, Fault> {
    let ordered: Vec<&Value> = match page.ascending {
        true => items.iter().collect(),
        false => items.iter().rev().collect(),
    };
    let start = match &page.after {
        Some(after) => {
            let at = ordered.iter().position(|i| i["id"] == after.as_str());
            1 + at.ok_or_else(|| no_after(after))?
        }
        None => 0,
    };
    let rest = &ordered[start..];
    let taken = rest.iter().take(page.limit).map(|i| (*i).clone()).collect();
    let taken = shown(taken, page.encrypted);
    Ok(item_list(taken, rest.len() > page.limit))
}

/// An answer to an operation, `body`, as its one output, which the format
/// writes as the response's body.
pub fn answer(body: Value) -> Output {
    Output::Native(OpenAiResponses::native(json!({ ANSWER: body })))
}

/// The key of an answer's body in its native.
const ANSWER: &str = "conversations_answer";

/// The id of a new item: `prefix_` and a random UUID's hex.
fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Whether `request` is one of the API's routes.
pub(super) fn is_route<B>(request: &http::Request<B>) -> bool {
    route(request.method(), request.uri().path()).is_some()
}

/// A request of the API at the client edge: read as an input of nothing,
/// as it calls no model, once its operation reads; answered by the body of
/// its [`answer`].
pub(super) fn bind(io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
    let io = io.with_error(|fault, request, head| OpenAiResponses.refusal(fault, request, head));
    Ok(io.bind(
        |head| {
            let mut route = route(&head.method, head.uri.path());
            let query = head.uri.query().map(str::to_owned);
            Box::new(move |raw: Bytes, out: &mut Vec<Input>| {
                if let Some(route) = route.take() {
                    route.read(query.as_deref(), &raw)?;
                }
                out.push(Input::default());
                Ok(())
            })
        },
        |reply| {
            let mut head = reply.head();
            let json = HeaderValue::from_static("application/json");
            head.headers.insert(CONTENT_TYPE, json);
            Ok(reply.write(head, Box::new(Answer(None))))
        },
    ))
}

/// Writes the body of the answer it is given, at the end.
struct Answer(Option<Value>);

impl Encoder<Output> for Answer {
    fn encode(&mut self, reply: &Output, _out: &mut Vec<Bytes>) -> Result<(), Fault> {
        if let Output::Native(native) = reply
            && let Some(body) = OpenAiResponses::value(native).and_then(|v| v.get(ANSWER))
        {
            self.0 = Some(body.clone());
        }
        Ok(())
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        end?;
        let unanswered = "the Conversations API is answered by the gateway's responses plugin, which is not registered";
        let body = self.0.take().ok_or_else(|| Fault::not_found(unanswered))?;
        out.push(body.to_string().into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(method: Method, uri: &str, body: &str) -> Option<Result<Operation, Fault>> {
        let mut request = http::Request::new(Bytes::copy_from_slice(body.as_bytes()));
        *request.method_mut() = method;
        *request.uri_mut() = uri.parse().expect("a uri");
        Operation::read(&request)
    }

    fn op(method: Method, uri: &str, body: &str) -> Operation {
        read(method, uri, body)
            .expect("a route")
            .expect("an operation")
    }

    /// The `param` and `code` a refusal of `method` at `uri` names.
    fn refused(method: Method, uri: &str, body: &str) -> (Value, Value) {
        let fault = read(method, uri, body)
            .expect("a route")
            .expect_err("a refusal");
        let body = OpenAiResponses::error_body(&fault);
        assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
        (
            body["error"]["param"].clone(),
            body["error"]["code"].clone(),
        )
    }

    #[test]
    fn each_route_is_its_operation_under_any_prefix() {
        let (c, i) = ("conv_1".to_owned(), "msg_1".to_owned());
        let id = || c.clone();
        assert!(matches!(
            op(Method::POST, "/v1/conversations", ""),
            Operation::Create { items, metadata } if items.is_empty() && metadata.is_empty()
        ));
        assert_eq!(
            op(Method::GET, "/conversations/conv_1", ""),
            Operation::Retrieve { id: id() }
        );
        assert_eq!(
            op(Method::DELETE, "/v1/conversations/conv_1", ""),
            Operation::Delete { id: id() }
        );
        let update = op(
            Method::POST,
            "/v1/conversations/conv_1",
            r#"{"metadata": {"k": "v"}}"#,
        );
        assert!(
            matches!(update, Operation::Update { metadata, .. } if metadata.get("k") == Some(&json!("v")))
        );
        let page = Page {
            after: Some(i.clone()),
            limit: 5,
            ascending: true,
            encrypted: true,
        };
        let list = op(
            Method::GET,
            "/v1/conversations/conv_1/items?limit=5&order=asc&after=msg_1&include%5B%5D=reasoning.encrypted_content",
            "",
        );
        assert_eq!(list, Operation::List { id: id(), page });
        let item = Operation::Item {
            id: id(),
            item: i.clone(),
            encrypted: false,
        };
        assert_eq!(
            op(Method::GET, "/v1/conversations/conv_1/items/msg_1", ""),
            item
        );
        let gone = Operation::DeleteItem { id: id(), item: i };
        assert_eq!(
            op(Method::DELETE, "/v1/conversations/conv_1/items/msg_1", ""),
            gone
        );
        assert!(read(Method::PUT, "/v1/conversations/conv_1", "").is_none());
        assert!(read(Method::POST, "/v1/responses", "{}").is_none());
    }

    #[test]
    fn a_request_said_wrong_is_refused_as_openai_refuses_it() {
        let cases = [
            (
                Method::GET,
                "/v1/conversations/abc",
                "",
                "conversation_id",
                "invalid_value",
            ),
            (
                Method::GET,
                "/v1/conversations/conv_1/items/a.b",
                "",
                "item_id",
                "invalid_value",
            ),
            (
                Method::GET,
                "/v1/conversations/conv_1/items?limit=101",
                "",
                "limit",
                "integer_above_max_value",
            ),
            (
                Method::GET,
                "/v1/conversations/conv_1/items?limit=-1",
                "",
                "limit",
                "integer_below_min_value",
            ),
            (
                Method::GET,
                "/v1/conversations/conv_1/items?order=up",
                "",
                "order",
                "invalid_value",
            ),
            (
                Method::GET,
                "/v1/conversations/conv_1/items?include[]=bogus",
                "",
                "include[0]",
                "invalid_value",
            ),
            (
                Method::POST,
                "/v1/conversations/conv_1/items",
                "{}",
                "items",
                "missing_required_parameter",
            ),
            (
                Method::POST,
                "/v1/conversations/conv_1/items",
                r#"{"items": []}"#,
                "items",
                "empty_array",
            ),
            (
                Method::POST,
                "/v1/conversations/conv_1/items",
                r#"{"items": [{"role": "user", "content": "x"}], "include": []}"#,
                "include",
                "unknown_parameter",
            ),
            (
                Method::POST,
                "/v1/conversations",
                r#"{"foo": 1}"#,
                "foo",
                "unknown_parameter",
            ),
            (
                Method::POST,
                "/v1/conversations",
                r#"{"items": 5}"#,
                "items",
                "invalid_type",
            ),
            (
                Method::POST,
                "/v1/conversations",
                r#"{"metadata": {"k": 1}}"#,
                "metadata.k",
                "invalid_type",
            ),
            (
                Method::POST,
                "/v1/conversations/conv_1",
                "{}",
                "metadata",
                "missing_required_parameter",
            ),
            (
                Method::POST,
                "/v1/conversations/conv_1",
                r#"{"metadata": null}"#,
                "metadata",
                "invalid_type",
            ),
        ];
        for (method, uri, body, param, code) in cases {
            assert_eq!(
                refused(method, uri, body),
                (json!(param), json!(code)),
                "{uri} {body}"
            );
        }
        let many = format!(
            r#"{{"items": [{}]}}"#,
            vec![r#"{"role": "user", "content": "x"}"#; 21].join(",")
        );
        let refusal = refused(Method::POST, "/v1/conversations", &many);
        assert_eq!(refusal, (json!("items"), json!("array_above_max_length")));
        assert!(
            read(Method::POST, "/v1/conversations", r#"{"items": []}"#)
                .expect("a route")
                .is_ok()
        );
        let limit = op(Method::GET, "/v1/conversations/conv_1/items?limit=0", "");
        assert!(matches!(limit, Operation::List { page, .. } if page.limit == 0));
    }

    #[test]
    fn an_item_openai_refuses_is_refused() {
        let item = |item: Value| {
            items(&[json!({"role": "user", "content": "a"}), item])
                .map_err(|f| OpenAiResponses::error_body(&f)["error"]["param"].clone())
        };
        assert_eq!(item(json!({"type": "bogus"})), Err(json!("items[1]")));
        assert_eq!(
            item(json!({"type": "message", "content": "x"})),
            Err(json!("items[1]"))
        );
        assert_eq!(
            item(json!({"role": "tool", "content": "x"})),
            Err(json!("items[1]"))
        );
        assert_eq!(
            item(json!({"type": "item_reference", "id": "msg_1"})),
            Err(json!("items[1]"))
        );
        assert_eq!(
            item(json!({"role": "user"})),
            Err(json!("items[1].content"))
        );
        assert_eq!(
            item(json!({"role": "user", "content": [{"type": "output_text", "text": "u"}]})),
            Err(json!("items[1].content[0]"))
        );
        assert_eq!(
            item(json!({"type": "function_call", "name": "f", "arguments": "{}"})),
            Err(json!("items[1].call_id"))
        );
        assert_eq!(
            item(json!({"type": "reasoning", "summary": []})),
            Err(json!("item"))
        );
        assert!(
            item(json!({"type": "reasoning", "summary": [], "encrypted_content": "e"})).is_ok()
        );
        assert_eq!(item(json!(5)), Err(json!("items[1]")));
    }

    #[test]
    fn an_item_is_listed_as_the_api_lists_it() {
        let raw = [
            json!({"role": "user", "content": "hi", "id": "mine"}),
            json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c", "output": "ok"}),
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "done"}]}),
        ];
        let items = items(&raw).expect("items");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(
            items[0]["content"],
            json!([{"type": "input_text", "text": "hi"}])
        );
        let id = items[0]["id"].as_str().unwrap_or_default();
        assert!(id.starts_with("msg_") && id != "mine", "{id}");
        assert_eq!(
            (items[1]["status"].clone(), items[2]["status"].clone()),
            (json!("in_progress"), json!("completed"))
        );
        assert!(
            items[2]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("fc_"))
        );
        assert_eq!(
            items[3]["content"][0],
            json!({"type": "output_text", "text": "done", "annotations": [], "logprobs": []})
        );
    }

    #[test]
    fn a_conversation_named_wrong_is_refused() {
        let named = |conversation: Value| {
            let mut input = Input::default();
            input.native = OpenAiResponses::native(json!({ "conversation": conversation }));
            match conversation_of(&input) {
                Ok(id) => id.unwrap_or_default(),
                Err(f) => {
                    let error = &OpenAiResponses::error_body(&f)["error"];
                    format!("{} {}", error["param"], error["code"])
                }
            }
        };
        let cases = [
            (
                json!({"foo": 1}),
                r#""conversation.id" "missing_required_parameter""#,
            ),
            (
                json!({}),
                r#""conversation.id" "missing_required_parameter""#,
            ),
            (json!([]), r#""conversation" "invalid_type""#),
            (json!(5), r#""conversation" "invalid_type""#),
            (json!(""), r#""conversation" "empty_string""#),
            (json!("abc"), r#""conversation" "invalid_value""#),
            (json!({"id": "conv_1"}), "conv_1"),
            (json!("conv_1"), "conv_1"),
        ];
        for (conversation, said) in cases {
            assert_eq!(named(conversation), said);
        }
    }

    #[test]
    fn a_page_is_newest_first_unless_asked() {
        let items: Vec<Value> = (1..=5).map(|n| json!({"id": format!("i{n}")})).collect();
        let ids = |page: Value| {
            (page["data"].as_array().expect("data").iter())
                .map(|i| i["id"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        let page = |after: Option<&str>, ascending, limit| Page {
            after: after.map(Into::into),
            limit,
            ascending,
            encrypted: false,
        };
        let newest = page_of(&items, &page(None, false, 2)).expect("a page");
        assert_eq!(
            (newest["has_more"].clone(), newest["first_id"].clone()),
            (json!(true), json!("i5"))
        );
        assert_eq!(ids(newest), ["i5", "i4"]);
        assert_eq!(
            ids(page_of(&items, &page(Some("i3"), true, 2)).expect("a page")),
            ["i4", "i5"]
        );
        let none = page_of(&items, &page(None, true, 0)).expect("a page");
        assert_eq!(
            none,
            json!({"object": "list", "data": [], "first_id": null, "last_id": null, "has_more": true})
        );
        assert!(page_of(&items, &page(Some("none"), true, 2)).is_err());
    }

    #[test]
    fn encrypted_reasoning_is_shown_only_when_included() {
        let items = vec![
            json!({"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "e"}),
        ];
        assert_eq!(
            shown(items.clone(), false)[0].get("encrypted_content"),
            None
        );
        assert_eq!(shown(items, true)[0]["encrypted_content"], "e");
    }

    #[test]
    fn items_are_sent_without_their_ids_but_sealed_reasoning() {
        let items = [
            json!({"id": "msg_1", "type": "message", "status": "completed", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            json!({"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "sealed"}),
        ];
        let mut written = Map::new();
        request::write_input(
            &Input::new(messages(&items).expect("messages")),
            &mut written,
        )
        .expect("written");
        assert_eq!(
            written["input"][0],
            json!({"role": "user", "content": "hi"})
        );
        assert_eq!(written["input"][1]["id"], "rs_1");
    }
}
