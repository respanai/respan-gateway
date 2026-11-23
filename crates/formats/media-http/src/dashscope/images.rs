//! DashScope's image generation, at once through `multimodal-generation`
//! or as a task run to its end: a provider format of the
//! [`image`](respan_gateway_media::image) kind, [`DashScopeImages`].

use super::{said_error, says_error, videos};
use crate::Natives;
use crate::edge::{Step, bind_waited, boolean, dimensions, job_in, posts, whole_number};
use crate::openai::OpenAiImages;
use crate::openai::missing;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{invalid, media_url, set};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{HeaderName, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, native::Json};
use respan_gateway_media::image::{Image, Input, Output, Start};
use respan_gateway_media::video::Status;
use serde_json::{Map, Value, json};

const NAME: &str = "dashscope-images";

type Key = crate::Own<DashScopeImages>;

const GENERATION: &str = "/services/aigc/multimodal-generation/generation";

/// Where a task that draws a prompt is made.
const TEXT_TO_IMAGE: &str = "/services/aigc/text2image/image-synthesis";

/// Where a task that redraws an image is made.
const IMAGE_TO_IMAGE: &str = "/services/aigc/image2image/image-synthesis";

/// The Wan models that draw only as a task: those before 2.6.
const TASKED_WAN: [&str; 3] = ["wan2.1-", "wan2.2-", "wan2.5-"];

/// Alibaba Cloud Model Studio's image models on DashScope's own API, as a
/// provider only. Qwen-Image (`qwen-image-2.0`) and Wan from 2.6
/// (`wan2.6-t2i`) answer at once, a `POST` of JSON to
/// `/services/aigc/multimodal-generation/generation`. Wan before 2.6
/// (`wan2.2-t2i-flash`) and Qwen-MT-Image (`qwen-mt-image-2.0`) draw only
/// as a task, made at `/services/aigc/text2image/image-synthesis` and
/// `/services/aigc/image2image/image-synthesis` (`X-DashScope-Async:
/// enable`) and polled at `GET /tasks/{id}` until it ends, within the
/// call: its client waits as for any other image.
///
/// The prompt is a user message's text, after the images to edit, each a URL
/// or a data URL of its bytes; a task's `input`'s `prompt`, which takes no
/// image. The count is `n`, one unless asked, as Wan makes four otherwise;
/// a `size` is written `1024*768`, and `auto` sends none. A request's
/// `negative_prompt`, `prompt_extend`, `watermark` and `seed`, as OpenAI's
/// clients send them, are parameters too, a form's text read as a number or
/// a boolean, a task's `negative_prompt` its `input`'s; other fields of
/// another format, such as a `quality`, are dropped.
///
/// Qwen-MT-Image translates the text in an image: the first image is its
/// `image_url`, and a request's `target_lang`, which it needs, its
/// `source_lang`, `auto` unless said, and its `ext` are its `input`'s. Its
/// prompt, count and size say nothing to it.
///
/// An answer's images are those of its choices' content, a task's those of
/// its `results` or its `image_url`, at URLs DashScope keeps for 24 hours,
/// and its `usage`, the images it made and their size, a native
/// `{"usage": {..}}`: it reports no tokens. A task that failed is a 502,
/// or a 400 for a parameter it refused.
///
/// A mask, a `response_format` but `url`, and an image for a model that
/// draws only a prompt are refused, 400, before the call; a `size`
/// DashScope does not draw is its own 400.
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
        Ok(bind_waited(io, NAME, post, videos::call, read))
    }
}

/// The service that draws for a model.
#[derive(Clone, Copy, PartialEq)]
enum Service {
    /// At once.
    Generation,
    /// A task, of a prompt.
    TextToImage,
    /// A task, of an image whose text it translates.
    ImageToImage,
}

/// The service that draws for `model`.
fn service(model: Option<&str>) -> Service {
    let model = model.unwrap_or_default();
    if model.starts_with("qwen-mt-image") {
        Service::ImageToImage
    } else if TASKED_WAN.iter().any(|wan| model.starts_with(wan)) {
        Service::TextToImage
    } else {
        Service::Generation
    }
}

/// The call of `input`, to the service that draws for its model: a task
/// where it draws no other way.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let service = service(input.model.as_deref());
    let path = match service {
        Service::Generation => GENERATION,
        Service::TextToImage => TEXT_TO_IMAGE,
        Service::ImageToImage => IMAGE_TO_IMAGE,
    };
    let call = posts(head, path, &body(input, service)?)?;
    if service != Service::Generation {
        let asynchronous = HeaderValue::from_static("enable");
        (head.headers).insert(HeaderName::from_static("x-dashscope-async"), asynchronous);
    }
    Ok(call)
}

/// The body of a call for `input` to `service`.
fn body(input: &Input, service: Service) -> Result<Map<String, Value>, Fault> {
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
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    if service == Service::ImageToImage {
        v.insert("input".into(), translated(input, native)?.into());
        return Ok(v);
    }
    let mut parameters = Map::new();
    parameters.insert("n".into(), input.count.unwrap_or(1).into());
    if let Some(size) = input.size.as_deref().filter(|s| *s != "auto") {
        let (w, h) = dimensions(size).ok_or_else(|| invalid("size", "must be WIDTHxHEIGHT"))?;
        parameters.insert("size".into(), format!("{w}*{h}").into());
    }
    passed(native, &mut parameters)?;
    let drawn = match service {
        Service::Generation => {
            let mut content: Vec<Value> = (input.images.iter())
                .map(|image| json!({ "image": media_url(image) }))
                .collect();
            content.push(json!({ "text": input.prompt }));
            json!({ "messages": [{ "role": "user", "content": content }] })
        }
        _ if !input.images.is_empty() => {
            return Err(invalid(
                "image",
                "is not taken by a model that draws a prompt",
            ));
        }
        _ => {
            let mut drawn = Map::new();
            drawn.insert("prompt".into(), input.prompt.clone().into());
            set(
                &mut drawn,
                "negative_prompt",
                parameters.remove("negative_prompt"),
            );
            drawn.into()
        }
    };
    v.insert("input".into(), drawn);
    v.insert("parameters".into(), parameters.into());
    Ok(v)
}

/// The `input` of a translation of `input`'s first image, into the
/// language its `native` fields name.
fn translated(input: &Input, native: &Map<String, Value>) -> Result<Map<String, Value>, Fault> {
    let image = input.images.first().ok_or_else(|| missing("image"))?;
    let target = native
        .get("target_lang")
        .ok_or_else(|| missing("target_lang"))?;
    let source = native.get("source_lang").cloned();
    let mut v = Map::new();
    v.insert("image_url".into(), media_url(image).into());
    v.insert(
        "source_lang".into(),
        source.unwrap_or_else(|| "auto".into()),
    );
    v.insert("target_lang".into(), target.clone());
    set(&mut v, "ext", native.get("ext").cloned());
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

/// What an answer `raw` says: its images and its usage, or, of a task,
/// those once it succeeded, its failure, or that it has not ended.
fn read(raw: Bytes) -> Result<Step<Output>, Fault> {
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    if says_error(&v) {
        return Err(said_error(raw, &v));
    }
    let task = &v["output"];
    let items = |of: &str| task[of].as_array().into_iter().flatten();
    if task["task_id"].is_null() {
        let parts = items("choices").flat_map(|choice| {
            choice["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
        });
        return answered(parts.filter_map(|part| part["image"].as_str()), &v["usage"]);
    }
    let job = job_in(videos::read_job, raw.clone())?;
    match job.status {
        Status::Queued | Status::InProgress => Ok(Step::Wait(job.id)),
        Status::Completed => {
            let drawn = items("results").filter_map(|result| result["url"].as_str());
            answered(drawn.chain(task["image_url"].as_str()), &v["usage"])
        }
        Status::Failed => {
            let said = json!({ "code": task["code"], "message": task["message"] });
            Err(said_error(Bytes::from(said.to_string()), task))
        }
        _ => Err(Fault::upstream(
            "DashScope's task is in no state it documents",
        )),
    }
}

/// The events of an answer of `images`, each at a URL, and its `usage`.
fn answered<'a>(
    images: impl Iterator<Item = &'a str>,
    usage: &Value,
) -> Result<Step<Output>, Fault> {
    let mut out = vec![Output::Start(Start::default())];
    for url in images {
        let media = Media::new("image/png", MediaSource::Url(url.to_owned()));
        out.push(Output::Image(Image::new(media)));
    }
    if out.len() == 1 {
        return Err(Fault::upstream("DashScope's answer holds no image"));
    }
    if usage.is_object() {
        out.push(Output::Native(Native::json::<Key>(
            json!({ "usage": usage }),
        )));
    }
    Ok(Step::Done(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;
    use respan_gateway_http::http;

    /// An input with `native` as an OpenAI client sent it.
    fn input(prompt: &str, native: Value) -> Input {
        let mut input = Input::new(prompt);
        input.model = Some("qwen-image-2.0".into());
        let fields = native.as_object().cloned().unwrap_or_default();
        input.native = OpenAiImages::with_fields(&Native::default(), |f| *f = fields);
        input
    }

    fn sent(input: &Input) -> Value {
        let service = service(input.model.as_deref());
        Value::Object(body(input, service).expect("a body"))
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
        let refuses =
            |i: &Input| body(i, Service::Generation).is_err_and(|f| f.is(FaultKind::Client));
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

    #[tokio::test(start_paused = true)]
    async fn a_task_is_polled_within_the_call_until_it_drew() {
        use futures::{SinkExt, StreamExt};
        use respan_gateway_core::{EventStream, Extensions};
        use respan_gateway_http::{Body, connect};
        use std::sync::{Arc, Mutex};

        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let send = move |call: http::Request<Body>| {
            let asynchronous = call.headers().contains_key("x-dashscope-async");
            let call = (
                call.method().clone(),
                call.uri().path().to_owned(),
                asynchronous,
            );
            let mut seen = seen.lock().expect("lock");
            seen.push(call);
            let task = match seen.len() {
                1 => json!({ "task_id": "t1", "task_status": "PENDING" }),
                2 => json!({ "task_id": "t1", "task_status": "RUNNING" }),
                _ => json!({ "task_id": "t1", "task_status": "SUCCEEDED",
                    "results": [{ "url": "https://a.example/1.png" }] }),
            };
            let answer = json!({ "output": task, "usage": { "image_count": 1 } });
            async move { Ok((http::Response::new(Body::full(answer.to_string())), None)) }
        };
        let io = connect(send, 1024)(Extensions::new());
        let (incoming, mut outgoing) = DashScopeImages.bind(io).expect("bound");
        let mut i = input("an apple", json!({}));
        i.model = Some("wan2.2-t2i-flash".into());
        outgoing
            .prepare(&mut EventStream::single(i))
            .expect("prepared");
        let (answer, sent) = futures::join!(incoming, outgoing.close());
        sent.expect("sent");
        let out: Vec<_> = answer.expect("an answer").collect().await;
        assert!(matches!(&out[1], Ok(Output::Image(_))), "{out:?}");
        let Some(Ok(Output::Native(native))) = out.last() else {
            panic!("a native usage");
        };
        assert_eq!(DashScopeImages::fields(native)["usage"]["image_count"], 1);
        let poll = (http::Method::GET, "/tasks/t1".to_owned(), false);
        assert_eq!(
            *calls.lock().expect("lock"),
            [
                (http::Method::POST, TEXT_TO_IMAGE.to_owned(), true),
                poll.clone(),
                poll
            ]
        );
    }

    #[test]
    fn an_answers_images_are_every_choices_and_its_usage_native() {
        let raw = json!({ "output": { "choices": [
            { "message": { "role": "assistant", "content": [{ "image": "https://a.example/1.png" },
                { "image": "https://a.example/2.png" }] }, "finish_reason": "stop" },
            { "message": { "role": "assistant", "content": [{ "image": "https://a.example/3.png", "type": "image" }] } }] },
            "usage": { "image_count": 3, "width": 1024, "height": 768 } });
        let Ok(Step::Done(out)) = read(Bytes::from(raw.to_string())) else {
            panic!("an answer");
        };
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
