//! Traverse selected chat fields and run Core checks under one work budget.
use crate::{
    config::{Config, ScanSource, enabled},
    errors::{fault, scan_fault},
    json::inspect_arguments_json,
    request::{MAX_REQUEST_STAMPS, RequestChecks, ScanStamp},
    text::inspect_text,
};
use patronus_ark_core::{ScanChecks, ScanControl, ScanError, Scanner};
use respan_gateway_conversation::{Content, Input, Role};
use respan_gateway_core::Fault;
use std::collections::HashSet;
#[cfg(test)]
use std::sync::atomic::Ordering;

pub(super) struct Inspection<'a> {
    pub(super) scanner: &'a mut Scanner,
    pub(super) config: &'a Config,
    pub(super) control: &'a ScanControl,
    pub(super) ledger: &'a RequestChecks,
    pub(super) identity: u64,
    pub(super) pending: HashSet<ScanStamp>,
    #[cfg(test)]
    pub(super) scans: Option<&'a std::sync::atomic::AtomicUsize>,
    #[cfg(all(test, feature = "wolf"))]
    pub(super) model_documents: Option<&'a std::sync::atomic::AtomicUsize>,
}
impl Inspection<'_> {
    pub(super) fn scan(
        &mut self,
        text: &str,
        checks: ScanChecks,
    ) -> Result<patronus_ark_core::ScanReport, Fault> {
        self.control.check().map_err(scan_fault)?;
        #[cfg(test)]
        if let Some(scans) = self.scans {
            scans.fetch_add(1, Ordering::Relaxed);
        }
        let report = self
            .scanner
            .scan_with_control(text, checks, self.control)
            .map_err(scan_fault)?;
        #[cfg(all(test, feature = "wolf"))]
        if report.wolf.is_some()
            && let Some(counter) = self.model_documents
        {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        Ok(report)
    }
    fn known(&self, stamp: &ScanStamp) -> Result<bool, Fault> {
        self.control.check().map_err(scan_fault)?;
        Ok(self.pending.contains(stamp)
            || self
                .ledger
                .0
                .lock()
                .map_err(|_| Fault::internal("Ark request checks poisoned"))?
                .contains(stamp))
    }
    fn remember(&mut self, stamp: ScanStamp) {
        if self.pending.len() < MAX_REQUEST_STAMPS {
            self.pending.insert(stamp);
        }
    }
}

pub(super) fn preflight(input: &Input, config: &Config) -> Result<bool, Fault> {
    fn visit(
        content: &Content,
        source: Option<ScanSource>,
        config: &Config,
        total: &mut usize,
        selected: &mut bool,
        depth: usize,
    ) -> Result<(), Fault> {
        if depth > 32 {
            return Err(fault(
                "ark_input_limit",
                "Ark content nesting limit exceeded",
            ));
        }
        match content {
            Content::Text { text, .. }
                if source.is_some_and(|source| enabled(config.checks(source))) =>
            {
                if text.len() > config.scanner.max_input_bytes {
                    return Err(scan_fault(ScanError::InputLimit {
                        limit: config.scanner.max_input_bytes,
                    }));
                }
                *selected |= !text.is_empty();
                *total = total.saturating_add(text.len());
            }
            Content::Text { .. } | Content::Reasoning { .. } => {}
            Content::ToolCall { arguments, .. } => {
                if enabled(config.checks(ScanSource::ToolCall)) {
                    if arguments.len() > config.scanner.max_input_bytes {
                        return Err(scan_fault(ScanError::InputLimit {
                            limit: config.scanner.max_input_bytes,
                        }));
                    }
                    *selected = true;
                    *total = total.saturating_add(arguments.len());
                }
            }
            Content::ToolResult { content, .. } => {
                for part in content {
                    visit(
                        part,
                        Some(ScanSource::ToolResult),
                        config,
                        total,
                        selected,
                        depth + 1,
                    )?;
                }
            }
            _ if source.is_some_and(|source| enabled(config.checks(source))) => {
                return Err(fault(
                    "ark_unsupported_content",
                    "Ark cannot scan this content type",
                ));
            }
            _ => {}
        }
        Ok(())
    }
    let mut total = 0;
    let mut selected = false;
    for message in &input.messages {
        let source = matches!(message.role, Role::User).then_some(ScanSource::UserInput);
        for content in &message.content {
            visit(content, source, config, &mut total, &mut selected, 0)?;
        }
    }
    if total > config.max_request_bytes {
        return Err(fault("ark_input_limit", "Ark request byte limit exceeded"));
    }
    Ok(selected)
}
#[cfg(test)]
pub(super) fn inspect_input(
    input: Input,
    scanner: &mut Scanner,
    config: &Config,
) -> Result<Input, Fault> {
    let control = ScanControl::default();
    let ledger = RequestChecks::default();
    inspect_input_controlled(
        input,
        &mut Inspection {
            scanner,
            config,
            control: &control,
            ledger: &ledger,
            identity: 0,
            pending: HashSet::new(),
            scans: None,
            #[cfg(feature = "wolf")]
            model_documents: None,
        },
    )
}
pub(super) fn inspect_input_controlled(
    mut input: Input,
    inspection: &mut Inspection<'_>,
) -> Result<Input, Fault> {
    fn part(
        content: &mut Content,
        source: Option<ScanSource>,
        inspection: &mut Inspection<'_>,
        path: &str,
    ) -> Result<(), Fault> {
        inspection.control.check().map_err(scan_fault)?;
        match content {
            Content::Text { text, .. } => {
                if let Some(source) = source {
                    let checks = inspection.config.checks(source);
                    let stamp = ScanStamp::new(inspection.identity, source, checks, 0, text);
                    if enabled(checks) && !text.is_empty() && !inspection.known(&stamp)? {
                        inspect_text(text, inspection, path, checks)?;
                        inspection.remember(ScanStamp::new(
                            inspection.identity,
                            source,
                            checks,
                            0,
                            text,
                        ));
                    }
                }
                Ok(())
            }
            Content::Reasoning { .. } => Ok(()),
            Content::ToolCall { arguments, .. } => {
                let source = ScanSource::ToolCall;
                let checks = inspection.config.checks(source);
                let stamp = ScanStamp::new(inspection.identity, source, checks, 1, arguments);
                if enabled(checks) && !inspection.known(&stamp)? {
                    let value = inspect_arguments_json(arguments, inspection, path, checks)?;
                    *arguments = serde_json::to_string(&value)
                        .map_err(|_| Fault::internal("Ark JSON encoding failed"))?;
                    inspection.remember(ScanStamp::new(
                        inspection.identity,
                        source,
                        checks,
                        1,
                        arguments,
                    ));
                }
                Ok(())
            }
            Content::ToolResult { content, .. } => {
                for (i, content) in content.iter_mut().enumerate() {
                    part(
                        content,
                        Some(ScanSource::ToolResult),
                        inspection,
                        &format!("{path}.result[{i}]"),
                    )?;
                }
                Ok(())
            }
            _ if source.is_some_and(|source| enabled(inspection.config.checks(source))) => {
                Err(fault(
                    "ark_unsupported_content",
                    "Ark cannot scan this content type",
                ))
            }
            _ => Ok(()),
        }
    }
    preflight(&input, inspection.config)?;
    for (m, message) in input.messages.iter_mut().enumerate() {
        let source = matches!(message.role, Role::User).then_some(ScanSource::UserInput);
        for (c, content) in message.content.iter_mut().enumerate() {
            part(
                content,
                source,
                inspection,
                &format!("messages[{m}].content[{c}]"),
            )?;
        }
    }
    Ok(input)
}
