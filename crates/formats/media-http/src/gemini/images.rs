//! Gemini's image models, such as Nano Banana, through `generateContent`:
//! a format of the [`image`](respan_gateway_media::image) kind,
//! [`GeminiImages`].

use super::{config_of, generate, model_of, refusal};
use crate::edge::bind_provider;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_conversation_http::fields::{invalid, kept, native_of};
use respan_gateway_conversation_http::gemini::{call_of, sse, usage_body, usage_of};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{All, ClientIo, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, KeyIn, Upstream};
use respan_gateway_media::image::{Image, Input, Output, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-images";

/// Gemini's image models, such as Nano Banana, as the Gemini API serves
/// them: a `POST` to `.../models/{model}:generateContent`, its answer's
/// image parts the images.
///
/// A call is one user turn: the images to edit, as `inlineData` (or
/// `fileData`, for an image named by URL), then the prompt. Its `count` is
/// `candidateCount`, which Gemini's image models refuse above 1; its `size`
/// an `imageConfig` of the nearest `aspectRatio` Gemini draws (within 5%,
/// else 400), and an `imageSize` of `2K` or `4K` for a size above 1K. A mask
/// is refused, 400, as Gemini edits by prompt alone; quality, file format and
/// user are dropped. Its native fields go in the body, `generationConfig`
/// merged with what it says. An answer's images are its parts' `inlineData`
/// (or `fileData`), not its thoughts', its usage `usageMetadata`'s; its text
/// is dropped. An answer with no image is refused, 400 `no_image`, saying why.
///
/// A client speaks it too, of any provider of the kind: a `POST` to
/// `.../models/{model}:generateContent` or `:streamGenerateContent` with
/// one user turn of text, the prompt, and images (`inlineData` or
/// `fileData`) to edit. Its `generationConfig`'s `candidateCount` is the
/// count, and its `imageConfig`, of an aspect ratio Gemini draws, a size;
/// `responseModalities` is dropped, as the answer is images; the rest of it,
/// and its other fields, are this format's native fields. The answer is
/// one candidate of the images as `inlineData` (or `fileData`, for an image
/// at a URL), and the usage as `usageMetadata`; a stream is that response,
/// once, in the framing it asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiImages;

impl GeminiImages {
    /// The format's name, `gemini-images`, which tags its native fields.
    pub const NAME: &'static str = NAME;

    /// The model a client's call at `uri` names, if it is one of this
    /// format's.
    pub fn model(uri: &Uri) -> Option<String> {
        call_of(uri).map(|(model, _)| model)
    }
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiImages {
    fn name(&self) -> &str {
        NAME
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiImages {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, _| {
            Some(Box::new(Whole::new(read_response)))
        }))
    }
}

/// Gemini's aspect ratios, and the size of each at 1K.
const ASPECTS: [(&str, u32, u32); 10] = [
    ("1:1", 1024, 1024),
    ("2:3", 832, 1248),
    ("3:2", 1248, 832),
    ("3:4", 864, 1184),
    ("4:3", 1184, 864),
    ("4:5", 896, 1152),
    ("5:4", 1152, 896),
    ("9:16", 768, 1344),
    ("16:9", 1344, 768),
    ("21:9", 1536, 672),
];

/// Gemini's image sizes above 1K, and how many times 1K each is.
const SIZES: [(&str, u32); 3] = [("1K", 1), ("2K", 2), ("4K", 4)];

/// The call of `input`: one user turn of its images and prompt, its
/// `generationConfig`, and its native fields.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let needs = "a Gemini image needs a model, such as gemini-2.5-flash-image";
    let model = model_of(input.model.as_deref(), needs)?;
    if input.mask.is_some() {
        return Err(
            Fault::bad_request("Gemini edits by prompt, and takes no mask")
                .with(ErrorCode("unsupported".into())),
        );
    }
    let mut native = kept(NAME, &input.native)?;
    let mut parts: Vec<Value> = input.images.iter().map(part_of).collect();
    parts.push(json!({ "text": input.prompt }));
    let mut config = config_of(&mut native)?;
    if let Some(count) = input.count {
        config.insert("candidateCount".into(), count.into());
    }
    if let Some((aspect, size)) = input.size.as_deref().map(aspect_of).transpose()?.flatten() {
        let image = config.entry("imageConfig").or_insert_with(|| json!({}));
        image["aspectRatio"] = aspect.into();
        if let Some(size) = size {
            image["imageSize"] = size.into();
        }
    }
    generate(head, model, parts.into(), config, native)
}

/// An image as a part: its bytes inline, or its URL as a file's.
fn part_of(media: &Media) -> Value {
    match &media.source {
        MediaSource::Bytes(bytes) => json!({
            "inlineData": { "mimeType": media.mime_type, "data": STANDARD.encode(bytes) },
        }),
        MediaSource::Url(url) => json!({
            "fileData": { "mimeType": media.mime_type, "fileUri": url },
        }),
    }
}

/// Gemini's aspect ratio of a size `WxH`, and its image size if above 1K;
/// none for `auto`.
fn aspect_of(size: &str) -> Result<Option<(&'static str, Option<&'static str>)>, Fault> {
    if size == "auto" {
        return Ok(None);
    }
    let parsed = size.split_once(['x', 'X']).and_then(|(w, h)| {
        let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
        (w > 0 && h > 0).then_some((w, h))
    });
    let ratio = |w: u32, h: u32| f64::from(w) / f64::from(h);
    let nearest = parsed.and_then(|(w, h)| {
        let off = |&&(_, aw, ah): &&(&str, u32, u32)| (ratio(w, h) / ratio(aw, ah) - 1.0).abs();
        let best = ASPECTS.iter().min_by(|a, b| off(a).total_cmp(&off(b)))?;
        (off(&best) <= 0.05).then_some((best, w.max(h)))
    });
    let Some(((aspect, aw, ah), long)) = nearest else {
        let aspects: Vec<&str> = ASPECTS.iter().map(|(a, ..)| *a).collect();
        return Err(invalid(
            "size",
            &format!(
                "is not of an aspect ratio Gemini draws: {}",
                aspects.join(", ")
            ),
        ));
    };
    let times = f64::from(long) / f64::from(*aw.max(ah));
    let size = match times {
        t if t <= 1.5 => None,
        t if t <= 3.0 => Some("2K"),
        _ => Some("4K"),
    };
    Ok(Some((aspect, size)))
}

/// The size, `WxH`, of an image of `aspect` and `size`, 1K if it says
/// none; none if Gemini's aspect ratios or sizes have no such one.
fn size_of(aspect: &str, size: Option<&str>) -> Option<String> {
    let (_, w, h) = ASPECTS.iter().find(|(a, ..)| *a == aspect)?;
    let times = match size {
        None => 1,
        Some(size) => SIZES.iter().find(|(s, _)| *s == size)?.1,
    };
    Some(format!("{}x{}", w * times, h * times))
}

/// The events of an answer: its images, then its usage. One with no image
/// is refused, saying why.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} response is not JSON")))?;
    out.push(Output::Start(Start::new(None)));
    let (mut images, mut said, mut why) = (0, Vec::new(), None);
    for candidate in v["candidates"].as_array().into_iter().flatten() {
        let parts = candidate["content"]["parts"].as_array();
        for part in parts.into_iter().flatten().filter(|p| p["thought"] != true) {
            match media_of(part) {
                Some(media) => {
                    out.push(Output::Image(Image::new(media)));
                    images += 1;
                }
                None => said.extend(part["text"].as_str()),
            }
        }
        why = why.or(candidate["finishReason"].as_str().filter(|r| *r != "STOP"));
    }
    let usage = &v["usageMetadata"];
    if usage.is_object() {
        out.push(Output::Usage(usage_of(usage)?));
    }
    if images > 0 {
        return Ok(());
    }
    let why = v["promptFeedback"]["blockReason"].as_str().or(why);
    let mut message = String::from("Gemini made no image");
    if let Some(why) = why {
        message.push_str(&format!(" ({why})"));
    }
    if !said.is_empty() {
        message.push_str(&format!(": {}", said.join("")));
    }
    Err(Fault::bad_request(message).with(ErrorCode("no_image".into())))
}

/// The image of a part, if it holds one: its bytes inline, or a file's URL.
fn media_of(part: &Value) -> Option<Media> {
    if let Some(data) = part.get("inlineData") {
        let mime = data["mimeType"].as_str().unwrap_or("image/png");
        let bytes = STANDARD.decode(data["data"].as_str()?).ok()?;
        return Some(Media::new(mime, MediaSource::Bytes(bytes.into())));
    }
    let file = part.get("fileData")?;
    let mime = file["mimeType"].as_str().unwrap_or("image/png");
    let url = file["fileUri"].as_str()?;
    Some(Media::new(mime, MediaSource::Url(url.to_owned())))
}

// --- A client of Gemini's image models: its requests read, the kind's
// answers written.

impl Format<ClientIo, Input, Output> for GeminiImages {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && call_of(request.uri()).is_some()
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(io.with_error(refusal).bind(
            move |head| {
                let head = head.clone();
                Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Input>| {
                    out.push(read_request(&head, &raw)?);
                    Ok(())
                }))
            },
            move |reply| {
                let mut head = reply.head();
                let encoder = response_encoder(reply.request(), &mut head);
                Ok(reply.write(head, encoder))
            },
        ))
    }
}

/// A client's call: its model, its one turn, and its `generationConfig`.
fn read_request(head: &RequestHead, raw: &[u8]) -> Result<Input, Fault> {
    let model = GeminiImages::model(&head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no generateContent endpoint"))?;
    let mut v: Map<String, Value> = serde_json::from_slice(raw)
        .map_err(|_| Fault::bad_request("the request body is not a JSON object"))?;
    v.remove("model");
    let (prompt, images) = read_turn(v.remove("contents"))?;
    let mut input = Input::new(prompt);
    input.model = Some(model);
    input.images = images;
    match v.remove("generationConfig") {
        Some(Value::Object(config)) => {
            if let Some(rest) = read_config(config, &mut input)? {
                v.insert("generationConfig".into(), rest.into());
            }
        }
        None => {}
        Some(_) => return Err(invalid("generationConfig", "must be an object")),
    }
    input.native = native_of(NAME, v);
    Ok(input)
}

/// The prompt and images of `contents`, one user turn: its text parts,
/// joined, and its images.
fn read_turn(contents: Option<Value>) -> Result<(String, Vec<Media>), Fault> {
    let one_turn = || {
        invalid(
            "contents",
            "must be one user turn: an image is drawn of no history",
        )
    };
    let mut contents = match contents {
        Some(Value::Array(contents)) => contents,
        _ => return Err(one_turn()),
    };
    let (Some(turn), true) = (contents.pop(), contents.is_empty()) else {
        return Err(one_turn());
    };
    if !matches!(
        turn.get("role").and_then(Value::as_str),
        None | Some("user")
    ) {
        return Err(one_turn());
    }
    let (mut text, mut images) = (Vec::new(), Vec::new());
    for (i, part) in turn["parts"].as_array().into_iter().flatten().enumerate() {
        match (part["text"].as_str(), media_of(part)) {
            (Some(said), _) => text.push(said),
            (None, Some(media)) => images.push(media),
            (None, None) => {
                let param = format!("contents[0].parts[{i}]");
                return Err(invalid(&param, "is neither text nor an image"));
            }
        }
    }
    let prompt = text.join("\n");
    if prompt.is_empty() {
        return Err(invalid("contents", "must say what to draw, in text"));
    }
    Ok((prompt, images))
}

/// Reads a `generationConfig` into `input`: its `candidateCount`, and its
/// `imageConfig` if a size can say it; the rest, if any, is given back.
fn read_config(
    mut config: Map<String, Value>,
    input: &mut Input,
) -> Result<Option<Map<String, Value>>, Fault> {
    config.remove("responseModalities");
    input.count = match config.remove("candidateCount") {
        None => None,
        Some(n) => Some(
            n.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("generationConfig.candidateCount", "must be a count"))?,
        ),
    };
    if let Some(Value::Object(image)) = config.get_mut("imageConfig") {
        let aspect = image.get("aspectRatio").and_then(Value::as_str);
        let size = match image.get("imageSize") {
            None => aspect.and_then(|aspect| size_of(aspect, None)),
            Some(size) => (aspect.zip(size.as_str())).and_then(|(a, s)| size_of(a, Some(s))),
        };
        // A size the kind can say goes as one; another stays Gemini's own.
        if let Some(size) = size {
            input.size = Some(size);
            image.remove("aspectRatio");
            image.remove("imageSize");
            if image.is_empty() {
                config.remove("imageConfig");
            }
        }
    }
    Ok((!config.is_empty()).then_some(config))
}

/// The encoder of an answer to `request`: one response of its images and
/// usage, whole, or as a stream of it in the framing asked for.
fn response_encoder(
    request: &http::Request<Bytes>,
    head: &mut ResponseHead,
) -> Box<dyn Encoder<Output>> {
    let streams = call_of(request.uri()).is_some_and(|(_, m)| m == "streamGenerateContent");
    let sse = streams && sse(request.uri());
    let typed = if sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static(typed));
    Box::new(All::new(move |events: &[Output]| {
        let response = serde_json::to_string(&response_of(events))
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
        Ok::<_, Fault>(match (streams, sse) {
            (true, true) => format!("data: {response}\r\n\r\n"),
            (true, false) => format!("[{response}]"),
            _ => response,
        })
    }))
}

/// A response of `events`: one candidate of their images, and their usage.
/// What a response has no place for, such as a preview, is dropped.
fn response_of(events: &[Output]) -> Value {
    let (mut parts, mut usage) = (Vec::new(), None);
    for event in events {
        match event {
            Output::Image(image) => parts.push(part_of(&image.media)),
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            _ => {}
        }
    }
    let content = json!({ "role": "model", "parts": parts });
    let candidate = json!({ "content": content, "finishReason": "STOP", "index": 0 });
    let mut response = json!({ "candidates": [candidate] });
    if let Some(usage) = usage.and_then(|u| usage_body(&u, &Map::new())) {
        response["usageMetadata"] = usage;
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    fn request(path: &str, body: Value) -> Result<Input, Fault> {
        let head = http::Request::post(path).body(()).unwrap_or_default();
        read_request(&head.into_parts().0, body.to_string().as_bytes())
    }

    fn png(data: &'static [u8]) -> Media {
        Media::new("image/png", MediaSource::Bytes(Bytes::from_static(data)))
    }

    #[test]
    fn a_clients_turn_is_a_prompt_and_its_images() {
        let body = json!({
            "contents": [{ "role": "user", "parts": [
                { "inlineData": { "mimeType": "image/png", "data": "YQ==" } },
                { "text": "make it" },
                { "text": "blue" },
            ]}],
            "generationConfig": {
                "responseModalities": ["TEXT", "IMAGE"],
                "imageConfig": { "aspectRatio": "16:9", "imageSize": "2K" },
                "temperature": 1,
            },
            "safetySettings": [],
        });
        let path = "/v1beta/models/gpt-image-2:generateContent";
        let input = request(path, body).expect("a request");
        assert_eq!(input.model.as_deref(), Some("gpt-image-2"));
        assert_eq!(input.prompt, "make it\nblue");
        assert_eq!(input.images, [png(b"a")]);
        assert_eq!(input.size.as_deref(), Some("2688x1536"));
        let native = input.native.map(|n| Value::Object(n.fields));
        let rest = json!({ "generationConfig": { "temperature": 1 }, "safetySettings": [] });
        assert_eq!(native, Some(rest));
    }

    #[test]
    fn a_client_without_one_user_turn_of_text_is_refused() {
        let path = "/v1beta/models/m:generateContent";
        let text = |t: &str| json!({ "parts": [{ "text": t }] });
        let model = json!({ "role": "model", "parts": [{ "text": "a" }] });
        for contents in [
            json!([]),
            json!([text("a"), text("b")]),
            json!([model]),
            json!([{ "parts": [{ "functionCall": {} }] }]),
            json!([{ "parts": [{ "inlineData": { "mimeType": "image/png", "data": "YQ==" } }] }]),
        ] {
            let refused = request(path, json!({ "contents": contents }));
            assert!(
                refused.is_err_and(|f| f.is(FaultKind::Client)),
                "{contents}"
            );
        }
    }

    #[test]
    fn a_size_is_the_nearest_aspect_ratio_and_back() {
        for (size, aspect, image_size) in [
            ("1024x1024", "1:1", None),
            ("1536x1024", "3:2", None),
            ("1024x1536", "2:3", None),
            ("1792x1024", "16:9", None),
            ("2688x1536", "16:9", Some("2K")),
            ("4096x4096", "1:1", Some("4K")),
        ] {
            assert_eq!(
                aspect_of(size).ok(),
                Some(Some((aspect, image_size))),
                "{size}"
            );
        }
        assert_eq!(aspect_of("auto").ok(), Some(None));
        for size in ["1000x100", "big", "0x5"] {
            assert!(aspect_of(size).is_err(), "{size}");
        }
        assert_eq!(size_of("3:2", None).as_deref(), Some("1248x832"));
        assert_eq!(size_of("9:16", Some("4K")).as_deref(), Some("3072x5376"));
        assert_eq!(size_of("1:4", None), None);
    }

    #[test]
    fn a_call_is_one_turn_of_its_images_then_its_prompt() {
        let mut input = Input::new("blue");
        input.model = Some("gemini-3.1-flash-image".into());
        input.images = vec![png(b"a")];
        input.size = Some("1536x1024".into());
        let generation = json!({ "generationConfig": { "temperature": 1 } });
        input.native = native_of(NAME, generation.as_object().cloned().unwrap_or_default());
        let mut head = http::Request::new(()).into_parts().0;
        let encoder = call(&mut head, &input).expect("a call");
        assert_eq!(
            head.uri.path(),
            "/models/gemini-3.1-flash-image:generateContent"
        );
        let written = {
            let mut encoder = encoder;
            encoder.encode_all(&[input])
        };
        let written: Value =
            serde_json::from_slice(&written.unwrap_or_default()).unwrap_or_default();
        let want = json!({
            "contents": [{ "role": "user", "parts": [
                { "inlineData": { "mimeType": "image/png", "data": "YQ==" } },
                { "text": "blue" },
            ]}],
            "generationConfig": { "temperature": 1, "imageConfig": { "aspectRatio": "3:2" } },
        });
        assert_eq!(written, want);
    }

    #[test]
    fn a_mask_is_refused() {
        let mut input = Input::new("p");
        input.model = Some("m".into());
        input.images = vec![png(b"a")];
        input.mask = Some(png(b"m"));
        let mut head = http::Request::new(()).into_parts().0;
        assert!(call(&mut head, &input).is_err_and(|f| f.is(FaultKind::Client)));
    }

    #[test]
    fn an_answers_images_are_its_parts_but_thoughts() {
        let answer = json!({
            "candidates": [{ "content": { "parts": [
                { "text": "here", "thought": true },
                { "inlineData": { "mimeType": "image/png", "data": "Yg==" }, "thought": true },
                { "text": "here it is" },
                { "inlineData": { "mimeType": "image/jpeg", "data": "YQ==" }, "thoughtSignature": "s" },
            ]}, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 5, "thoughtsTokenCount": 2 },
        });
        let mut out = Vec::new();
        read_response(Bytes::from(answer.to_string()), &mut out).expect("an answer");
        let jpeg = Media::new("image/jpeg", MediaSource::Bytes(Bytes::from_static(b"a")));
        let mut usage = Usage::new(3, 7);
        usage.reasoning_tokens = 2;
        let want = [
            Output::Start(Start::new(None)),
            Output::Image(Image::new(jpeg)),
            Output::Usage(usage),
        ];
        assert_eq!(out, want);
    }

    #[test]
    fn an_answer_without_an_image_says_why() {
        let answer = json!({
            "candidates": [{ "content": { "parts": [{ "text": "I cannot." }] },
                "finishReason": "IMAGE_SAFETY" }],
        });
        let mut out = Vec::new();
        let refused = read_response(Bytes::from(answer.to_string()), &mut out);
        let refused = refused.expect_err("no image");
        assert!(refused.is(FaultKind::Client));
        assert!(
            refused.message().contains("IMAGE_SAFETY"),
            "{}",
            refused.message()
        );
        assert!(
            refused.message().contains("I cannot."),
            "{}",
            refused.message()
        );
    }

    #[test]
    fn a_response_is_written_in_the_framing_asked() {
        let events = [
            Output::Start(Start::new(Some(1))),
            Output::Image(Image::new(png(b"a"))),
            Output::Usage(Usage::new(3, 5)),
        ];
        let written = |path: &str| {
            let request = http::Request::post(path)
                .body(Bytes::new())
                .unwrap_or_default();
            let mut head = http::Response::new(()).into_parts().0;
            let mut encoder = response_encoder(&request, &mut head);
            let body = encoder.encode_all(&events).unwrap_or_default();
            (
                head.headers[CONTENT_TYPE].clone(),
                String::from_utf8_lossy(&body).into_owned(),
            )
        };
        let whole = json!({
            "candidates": [{ "content": { "role": "model", "parts": [
                { "inlineData": { "mimeType": "image/png", "data": "YQ==" } },
            ]}, "finishReason": "STOP", "index": 0 }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 5, "totalTokenCount": 8 },
        });
        let (typed, body) = written("/v1beta/models/m:generateContent");
        assert_eq!(typed, "application/json");
        assert_eq!(
            serde_json::from_str::<Value>(&body).ok(),
            Some(whole.clone())
        );
        let (typed, body) = written("/v1beta/models/m:streamGenerateContent?alt=sse");
        assert_eq!(typed, "text/event-stream");
        let event = body
            .strip_prefix("data: ")
            .and_then(|b| b.strip_suffix("\r\n\r\n"));
        let event = event.and_then(|e| serde_json::from_str::<Value>(e).ok());
        assert_eq!(event, Some(whole.clone()));
        let (_, body) = written("/v1beta/models/m:streamGenerateContent");
        assert_eq!(
            serde_json::from_str::<Value>(&body).ok(),
            Some(json!([whole]))
        );
    }
}
