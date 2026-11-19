//! Alibaba Cloud Model Studio's audio and video models on DashScope's own
//! API, over HTTP: speech synthesis ([`DashScopeSpeech`]), recognition
//! ([`DashScopeTranscriptions`]) and video generation
//! ([`DashScopeVideos`]), provider formats only. A provider's base
//! URL is its region's API, with the version,
//! `https://dashscope-intl.aliyuncs.com/api/v1`, its key a bearer token.

mod speech;
mod transcriptions;
mod videos;

pub use speech::DashScopeSpeech;
pub use transcriptions::DashScopeTranscriptions;
pub use videos::DashScopeVideos;

use respan_gateway_conversation_http::read_provider_error;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{self, StatusCode, request::Parts as RequestHead};
use serde_json::Value;

/// Set a call's path, below the base URL's, and its JSON body's type.
fn post_json(head: &mut RequestHead, path: &str) -> Result<(), Fault> {
    respan_gateway_http::path::set_path(head, path)?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Ok(())
}

/// Whether DashScope's JSON says an error: a `code` and no `output`.
fn says_error(v: &Value) -> bool {
    v["code"].as_str().is_some_and(|c| !c.is_empty()) && v["output"].is_null()
}

/// The fault of a refused answer whose body may be an event stream, read
/// from its event's data.
fn refusal(answer: http::Response<Bytes>) -> Fault {
    let (head, body) = answer.into_parts();
    let body = respan_gateway_sse::event_data(&body).map_or(body, Bytes::from);
    read_provider_error(http::Response::from_parts(head, body))
}

/// The fault of an error DashScope said in a success, `raw`: a 400 for a
/// parameter it refused, else a 502.
fn said_error(raw: Bytes, v: &Value) -> Fault {
    let refused = v["code"]
        .as_str()
        .is_some_and(|c| c.starts_with("InvalidParameter"));
    let mut answer = http::Response::new(raw);
    *answer.status_mut() = match refused {
        true => StatusCode::BAD_REQUEST,
        false => StatusCode::BAD_GATEWAY,
    };
    read_provider_error(answer)
}
