//! Map typed Core failures and adapter admission errors to Respan faults.
use patronus_ark_core::ScanError;
use respan_gateway_core::{ErrorCode, Fault};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;

pub(super) fn fault(code: &'static str, message: &str) -> Fault {
    let status = match code {
        "ark_overloaded" => StatusCode::TOO_MANY_REQUESTS,
        "ark_queue_timeout" | "ark_scan_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "ark_input_limit" | "ark_finding_limit" => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::BAD_REQUEST,
    };
    let fault = if matches!(
        code,
        "ark_overloaded" | "ark_queue_timeout" | "ark_scan_timeout"
    ) {
        Fault::new(message)
    } else {
        Fault::bad_request(message)
    };
    fault.with(ErrorCode(code.into())).with(Status(status))
}
pub(super) fn scan_fault(error: ScanError) -> Fault {
    match error {
        ScanError::InputLimit { .. } => {
            fault("ark_input_limit", "Ark scanner input limit exceeded")
        }
        ScanError::FindingLimit { .. } => {
            fault("ark_finding_limit", "Ark scanner finding limit exceeded")
        }
        ScanError::Cancelled => Fault::abandoned().with(ErrorCode("ark_scan_cancelled".into())),
        ScanError::DeadlineExceeded => fault("ark_scan_timeout", "Ark scan deadline exceeded"),
        ScanError::Config(_) => Fault::config("Ark scanner configuration failed")
            .with(ErrorCode("ark_config_failed".into())),
        ScanError::Asset(_) => Fault::internal("Ark model assets unavailable")
            .with(ErrorCode("ark_asset_failed".into())),
        ScanError::Inference(_) => Fault::internal("Ark model inference failed")
            .with(ErrorCode("ark_inference_failed".into())),
        ScanError::Detector(_) => Fault::internal("Ark native detector failed")
            .with(ErrorCode("ark_detector_failed".into())),
    }
}
