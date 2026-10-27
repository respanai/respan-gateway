//! Tool argument scanning and redaction with valid JSON and original offsets.
use crate::{
    config::{Action, enabled},
    errors::{fault, scan_fault},
    scan::Inspection,
    text::{inspect_text, reject_injection},
};
use patronus_ark_core::{ScanChecks, ScanControl};
use respan_gateway_core::Fault;

pub(super) fn inspect_arguments_json(
    json: &str,
    inspection: &mut Inspection<'_>,
    path: &str,
    checks: ScanChecks,
) -> Result<serde_json::Value, Fault> {
    let native_checks = ScanChecks {
        injection: false,
        ..checks
    };
    let native = if enabled(native_checks) {
        inspection.scan(json, native_checks)?.native
    } else {
        Vec::new()
    };
    let mut json_spans = Vec::new();
    for result in &native {
        let blocking = match result.category.as_str() {
            "pii" => matches!(inspection.config.pii, Action::Block),
            "dlp" => matches!(inspection.config.dlp, Action::Block),
            _ => false,
        };
        if blocking && !result.evidence_spans.is_empty() {
            return Err(fault(
                "ark_sensitive_content",
                &format!("Ark blocked sensitive content in {path}"),
            ));
        }
        if !blocking && matches!(result.category.as_str(), "pii" | "dlp") {
            json_spans.extend(
                result
                    .evidence_spans
                    .iter()
                    .map(|span| (span.start_byte, span.end_byte)),
            );
        }
    }
    let mut value: serde_json::Value = serde_json::from_str(json).map_err(|_| {
        fault(
            "ark_invalid_tool_arguments",
            "Ark requires valid JSON tool arguments",
        )
    })?;
    if checks.injection {
        let decoded = decoded_json_document(&value, inspection.control)?;
        let report = inspection.scan(
            &decoded,
            ScanChecks {
                pii: false,
                dlp: false,
                injection: true,
            },
        )?;
        reject_injection(&report, path)?;
    }
    if !json_spans.is_empty() {
        // Raw findings retain field-name context; mask their containing JSON
        // values before checking decoded strings for escaped sensitive data.
        let redacted = redact_json_values(json, &json_spans)?;
        value = serde_json::from_str(&redacted)
            .map_err(|_| Fault::internal("Ark JSON redaction failed"))?;
    }
    inspect_json(&mut value, inspection, path, native_checks)?;
    Ok(value)
}

/// Map source evidence to whole JSON scalar values. The caller has already
/// validated JSON syntax. Never rewrite a key or splice across JSON tokens.
pub(super) fn redact_json_values(json: &str, spans: &[(usize, usize)]) -> Result<String, Fault> {
    let bytes = json.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        match bytes[index] {
            b'"' => {
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                let mut next = index;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                let key = next < bytes.len() && bytes[next] == b':';
                tokens.push((start, index, key));
            }
            b'-' | b'0'..=b'9' => {
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    index += 1;
                }
                tokens.push((start, index, false));
            }
            _ => index += 1,
        }
    }
    let mut replacements = Vec::new();
    for &(start, end) in spans {
        if start >= end
            || end > json.len()
            || !json.is_char_boundary(start)
            || !json.is_char_boundary(end)
        {
            return Err(Fault::internal("Ark returned invalid evidence offsets"));
        }
        let token = tokens.get(tokens.partition_point(|&(_, end, _)| end <= start));
        let Some(&(token_start, token_end, key)) = token else {
            return Err(fault(
                "ark_unsafe_json_redaction",
                "Ark cannot safely redact this JSON finding",
            ));
        };
        if key {
            return Err(fault(
                "ark_sensitive_key",
                "Ark cannot mask a sensitive tool argument key",
            ));
        }
        if token_start > start || token_end < end {
            return Err(fault(
                "ark_unsafe_json_redaction",
                "Ark cannot safely redact this JSON finding",
            ));
        }
        replacements.push((token_start, token_end));
    }
    replacements.sort_unstable();
    replacements.dedup();
    let mut redacted = json.to_owned();
    for (start, end) in replacements.into_iter().rev() {
        redacted.replace_range(start..end, "\"[REDACTED]\"");
    }
    Ok(redacted)
}

pub(super) fn decoded_json_document(
    value: &serde_json::Value,
    control: &ScanControl,
) -> Result<String, Fault> {
    fn append(
        value: &serde_json::Value,
        out: &mut String,
        control: &ScanControl,
    ) -> Result<(), Fault> {
        control.check().map_err(scan_fault)?;
        match value {
            serde_json::Value::String(text) => {
                out.push('"');
                out.push_str(text);
                out.push('"');
            }
            serde_json::Value::Array(values) => {
                out.push('[');
                for value in values {
                    append(value, out, control)?;
                    out.push(',');
                }
                out.push(']');
            }
            serde_json::Value::Object(values) => {
                out.push('{');
                for (key, value) in values {
                    out.push('"');
                    out.push_str(key);
                    out.push_str("\":");
                    append(value, out, control)?;
                    out.push(',');
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
        Ok(())
    }
    let mut document = String::new();
    append(value, &mut document, control)?;
    Ok(document)
}

fn inspect_json(
    value: &mut serde_json::Value,
    inspection: &mut Inspection<'_>,
    path: &str,
    checks: ScanChecks,
) -> Result<(), Fault> {
    inspection.control.check().map_err(scan_fault)?;
    match value {
        serde_json::Value::String(text) => inspect_text(text, inspection, path, checks)?,
        serde_json::Value::Array(values) => {
            for (i, value) in values.iter_mut().enumerate() {
                inspect_json(value, inspection, &format!("{path}[{i}]"), checks)?;
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values.iter_mut() {
                let mut checked_key = key.clone();
                inspect_text(&mut checked_key, inspection, &format!("{path}.key"), checks)?;
                if checked_key != *key {
                    return Err(fault(
                        "ark_sensitive_key",
                        "Ark cannot mask a sensitive tool argument key",
                    ));
                }
                inspect_json(value, inspection, &format!("{path}.value"), checks)?;
            }
        }
        serde_json::Value::Number(number) => {
            let mut checked = number.to_string();
            inspect_text(&mut checked, inspection, path, checks)?;
            if checked != number.to_string() {
                *value = serde_json::Value::String(checked);
            }
        }
        _ => {}
    }
    Ok(())
}
