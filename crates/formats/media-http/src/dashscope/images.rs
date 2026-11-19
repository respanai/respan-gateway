//! DashScope's image generation through `multimodal-generation`: a provider
//! format of the [`image`](respan_gateway_media::image) kind,
//! [`DashScopeImages`].

use super::{post_json, said_error, says_error};
use crate::Natives;
use crate::edge::{bind_provider, boolean, dimensions, sends, whole_number};
use crate::openai::OpenAiImages;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{invalid, media_url, set};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::image::{Image, Input, Output, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "dashscope-images";

type Key = crate::Own<DashScopeImages>;

const GENERATION: &str = "/services/aigc/multimodal-generation/generation";

/// Alibaba Cloud Model Studio's image models that answer at once, Qwen-Image
/// (`qwen-image-2.0`) and Wan from 2.6 (`wan2.6-t2i`), on DashScope's own
/// API: a `POST` of JSON to `/services/aigc/multimodal-generation/generation`,
/// as a provider only. Older Wan models and Qwen-MT-Image draw only as a
/// task, which DashScope refuses there.
///
/// The prompt is a user message's text, after the images to edit, each a URL
/// or a data URL of its bytes. The count is `n`, one unless asked, as Wan
/// makes four otherwise; a `size` is written `1024*768`, and `auto` sends
/// none. A request's `negative_prompt`, `prompt_extend`, `watermark` and
/// `seed`, as OpenAI's clients send them, are parameters too, a form's text
/// read as a number or a boolean; other fields of another format, such as a
/// `quality`, are dropped.
///
/// An answer's images are those of its choices' content, at URLs DashScope
/// keeps for 24 hours, and its `usage`, the images it made and their size,
/// a native `{"usage": {..}}`: it reports no tokens.
///
/// A mask and a `response_format` but `url` are refused, 400, before the
/// call; a `size` DashScope does not draw is its own 400.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DashScopeImages;

impl DashScopeImages {
    /// The format's name, `dashscope-images`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for DashScopeImages {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for DashScopeImages {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| {
            Some(Box::new(Whole::new(read_answer)))
        }))
    }
}

/// The call of `input`: a user message of its images and prompt.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let body = serde_json::to_vec(&body(input)?)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    post_json(head, GENERATION)?;
    Ok(sends(Bytes::from(body)))
}

/// The body of a call for `input`.
fn body(input: &Input) -> Result<Map<String, Value>, Fault> {
    let native = OpenAiImages::fields(&input.native);
    if native.contains_key("mask") {
        return Err(invalid("mask", "is not taken by DashScope"));
    }
    if native.get("response_format").is_some_and(|f| f != "url") {
        return Err(invalid(
            "response_format",
            "must be url: DashScope answers images by URL",
        ));
    }
    let mut content: Vec<Value> = (input.images.iter())
        .map(|image| json!({ "image": media_url(image) }))
        .collect();
    content.push(json!({ "text": input.prompt }));
    let mut parameters = Map::new();
    parameters.insert("n".into(), input.count.unwrap_or(1).into());
    if let Some(size) = input.size.as_deref().filter(|s| *s != "auto") {
        let (w, h) = dimensions(size).ok_or_else(|| invalid("size", "must be WIDTHxHEIGHT"))?;
        parameters.insert("size".into(), format!("{w}*{h}").into());
    }
    passed(native, &mut parameters)?;
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    let message = json!({ "role": "user", "content": content });
    v.insert("input".into(), json!({ "messages": [message] }));
    v.insert("parameters".into(), parameters.into());
    Ok(v)
}

/// DashScope's parameters of `native` into `parameters`, a form's text read
/// as the number or boolean it says.
fn passed(native: &Map<String, Value>, parameters: &mut Map<String, Value>) -> Result<(), Fault> {
    if let Some(text) = native.get("negative_prompt") {
        parameters.insert("negative_prompt".into(), text.clone());
    }
    for key in ["prompt_extend", "watermark"] {
        if let Some(flag) = native.get(key) {
            let flag = boolean(flag).ok_or_else(|| invalid(key, "must be true or false"))?;
            parameters.insert(key.into(), flag.into());
        }
    }
    if let Some(seed) = native.get("seed") {
        let seed = whole_number(seed).ok_or_else(|| invalid("seed", "must be a whole number"))?;
        parameters.insert("seed".into(), seed.into());
    }
    Ok(())
}

/// The events of an answer `raw`: its images and its usage.
fn read_answer(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    if says_error(&v) {
        return Err(said_error(raw, &v));
    }
    let choices = v["output"]["choices"].as_array().into_iter().flatten();
    let parts = choices.flat_map(|c| c["message"]["content"].as_array().into_iter().flatten());
    let images: Vec<_> = parts.filter_map(|part| part["image"].as_str()).collect();
    if images.is_empty() {
        return Err(Fault::upstream("DashScope's answer holds no image"));
    }
    out.push(Output::Start(Start::default()));
    for url in images {
        let media = Media::new("image/png", MediaSource::Url(url.to_owned()));
        out.push(Output::Image(Image::new(media)));
    }
    if v["usage"].is_object() {
        let usage = json!({ "usage": v["usage"] });
        out.push(Output::Native(Native::json::<Key>(usage)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    /// An input with `native` as an OpenAI client sent it.
    fn input(prompt: &str, native: Value) -> Input {
        let mut input = Input::new(prompt);
        input.model = Some("qwen-image-2.0".into());
        let fields = native.as_object().cloned().unwrap_or_default();
        input.native = OpenAiImages::with_fields(&Native::default(), |f| *f = fields);
        input
    }

    fn sent(input: &Input) -> Value {
        Value::Object(body(input).expect("a body"))
    }

    #[test]
    fn a_generation_is_a_message_of_its_prompt_and_its_parameters() {
        let native = json!({ "negative_prompt": "blur", "seed": "7", "watermark": "false",
            "prompt_extend": true, "quality": "high", "output_format": "png" });
        let mut i = input("an apple", native);
        i.size = Some("1024x768".into());
        let want = json!({ "model": "qwen-image-2.0",
            "input": { "messages": [{ "role": "user", "content": [{ "text": "an apple" }] }] },
            "parameters": { "n": 1, "size": "1024*768", "negative_prompt": "blur",
                "prompt_extend": true, "watermark": false, "seed": 7 } });
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn an_edits_images_come_before_its_prompt() {
        let mut i = input("make it green", json!({}));
        i.count = Some(2);
        i.size = Some("auto".into());
        i.images = vec![
            Media::new(
                "image/png",
                MediaSource::Url("https://a.example/a.png".into()),
            ),
            Media::new("image/png", MediaSource::Bytes(Bytes::from_static(b"img"))),
        ];
        let content = json!([{ "image": "https://a.example/a.png" },
            { "image": "data:image/png;base64,aW1n" }, { "text": "make it green" }]);
        let sent = sent(&i);
        assert_eq!(sent["input"]["messages"][0]["content"], content);
        assert_eq!(sent["parameters"], json!({ "n": 2 }));
    }

    #[test]
    fn what_it_cannot_do_is_refused_before_the_call() {
        let refuses = |i: &Input| body(i).is_err_and(|f| f.is(FaultKind::Client));
        let mask = json!({ "mask": { "image_url": "data:image/png;base64,bXNr" } });
        assert!(refuses(&input("p", mask)));
        assert!(refuses(&input(
            "p",
            json!({ "response_format": "b64_json" })
        )));
        assert!(!refuses(&input("p", json!({ "response_format": "url" }))));
        assert!(refuses(&input("p", json!({ "seed": "seven" }))));
        let mut sized = input("p", json!({}));
        sized.size = Some("large".into());
        assert!(refuses(&sized));
    }

    #[test]
    fn an_answers_images_are_every_choices_and_its_usage_native() {
        let raw = json!({ "output": { "choices": [
            { "message": { "role": "assistant", "content": [{ "image": "https://a.example/1.png" },
                { "image": "https://a.example/2.png" }] }, "finish_reason": "stop" },
            { "message": { "role": "assistant", "content": [{ "image": "https://a.example/3.png", "type": "image" }] } }] },
            "usage": { "image_count": 3, "width": 1024, "height": 768 } });
        let mut out = Vec::new();
        read_answer(Bytes::from(raw.to_string()), &mut out).expect("read");
        assert!(matches!(out[0], Output::Start(_)));
        let urls: Vec<_> = (out.iter())
            .filter_map(|o| match o {
                Output::Image(i) => match &i.media.source {
                    MediaSource::Url(url) => Some(url.as_str()),
                    MediaSource::Bytes(_) => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            urls,
            [
                "https://a.example/1.png",
                "https://a.example/2.png",
                "https://a.example/3.png"
            ]
        );
        let Some(Output::Native(native)) = out.last() else {
            panic!("a native usage");
        };
        assert_eq!(DashScopeImages::fields(native)["usage"]["image_count"], 3);
    }
}
