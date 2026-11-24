//! DeepRelay: Chat Completions, embeddings and images as OpenAI
//! serves them, and videos in its dialect.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::Request;
use respan_gateway_http::http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_media::video::{Input, Output};
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiImages, OpenAiVideos};
use serde_json::{Map, Value};

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.deeprelay.ai/v1";

/// DeepRelay.
pub const VENDOR: Vendor = Vendor {
    id: "deeprelay",
    key_env: &["DEEPRELAY_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (&Dialected::new(OpenAiVideos, DeepRelay), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// DeepRelay's videos, as they depart from OpenAI's: a creation's `seconds`
/// a number, not a string, which it refuses as no JSON. Its jobs also end
/// `cancelled` or `expired`, read as `failed`, and say why one failed in
/// `error` (a code) and `error_message`.
#[derive(Debug, Clone, Copy)]
pub struct DeepRelay;

impl Dialect for DeepRelay {
    type Input = Input;
    type Output = Output;

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        Ok(Some(Box::new(seconds_counted)))
    }

    fn response(&self, _: &Request<Bytes>, head: &mut ResponseHead) -> Option<Rewrite> {
        crate::json_answer(head).then(|| Box::new(openai_job) as Rewrite)
    }
}

/// A call with its `seconds` as a number; a call that is no JSON object,
/// such as a form, or whose `seconds` is no whole number, as it is.
fn seconds_counted(body: Bytes) -> Result<Bytes, Fault> {
    let Ok(mut call) = serde_json::from_slice::<Map<String, Value>>(&body) else {
        return Ok(body);
    };
    let seconds = call.get("seconds").and_then(Value::as_str);
    let Some(seconds) = seconds.and_then(|s| s.parse::<u64>().ok()) else {
        return Ok(body);
    };
    call.insert("seconds".into(), seconds.into());
    serde_json::to_vec(&call)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a call is no JSON").with_source(e))
}

/// The states its jobs end in that OpenAI has none of, each read as failed.
const ENDED: &[&str] = &["cancelled", "expired"];

/// Its job as OpenAI's; an answer that is no JSON object is left as it is.
fn openai_job(raw: Bytes) -> Result<Bytes, Fault> {
    let Ok(mut job) = serde_json::from_slice::<Map<String, Value>>(&raw) else {
        return Ok(raw);
    };
    let ended = job.get("status").and_then(Value::as_str);
    let ended = ended
        .filter(|status| ENDED.contains(status))
        .map(str::to_owned);
    if ended.is_some() {
        job.insert("status".into(), "failed".into());
    }
    let code = job.get("error").and_then(Value::as_str).map(str::to_owned);
    if let Some(code) = code.or(ended) {
        let message = job.shift_remove("error_message");
        let message = message
            .filter(Value::is_string)
            .unwrap_or(code.as_str().into());
        let error = Map::from_iter([
            ("code".to_owned(), code.into()),
            ("message".to_owned(), message),
        ]);
        job.insert("error".into(), error.into());
    }
    serde_json::to_vec(&job)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a video job is no JSON").with_source(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn json_of(bytes: Bytes) -> Value {
        serde_json::from_slice(&bytes).expect("JSON")
    }

    #[test]
    fn a_creations_seconds_go_as_a_number() {
        let call = json!({ "model": "deeprelay/wan2.7-t2v", "prompt": "waves", "seconds": "5" });
        let sent = seconds_counted(Bytes::from(call.to_string())).expect("rewritten");
        assert_eq!(json_of(sent)["seconds"], json!(5));
    }

    #[test]
    fn a_call_with_no_whole_seconds_or_no_json_is_left_as_it_came() {
        for body in [
            r#"{"seconds":"5.5"}"#,
            r#"{"prompt":"waves"}"#,
            "--boundary",
            "",
        ] {
            let sent = seconds_counted(Bytes::from(body)).expect("left");
            assert_eq!(sent, Bytes::from(body));
        }
    }

    #[test]
    fn a_job_that_ended_cancelled_or_expired_has_failed_with_its_state_as_the_error() {
        for state in ["cancelled", "expired"] {
            let job = json!({ "id": "vid_1", "object": "video", "status": state, "error": null });
            let read = json_of(openai_job(Bytes::from(job.to_string())).expect("read"));
            assert_eq!(read["status"], "failed");
            assert_eq!(read["error"], json!({ "code": state, "message": state }));
        }
    }

    #[test]
    fn a_failed_jobs_code_and_message_are_openais_error() {
        let job = json!({ "id": "vid_1", "status": "failed", "error": "content_policy",
            "error_message": "The prompt was refused." });
        let read = json_of(openai_job(Bytes::from(job.to_string())).expect("read"));
        assert_eq!(
            read["error"],
            json!({ "code": "content_policy", "message": "The prompt was refused." })
        );
        assert!(read.get("error_message").is_none());
    }

    #[test]
    fn a_job_still_running_is_left_as_it_came() {
        let job = json!({ "id": "vid_1", "status": "in_progress", "progress": 40 });
        let read = json_of(openai_job(Bytes::from(job.to_string())).expect("read"));
        assert_eq!(read, job);
    }
}
