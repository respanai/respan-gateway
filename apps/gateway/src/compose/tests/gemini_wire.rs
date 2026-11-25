use super::*;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MODEL: &str = "gemini-3-flash-preview";
const LIVE_MODEL: &str = "gemini-2.5-flash-native-audio-preview-12-2025";

fn config(mock: &FakeGemini) -> String {
    let base = mock.base_url();
    format!(
        r#"formats = ["gemini-generate", "openai-chat", "gemini-live", "openai-realtime"]
providers.gemini = {{ vendor = "gemini", key = "test", formats = {{ gemini-generate = "{base}/v1beta", gemini-live = "{base}" }} }}
"#
    )
}

async fn sse(mock: &FakeGemini, path: &str, request: Value) -> Vec<Value> {
    let gateway = resolve(&config(mock)).expect("gateway");
    let mut req = post(path, request.to_string());
    req.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    let (response, _) = serve(&gateway, req).await;
    let (status, _, body) = read_whole(response).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.expect("response body");
    String::from_utf8_lossy(&body)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn native_gemini_stream_keeps_every_upstream_chunk_and_its_usage() {
    let mock = FakeGemini::start().await;
    let upstream = vec![
        json!({"usageMetadata": {"promptTokenCount": 5}}),
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "Hello"}]}}],
               "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1}}),
        json!({"candidates": [{"index": 0, "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1, "totalTokenCount": 6}}),
    ];
    mock.answer(Answer::events(upstream.clone()));
    let response = sse(
        &mock,
        &format!("/v1beta/models/{MODEL}:streamGenerateContent?alt=sse"),
        json!({"contents": [{"role": "user", "parts": [{"text": "Hi"}]}]}),
    )
    .await;
    assert_eq!(response, upstream);
}

#[tokio::test]
async fn gemini_chat_signs_the_finishing_openai_delta() {
    let mock = FakeGemini::start().await;
    mock.answer(Answer::events(vec![
        json!({"responseId": "r", "modelVersion": MODEL,
               "candidates": [{"content": {"parts": [{"text": "Hello"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "", "thoughtSignature": "opaque"}]},
                               "finishReason": "STOP"}]}),
    ]));
    let chunks = sse(
        &mock,
        "/v1/chat/completions",
        json!({"model": MODEL, "stream": true, "messages": [{"role": "user", "content": "Hi"}]}),
    )
    .await;
    let choice = &chunks.last().expect("finishing chunk")["choices"][0];
    assert_eq!(choice["finish_reason"], "stop");
    assert_eq!(
        choice["delta"]["extra_content"]["google"]["thought_signature"], "opaque",
        "{chunks:?}"
    );
}

async fn gateway_addr(mock: &FakeGemini) -> std::net::SocketAddr {
    let config = Config::parse(&config(mock)).expect("config");
    let app = App::new(config).expect("gateway");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { app.serve(listener, std::future::pending()).await.unwrap() });
    addr
}

#[tokio::test]
async fn gemini_live_keeps_the_transcript_in_the_model_turn_frame() {
    let mock = FakeGemini::start().await;
    let expected = json!({"serverContent": {
        "modelTurn": {"parts": [{"inlineData": {"mimeType": "audio/pcm;rate=24000", "data": "AAEC"}}]},
        "outputTranscription": {"text": "Hello", "finished": true}
    }});
    mock.answer(Answer::events(vec![expected.clone()]));
    let addr = gateway_addr(&mock).await;
    let path = respan_gateway_realtime_ws::GeminiLive::PATH;
    let (mut socket, _) = connect_async(format!("ws://{addr}{path}")).await.unwrap();
    socket
        .send(Message::Text(
            json!({"setup": {"model": format!("models/{LIVE_MODEL}"),
        "generationConfig": {"responseModalities": ["AUDIO"]}, "outputAudioTranscription": {}}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let setup = socket.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&setup.into_data()).unwrap(),
        json!({"setupComplete": {}})
    );
    socket
        .send(Message::Text(
            json!({"realtimeInput": {"text": "Hi"}}).to_string().into(),
        ))
        .await
        .unwrap();
    let frame = socket.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&frame.into_data()).unwrap(),
        expected
    );
}

#[tokio::test]
async fn openai_realtime_reports_unsupported_text_output_for_native_audio() {
    let mock = FakeGemini::start().await;
    mock.answer(Answer::error(503).message("service_unavailable"));
    let addr = gateway_addr(&mock).await;
    let (mut socket, _) = connect_async(format!("ws://{addr}/v1/realtime?model={LIVE_MODEL}"))
        .await
        .unwrap();
    let opened = socket.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&opened.into_data()).unwrap()["type"],
        "session.created"
    );
    for event in [
        json!({"type": "session.update", "session": {"type": "realtime", "output_modalities": ["text"]}}),
        json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Hi"}]}}),
        json!({"type": "response.create"}),
    ] {
        socket
            .send(Message::Text(event.to_string().into()))
            .await
            .unwrap();
    }
    let error = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("gateway response")
            .expect("frame")
            .expect("WebSocket");
        let value: Value = serde_json::from_slice(&frame.into_data()).expect("realtime event");
        if value["type"] == "error" {
            break value;
        }
    };
    assert_eq!(error["error"]["type"], "invalid_request_error", "{error:?}");
    assert_eq!(error["error"]["code"], "unsupported", "{error:?}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("text output modality"),
        "{error:?}"
    );
}
