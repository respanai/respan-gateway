//! HTTP envelopes keep transport metadata attached to typed messages.
use crate::messages::Message;
use http::Method;
use serde_json::Value;

/// The kind's input: one HTTP request from an MCP client.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input {
    /// `POST` sends messages, `GET` opens the server's stream, `DELETE`
    /// ends the session.
    pub method: Method,
    /// A `POST`'s messages, in order; none for `GET` or `DELETE`.
    pub messages: Vec<Message>,
    /// Whether they came as a JSON array (a batch).
    pub batch: bool,
    /// The session it belongs to (`Mcp-Session-Id`), none before the server
    /// assigned one.
    pub session: Option<String>,
    /// The protocol version the client and server agreed on
    /// (`MCP-Protocol-Version`), which MCP has a client send on every
    /// request after `initialize`.
    pub protocol_version: Option<String>,
    /// The SSE `id:` of the last event the client got (`Last-Event-ID`),
    /// on a `GET` that resumes a stream.
    pub last_event_id: Option<String>,
    /// The request's query as the client sent it, without the credential
    /// parameters the HTTP adapter lifted out, sent to the server as it
    /// stands.
    pub query: Option<String>,
}

impl Input {
    /// A request of `method` with `messages`, in no session and not a batch
    /// (MCP 2025-06-18 has none).
    pub fn new(method: Method, messages: Vec<Message>) -> Self {
        Self {
            method,
            messages,
            ..Self::default()
        }
    }

    /// The ids of its requests, which the server must answer.
    pub fn request_ids(&self) -> impl Iterator<Item = Value> {
        self.messages.iter().filter_map(|m| m.request_id())
    }
}

/// The kind's output: one message of the server's, a JSON body or an
/// event. An event the kind cannot read, such as a comment, has none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// Its JSON-RPC messages.
    pub messages: Vec<Message>,
    /// The session the server's answer named (`Mcp-Session-Id`), as on the
    /// answer to `initialize`, which assigns it. The client's format writes
    /// the first output's into the head of a `POST`'s answer.
    pub session: Option<String>,
    /// The SSE `id:` of its event, which a client resumes from
    /// (`Last-Event-ID`); written only when the client's framing is an event
    /// stream, and ignored in a JSON body.
    pub event_id: Option<String>,
    /// The SSE `event:` name of its event, written like `event_id`.
    pub event: Option<String>,
}

impl Output {
    /// A message of `messages`, in no session.
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }

    /// The ids of the responses in it.
    pub fn answers(&self) -> impl Iterator<Item = Value> {
        self.messages.iter().filter_map(|m| m.response_id())
    }
}
