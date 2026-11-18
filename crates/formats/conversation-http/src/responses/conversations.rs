//! OpenAI's Conversations API, of the Responses format: the routes a
//! conversation's state is reached by, read as [`Operation`]s, and the JSON
//! of what they answer. No model is called: whoever keeps conversations,
//! such as the gateway's `plugins.responses`, answers each with
//! [`answer`], which the format writes as the body whole.
//!
//! A conversation's items are Responses items, each with an `id`: what a
//! client adds is given the type, content parts, `status` and `id` the API
//! lists it with ([`items`]), and a response's output is its items as the
//! format writes them ([`output_items`]). [`messages`] reads them back as
//! the conversation they are.
//!
//! The routes, under any prefix, such as `/v1`:
//!
//! - `POST /conversations`, `GET`, `POST` (its metadata) and `DELETE`
//!   `/conversations/{id}`;
//! - `GET /conversations/{id}/items` (`limit`, 1 to 100, 20 by default;
//!   `order`, `desc` by default; `after`, an item's id) and `POST` (up to 20
//!   items);
//! - `GET` and `DELETE /conversations/{id}/items/{item_id}`.

use respan_gateway_conversation::{Assembled, Input, Message, Output};
use respan_gateway_core::{Connection, Fault};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderValue, Method, header::CONTENT_TYPE};
use respan_gateway_http::{ClientIo, Encoder};
use serde_json::{Map, Value, json};

use super::{OpenAiResponses, request};
use crate::Schema;
use crate::schema::Own;

/// The most items a request adds at once.
pub const MAX_ITEMS: usize = 20;
/// The most metadata pairs; a key's most characters, and a value's.
const METADATA: (usize, usize, usize) = (16, 64, 512);

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
    /// Set a conversation's metadata.
    Update {
        /// The conversation.
        id: String,
        /// Its metadata, in place of what it had.
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
    },
    /// One of a conversation's items.
    Item {
        /// The conversation.
        id: String,
        /// The item.
        item: String,
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
                let body = object(body, true)?;
                let items = match &body["items"] {
                    Value::Null => Vec::new(),
                    raw => items(list(raw)?)?,
                };
                let metadata = metadata(&body["metadata"])?;
                Operation::Create { items, metadata }
            }
            Self::Retrieve(id) => Operation::Retrieve { id },
            Self::Update(id) => {
                let body = object(body, false)?;
                let metadata = metadata(&body["metadata"])?;
                Operation::Update { id, metadata }
            }
            Self::Delete(id) => Operation::Delete { id },
            Self::List(id) => Operation::List {
                id,
                page: page(query)?,
            },
            Self::Add(id) => {
                let body = object(body, false)?;
                let raw = match &body["items"] {
                    Value::Null => return Err(Fault::bad_request("`items` is required")),
                    raw => list(raw)?,
                };
                Operation::Add {
                    id,
                    items: items(raw)?,
                }
            }
            Self::Item(id, item) => Operation::Item { id, item },
            Self::DeleteItem(id, item) => Operation::DeleteItem { id, item },
        })
    }
}

/// A body's JSON object; with `empty`, no body is an empty one.
fn object(body: &[u8], empty: bool) -> Result<Value, Fault> {
    if empty && body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v) if v.is_object() => Ok(v),
        _ => Err(Fault::bad_request("request body is not a JSON object")),
    }
}

/// `raw` as up to [`MAX_ITEMS`] items.
fn list(raw: &Value) -> Result<&[Value], Fault> {
    let items = raw
        .as_array()
        .ok_or_else(|| Fault::bad_request("`items` is not an array"))?;
    if items.len() > MAX_ITEMS {
        let message = format!(
            "`items` has {}; at most {MAX_ITEMS} are added at once",
            items.len()
        );
        return Err(Fault::bad_request(message));
    }
    Ok(items)
}

/// Metadata: up to 16 pairs of strings, a key up to 64 characters, a value
/// up to 512; none if `null`.
fn metadata(raw: &Value) -> Result<Map<String, Value>, Fault> {
    let (pairs, key, value) = METADATA;
    let metadata = match raw {
        Value::Null => return Ok(Map::new()),
        Value::Object(metadata) => metadata,
        _ => return Err(Fault::bad_request("`metadata` is not an object")),
    };
    if metadata.len() > pairs {
        return Err(Fault::bad_request(format!(
            "`metadata` has more than {pairs} pairs"
        )));
    }
    for (k, v) in metadata {
        let fits = v.as_str().is_some_and(|v| v.chars().count() <= value);
        if k.chars().count() > key || !fits {
            return Err(Fault::bad_request(format!(
                "`metadata.{k}` is not a string of up to {value} characters under a key of up to {key}"
            )));
        }
    }
    Ok(metadata.clone())
}

/// A page from `query`'s `limit`, `order` and `after`.
fn page(query: Option<&str>) -> Result<Page, Fault> {
    let mut page = Page {
        after: None,
        limit: 20,
        ascending: false,
    };
    let pairs = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|p| p.split_once('='));
    for (key, value) in pairs {
        let value = percent_encoding::percent_decode_str(value).decode_utf8_lossy();
        match key {
            "limit" => {
                let limit = value.parse().ok().filter(|l| (1..=100).contains(l));
                let limit = limit.ok_or_else(|| Fault::bad_request("`limit` is not 1 to 100"))?;
                page.limit = limit;
            }
            "order" => {
                page.ascending = match &*value {
                    "asc" => true,
                    "desc" => false,
                    _ => return Err(Fault::bad_request("`order` is not `asc` or `desc`")),
                };
            }
            "after" => page.after = Some(value.into_owned()),
            _ => {}
        }
    }
    Ok(page)
}

/// Items a client adds, as a conversation lists them: a message given its
/// `type`, its text as a part, and `status`; each given an `id` if it has
/// none. `Err` for one the format cannot read.
pub fn items(raw: &[Value]) -> Result<Vec<Value>, Fault> {
    let items = raw.iter().map(item).collect::<Result<Vec<_>, _>>()?;
    let mut read = Input::default();
    request::read_input(&json!({ "input": items }), &mut read)?;
    Ok(items)
}

fn item(raw: &Value) -> Result<Value, Fault> {
    let Some(fields) = raw.as_object() else {
        return Err(Fault::bad_request("an item is not an object"));
    };
    let mut item = fields.clone();
    if !item.contains_key("type") && item.contains_key("role") {
        item.insert("type".into(), "message".into());
    }
    let prefix = match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            let part = match item.get("role").and_then(Value::as_str) == Some("assistant") {
                true => json!({ "type": "output_text", "text": "", "annotations": [] }),
                false => json!({ "type": "input_text", "text": "" }),
            };
            if let Some(Value::String(text)) = item.get_mut("content") {
                let mut part = part;
                part["text"] = std::mem::take(text).into();
                item.insert("content".into(), json!([part]));
            }
            item.entry("status").or_insert("completed".into());
            "msg"
        }
        Some("function_call") => "fc",
        Some("function_call_output") => "fco",
        Some("reasoning") => "rs",
        _ => "item",
    };
    if !item.get("id").is_some_and(Value::is_string) {
        item.insert("id".into(), new_id(prefix).into());
    }
    Ok(item.into())
}

/// The items `messages` are, as a request writes them, then as [`items`]
/// lists them: what a response's request added to its conversation.
pub fn input_items(messages: &[Message]) -> Result<Vec<Value>, Fault> {
    let mut written = Map::new();
    request::write_input(&Input::new(messages.iter().cloned()), &mut written)?;
    let written = written.remove("input").unwrap_or_default();
    written.as_array().map_or(Ok(Vec::new()), |raw| items(raw))
}

/// The items of a response, `said`, as its body's `output` has them.
pub fn output_items(said: &Assembled) -> Result<Vec<Value>, Fault> {
    let mut body = OpenAiResponses.encode_whole(said)?;
    Ok(match body["output"].take() {
        Value::Array(items) => items,
        _ => Vec::new(),
    })
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
/// `{"id": ..}`; `Err` if it names one wrong.
pub fn conversation_of(input: &Input) -> Result<Option<String>, Fault> {
    match OpenAiResponses::fields(&input.native).get("conversation") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) => Ok(Some(id.clone())),
        Some(Value::Object(c)) if c["id"].is_string() => Ok(c["id"].as_str().map(Into::into)),
        Some(_) => Err(Fault::bad_request(
            "`conversation` is not a conversation's id, or an object of one",
        )),
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

/// The page `page` of `items`, oldest first; `None` if it starts after an
/// item they lack.
pub fn page_of(items: &[Value], page: &Page) -> Option<Value> {
    let ordered: Vec<&Value> = match page.ascending {
        true => items.iter().collect(),
        false => items.iter().rev().collect(),
    };
    let start = match &page.after {
        Some(after) => 1 + ordered.iter().position(|i| i["id"] == after.as_str())?,
        None => 0,
    };
    let rest = &ordered[start..];
    let taken = rest.iter().take(page.limit).map(|i| (*i).clone()).collect();
    Some(item_list(taken, rest.len() > page.limit))
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
        assert!(matches!(update, Operation::Update { metadata, .. } if metadata["k"] == "v"));
        let page = Page {
            after: Some(i.clone()),
            limit: 5,
            ascending: true,
        };
        let list = op(
            Method::GET,
            "/v1/conversations/conv_1/items?limit=5&order=asc&after=msg_1",
            "",
        );
        assert_eq!(list, Operation::List { id: id(), page });
        let item = Operation::Item {
            id: id(),
            item: i.clone(),
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
    fn a_request_said_wrong_is_refused() {
        let refused = |method, uri, body| read(method, uri, body).expect("a route").is_err();
        assert!(refused(
            Method::GET,
            "/v1/conversations/c/items?limit=101",
            ""
        ));
        assert!(refused(
            Method::GET,
            "/v1/conversations/c/items?order=up",
            ""
        ));
        assert!(refused(Method::POST, "/v1/conversations/c/items", "{}"));
        assert!(refused(
            Method::POST,
            "/v1/conversations",
            r#"{"items": [1]}"#
        ));
        let many = format!(
            r#"{{"items": [{}]}}"#,
            vec![r#"{"role": "user", "content": "x"}"#; 21].join(",")
        );
        assert!(refused(Method::POST, "/v1/conversations", &many));
        assert!(refused(
            Method::POST,
            "/v1/conversations",
            r#"{"metadata": {"k": 1}}"#
        ));
    }

    #[test]
    fn an_item_is_listed_as_the_api_lists_it() {
        let raw = [
            json!({"role": "user", "content": "hi"}),
            json!({"type": "function_call_output", "call_id": "c", "output": "ok", "id": "mine"}),
        ];
        let items = items(&raw).expect("items");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(
            items[0]["content"],
            json!([{"type": "input_text", "text": "hi"}])
        );
        assert!(
            items[0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_"))
        );
        assert_eq!(items[1]["id"], "mine");
    }

    #[test]
    fn a_page_is_newest_first_unless_asked() {
        let items: Vec<Value> = (1..=5).map(|n| json!({"id": format!("i{n}")})).collect();
        let ids = |page: Value| {
            (page["data"].as_array().expect("data").iter())
                .map(|i| i["id"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        let page = |after: Option<&str>, ascending| Page {
            after: after.map(Into::into),
            limit: 2,
            ascending,
        };
        let newest = page_of(&items, &page(None, false)).expect("a page");
        assert_eq!(
            (newest["has_more"].clone(), newest["first_id"].clone()),
            (json!(true), json!("i5"))
        );
        assert_eq!(ids(newest), ["i5", "i4"]);
        assert_eq!(
            ids(page_of(&items, &page(Some("i3"), true)).expect("a page")),
            ["i4", "i5"]
        );
        assert!(page_of(&items, &page(Some("none"), true)).is_none());
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
