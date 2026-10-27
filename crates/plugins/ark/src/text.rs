//! Text policy decisions and merged UTF-8 evidence redaction.
use crate::{
    config::{Action, enabled},
    errors::fault,
    scan::Inspection,
};
use patronus_ark_core::ScanChecks;
use respan_gateway_core::Fault;

pub(super) fn reject_injection(
    report: &patronus_ark_core::ScanReport,
    path: &str,
) -> Result<(), Fault> {
    #[cfg(feature = "wolf")]
    if report.wolf.as_ref().is_some_and(|r| r.matched) {
        return Err(fault(
            "ark_injection_blocked",
            &format!("Ark blocked prompt injection in {path}"),
        ));
    }
    if report.native.iter().any(|result| {
        result.category == "injection"
            && result
                .decision
                .as_ref()
                .is_some_and(|d| d.recommendation.accepted)
    }) {
        return Err(fault(
            "ark_injection_blocked",
            &format!("Ark blocked prompt injection in {path}"),
        ));
    }
    Ok(())
}

pub(super) fn inspect_text(
    text: &mut String,
    inspection: &mut Inspection<'_>,
    path: &str,
    checks: ScanChecks,
) -> Result<(), Fault> {
    if !enabled(checks) || text.is_empty() {
        return Ok(());
    }
    let report = inspection.scan(text, checks)?;
    reject_injection(&report, path)?;
    let mut intervals = Vec::new();
    for result in report.native {
        if result.category == "injection" {
            continue;
        }
        if result.evidence_spans.is_empty() {
            continue;
        }
        let action = if result.category == "pii" {
            inspection.config.pii
        } else {
            inspection.config.dlp
        };
        match action {
            Action::Block => {
                return Err(fault(
                    "ark_sensitive_content",
                    &format!("Ark blocked {} in {path}", result.category),
                ));
            }
            Action::Mask => {
                for span in result.evidence_spans {
                    intervals.push((span.start_byte, span.end_byte));
                }
            }
        }
    }
    intervals.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in intervals {
        if start >= end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(Fault::internal("Ark returned invalid evidence offsets"));
        }
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    for (start, end) in merged.into_iter().rev() {
        text.replace_range(start..end, "[REDACTED]");
    }
    Ok(())
}
