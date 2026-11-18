use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::*;

/// Turns running in the stub now, and the most at once.
#[derive(Default)]
struct Running(AtomicUsize, AtomicUsize);

/// A Responses service: `response.created` echoing what it was sent, then
/// after `delay` ms `response.completed`; `input: "fail"` is refused 400.
async fn stub(State(running): State<Arc<Running>>, req: Request) -> Response {
    let auth = req.headers().get("authorization").cloned();
    let upgrade = req.headers().contains_key("upgrade");
    let bytes = req.into_body().collect().await.expect("a body").to_bytes();
    let sent: Value = serde_json::from_slice(&bytes).expect("JSON");
    if sent["input"] == "fail" {
        let refused =
            json!({"error": {"type": "invalid_request_error", "code": "bad", "message": "no"}});
        return http::Response::builder()
            .status(400)
            .body(Body::from(refused.to_string()))
            .expect("a response");
    }
    let now = running.0.fetch_add(1, Ordering::SeqCst) + 1;
    running.1.fetch_max(now, Ordering::SeqCst);
    let created = json!({"type": "response.created", "sent": sent, "auth": auth.map(|a| a.to_str().expect("text").to_owned()), "upgrade": upgrade});
    let delay = sent["delay"].as_u64().unwrap_or(0);
    let input = sent["input"].clone();
    let events = futures::stream::iter([0, 1]).then(move |n| {
        let (running, input, created) = (running.clone(), input.clone(), created.clone());
        async move {
            let event = match n {
                0 => created,
                _ => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    running.0.fetch_sub(1, Ordering::SeqCst);
                    json!({"type": "response.completed", "input": input})
                }
            };
            Ok::<_, std::io::Error>(format!("event: x\ndata: {event}\n\n"))
        }
    });
    Response::new(Body::from_stream(
        events.chain(futures::stream::iter([Ok("data: [DONE]\n\n".into())])),
    ))
}

type Client = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn serve() -> (String, Arc<Running>) {
    let running = Arc::new(Running::default());
    let app = Router::new()
        .fallback(stub)
        .with_state(running.clone())
        .layer(ResponsesWebSocket);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let at = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("ws://{at}/v1/responses?x=1"), running)
}

async fn connect(url: &str) -> Client {
    let mut req = url.into_client_request().expect("a request");
    req.headers_mut()
        .insert("authorization", HeaderValue::from_static("Bearer k"));
    tokio_tungstenite::connect_async(req)
        .await
        .expect("upgraded")
        .0
}

async fn send(ws: &mut Client, event: Value) {
    ws.send(Message::text(event.to_string()))
        .await
        .expect("sent");
}

async fn next(ws: &mut Client) -> Value {
    loop {
        match ws.next().await.expect("a frame").expect("a message") {
            Message::Text(text) => return serde_json::from_str(&text).expect("JSON"),
            _ => continue,
        }
    }
}

fn create(input: &str, lane: Option<&str>, delay: u64) -> Value {
    let mut event =
        json!({"type": "response.create", "model": "m", "input": input, "delay": delay});
    if let Some(lane) = lane {
        event["stream_id"] = lane.into();
    }
    event
}

#[tokio::test]
async fn a_turn_is_a_streamed_post_with_the_upgrades_headers() {
    let (url, _) = serve().await;
    let mut ws = connect(&url).await;
    send(&mut ws, create("hi", None, 0)).await;
    let created = next(&mut ws).await;
    assert_eq!(created["type"], "response.created");
    let sent = &created["sent"];
    assert_eq!(sent["stream"], true);
    assert!(sent.get("type").is_none() && sent.get("stream_id").is_none());
    assert_eq!(created["auth"], "Bearer k");
    assert_eq!(created["upgrade"], false);
    let completed = next(&mut ws).await;
    assert_eq!(completed["type"], "response.completed");
    assert!(completed.get("stream_id").is_none());
}

#[tokio::test]
async fn a_refused_turn_is_an_error_event_and_the_socket_goes_on() {
    let (url, _) = serve().await;
    let mut ws = connect(&url).await;
    send(&mut ws, create("fail", Some("a"), 0)).await;
    let error = next(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 400);
    assert_eq!(error["error"]["code"], "bad");
    assert_eq!(error["stream_id"], "a");
    send(&mut ws, json!({"type": "response.cancel"})).await;
    assert_eq!(
        next(&mut ws).await["error"]["type"],
        "invalid_request_error"
    );
    send(&mut ws, create("ok", None, 0)).await;
    assert_eq!(next(&mut ws).await["type"], "response.created");
}

#[tokio::test]
async fn lanes_run_side_by_side_and_a_lanes_turns_in_order() {
    let (url, _) = serve().await;
    let mut ws = connect(&url).await;
    send(&mut ws, create("a1", Some("a"), 300)).await;
    send(&mut ws, create("a2", Some("a"), 0)).await;
    send(&mut ws, create("b1", Some("b"), 0)).await;
    let mut seen = Vec::new();
    for _ in 0..6 {
        let event = next(&mut ws).await;
        let input = match event["type"].as_str() {
            Some("response.created") => event["sent"]["input"].clone(),
            _ => event["input"].clone(),
        };
        let lane = event["stream_id"].as_str().expect("a lane").to_owned();
        assert_eq!(lane, input.as_str().expect("an input")[..1]);
        seen.push(format!(
            "{}:{}",
            event["type"].as_str().expect("a type"),
            input.as_str().expect("an input")
        ));
    }
    let at = |s: &str| seen.iter().position(|e| e == s).expect(s);
    assert!(
        at("response.completed:b1") < at("response.completed:a1"),
        "{seen:?}"
    );
    assert!(
        at("response.completed:a1") < at("response.created:a2"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_socket_runs_16_turns_at_once_and_names_32_lanes() {
    let (url, running) = serve().await;
    let mut ws = connect(&url).await;
    for n in 0..20 {
        send(&mut ws, create("x", Some(&format!("l{n}")), 200)).await;
    }
    let mut completed = 0;
    while completed < 20 {
        completed += usize::from(next(&mut ws).await["type"] == "response.completed");
    }
    assert_eq!(running.1.load(Ordering::SeqCst), IN_FLIGHT);
    for n in 20..33 {
        send(&mut ws, create("x", Some(&format!("l{n}")), 0)).await;
    }
    let refused = loop {
        let event = next(&mut ws).await;
        if event["type"] == "error" {
            break event;
        }
    };
    assert_eq!(refused["error"]["code"], "websocket_stream_limit_reached");
    assert_eq!(refused["stream_id"], "l32");
    send(&mut ws, create("x", Some(""), 0)).await;
    let refused = loop {
        let event = next(&mut ws).await;
        if event["type"] == "error" {
            break event;
        }
    };
    assert_eq!(refused["error"]["code"], "invalid_stream_id");
}

#[tokio::test]
async fn any_other_request_passes_through() {
    let running = Arc::new(Running::default());
    let mut app = Router::new()
        .fallback(stub)
        .with_state(running)
        .layer(ResponsesWebSocket);
    let req = http::Request::post("/v1/responses")
        .body(Body::from(r#"{"input":"fail"}"#))
        .expect("a request");
    poll_fn(|cx| Service::<Request>::poll_ready(&mut app, cx))
        .await
        .expect("ready");
    let resp = app.call(req).await.expect("an answer");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
