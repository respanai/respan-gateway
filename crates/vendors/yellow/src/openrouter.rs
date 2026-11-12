//! OpenRouter: Chat Completions, Responses and Messages, for many vendors'
//! models, and images, embeddings, transcriptions and speech.

use respan_gateway_conversation::{Input, Output, Usage};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::fields::{kept, media_url, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Fault, Filter, NativeFields, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::set_path;
use respan_gateway_media::{image, speech, transcription};
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};
use serde_json::{Map, Value, json};

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Where its Messages API is: the format adds the `/v1`.
const MESSAGES_URL: &str = "https://openrouter.ai/api";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first. A chat goes in its
/// client's format, Chat Completions for a client of another.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenRouter), Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&AnthropicMessages, Some(MESSAGES_URL)),
        (
            &Dialected::new(OpenAiImages, OpenRouterImages),
            Some(BASE_URL),
        ),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiTranscriptions, OpenRouterTranscriptions),
            Some(BASE_URL),
        ),
        (
            &Dialected::new(OpenAiSpeech, OpenRouterSpeech),
            Some(BASE_URL),
        ),
    ],
    ..Vendor::DEFAULT
};

/// OpenRouter's Chat Completions: a thinking budget, such as Anthropic's
/// clients send, is its `reasoning.max_tokens`.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouter;

impl Dialect for OpenRouter {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(reasoning_limit))
    }
}

/// `input` with a thinking budget as its `reasoning.max_tokens`, unless
/// another format's fields leave no room for its own.
fn reasoning_limit(input: &mut Input) -> Result<(), Fault> {
    let budget = input
        .options
        .reasoning
        .as_ref()
        .and_then(|r| r.budget_tokens);
    let own = input
        .native
        .as_ref()
        .is_none_or(|n| n.is_empty() || &*n.format == OpenAiChat::NAME);
    let (Some(budget), true) = (budget, own) else {
        return Ok(());
    };
    let withheld = input.options.reasoning.take().and_then(|r| r.summary) == Some(false);
    let mut reasoning = json!({ "max_tokens": budget });
    if withheld {
        reasoning["exclude"] = true.into();
    }
    let native = input
        .native
        .get_or_insert_with(|| NativeFields::new(OpenAiChat::NAME, Map::new()));
    native.format = OpenAiChat::NAME.into();
    native.fields.entry("reasoning").or_insert(reasoning);
    Ok(())
}

/// OpenRouter's images, as they depart from OpenAI's: generations and edits
/// both at `/images`, an edit's images as JSON `input_references`, each a
/// URL or a data URL of its bytes, and its usage counted in Chat
/// Completions' `prompt_tokens` and `completion_tokens`. It takes no mask,
/// which fails `bad_request` before sending.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterImages;

impl Dialect for OpenRouterImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(referenced))
    }

    fn outputs(&self) -> Option<Box<dyn Filter<image::Output>>> {
        Some(Box::new(Counted))
    }

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/images")?;
        Ok(None)
    }
}

/// `input` with its images in `input_references`, and no mask.
fn referenced(input: &mut image::Input) -> Result<(), Fault> {
    if input.mask.is_some() {
        return Err(Fault::bad_request("OpenRouter edits an image with no mask"));
    }
    if input.images.is_empty() {
        return Ok(());
    }
    let mut fields = kept(OpenAiImages::NAME, &input.native);
    let references = input
        .images
        .drain(..)
        .map(|m| json!({ "type": "image_url", "image_url": { "url": media_url(&m) } }));
    fields.insert(
        "input_references".into(),
        references.collect::<Vec<Value>>().into(),
    );
    input.native = native_of(OpenAiImages::NAME, fields);
    Ok(())
}

/// Its usage's token counts as a [`Usage`], its other fields left native.
struct Counted;

impl Filter<image::Output> for Counted {
    fn edit(&mut self, value: image::Output, out: &mut Vec<image::Output>) -> Result<(), Fault> {
        let image::Output::Native(native) = &value else {
            out.push(value);
            return Ok(());
        };
        let mut v = (*native.value).clone();
        let Some(usage) = v["usage"].as_object_mut() else {
            out.push(value);
            return Ok(());
        };
        let mut count = |key| usage.shift_remove(key).and_then(|n| n.as_u64());
        let (input, output) = (count("prompt_tokens"), count("completion_tokens"));
        if input.is_none() && output.is_none() {
            out.push(value);
            return Ok(());
        }
        usage.shift_remove("total_tokens");
        out.push(image::Output::Usage(Usage::new(
            input.unwrap_or(0),
            output.unwrap_or(0),
        )));
        out.push(image::Output::Native(NativeValue::new(
            native.format.clone(),
            v,
        )));
        Ok(())
    }
}

/// OpenRouter's transcriptions, as they depart from OpenAI's: a model
/// billed by the second answers its usage's `seconds` with no `type`,
/// which is read as OpenAI's `duration`.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterTranscriptions;

impl Dialect for OpenRouterTranscriptions {
    type Input = transcription::Input;
    type Output = transcription::Output;

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        crate::json_answer(head).then(|| Box::new(timed) as Rewrite)
    }
}

/// An answer with its usage of `seconds` typed a `duration`; any other as
/// it came.
fn timed(raw: Bytes) -> Result<Bytes, Fault> {
    let Ok(mut answer) = serde_json::from_slice::<Value>(&raw) else {
        return Ok(raw);
    };
    match answer["usage"].as_object_mut() {
        Some(usage) if usage.contains_key("seconds") && !usage.contains_key("type") => {
            usage.insert("type".into(), "duration".into());
        }
        _ => return Ok(raw),
    }
    let unwritten = |e| Fault::internal("an answer is no JSON").with_source(e);
    serde_json::to_vec(&answer)
        .map(Bytes::from)
        .map_err(unwritten)
}

/// OpenRouter's speech, as it departs from OpenAI's: asked for no
/// `response_format` it answers `pcm`, so it is asked for `mp3`, which
/// OpenAI answers then. A model that speaks `pcm` alone, as Gemini's and
/// HeyGen's do, refuses that itself, and is to be asked for `pcm`. Its
/// `pcm` is 16-bit as OpenAI's, at 24 kHz or, from some models, such as
/// Fish Audio's and HeyGen's, 44.1 kHz.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterSpeech;

impl Dialect for OpenRouterSpeech {
    type Input = speech::Input;
    type Output = speech::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<speech::Input>>> {
        Some(Box::new(in_mp3))
    }
}

/// `input` asking for `mp3`, unless it asks for a format.
fn in_mp3(input: &mut speech::Input) -> Result<(), Fault> {
    input.audio_format.get_or_insert_with(|| "mp3".into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Media, MediaSource};

    #[test]
    fn an_edits_images_are_its_input_references() {
        let mut input = image::Input::new("a hat on it");
        input.images = vec![Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        )];
        referenced(&mut input).expect("taken");
        assert!(input.images.is_empty());
        let fields = kept(OpenAiImages::NAME, &input.native);
        assert_eq!(
            fields["input_references"],
            json!([{ "type": "image_url", "image_url": { "url": "https://a.example/cat.png" } }])
        );
        input.mask = Some(Media::new(
            "image/png",
            MediaSource::Url("https://a.example/mask.png".into()),
        ));
        assert!(referenced(&mut input).is_err());
    }

    #[test]
    fn its_usage_counts_are_a_usage() {
        let usage =
            json!({ "usage": { "prompt_tokens": 6, "completion_tokens": 1290, "cost": 0.04 } });
        let mut out = Vec::new();
        let native = NativeValue::new(OpenAiImages::NAME, usage);
        Counted
            .edit(image::Output::Native(native), &mut out)
            .expect("counted");
        assert!(matches!(&out[0], image::Output::Usage(u) if *u == Usage::new(6, 1290)));
        assert!(
            matches!(&out[1], image::Output::Native(n) if *n.value == json!({ "usage": { "cost": 0.04 } }))
        );
    }

    #[test]
    fn it_speaks_chat_first_and_responses_and_messages_beside_it() {
        let names: Vec<_> = VENDOR.formats.iter().map(|(f, _)| f.name()).collect();
        assert_eq!(
            names[..3],
            [
                OpenAiChat::NAME,
                OpenAiResponses::NAME,
                AnthropicMessages::NAME
            ]
        );
        // Messages' base leaves out the `/v1` its format adds.
        assert!(VENDOR.formats.iter().all(|(f, base)| *base
            == Some(match f.name() == AnthropicMessages::NAME {
                true => MESSAGES_URL,
                false => BASE_URL,
            })));
    }

    #[test]
    fn its_responses_usage_reads_with_its_own_fields_beside() {
        use respan_gateway_conversation::Output;
        let body = json!({
            "id": "gen-1", "object": "response", "created_at": 1, "status": "completed",
            "model": "openai/gpt-5.5-codex",
            "output": [{ "id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                "content": [{ "type": "output_text", "text": "hi", "annotations": [] }] }],
            "usage": { "input_tokens": 5, "output_tokens": 2, "total_tokens": 7,
                "input_tokens_details": { "cached_tokens": 1 },
                "output_tokens_details": { "reasoning_tokens": 0 },
                "cost": 0.0001, "is_byok": false },
        });
        let mut out = Vec::new();
        OpenAiResponses.decode_body(&body, &mut out).expect("read");
        let usage = out.iter().find_map(|o| match o {
            Output::Usage(u) => Some(*u),
            _ => None,
        });
        assert_eq!(
            usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((5, 2))
        );
    }

    #[test]
    fn a_thinking_budget_is_its_reasoning_limit() {
        use respan_gateway_conversation_http::anthropic::AnthropicMessages;
        let mut messages = AnthropicMessages.decode_json(&json!({
            "max_tokens": 9000,
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "enabled", "budget_tokens": 2048 },
        }));
        reasoning_limit(&mut messages).expect("taken");
        assert!(messages.options.reasoning.is_none());
        let sent = OpenAiChat.encode_request(&messages).expect("written");
        assert_eq!(sent["reasoning"], json!({ "max_tokens": 2048 }));
        assert!(sent.get("reasoning_effort").is_none());
        // Without a budget the input is as it came.
        let plain = AnthropicMessages.decode_json(&json!({
            "max_tokens": 9,
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        let mut taken = plain.clone();
        reasoning_limit(&mut taken).expect("taken");
        assert_eq!(taken, plain);
    }

    #[test]
    fn its_images_are_at_images() {
        let call = respan_gateway_vendor_kit::http::Request::post("/images/generations")
            .body(())
            .expect("a call");
        let mut head = call.into_parts().0;
        OpenRouterImages.request(&mut head).expect("a path");
        assert_eq!(head.uri.path(), "/images");
    }

    #[test]
    fn its_seconds_of_audio_are_a_duration() {
        let timed = |v: Value| -> Value {
            let raw = Bytes::from(v.to_string());
            serde_json::from_slice(&timed(raw).expect("read")).expect("JSON")
        };
        assert_eq!(
            timed(json!({ "text": "hi", "usage": { "seconds": 2, "cost": 0.0002 } }))["usage"],
            json!({ "seconds": 2, "cost": 0.0002, "type": "duration" })
        );
        let tokens = json!({ "text": "hi", "usage": { "input_tokens": 19, "output_tokens": 10 } });
        assert_eq!(timed(tokens.clone()), tokens);
    }

    #[test]
    fn speech_of_no_format_is_asked_in_mp3() {
        let mut input = speech::Input::new("hi");
        in_mp3(&mut input).expect("taken");
        assert_eq!(input.audio_format.as_deref(), Some("mp3"));
        input.audio_format = Some("pcm".into());
        in_mp3(&mut input).expect("taken");
        assert_eq!(input.audio_format.as_deref(), Some("pcm"));
    }
}
