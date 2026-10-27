use super::*;
use respan_gateway_conversation::{Message, Role};

#[tokio::test]
async fn admission_full_and_wait_deadline_have_distinct_plugin_codes() {
    let guard = Ark::new(Config {
        max_queued: 1,
        queue_timeout_ms: 10,
        ..Config::default()
    })
    .unwrap();
    let accepted = Arc::clone(&guard.0.slots).acquire_owned().await.unwrap();
    let active = Arc::clone(&guard.0.active).acquire_owned().await.unwrap();
    let queued = Arc::clone(&guard.0.slots).acquire_owned().await.unwrap();
    let input = || Input::new([Message::new(Role::User, "hello")]);
    assert_eq!(
        guard.inspect(input()).await.unwrap_err().code(),
        "ark_overloaded"
    );
    drop(queued);
    assert_eq!(
        guard.inspect(input()).await.unwrap_err().code(),
        "ark_queue_timeout"
    );
    assert_eq!(guard.0.slots.available_permits(), 1);
    drop(active);
    drop(accepted);
    assert!(guard.inspect(input()).await.is_ok());
}
