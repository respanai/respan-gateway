//! What vendors of Gemini's transcriptions share: the client's request in
//! `openai-transcriptions`' fields, read for a model that gives the text
//! alone.

use respan_gateway_conversation_http::fields::invalid;
use respan_gateway_core::{ErrorCode, Fault};
use respan_gateway_media::transcription::Input;
use respan_gateway_media_http::gemini::GeminiTranscriptions;
use respan_gateway_media_http::openai::OpenAiTranscriptions;

/// `input` for `gemini-transcriptions`: of the fields of
/// `openai-transcriptions` only the `response_format` Gemini can give
/// (`json` and `text`, one transcript) is taken, and dropped; another is
/// refused, 400 `unsupported`, as Gemini gives no timings, speakers or log
/// probabilities. Fields of another format are left for the format to refuse.
pub fn text_only(input: &mut Input) -> Result<(), Fault> {
    let Some(fields) = input
        .native
        .as_ref()
        .filter(|n| &*n.format == OpenAiTranscriptions::NAME)
    else {
        return Ok(());
    };
    for (key, value) in &fields.fields {
        let taken = match key.as_str() {
            "response_format" => matches!(value.as_str(), Some("json" | "text")),
            "timestamp_granularities" | "timestamp_granularities[]" => {
                value.as_array().is_some_and(Vec::is_empty)
            }
            _ => false,
        };
        if !taken {
            let what = format!(
                "is not one {} transcribes with: it gives the text alone",
                GeminiTranscriptions::NAME
            );
            return Err(invalid(key, &what).with(ErrorCode("unsupported".into())));
        }
    }
    input.native = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Media, MediaSource};
    use respan_gateway_conversation_http::fields::native_of;
    use serde_json::json;

    fn with(fields: serde_json::Value) -> Input {
        let mut input = Input::new(Media::new("audio/wav", MediaSource::Bytes(vec![1].into())));
        let fields = fields.as_object().cloned().unwrap_or_default();
        input.native = native_of(OpenAiTranscriptions::NAME, fields);
        input
    }

    #[test]
    fn json_and_text_are_taken_but_timings_are_refused() {
        for format in ["json", "text"] {
            let mut input = with(json!({"response_format": format}));
            text_only(&mut input).expect("taken");
            assert!(input.native.is_none(), "{format}");
        }
        for fields in [
            json!({"response_format": "verbose_json"}),
            json!({"response_format": "srt"}),
            json!({"response_format": "vtt"}),
            json!({"response_format": "diarized_json"}),
            json!({"timestamp_granularities": ["word"]}),
            json!({"include": ["logprobs"]}),
        ] {
            assert!(text_only(&mut with(fields.clone())).is_err(), "{fields}");
        }
    }
}
