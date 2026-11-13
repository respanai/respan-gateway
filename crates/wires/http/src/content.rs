//! A body's content, as formats read it: its content type, whether a
//! response is a stream of server-sent events ([`is_stream`]; reading one
//! is [`Sse`](crate::Sse)'s), and a body's or an event's JSON. The formats
//! themselves are the `respan-gateway-*-http` crates'; traffic of no kind
//! relays through `respan-gateway-passthrough-http`.

use respan_gateway_http::http::{
    self, HeaderValue, header::CONTENT_TYPE, response::Parts as ResponseHead,
};

use serde_json::{self, Value};

/// A provider's message as JSON: a whole body, or an event's `data:`.
pub fn json(raw: impl AsRef<[u8]>) -> Option<Value> {
    let raw = raw.as_ref();
    serde_json::from_slice(raw)
        .ok()
        .or_else(|| respan_gateway_sse::event_json(raw))
}

/// Set a response's content type to `value` if it has none (RFC 9110 8.3).
pub fn default_content_type(head: &mut ResponseHead, value: &'static str) {
    let value = HeaderValue::from_static(value);
    head.headers.entry(CONTENT_TYPE).or_insert(value);
}

/// A response's content type, if it is readable.
pub fn content_type(head: &ResponseHead) -> Option<&str> {
    let value = head.headers.get(http::header::CONTENT_TYPE)?;
    value.to_str().ok()
}

/// Whether a response is a stream: a success labelled `text/event-stream`.
pub fn is_stream(head: &ResponseHead) -> bool {
    let essence = content_type(head).and_then(|c| c.split(';').next());
    let sse = essence.is_some_and(|e| e.trim().eq_ignore_ascii_case("text/event-stream"));
    sse && head.status.is_success()
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_sse::event;

    #[test]
    fn a_stream_is_a_success_labelled_as_sse_in_any_case() {
        let head = |status: u16, content_type: &'static str| {
            let mut head = http::Response::new(()).into_parts().0;
            head.status = http::StatusCode::from_u16(status).unwrap_or_default();
            let value = http::HeaderValue::from_static(content_type);
            head.headers.insert(http::header::CONTENT_TYPE, value);
            head
        };
        assert!(is_stream(&head(200, "Text/Event-Stream; charset=utf-8")));
        assert!(!is_stream(&head(200, "text/event-streamx")));
        assert!(!is_stream(&head(503, "text/event-stream")));
    }

    #[test]
    fn a_body_or_an_events_data_reads_as_json() {
        assert_eq!(json(b"{\"b\":3}"), Some(serde_json::json!({"b": 3})));
        assert_eq!(
            json(event(None, "{\"a\":2}").as_bytes()),
            Some(serde_json::json!({"a": 2}))
        );
    }
}
