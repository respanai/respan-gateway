//! Local PII, DLP and prompt-injection guardrails for Respan chat requests.
//! Register [`Ark`] before answering plugins and [`ProviderGuard`] after input modifiers.
mod config;
mod errors;
mod json;
mod plugin;
mod request;
mod runtime;
mod scan;
mod text;

pub use config::{Action, Config, ScanSource};
pub use runtime::{Ark, ProviderGuard};

#[cfg(test)]
mod tests;
