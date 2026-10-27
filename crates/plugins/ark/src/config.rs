//! Public policy and scan-source configuration.
use patronus_ark_core::{ScanChecks, ScannerConfig, WarmupOptions};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Block,
    #[serde(alias = "redact")]
    Mask,
}
/// Typed Respan chat sources supported by this plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanSource {
    UserInput,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scanner: ScannerConfig,
    pub warmup: WarmupOptions,
    pub pii: Action,
    pub dlp: Action,
    pub pii_sources: Vec<ScanSource>,
    pub dlp_sources: Vec<ScanSource>,
    pub pi_sources: Vec<ScanSource>,
    pub max_queued: usize,
    pub queue_timeout_ms: u64,
    pub scan_timeout_ms: u64,
    pub max_request_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            scanner: ScannerConfig::default(),
            warmup: WarmupOptions::default(),
            pii: Action::Block,
            dlp: Action::Block,
            pii_sources: vec![
                ScanSource::UserInput,
                ScanSource::ToolCall,
                ScanSource::ToolResult,
            ],
            dlp_sources: vec![
                ScanSource::UserInput,
                ScanSource::ToolCall,
                ScanSource::ToolResult,
            ],
            pi_sources: vec![ScanSource::UserInput, ScanSource::ToolResult],
            max_queued: 32,
            queue_timeout_ms: 1000,
            scan_timeout_ms: 30000,
            max_request_bytes: 1024 * 1024,
        }
    }
}
impl Config {
    pub(super) fn checks(&self, source: ScanSource) -> ScanChecks {
        ScanChecks {
            pii: self.pii_sources.contains(&source),
            dlp: self.dlp_sources.contains(&source),
            injection: self.pi_sources.contains(&source),
        }
    }
}
pub(super) fn enabled(checks: ScanChecks) -> bool {
    checks.pii || checks.dlp || checks.injection
}
