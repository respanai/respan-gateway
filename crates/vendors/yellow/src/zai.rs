//! Z.ai: Chat Completions, images and transcriptions as OpenAI serves them,
//! its transcriptions' usage in its dialect, and its own videos.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_media::Usage;
use respan_gateway_media::transcription::{Input, Output};
use respan_gateway_media_http::Natives;
use respan_gateway_media_http::openai::{OpenAiImages, OpenAiTranscriptions};
use respan_gateway_media_http::zai::ZaiVideos;
use serde_json::Value;

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor};

/// Where its API is, on its international site.
const BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// Z.ai.
pub const VENDOR: Vendor = Vendor {
    id: "zai",
    key_env: &["ZAI_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiTranscriptions, ZaiTranscriptions),
            Some(BASE_URL),
        ),
        (&ZaiVideos, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Z.ai's transcriptions, as they depart from OpenAI's: usage counted in
/// Chat Completions' `prompt_tokens` and `completion_tokens`, with no
/// `type`, in a whole answer and in a stream's `transcript.text.done`.
#[derive(Debug, Clone, Copy)]
pub struct ZaiTranscriptions;

impl Dialect for ZaiTranscriptions {
    type Input = Input;
    type Output = Output;

    fn outputs(&self) -> Option<Box<dyn Filter<Output>>> {
        Some(Box::new(Counted))
    }
}

/// Usage of `prompt_tokens` and `completion_tokens`, which the format keeps
/// as native, read as tokens.
#[derive(Debug)]
struct Counted;

impl Filter<Output> for Counted {
    fn edit(&mut self, value: Output, out: &mut Vec<Output>) -> Result<(), Fault> {
        let Output::Native(native) = &value else {
            out.push(value);
            return Ok(());
        };
        let usage = OpenAiTranscriptions::fields(native).get("usage");
        let Some(mut usage) = usage.and_then(Value::as_object).cloned() else {
            out.push(value);
            return Ok(());
        };
        let mut count = |key| usage.shift_remove(key).and_then(|n| n.as_u64());
        let (input, output) = (count("prompt_tokens"), count("completion_tokens"));
        if input.is_none() && output.is_none() {
            out.push(value);
            return Ok(());
        }
        let used = Usage::new(input.unwrap_or(0), output.unwrap_or(0));
        out.push(Output::Usage(used));
        let native = OpenAiTranscriptions::with_fields(native, |f| {
            _ = f.insert("usage".into(), usage.into())
        });
        out.push(Output::Native(native));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::Native;
    use serde_json::{from_value, json};

    fn usage(usage: Value) -> Output {
        let native = OpenAiTranscriptions::with_fields(&Native::default(), |f| {
            *f = from_value(json!({ "usage": usage })).expect("an object")
        });
        Output::Native(native)
    }

    #[test]
    fn its_chat_counts_are_a_usage_and_the_rest_stays_native() {
        let mut out = Vec::new();
        let counts = json!({ "prompt_tokens": 199, "completion_tokens": 27,
            "prompt_tokens_details": { "cached_tokens": 0 } });
        Counted.edit(usage(counts), &mut out).expect("edited");
        assert!(
            matches!(&out[0], Output::Usage(u) if u.input_tokens == 199 && u.output_tokens == 27)
        );
        assert!(matches!(&out[1], Output::Native(n)
            if Value::Object(OpenAiTranscriptions::fields(n).clone())
                == json!({ "usage": { "prompt_tokens_details": { "cached_tokens": 0 } } })));
    }

    #[test]
    fn a_usage_with_no_chat_counts_is_left_as_it_came() {
        let mut out = Vec::new();
        Counted
            .edit(usage(json!({ "cost": 1 })), &mut out)
            .expect("edited");
        assert!(matches!(&out[..], [Output::Native(_)]));
    }
}
