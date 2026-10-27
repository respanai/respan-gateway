//! Request-local proofs for unchanged fields, scoped to source and policy.
use crate::{
    config::{ScanSource, enabled},
    runtime::State,
};
use patronus_ark_core::ScanChecks;
use respan_gateway_conversation::{Content, Input, Role};
use respan_gateway_core::Fault;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

pub(super) const MAX_REQUEST_STAMPS: usize = 4096;
#[derive(Clone, Default)]
pub(super) struct RequestChecks(pub(super) Arc<Mutex<HashSet<ScanStamp>>>);
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(super) struct ScanStamp {
    scanner: u64,
    source: u8,
    checks: u8,
    representation: u8,
    digest: [u8; 32],
}
impl ScanStamp {
    pub(super) fn new(
        scanner: u64,
        source: ScanSource,
        checks: ScanChecks,
        representation: u8,
        text: &str,
    ) -> Self {
        Self {
            scanner,
            source: source as u8,
            checks: u8::from(checks.pii)
                | (u8::from(checks.dlp) << 1)
                | (u8::from(checks.injection) << 2),
            representation,
            digest: *blake3::hash(text.as_bytes()).as_bytes(),
        }
    }
}
pub(super) fn request_is_checked(
    input: &Input,
    state: &State,
    ledger: &RequestChecks,
) -> Result<bool, Fault> {
    fn part(
        content: &Content,
        source: Option<ScanSource>,
        state: &State,
        checked: &HashSet<ScanStamp>,
    ) -> bool {
        let known = |source, representation, text: &str| {
            let checks = state.config.checks(source);
            !enabled(checks)
                || checked.contains(&ScanStamp::new(
                    state.identity,
                    source,
                    checks,
                    representation,
                    text,
                ))
        };
        match content {
            Content::Text { text, .. } => {
                text.is_empty() || source.is_none_or(|source| known(source, 0, text))
            }
            Content::ToolCall { arguments, .. } => known(ScanSource::ToolCall, 1, arguments),
            Content::ToolResult { content, .. } => content
                .iter()
                .all(|content| part(content, Some(ScanSource::ToolResult), state, checked)),
            _ => true, // preflight rejects unsupported selected content before this lookup.
        }
    }
    let checked = ledger
        .0
        .lock()
        .map_err(|_| Fault::internal("Ark request checks poisoned"))?;
    Ok(input.messages.iter().all(|message| {
        let source = matches!(message.role, Role::User).then_some(ScanSource::UserInput);
        message
            .content
            .iter()
            .all(|content| part(content, source, state, &checked))
    }))
}
