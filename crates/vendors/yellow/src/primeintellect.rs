//! Prime Intellect: Chat Completions as OpenAI serves them, and decisions in
//! its dialect of System One's contract, its evaluations.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Fault;
use respan_gateway_decision::{Input, Output};
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::Request;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::set_path;
use serde_json::{Map, Value};

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.pinference.ai/api/v1";

/// Prime Intellect.
pub const VENDOR: Vendor = Vendor {
    id: "primeintellect",
    key_env: &["PRIME_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&Dialected::new(Decisions, PrimeIntellect), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Prime Intellect's evaluations: System One at `/evaluations`, a yes or no
/// a `boolean` whose answer is its `probability` of yes, and usage counted
/// in `inputTokens` and `outputTokens`.
#[derive(Debug, Clone, Copy)]
pub struct PrimeIntellect;

impl Dialect for PrimeIntellect {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/evaluations")?;
        Ok(Some(Box::new(asked)))
    }

    fn response(&self, _: &Request<Bytes>, head: &mut ResponseHead) -> Option<Rewrite> {
        crate::json_answer(head).then(|| Box::new(answered) as Rewrite)
    }
}

/// A call with each `noul` asked as a `boolean`.
fn asked(body: Bytes) -> Result<Bytes, Fault> {
    let unwritten = |e| Fault::internal("a call is no JSON object").with_source(e);
    let mut call: Map<String, Value> = serde_json::from_slice(&body).map_err(unwritten)?;
    let questions = call.get_mut("questions").and_then(Value::as_object_mut);
    for question in questions.into_iter().flat_map(|q| q.values_mut()) {
        if question["type"] == "noul" {
            question["type"] = "boolean".into();
        }
    }
    serde_json::to_vec(&call)
        .map(Bytes::from)
        .map_err(unwritten)
}

/// Its answer in System One's names; one that is no JSON object is left as
/// it is.
fn answered(raw: Bytes) -> Result<Bytes, Fault> {
    let Ok(mut answer) = serde_json::from_slice::<Map<String, Value>>(&raw) else {
        return Ok(raw);
    };
    let answers = answer.get_mut("answers").and_then(Value::as_object_mut);
    for each in answers.into_iter().flat_map(|a| a.values_mut()) {
        let Some(each) = each
            .as_object_mut()
            .filter(|a| a.get("type").and_then(Value::as_str) == Some("boolean"))
        else {
            continue;
        };
        each.insert("type".into(), "noul".into());
        if let Some(yes) = each.shift_remove("probability") {
            each.insert("noul".into(), yes);
        }
    }
    if let Some(usage) = answer.get_mut("usage").and_then(Value::as_object_mut) {
        for (its, ours) in [
            ("inputTokens", "input_tokens"),
            ("outputTokens", "output_tokens"),
        ] {
            if let Some(count) = usage.shift_remove(its) {
                usage.insert(ours.into(), count);
            }
        }
    }
    serde_json::to_vec(&answer)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("an evaluation is no JSON").with_source(e))
}
