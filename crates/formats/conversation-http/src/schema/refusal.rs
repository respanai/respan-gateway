//! What a format refuses of a request, phrased in one place: each message
//! names the format first, then what it does not take and what to send
//! instead.

use std::fmt;

use respan_gateway_conversation::{Content, Role};
use respan_gateway_core::Fault;

/// Something a format has no place for in a request, told to the client
/// as a 400 by [`Refusal::by`].
pub(crate) enum Refusal<'a> {
    /// A part where it stands.
    Part(&'a Content),
    /// A part in a place, such as "a tool result".
    PartIn(&'a Content, &'static str),
    /// A role the format has no messages of.
    Role(&'a Role),
    /// A setting the format has none of, such as "top_k".
    Setting(&'static str),
    /// Audio, which the format takes only as its transcript.
    Audio,
    /// A file name on media of this kind, which the format takes only on a
    /// document.
    FileName(&'a str),
    /// A tool result marked as an error.
    ToolError,
    /// A system message after the first of another role.
    LateSystem,
    /// Parallel tool calls turned off.
    SerialTools,
    /// A reasoning token budget, where the format takes an effort.
    ReasoningBudget,
}

impl Refusal<'_> {
    /// The refusal of `format` (its id, such as `anthropic-messages`).
    pub(crate) fn by(self, format: &str) -> Fault {
        Fault::bad_request(Refused(title(format), self).to_string())
    }
}

/// A refusal and the name of the format that refuses it.
struct Refused<'a>(&'a str, Refusal<'a>);

impl fmt::Display for Refused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let to = self.0;
        if let Refusal::Part(Content::Native(raw)) | Refusal::PartIn(Content::Native(raw), _) =
            &self.1
        {
            let from = title(&raw.format);
            let kind = raw.value["type"]
                .as_str()
                .or_else(|| raw.value.as_object()?.keys().next().map(String::as_str))
                .unwrap_or("unnamed");
            return write!(
                f,
                "{to} cannot take the {from} part `{kind}`, which only {from} takes; \
                 remove it from the messages"
            );
        }
        match &self.1 {
            Refusal::Part(part) => write!(f, "{to} cannot take {} here", part_name(part)),
            Refusal::PartIn(part, place) => {
                write!(f, "{to} cannot take {} in {place}", part_name(part))
            }
            Refusal::Role(Role::Native { format, name, .. }) => write!(
                f,
                "{to} has no `{name}` role, which only {} has; \
                 remove that message or give it a user or assistant role",
                title(format)
            ),
            Refusal::Role(role) => match role_name(role) {
                Some(role) => write!(f, "{to} has no {role} role here"),
                None => write!(f, "{to} has no such role"),
            },
            Refusal::Setting(name) => write!(f, "{to} does not take {name}; leave it unset"),
            Refusal::Audio => write!(f, "{to} does not take audio; send its transcript as text"),
            Refusal::FileName(kind) => write!(
                f,
                "{to} takes a file name only on a document; remove it from the {kind}"
            ),
            Refusal::ToolError => write!(
                f,
                "{to} cannot mark a tool result as an error; say the error in its text"
            ),
            Refusal::LateSystem => write!(
                f,
                "{to} has no place for a system message after the conversation starts"
            ),
            Refusal::SerialTools => write!(f, "{to} cannot turn off parallel tool calls"),
            Refusal::ReasoningBudget => write!(
                f,
                "{to} does not take a reasoning token budget; use a reasoning effort instead"
            ),
        }
    }
}

/// The name a client knows format `id` by.
fn title(id: &str) -> &str {
    match id {
        "openai-chat" => "OpenAI Chat",
        "openai-responses" => "OpenAI Responses",
        "anthropic-messages" => "Anthropic",
        "gemini-generate" => "Gemini",
        "bedrock-converse" => "Bedrock",
        id => id,
    }
}

/// What `part` is, as a message names it.
pub(crate) fn part_name(part: &Content) -> &'static str {
    match part {
        Content::Text { .. } => "text",
        Content::Reasoning { .. } => "reasoning",
        Content::Image(_) => "an image",
        Content::Audio(_) => "audio",
        Content::Document(_) => "a document",
        Content::ToolCall { .. } => "a tool call",
        Content::ToolResult { .. } => "a tool result",
        Content::Native(_) => "another format's part",
        _ => "this kind of part",
    }
}

/// `role` as a message names it, unless it is one the kind adds later.
fn role_name(role: &Role) -> Option<&'static str> {
    match role {
        Role::System => Some("system"),
        Role::Developer => Some("developer"),
        Role::User => Some("user"),
        Role::Assistant => Some("assistant"),
        _ => None,
    }
}
