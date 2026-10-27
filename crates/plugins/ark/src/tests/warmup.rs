use super::*;
use patronus_ark_core::{DownloadPolicy, WarmupOptions};

#[test]
fn startup_policy_is_explicit_and_offline_by_default() {
    let config = Config::default();
    assert_eq!(config.warmup.download, DownloadPolicy::Never);
    assert_eq!(config.warmup.download_timeout_ms, 600_000);
    let config: Config = serde_json::from_value(
        serde_json::json!({"warmup":{"download":"missing","download_timeout_ms":1234}}),
    )
    .unwrap();
    assert_eq!(config.warmup.download, DownloadPolicy::Missing);
    assert_eq!(config.warmup.download_timeout_ms, 1234);
    assert!(
        serde_json::from_value::<Config>(serde_json::json!({"warmup":{"download":"always"}}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<Config>(serde_json::json!({"warmup":{"unknown_option":1}}))
            .is_err()
    );
    assert!(
        Ark::new(Config {
            warmup: WarmupOptions {
                download_timeout_ms: 0,
                ..WarmupOptions::default()
            },
            ..Config::default()
        })
        .is_err()
    );
}

#[cfg(feature = "wolf")]
fn model_config(bundle: &super::request_reuse::WolfTestBundle) -> Config {
    let mut config = Config::default();
    let mut wolf = patronus_ark_core::wolf::WolfConfig::new(&bundle.0, 1.0);
    wolf.precision = patronus_ark_core::wolf::WolfPrecision::Fp16;
    config.scanner.wolf = Some(wolf);
    config
}
#[cfg(feature = "wolf")]
#[tokio::test]
async fn cached_warmup_returns_the_session_used_for_request_scans() {
    use respan_gateway_conversation::Message;
    let bundle = super::request_reuse::WolfTestBundle::new();
    let mut config = model_config(&bundle);
    config.warmup.download = DownloadPolicy::Missing;
    let guard = Ark::new(config).unwrap();
    let provider = guard.provider_guard();
    assert!(Arc::ptr_eq(&guard.0, &provider.0.0));
    // A second loader or request-time download would now fail. The already-warm
    // runtime must serve both independent inspections from its loaded session.
    std::fs::remove_file(bundle.0.join("model_fp16.onnx")).unwrap();
    for _ in 0..2 {
        assert!(
            guard
                .inspect(Input::new([Message::new(Role::User, "hello")]))
                .await
                .is_ok()
        );
    }
    assert_eq!(guard.0.model_documents.load(Ordering::Relaxed), 2);
}
#[cfg(feature = "wolf")]
#[test]
fn offline_startup_never_prepares_a_missing_model() {
    let bundle = super::request_reuse::WolfTestBundle::new();
    std::fs::remove_file(bundle.0.join("model_fp16.onnx")).unwrap();
    assert!(Ark::new(model_config(&bundle)).is_err());
    assert!(!bundle.0.join("model_fp16.onnx").exists());
    assert!(!bundle.0.join(".patronus-wolf-prepare.lock").exists());
}
#[cfg(feature = "wolf")]
#[test]
fn startup_asset_budget_is_separate_from_request_deadlines() {
    use fs2::FileExt;
    let bundle = super::request_reuse::WolfTestBundle::new();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(bundle.0.join(".patronus-wolf-prepare.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let mut config = model_config(&bundle);
    config.warmup = WarmupOptions {
        download: DownloadPolicy::Missing,
        download_timeout_ms: 50,
    };
    config.scan_timeout_ms = 30_000;
    assert!(
        Ark::new(config.clone()).is_err(),
        "held cache preparation lock must consume only its startup asset budget"
    );
    FileExt::unlock(&lock).unwrap();
    config.warmup.download_timeout_ms = 30_000;
    assert!(
        Ark::new(config).is_ok(),
        "ready local assets must warm successfully without any network request"
    );
}
