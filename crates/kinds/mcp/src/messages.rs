//! Typed JSON-RPC messages carried by MCP. HTTP batching, sessions and SSE
//! cursors belong to the envelopes ([`Input`](crate::Input), [`Output`](crate::Output)). Both directions can carry requests,
//! notifications and responses.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};

/// A JSON-RPC request identifier. String and numeric IDs remain distinct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// A string identifier.
    String(String),
    /// An integer identifier.
    Number(Number),
}

/// A JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Protocol or application error code.
    pub code: i64,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured details.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub data: Option<Value>,
    /// Additional protocol fields.
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

/// Exactly one successful result or error.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Successful result (including JSON null).
    Success(Value),
    /// Failed request.
    Error(RpcError),
}

/// The three JSON-RPC message shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub enum Message {
    /// A call requiring a response.
    Request {
        /// Request identifier.
        id: Id,
        /// Method name.
        method: String,
        /// Structured parameters.
        params: Option<Value>,
        /// Additional protocol fields.
        extensions: Map<String, Value>,
    },
    /// A call requiring no response.
    Notification {
        /// Method name.
        method: String,
        /// Structured parameters.
        params: Option<Value>,
        /// Additional protocol fields.
        extensions: Map<String, Value>,
    },
    /// An answer to a request.
    Response {
        /// Request ID; absent only for errors with an unknown ID.
        id: Option<Id>,
        /// Result or error.
        outcome: Outcome,
        /// Additional protocol fields.
        extensions: Map<String, Value>,
    },
}
impl Message {
    /// Method name for requests and notifications.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            _ => None,
        }
    }
    /// Parameters of a request or notification.
    pub fn params(&self) -> Option<&Value> {
        match self {
            Self::Request { params, .. } | Self::Notification { params, .. } => params.as_ref(),
            _ => None,
        }
    }
    /// The request ID, if this message requires a response.
    pub fn request_id(&self) -> Option<Value> {
        match self {
            Self::Request { id, .. } => Some(id.clone().into()),
            _ => None,
        }
    }
    /// The answered ID, if known.
    pub fn response_id(&self) -> Option<Value> {
        match self {
            Self::Response { id: Some(id), .. } => Some(id.clone().into()),
            _ => None,
        }
    }
    /// Mutable successful result, for response filters.
    pub fn result_mut(&mut self) -> Option<&mut Value> {
        match self {
            Self::Response {
                outcome: Outcome::Success(result),
                ..
            } => Some(result),
            _ => None,
        }
    }
}
impl From<Id> for Value {
    fn from(id: Id) -> Self {
        match id {
            Id::String(s) => s.into(),
            Id::Number(n) => n.into(),
        }
    }
}
impl TryFrom<Value> for Message {
    type Error = String;
    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let Value::Object(mut fields) = value else {
            return Err("expected JSON-RPC object".into());
        };
        if fields.remove("jsonrpc") != Some(json!("2.0")) {
            return Err("expected JSON-RPC 2.0".into());
        }
        let id = fields.remove("id");
        let parse_id = |value: Value| -> Result<Id, String> {
            match value {
                Value::String(s) => Ok(Id::String(s)),
                Value::Number(n) if n.is_i64() || n.is_u64() => Ok(Id::Number(n)),
                _ => Err("invalid JSON-RPC id".into()),
            }
        };
        if let Some(method) = fields.remove("method") {
            let Value::String(method) = method else {
                return Err("invalid method".into());
            };
            if fields.contains_key("result") || fields.contains_key("error") {
                return Err("call contains response fields".into());
            }
            let params = fields.remove("params");
            if params
                .as_ref()
                .is_some_and(|p| !p.is_array() && !p.is_object())
            {
                return Err("invalid parameters".into());
            }
            return Ok(match id {
                Some(id) => Self::Request {
                    id: parse_id(id)?,
                    method,
                    params,
                    extensions: fields,
                },
                None => Self::Notification {
                    method,
                    params,
                    extensions: fields,
                },
            });
        }
        let id = id.ok_or("response lacks id")?;
        let id = if id.is_null() {
            None
        } else {
            Some(parse_id(id)?)
        };
        let outcome = match (fields.remove("result"), fields.remove("error")) {
            (Some(value), None) if id.is_some() => Outcome::Success(value),
            (None, Some(error)) => {
                Outcome::Error(serde_json::from_value(error).map_err(|e| e.to_string())?)
            }
            _ => return Err("response requires exactly one result or error".into()),
        };
        Ok(Self::Response {
            id,
            outcome,
            extensions: fields,
        })
    }
}
impl From<Message> for Value {
    fn from(message: Message) -> Self {
        let (mut fields, pairs) = match message {
            Message::Request {
                id,
                method,
                params,
                extensions,
            } => (
                extensions,
                vec![
                    ("id", Some(id.into())),
                    ("method", Some(method.into())),
                    ("params", params),
                ],
            ),
            Message::Notification {
                method,
                params,
                extensions,
            } => (
                extensions,
                vec![("method", Some(method.into())), ("params", params)],
            ),
            Message::Response {
                id,
                outcome,
                extensions,
            } => {
                let pair = match outcome {
                    Outcome::Success(value) => ("result", Some(value)),
                    Outcome::Error(error) => ("error", Some(json!(error))),
                };
                let id = id.map_or(Value::Null, Value::from);
                (extensions, vec![("id", Some(id)), pair])
            }
        };
        // Extensions cannot override the typed envelope.
        for key in ["jsonrpc", "id", "method", "params", "result", "error"] {
            fields.remove(key);
        }
        fields.insert("jsonrpc".into(), "2.0".into());
        fields.extend(pairs.into_iter().filter_map(|(k, v)| Some((k.into(), v?))));
        fields.into()
    }
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shapes_round_trip_without_losing_extensions() {
        for value in [
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"x"},"trace":"t"}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":"4","result":null}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse","data":null,"detail":1}}),
        ] {
            assert_eq!(
                Value::from(Message::try_from(value.clone()).expect("valid JSON-RPC message")),
                value
            );
        }
    }
    #[test]
    fn rejects_ambiguous_or_incomplete_messages() {
        for value in [
            json!({"jsonrpc":"2.0","id":1}),
            json!({"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"x"}}),
            json!({"jsonrpc":"2.0","id":null,"method":"x"}),
            json!({"jsonrpc":"2.0","id":1,"method":"x","params":false}),
        ] {
            assert!(Message::try_from(value).is_err());
        }
    }
}
