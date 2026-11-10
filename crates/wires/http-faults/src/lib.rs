#![warn(missing_docs)]
//! Faults in HTTP's terms, with no I/O: the status and error code a
//! [`Fault`] asks for, whether a failed attempt is worth another, and a
//! provider's error answer read into a fault. A plugin that sets or reads a
//! status depends on this crate alone, not on the HTTP wire.

use respan_gateway_core::FaultKind;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::StatusCode;
use http::header::RETRY_AFTER;
use respan_gateway_core::{Fault, RetryAfter};

/// The HTTP status a fault asks for, as an extension ([`Fault::with`]): a plugin's
/// `Fault::new(msg).with(Status(StatusCode::TOO_MANY_REQUESTS))`, a
/// provider's answer's. The kernel names no numbers; this is where they get
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub StatusCode);

/// The status of `fault` as HTTP's: its [`Status`] extension, else its
/// kind's: a 400 for the client's (a 404 coded `not_found`), a 502 for a
/// provider's, a 499 for an abandoned one, else a 500.
pub fn status(fault: &Fault) -> StatusCode {
    if let Some(Status(status)) = fault.get() {
        return *status;
    }
    match fault.kind() {
        FaultKind::Client if fault.code() == "not_found" => StatusCode::NOT_FOUND,
        FaultKind::Client => StatusCode::BAD_REQUEST,
        FaultKind::Provider => StatusCode::BAD_GATEWAY,
        FaultKind::Abandoned => CLIENT_CLOSED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// 499, nginx's "client closed request".
const CLIENT_CLOSED: StatusCode = match StatusCode::from_u16(499) {
    Ok(status) => status,
    Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
};

/// The statuses RFC 9110 names as transient: a later attempt may succeed
/// (408, 425, 429, 500, 502, 503, 504).
const TRANSIENT: [StatusCode; 7] = [
    StatusCode::REQUEST_TIMEOUT,
    StatusCode::TOO_EARLY,
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

/// HTTP retry eligibility for a failed `first_success` attempt. Before a
/// response opened, only a provider's fault is retryable: the provider
/// could not be reached. After opening, a fault whose [`Status`] is
/// transient is, or a provider's without one. An abandoned fault never is:
/// a client that left is gone on a second attempt.
/// `response_opened` means the call returned `Ok(Response)` before the
/// caller's response-selection criterion ran, not that bytes arrived.
pub fn retryable(fault: &Fault, response_opened: bool) -> bool {
    match (fault.kind(), fault.get::<Status>()) {
        (kind, _) if !response_opened => kind == FaultKind::Provider,
        (FaultKind::Abandoned, _) => false,
        (_, Some(Status(status))) => TRANSIENT.contains(status),
        (kind, None) => kind == FaultKind::Provider,
    }
}

/// A message a plugin or format built wrong: a bug, a 500.
pub fn built_wrong(e: http::Error) -> Fault {
    Fault::internal("an HTTP request or response was built wrong").with_source(e)
}

/// A provider's error answer as it came, for a plugin to inspect or a relay
/// to write; never written unasked.
#[derive(Clone, Debug)]
pub struct ProviderError(pub http::Response<Bytes>);

/// The fault of a provider's `answer` that is no success, read as by a
/// format whose error bodies it does not read
/// (`respan_gateway_http::ProviderIo::with_refusal`). It is upstream, says the
/// answer's status (and that its body is empty, if it is), with that
/// [`Status`] if an error's, else a 502, and its
/// `Retry-After` ([`RetryAfter`]): whole seconds, or an HTTP date, the wait
/// until it (none if it is past). Unless informational (an unasked 101), it carries the answer as
/// it came ([`ProviderError`]), for a plugin to inspect or a relay to write.
pub fn provider_fault(answer: http::Response<Bytes>) -> Fault {
    let status = answer.status();
    let to = match status.is_client_error() || status.is_server_error() {
        true => status,
        false => StatusCode::BAD_GATEWAY,
    };
    // An error with nothing to say says so: often a route the provider does
    // not have, rather than a request it refused.
    let empty = match answer.body().is_empty() && !status.is_informational() {
        true => ", with no body",
        false => "",
    };
    let fault = Fault::upstream(format!("the provider returned {}{empty}", status.as_u16()));
    let fault = fault.with(Status(to));
    if status.is_informational() {
        return fault;
    }
    let fault = match waits(answer.headers()) {
        Some(wait) => fault.with(wait),
        None => fault,
    };
    fault.with(ProviderError(answer))
}

/// The fault of a provider's answer that holds more than `limit` bytes at
/// once: an event, or a whole body. It is upstream, so a 502.
pub fn over_limit(limit: usize) -> Fault {
    Fault::upstream(format!(
        "the provider's answer is over the limit of {limit} bytes"
    ))
}

/// The wait an answer's `Retry-After` asks for, if it asks for one.
pub fn waits(headers: &http::HeaderMap) -> Option<RetryAfter> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    retry_after(value, SystemTime::now()).map(RetryAfter)
}

/// The wait a `Retry-After` value asks for at `now` (RFC 9110 10.2.3):
/// whole seconds, or the time until an HTTP date, `None` once it is past.
fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    match value.parse() {
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::ErrorCode;

    // An answer that is no error says a 502; an unasked 101 carries no
    // answer, as it was never relayed.
    #[test]
    fn a_refused_success_is_a_502_and_an_unasked_101_carries_no_answer() {
        for (status, carried) in [(200, true), (101, false)] {
            let mut answer = http::Response::new(Bytes::from_static(b"x"));
            *answer.status_mut() = StatusCode::from_u16(status).unwrap_or_default();
            let read = provider_fault(answer);
            assert_eq!(read.get(), Some(&Status(StatusCode::BAD_GATEWAY)));
            assert_eq!(read.get::<ProviderError>().is_some(), carried, "{status}");
        }
    }

    #[test]
    fn retry_after_is_seconds_or_a_date() {
        let now = httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT");
        let now = now.unwrap_or(SystemTime::UNIX_EPOCH);
        for (value, wait) in [
            (" 120 ", Some(120)),
            ("Sun, 06 Nov 1994 08:50:37 GMT", Some(60)),
            ("Sun, 06 Nov 1994 08:48:37 GMT", None),
            ("soon", None),
        ] {
            let got = retry_after(value, now);
            assert_eq!(got, wait.map(Duration::from_secs), "{value}");
        }
    }

    #[test]
    fn a_kind_has_a_default_status_and_a_status_fact_overrides_it() {
        let of = |f: Fault| status(&f).as_u16();
        for (fault, want) in [
            (Fault::bad_request("x"), 400),
            (Fault::not_found("x"), 404),
            (Fault::upstream("x"), 502),
            (Fault::truncated(), 502),
            (Fault::abandoned(), 499),
            (Fault::config("x"), 500),
            (Fault::internal("x"), 500),
            (Fault::new("x"), 500),
        ] {
            let code = fault.code().to_owned();
            assert_eq!(of(fault.clone()), want, "{code}");
            assert_eq!(
                of(fault.with(Status(StatusCode::TOO_MANY_REQUESTS))),
                429,
                "{code}"
            );
        }
    }

    #[test]
    fn a_code_is_the_extension_else_a_name_for_the_kind() {
        for (fault, want) in [
            (Fault::bad_request("x"), "bad_request"),
            (Fault::not_found("x"), "not_found"),
            (Fault::upstream("x"), "upstream"),
            (Fault::truncated(), "truncated"),
            (Fault::abandoned(), "abandoned"),
            (Fault::config("x"), "config"),
            (Fault::internal("x"), "internal"),
            (Fault::new("x"), "error"),
            (
                Fault::upstream("x").with(ErrorCode("over_limit".into())),
                "over_limit",
            ),
        ] {
            assert_eq!(fault.code(), want);
        }
    }

    #[test]
    fn retryable_respects_opening_stage_kind_and_transient_status() {
        let hinted = |fault: Fault, number| {
            fault.with(Status(StatusCode::from_u16(number).unwrap_or_default()))
        };
        let custom = || Fault::new("x").with(ErrorCode("custom".into()));
        for (fault, before, after) in [
            (Fault::upstream("x"), true, true),
            (Fault::truncated(), true, true),
            (hinted(Fault::upstream("x"), 400), true, false),
            // A status says whether to try again, whoever's fault it is.
            (hinted(Fault::config("x"), 503), false, true),
            (hinted(Fault::internal("x"), 400), false, false),
            (hinted(Fault::abandoned(), 502), false, false),
            (hinted(Fault::bad_request("x"), 503), false, true),
            (hinted(custom(), 400), false, false),
            // A plugin's own fault, with no status, is not retried.
            (custom(), false, false),
            (Fault::new("x"), false, false),
        ] {
            assert_eq!(retryable(&fault, false), before, "before: {fault:?}");
            assert_eq!(retryable(&fault, true), after, "after: {fault:?}");
        }
        for number in [408, 425, 429, 500, 502, 503, 504] {
            assert!(retryable(&hinted(custom(), number), true), "{number}");
        }
        for number in [
            200, 400, 401, 403, 404, 409, 422, 499, 501, 505, 528, 529, 530,
        ] {
            assert!(!retryable(&hinted(custom(), number), true), "{number}");
        }
    }
}
