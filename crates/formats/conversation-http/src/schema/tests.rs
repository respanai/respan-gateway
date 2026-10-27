//! Cross-format checks of the shared schema helpers, which are private to
//! this crate.

use respan_gateway_conversation::{Assembled, Content};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, StatusCode};
use serde_json::json;

use crate::Schema;
use crate::anthropic::AnthropicMessages;
use crate::gemini::GeminiGenerate;
use crate::openai::OpenAiChat;
use crate::responses::OpenAiResponses;
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

#[test]
fn a_web_search_is_asked_of_each_provider_in_its_own_terms() {
    let v =
        json!({ "messages": [{ "role": "user", "content": "news?" }], "web_search_options": {} });
    let input = OpenAiChat.decode_json(&v);
    assert_eq!(input.provider_tools.len(), 1);
    let anthropic = AnthropicMessages.encode_request(&input).expect("anthropic");
    let search = json!({ "type": "web_search_20250305", "name": "web_search" });
    assert_eq!(anthropic["tools"], json!([search]));
    let gemini = GeminiGenerate.encode_request(&input).expect("gemini");
    assert_eq!(gemini["tools"], json!([{ "googleSearch": {} }]));
    let responses = OpenAiResponses.encode_request(&input).expect("responses");
    assert_eq!(responses["tools"], json!([{ "type": "web_search" }]));
    let chat = OpenAiChat.encode_request(&input).expect("chat");
    assert_eq!(chat["web_search_options"], json!({}));
    // Most providers that speak Chat would ignore another format's search.
    let anthropic = AnthropicMessages.decode_json(&anthropic.into());
    assert!(OpenAiChat.encode_request(&anthropic).is_err());
}

#[test]
fn a_searchs_bounds_go_only_to_its_own_format_and_its_hints_are_dropped() {
    let search = json!({
        "type": "web_search_20260209", "name": "web_search", "max_uses": 3,
        "cache_control": { "type": "ephemeral" },
    });
    let v = json!({ "messages": [{ "role": "user", "content": "hi" }], "tools": [search] });
    let input = AnthropicMessages.decode_json(&v);
    let back = AnthropicMessages.encode_request(&input).expect("anthropic");
    assert_eq!(back["tools"], json!([search]));
    let refused = OpenAiResponses
        .encode_request(&input)
        .expect_err("max_uses");
    assert!(
        refused.message().contains("max_uses"),
        "{}",
        refused.message()
    );
    let v = json!({ "input": "hi", "tools": [{ "type": "web_search_preview", "search_context_size": "low" }] });
    let input = OpenAiResponses.decode_json(&v);
    assert_eq!(
        OpenAiResponses.encode_request(&input).expect("responses")["tools"],
        v["tools"]
    );
    let anthropic = AnthropicMessages.encode_request(&input).expect("anthropic");
    let plain = json!({ "type": "web_search_20250305", "name": "web_search" });
    assert_eq!(anthropic["tools"], json!([plain]));
}

#[test]
fn a_google_search_beside_another_google_tool_stays_geminis_own() {
    let v = json!({
        "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
        "tools": [{ "googleSearch": {}, "urlContext": {} }],
    });
    let input = GeminiGenerate.decode_json(&v);
    assert!(input.provider_tools.is_empty());
    assert_eq!(
        GeminiGenerate.encode_request(&input).expect("gemini")["tools"],
        v["tools"]
    );
}

#[test]
fn claudes_paused_search_reaches_a_chat_client_as_an_incomplete_answer() {
    let body = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
        "content": [
            { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": { "query": "news" } },
            { "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [] },
            { "type": "text", "text": "It rained.", "citations": [
                { "type": "web_search_result_location", "url": "https://example.com", "cited_text": "rain" },
            ] },
        ],
        "stop_reason": "pause_turn",
        "usage": { "input_tokens": 5, "output_tokens": 3 },
    });
    let mut events = Vec::new();
    AnthropicMessages
        .decode_body(&body, &mut events)
        .expect("a response");
    let mut whole = Assembled::default();
    events
        .iter()
        .try_for_each(|e| whole.push(e))
        .expect("assembled");
    let chat = OpenAiChat.encode_whole(&whole).expect("a chat body");
    assert_eq!(chat["choices"][0]["message"]["content"], "It rained.");
    assert_eq!(chat["choices"][0]["finish_reason"], "length");
    let back = AnthropicMessages.encode_whole(&whole).expect("anthropic");
    assert_eq!(back["stop_reason"], "pause_turn");
}
