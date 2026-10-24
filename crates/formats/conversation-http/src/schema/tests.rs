//! Cross-format checks of the shared schema helpers, which are private to
//! this crate.

use respan_gateway_conversation::Content;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, StatusCode};
use serde_json::json;

use crate::Schema;
use crate::anthropic::AnthropicMessages;
use crate::openai::OpenAiChat;
use crate::schema::errors::answer;
use crate::schema::{NativeFieldsExt, decode_content, encode_content};

#[test]
fn a_cache_marker_stays_with_its_own_part_among_equal_text() {
    let old = json!([
        { "type": "text", "text": "A", "cache_control": { "type": "ephemeral" } },
        { "type": "text", "text": "A" },
    ]);
    let mut content = decode_content::<AnthropicMessages>(&old);
    // A part that says more is text with its own native, and so equal text
    // beside it is a part of its own.
    assert!(matches!(&content[0], Content::Text { native, .. } if !native.is_empty()));
    assert_eq!(content[1], Content::from("A"));
    content.insert(0, Content::from("B"));
    content.push(Content::from("A"));
    let written = encode_content::<AnthropicMessages>(&content).expect("content");
    assert_eq!(written[0], json!({"type":"text","text":"B"}));
    assert_eq!(written[1], old[0], "the marker stays with its own part");
    assert_eq!(written[2], json!({"type":"text","text":"A"}));
    assert_eq!(written[3], json!({"type":"text","text":"A"}));
    // Another format drops a cache marker.
    let dropped = encode_content::<OpenAiChat>(&content).expect("content");
    assert_eq!(dropped[1], json!({"type":"text","text":"A"}));
}

#[test]
fn a_translated_error_keeps_each_formats_schema() {
    let shapes = [
        json!({"type": "error", "error": {"type": "overloaded_error", "message": ""}}),
        json!({"type": "https://tools.ietf.org/html/rfc7231#section-6.5.1", "title": "bad", "status": 400}),
        json!({"error": null, "type": "error", "message": "nope"}),
        json!({"object": "error", "message": {"detail": [{"msg": "Field required"}]}}),
        json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota"}}),
    ];
    for shape in shapes {
        let mut refused = http::Response::new(Bytes::from(shape.to_string()));
        *refused.status_mut() = StatusCode::BAD_REQUEST;
        let fault = answer(&OpenAiChat, refused);
        let words = (json!(fault.code()), json!(fault.message()));
        let openai = OpenAiChat::error_body(&fault);
        let anthropic = AnthropicMessages::error_body(&fault);
        assert!(!fault.message().is_empty(), "{shape}");
        for error in [&openai["error"], &anthropic["error"]] {
            assert_eq!(
                (error["type"].clone(), error["message"].clone()),
                words,
                "{shape}"
            );
        }
        assert!(openai["error"]["code"].is_string(), "{shape}");
        assert!(openai["error"].get("error").is_none(), "{shape}");
    }
}

#[test]
fn usage_is_written_only_where_each_count_has_a_field() {
    use crate::schema::{UsageFields, check_usage};
    use respan_gateway_conversation::Usage;

    let has = |cache_writes, reasoning| UsageFields {
        cache_writes,
        reasoning,
    };
    let mut usage = Usage::new(10, 5);
    usage.cached_input_tokens = 4;
    assert!(check_usage(&usage, has(false, false)).is_ok());

    usage.cache_write_input_tokens = 6;
    assert!(check_usage(&usage, has(true, false)).is_ok());
    assert!(
        check_usage(&usage, has(false, false)).is_err(),
        "a cache write has nowhere to go"
    );
    usage.cache_write_input_tokens = 7;
    assert!(
        check_usage(&usage, has(true, false)).is_err(),
        "the parts exceed the input"
    );

    usage.cache_write_input_tokens = 0;
    usage.reasoning_tokens = 6;
    assert!(check_usage(&usage, has(false, true)).is_err());
    assert!(check_usage(&usage, has(false, false)).is_ok());

    let mut huge = Usage::new(u64::MAX, 1);
    assert!(
        check_usage(&huge, has(true, true)).is_err(),
        "the total overflows"
    );
    huge.output_tokens = 0;
    assert!(check_usage(&huge, has(true, true)).is_ok());
}
