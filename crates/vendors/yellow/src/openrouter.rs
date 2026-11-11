//! OpenRouter: Chat Completions, Responses and Messages, for many vendors'
//! models, images and videos.

use respan_gateway_conversation::{Input, Output, Usage};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::fields::{kept, media_url, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{ErrorCode, Fault, Filter, NativeFields, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content::content_type;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::set_path;
use respan_gateway_media::{image, video};
use respan_gateway_media_http::openai::{OpenAiImages, OpenAiVideos};
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
        (
            &Dialected::new(OpenAiVideos, OpenRouterVideos),
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

/// What OpenAI's video ids start with.
const VIDEO_ID: &str = "video_";

/// OpenRouter's videos, as they depart from OpenAI's: a creation's
/// `seconds` is its `duration`, a number, and its reference image the
/// first of its `frame_images`, a URL or a data URL of its bytes, in JSON.
/// Its job says `pending` for OpenAI's `queued` and why it failed as a
/// string. Its ids start `gen-vid-`: they are given OpenAI's `video_`
/// before them, as a follow-up finds its job's provider by its id's
/// spelling, and are called without it. It serves a job and its video
/// alone: a remix, a deletion, a list or another variant fails
/// `unsupported` before sending.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterVideos;

impl Dialect for OpenRouterVideos {
    type Input = video::Input;
    type Output = video::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<video::Input>>> {
        Some(Box::new(taken_video))
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        let essence = content_type(head)?.split(';').next()?.trim();
        if !head.status.is_success() || !essence.eq_ignore_ascii_case("application/json") {
            return None;
        }
        Some(Box::new(openai_job))
    }
}

/// `input` as OpenRouter takes it: a creation's `duration` and
/// `frame_images`, and a job's id as OpenRouter gave it.
fn taken_video(input: &mut video::Input) -> Result<(), Fault> {
    match input {
        video::Input::Create(create) => {
            let mut fields = kept(OpenAiVideos::NAME, &create.native);
            if let Some(seconds) = create.seconds.take() {
                fields.entry("duration").or_insert_with(|| seconds.into());
            }
            if let Some(reference) = create.reference.take() {
                let frame = json!({ "type": "image_url", "frame_type": "first_frame",
                    "image_url": { "url": media_url(&reference) } });
                fields
                    .entry("frame_images")
                    .or_insert_with(|| vec![frame].into());
            }
            create.native = native_of(OpenAiVideos::NAME, fields);
        }
        video::Input::Retrieve { id }
        | video::Input::Content {
            id,
            variant: video::Variant::Video,
            ..
        } => {
            if let Some(its) = id.strip_prefix(VIDEO_ID) {
                *id = its.to_owned();
            }
        }
        _ => {
            return Err(
                Fault::bad_request("OpenRouter serves a video job and its video alone")
                    .with(ErrorCode("unsupported".into())),
            );
        }
    }
    Ok(())
}

/// Its job as OpenAI's; an answer that is no JSON object is left as it is.
fn openai_job(raw: Bytes) -> Result<Bytes, Fault> {
    let Ok(mut job) = serde_json::from_slice::<Map<String, Value>>(&raw) else {
        return Ok(raw);
    };
    if let Some(id) = job.get("id").and_then(Value::as_str) {
        job.insert("id".into(), format!("{VIDEO_ID}{id}").into());
    }
    if job.get("status").and_then(Value::as_str) == Some("pending") {
        job.insert("status".into(), "queued".into());
    }
    if let Some(message) = job.get("error").and_then(Value::as_str) {
        let error = Map::from_iter([("message".to_owned(), message.into())]);
        job.insert("error".into(), error.into());
    }
    serde_json::to_vec(&job)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a video job is no JSON").with_source(e))
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
    fn a_video_job_goes_and_comes_as_openais() {
        let mut create = video::Create::new("a wave");
        create.seconds = Some(4);
        let mut input = video::Input::Create(create);
        taken_video(&mut input).expect("taken");
        let video::Input::Create(create) = &input else {
            panic!("a creation");
        };
        assert_eq!(create.seconds, None);
        assert_eq!(kept(OpenAiVideos::NAME, &create.native)["duration"], 4);

        let job = json!({ "id": "gen-vid-1-a", "status": "pending", "error": "no credits" });
        let job = openai_job(Bytes::from(job.to_string())).expect("read");
        let job: Value = serde_json::from_slice(&job).expect("JSON");
        assert_eq!(
            job,
            json!({ "id": "video_gen-vid-1-a", "status": "queued",
                "error": { "message": "no credits" } })
        );

        let mut poll = video::Input::Retrieve {
            id: "video_gen-vid-1-a".into(),
        };
        taken_video(&mut poll).expect("taken");
        assert!(matches!(&poll, video::Input::Retrieve { id } if id == "gen-vid-1-a"));
        let mut delete = video::Input::Delete {
            id: "video_gen-vid-1-a".into(),
        };
        assert!(taken_video(&mut delete).is_err());
    }
}
