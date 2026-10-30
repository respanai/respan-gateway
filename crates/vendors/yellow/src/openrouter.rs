//! OpenRouter: Chat Completions and Responses, for many vendors' models,
//! and images.

use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Fault, Filter, NativeValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;
use respan_gateway_media::image;
use respan_gateway_media_http::openai::OpenAiImages;
use serde_json::{Value, json};

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first. A chat goes in its
/// client's format, Chat Completions for a client of another.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiImages, OpenRouterImages),
            Some(BASE_URL),
        ),
    ],
    ..Vendor::DEFAULT
};

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
    let mut fields = kept(OpenAiImages::NAME, &input.native)?;
    let references = input
        .images
        .drain(..)
        .map(|m| json!({ "type": "image_url", "image_url": { "url": crate::linked(&m) } }));
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

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Media, MediaSource};
    use respan_gateway_conversation_http::Schema;

    #[test]
    fn an_edits_images_are_its_input_references() {
        let mut input = image::Input::new("a hat on it");
        input.images = vec![Media::new(
            "image/png",
            MediaSource::Url("https://a.example/cat.png".into()),
        )];
        referenced(&mut input).expect("taken");
        assert!(input.images.is_empty());
        let fields = kept(OpenAiImages::NAME, &input.native).expect("fields");
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
    fn it_speaks_chat_first_and_responses_beside_it() {
        let names: Vec<_> = VENDOR.formats.iter().map(|(f, _)| f.name()).collect();
        assert_eq!(names[..2], [OpenAiChat::NAME, OpenAiResponses::NAME]);
        assert!(
            VENDOR
                .formats
                .iter()
                .all(|(_, base)| *base == Some(BASE_URL))
        );
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
    fn its_images_are_at_images() {
        let call = respan_gateway_vendor_kit::http::Request::post("/images/generations")
            .body(())
            .expect("a call");
        let mut head = call.into_parts().0;
        OpenRouterImages.request(&mut head).expect("a path");
        assert_eq!(head.uri.path(), "/images");
    }
}
