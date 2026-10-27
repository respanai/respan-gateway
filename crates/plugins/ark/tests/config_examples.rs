//! Keep the copyable README and recipe configurations aligned with the public API.
#![allow(clippy::unwrap_used)]
use respan_gateway_plugin_ark::{Action, Config, ScanSource};

fn config_from_document(document: &str) -> Option<Config> {
    let document: toml::Value = toml::from_str(document).unwrap();
    let ark = document.get("plugins").unwrap().get("ark").unwrap();
    #[cfg(not(feature = "wolf"))]
    if ark.get("scanner").and_then(|s| s.get("wolf")).is_some() {
        // Wolf examples deliberately require the documented ark-wolf build.
        assert!(ark.clone().try_into::<Config>().is_err());
        return None;
    }
    Some(ark.clone().try_into().unwrap())
}

#[test]
fn every_readme_toml_example_is_valid_for_its_documented_build() {
    let readme = include_str!("../README.md");
    let mut count = 0;
    for block in readme.split("```toml\n").skip(1) {
        let source = block.split("```").next().unwrap();
        config_from_document(source);
        count += 1;
    }
    assert!(
        count >= 8,
        "expected the quickstart and configuration recipes"
    );
}

#[test]
fn copyable_recipes_express_the_documented_policy() {
    let native = config_from_document(include_str!("../examples/config/native-only.toml")).unwrap();
    assert_eq!(
        native.warmup.download,
        patronus_ark_core::DownloadPolicy::Never
    );
    assert!(matches!(native.pii, Action::Mask));
    assert!(matches!(native.dlp, Action::Mask));
    assert_eq!(
        native.pi_sources,
        [ScanSource::UserInput, ScanSource::ToolResult]
    );
    #[cfg(feature = "wolf")]
    {
        use patronus_ark_core::wolf::WolfPrecision;
        assert!(native.scanner.wolf.is_none());
        let block = config_from_document(include_str!("../examples/config/block.toml")).unwrap();
        assert_eq!(
            block.warmup.download,
            patronus_ark_core::DownloadPolicy::Missing
        );
        assert_eq!(block.warmup.download_timeout_ms, 600_000);
        assert!(matches!(block.pii, Action::Block));
        assert!(matches!(block.dlp, Action::Block));
        let redact = config_from_document(include_str!("../examples/config/redact.toml")).unwrap();
        assert!(matches!(redact.pii, Action::Mask));
        assert!(matches!(redact.dlp, Action::Mask));
        let sources =
            config_from_document(include_str!("../examples/config/select-sources.toml")).unwrap();
        assert_eq!(sources.pi_sources, [ScanSource::ToolResult]);
        assert_eq!(sources.pii_sources.len(), 3);
        assert_eq!(sources.dlp_sources.len(), 3);
        assert_eq!(
            sources.scanner.wolf.unwrap().precision,
            WolfPrecision::Int8Int4
        );
        let pi = config_from_document(include_str!("../examples/config/pi-only.toml")).unwrap();
        assert!(pi.pii_sources.is_empty() && pi.dlp_sources.is_empty());
        assert_eq!(pi.scanner.wolf.unwrap().precision, WolfPrecision::Fp16);
        let bounded =
            config_from_document(include_str!("../examples/config/bounded-capacity.toml")).unwrap();
        assert_eq!(bounded.max_queued, 8);
        assert_eq!(bounded.queue_timeout_ms, 500);
        assert_eq!(bounded.scan_timeout_ms, 10_000);
        assert_eq!(bounded.scanner.max_input_bytes, bounded.max_request_bytes);
    }
}
