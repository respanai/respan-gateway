//! DashScope's image generation through `multimodal-generation`: a provider
//! format of the [`image`](respan_gateway_media::image) kind,
//! [`DashScopeImages`].

use super::{post_json, refusal, said_error, says_error};
use crate::Natives;
use crate::edge::{bind_provider_with, boolean, dimensions, ended_events, sends, whole_number};
use crate::openai::OpenAiImages;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{invalid, media_url, set};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::image::{Image, Input, Output, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "dashscope-images";

type Key = crate::Own<DashScopeImages>;

const GENERATION: &str = "/services/aigc/multimodal-generation/generation";

/// The header that asks DashScope to answer as an event stream.
const SSE: &str = "x-dashscope-sse";

/// Whether `native` asks for DashScope's interleaved mode, `enable_interleave`.
fn interleaves(native: &Map<String, Value>) -> Result<bool, Fault> {
    let Some(flag) = native.get("enable_interleave") else {
        return Ok(false);
    };
    boolean(flag).ok_or_else(|| invalid("enable_interleave", "must be true or false"))
}

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
/// A request's `enable_interleave` asks for DashScope's interleaved mode,
/// the one in which `wan2.6-image` draws from text alone: the call is asked
/// to stream, as DashScope answers that mode only as an event stream, with
/// `n` of one and the count as `max_images`, the most it may draw; its
/// events' images are read into the same whole answer.
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
        Ok(bind_provider_with(io, NAME, refusal, post, |_, call| {
            Some(match call.headers().contains_key(SSE) {
                true => ended_events(streamed()),
                false => Box::new(Whole::new(read_answer)),
            })
        }))
    }
}

/// The call of `input`: a user message of its images and prompt.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let body = serde_json::to_vec(&body(input)?)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    post_json(head, GENERATION)?;
    if interleaves(OpenAiImages::fields(&input.native))? {
        head.headers.insert(SSE, HeaderValue::from_static("enable"));
    }
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
    let count = input.count.unwrap_or(1);
    if interleaves(native)? {
        parameters.insert("n".into(), 1.into());
        parameters.insert("max_images".into(), count.into());
        parameters.insert("enable_interleave".into(), true.into());
        parameters.insert("stream".into(), true.into());
    } else {
        parameters.insert("n".into(), count.into());
    }
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
    answered(&images, &v["usage"], out)
}

/// The events of an answer of images and their `usage`.
fn answered(images: &[&str], usage: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
    if images.is_empty() {
        return Err(Fault::upstream("DashScope's answer holds no image"));
    }
    out.push(Output::Start(Start::default()));
    for url in images {
        let media = Media::new("image/png", MediaSource::Url((*url).to_owned()));
        out.push(Output::Image(Image::new(media)));
    }
    if usage.is_object() {
        out.push(Output::Native(Native::json::<Key>(
            json!({ "usage": usage }),
        )));
    }
    Ok(())
}

/// A stream's events, read into one whole answer at its end: the images its
/// events hold, as they come, and the last event's usage, which counts all.
/// Its text, a caption the model writes between images, is dropped.
fn streamed() -> impl FnMut(&mut bool, &[u8], &mut Vec<Output>) -> Result<(), Fault> {
    let mut images: Vec<String> = Vec::new();
    move |done, raw, out| {
        let Some(data) = respan_gateway_sse::event_data(raw) else {
            return Ok(());
        };
        let v: Value = serde_json::from_str(&data)
            .map_err(|_| Fault::upstream("DashScope's image event is not JSON"))?;
        if says_error(&v) {
            return Err(said_error(Bytes::from(data), &v));
        }
        let choices = v["output"]["choices"].as_array().into_iter().flatten();
        let parts = choices.flat_map(|c| c["message"]["content"].as_array().into_iter().flatten());
        images.extend(parts.filter_map(|p| p["image"].as_str()).map(str::to_owned));
        if v["output"]["finished"] == true {
            *done = true;
            let urls: Vec<&str> = images.iter().map(String::as_str).collect();
            answered(&urls, &v["usage"], out)?;
        }
        Ok(())
    }
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

    fn event(v: Value) -> Vec<u8> {
        format!("data:{v}\n\n").into_bytes()
    }

    #[test]
    fn an_interleaved_call_is_streamed_with_its_count_as_the_most_images() {
        let mut i = input("an apple", json!({ "enable_interleave": "true" }));
        i.count = Some(3);
        assert_eq!(
            sent(&i)["parameters"],
            json!({ "n": 1, "max_images": 3, "enable_interleave": true, "stream": true })
        );
        let (mut head, ()) = respan_gateway_http::http::Request::new(()).into_parts();
        post(&mut head, &i).expect("a call");
        assert_eq!(head.headers[SSE], "enable");
    }

    #[test]
    fn a_stream_is_its_images_and_the_last_events_usage() {
        let text = json!({ "output": { "choices": [{ "message": { "content":
            [{ "type": "text", "text": "A" }] }, "finish_reason": "null" }], "finished": false },
            "usage": { "image_count": 0, "size": "0*0" } });
        let image = json!({ "output": { "choices": [{ "message": { "content":
            [{ "type": "image", "image": "https://a.example/1.png" }] },
            "finish_reason": "stop" }], "finished": true },
            "usage": { "image_count": 1, "size": "1024*1024" } });
        let mut read = streamed();
        let (mut done, mut out) = (false, Vec::new());
        read(&mut done, &event(text), &mut out).expect("an event");
        assert!(out.is_empty() && !done);
        read(&mut done, &event(image), &mut out).expect("an event");
        assert!(done);
        assert!(matches!(out[0], Output::Start(_)));
        let Output::Image(image) = &out[1] else {
            panic!("an image");
        };
        assert_eq!(
            image.media.source,
            MediaSource::Url("https://a.example/1.png".into())
        );
        let Some(Output::Native(native)) = out.last() else {
            panic!("a native usage");
        };
        assert_eq!(DashScopeImages::fields(native)["usage"]["image_count"], 1);
    }
}
