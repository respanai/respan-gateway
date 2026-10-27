use super::*;
use respan_gateway_conversation::{Message, Role};

#[test]
fn redaction_alias_and_unknown_configuration_are_explicit() {
    for spelling in ["redact", "mask"] {
        let config: Config =
            serde_json::from_value(serde_json::json!({"pii":spelling,"dlp":spelling})).unwrap();
        assert!(matches!(config.pii, Action::Mask));
        assert!(matches!(config.dlp, Action::Mask));
    }
    assert!(serde_json::from_value::<Config>(serde_json::json!({"unknown_policy":true})).is_err());
}

#[test]
fn raw_json_redaction_preserves_escaping_and_deduplicates_value_spans() {
    let json = r#"{"password":"str\"ange","note":"keep"}"#;
    let start = json.find("str").unwrap();
    let redacted = redact_json_values(json, &[(start, start + 2), (start + 1, start + 3)]).unwrap();
    let value: serde_json::Value = serde_json::from_str(&redacted).unwrap();
    assert_eq!(value["password"], "[REDACTED]");
    assert_eq!(value["note"], "keep");
    let json = r#"{"account":123456789,"note":"keep"}"#;
    let start = json.find("123456789").unwrap();
    let redacted = redact_json_values(json, &[(start, start + 9)]).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&redacted).unwrap()["account"],
        "[REDACTED]"
    );
}

#[test]
fn unsafe_key_cross_token_and_utf8_offsets_are_refused() {
    let json = r#"{"sensitive-key":"Grüße","other":"safe"}"#;
    let key = json.find("sensitive-key").unwrap();
    assert!(redact_json_values(json, &[(key, key + 3)]).is_err());
    let text = json.find("Grüße").unwrap();
    let other = json.find("other").unwrap();
    assert!(redact_json_values(json, &[(text, other + 2)]).is_err());
    let u = json.find('ü').unwrap();
    assert!(redact_json_values(json, &[(u + 1, u + 2)]).is_err());
}

#[tokio::test]
async fn client_cancellation_keeps_real_work_reserved_until_it_ends() {
    let guard = Ark::new(Config {
        max_queued: 0,
        ..Config::default()
    })
    .unwrap();
    let state = Arc::clone(&guard.0);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _lock = state.scanner.lock().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();
    let running_guard = guard.clone();
    let task = tokio::spawn(async move {
        running_guard
            .inspect(Input::new([Message::new(Role::User, "hello")]))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while guard.0.active.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(guard.0.active.available_permits(), 0);
    assert_eq!(guard.0.slots.available_permits(), 0);
    assert!(
        guard
            .inspect(Input::new([Message::new(Role::User, "hello")]))
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    blocker.join().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while guard.0.slots.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        guard
            .inspect(Input::new([Message::new(Role::User, "hello")]))
            .await
            .is_ok()
    );
}
