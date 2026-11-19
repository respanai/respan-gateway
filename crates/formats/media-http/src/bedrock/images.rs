//! Amazon Bedrock's InvokeModel for Stability AI images: a provider format
//! of the [`image`](respan_gateway_media::image) kind, [`BedrockImages`].

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{base64_bytes, data_url, invalid};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::{escape, set_path};
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, Whole};
use respan_gateway_media::image::{Image, Input, Output, Start};
use serde_json::{Map, Number, Value};

use crate::Natives;
use crate::edge::{bind_provider, dimensions, nearest_ratio, sends};
use crate::openai::{OpenAiImages, sniff};

const NAME: &str = "bedrock-images";

/// The aspect ratios Stability draws.
const ASPECT_RATIOS: [(u32, u32); 9] = [
    (1, 1),
    (16, 9),
    (21, 9),
    (2, 3),
    (3, 2),
    (4, 5),
    (5, 4),
    (9, 16),
    (9, 21),
];

/// The native fields Stability takes: text, whole numbers, and other
/// numbers, which a form sends as text.
const TEXTS: &[&str] = &[
    "negative_prompt",
    "style_preset",
    "output_format",
    "search_prompt",
    "select_prompt",
];
const INTEGERS: &[&str] = &["seed", "grow_mask", "left", "right", "up", "down"];
const NUMBERS: &[&str] = &[
    "strength",
    "control_strength",
    "fidelity",
    "creativity",
    "composition_fidelity",
    "style_strength",
    "change_strength",
];

/// Amazon Bedrock's InvokeModel for Stability AI images, `POST
/// /model/{model}/invoke`, as a provider only: Stable Image Core and Ultra
/// and Stable Diffusion 3.5 Large (`stability.*`) for generations, and the
/// Image Services, which edit an image (inpaint, outpaint, erase, search and
/// replace, search and recolor, remove background, style transfer, style
/// guide, control sketch and structure, and the fast, creative and
/// conservative upscales). An inference profile's id (`us.stability.*`) is
/// the model as it is.
///
/// A generation is `{prompt}` with the nearest `aspect_ratio` of its `size`
/// (within 5%, else 400; `auto` is none). An edit is its first image as
/// `image` (style transfer's `init_image`, its second `style_image`): Stable
/// Diffusion 3.5 Large and Ultra take it as an image-to-image, Core takes
/// none, and the Image Services take no generation. A `mask` is `mask`, for
/// inpaint and erase only; an edit's `size` is dropped, but for style
/// guide's `aspect_ratio`. The prompt is dropped where a service takes none
/// (erase, remove background, fast upscale).
///
/// A request's `output_format` and Stability's own fields, `seed`,
/// `negative_prompt`, `style_preset`, `strength`, `search_prompt`,
/// `select_prompt`, `grow_mask`, `left`, `right`, `up`, `down`,
/// `control_strength`, `fidelity`, `creativity`, `composition_fidelity`,
/// `style_strength` and `change_strength`, are sent as they are, a form's
/// numbers as numbers; other fields of another format are dropped.
///
/// An answer is `{images, finish_reasons, seeds}`: an [`Image`] per image,
/// of the type its bytes say. A `finish_reasons` entry that is not null is a
/// fault, not an image: a filter's is a 400 coded `content_filter`. Bedrock
/// reports no tokens for these models, so there is no usage.
///
/// More than one image, images by URL, a mask or image the model takes none
/// of, a `size` of no aspect ratio Stability draws, and a `response_format`
/// but `b64_json` are refused, 400, before the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BedrockImages;

impl BedrockImages {
    /// The format's name, `bedrock-images`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for BedrockImages {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for BedrockImages {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| {
            Some(Box::new(Whole::new(read_answer)))
        }))
    }
}

/// A Stability model or service, as its id says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Core,
    Ultra,
    Sd35,
    Inpaint,
    Erase,
    RemoveBackground,
    StyleTransfer,
    StyleGuide,
    FastUpscale,
    /// Any other service: an image and a prompt.
    Service,
}

impl Family {
    fn of(model: &str) -> Option<Family> {
        use Family::*;
        const IDS: [(&str, Family); 16] = [
            ("stability.stable-image-core", Core),
            ("stability.stable-image-ultra", Ultra),
            ("stability.sd3-5-large", Sd35),
            ("stability.stable-image-inpaint", Inpaint),
            ("stability.stable-outpaint", Service),
            ("stability.stable-image-erase-object", Erase),
            ("stability.stable-image-search-replace", Service),
            ("stability.stable-image-search-recolor", Service),
            ("stability.stable-image-remove-background", RemoveBackground),
            ("stability.stable-style-transfer", StyleTransfer),
            ("stability.stable-image-style-guide", StyleGuide),
            ("stability.stable-image-control-sketch", Service),
            ("stability.stable-image-control-structure", Service),
            ("stability.stable-fast-upscale", FastUpscale),
            ("stability.stable-creative-upscale", Service),
            ("stability.stable-conservative-upscale", Service),
        ];
        IDS.iter()
            .find(|(id, _)| model.contains(id))
            .map(|(_, f)| *f)
    }

    /// Whether it draws from a prompt alone.
    fn generates(self) -> bool {
        matches!(self, Family::Core | Family::Ultra | Family::Sd35)
    }

    /// Whether it takes a source image.
    fn takes_image(self) -> bool {
        self != Family::Core
    }

    /// Whether it takes a prompt.
    fn takes_prompt(self) -> bool {
        !matches!(
            self,
            Family::Erase | Family::RemoveBackground | Family::FastUpscale
        )
    }

    /// Whether it takes a mask.
    fn takes_mask(self) -> bool {
        matches!(self, Family::Inpaint | Family::Erase)
    }
}

/// The call of `input`: its model's InvokeModel, in JSON.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input.model.as_deref().filter(|m| !m.is_empty());
    let model = model.ok_or_else(|| invalid("model", "is needed by Bedrock"))?;
    let family = Family::of(model)
        .ok_or_else(|| invalid("model", "is not a Bedrock Stability AI model or service"))?;
    let body = serde_json::to_vec(&body(family, input)?)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    set_path(head, &format!("/model/{}/invoke", escape(model)))?;
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(sends(Bytes::from(body)))
}

/// The body of a call of `family` for `input`.
fn body(family: Family, input: &Input) -> Result<Map<String, Value>, Fault> {
    let native = OpenAiImages::fields(&input.native);
    if input.count.is_some_and(|n| n > 1) {
        return Err(invalid("n", "must be 1: Bedrock makes one image per call"));
    }
    if native
        .get("response_format")
        .is_some_and(|f| f != "b64_json")
    {
        return Err(invalid("response_format", "must be b64_json"));
    }
    let edit = input.is_edit();
    let wanted = if family == Family::StyleTransfer {
        2
    } else {
        1
    };
    match (edit, family.generates(), family.takes_image()) {
        (false, false, _) => return Err(invalid("image", "is needed by this Bedrock service")),
        (true, _, false) => return Err(invalid("image", "is not taken by this model")),
        (true, ..) if input.images.len() != wanted => {
            let what = match wanted {
                1 => "must be one image",
                _ => "must be two images: the one to restyle, then its style",
            };
            return Err(invalid("image", what));
        }
        _ => {}
    }
    let mut v = Map::new();
    if family.takes_prompt() && !input.prompt.is_empty() {
        v.insert("prompt".into(), input.prompt.clone().into());
    }
    let keys: &[&str] = match family {
        Family::StyleTransfer => &["init_image", "style_image"],
        _ => &["image"],
    };
    for (key, image) in keys.iter().zip(&input.images) {
        v.insert((*key).into(), encoded(image)?.into());
    }
    if edit && family == Family::Sd35 {
        v.insert("mode".into(), "image-to-image".into());
    }
    if let Some(mask) = native.get("mask") {
        if !family.takes_mask() {
            return Err(invalid("mask", "is not taken by this model"));
        }
        v.insert("mask".into(), masked(mask)?.into());
    }
    if !edit || family == Family::StyleGuide {
        let ratio = input.size.as_deref().map(aspect_ratio).transpose()?;
        if let Some(ratio) = ratio.flatten() {
            v.insert("aspect_ratio".into(), ratio.into());
        }
    }
    passed(native, &mut v)?;
    Ok(v)
}

/// The base64 of `media`'s bytes.
fn encoded(media: &Media) -> Result<String, Fault> {
    match &media.source {
        MediaSource::Bytes(bytes) => Ok(STANDARD.encode(bytes)),
        MediaSource::Url(_) => Err(invalid("image", "must be bytes: Bedrock takes no URLs")),
    }
}

/// The base64 of a mask, `{"image_url"}` of a data URL.
fn masked(mask: &Value) -> Result<String, Fault> {
    let url = mask["image_url"].as_str();
    let url = url.ok_or_else(|| invalid("mask", "must name an image_url"))?;
    match data_url(url).map_err(|e| invalid("mask", e.reason()))? {
        Some((_, bytes)) => Ok(STANDARD.encode(bytes)),
        None => Err(invalid("mask", "must be bytes: Bedrock takes no URLs")),
    }
}

/// The aspect ratio Stability draws nearest to `size`, `1024x768`, within
/// 5%; none for `auto`.
fn aspect_ratio(size: &str) -> Result<Option<String>, Fault> {
    if size == "auto" {
        return Ok(None);
    }
    let nearest = dimensions(size).and_then(|(w, h)| nearest_ratio(w, h, &ASPECT_RATIOS));
    match nearest {
        Some((a, b)) => Ok(Some(format!("{a}:{b}"))),
        None => Err(invalid(
            "size",
            "is not of an aspect ratio Stability draws: 1:1, 16:9, 21:9, 2:3, 3:2, 4:5, 5:4, 9:16, 9:21",
        )),
    }
}

/// Stability's own fields of `native` into `v`, a form's numbers as numbers.
fn passed(native: &Map<String, Value>, v: &mut Map<String, Value>) -> Result<(), Fault> {
    for key in TEXTS {
        if let Some(text) = native.get(*key) {
            v.insert((*key).into(), text.clone());
        }
    }
    for (keys, whole) in [(INTEGERS, true), (NUMBERS, false)] {
        for key in keys {
            let number = match native.get(*key) {
                None => continue,
                Some(Value::Number(n)) => Some(n.clone()),
                Some(Value::String(t)) if whole => t.trim().parse::<i64>().ok().map(Number::from),
                Some(Value::String(t)) => t.trim().parse().ok().and_then(Number::from_f64),
                Some(_) => None,
            };
            let number = number.ok_or_else(|| invalid(key, "must be a number"))?;
            v.insert((*key).into(), number.into());
        }
    }
    Ok(())
}

/// The events of an answer: its images, or the fault of a `finish_reasons`
/// entry that is not null.
fn read_answer(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(&raw)
        .map_err(|_| Fault::upstream("Bedrock's answer is not JSON"))?;
    let reasons = v["finish_reasons"].as_array().into_iter().flatten();
    if let Some(reason) = reasons.filter_map(Value::as_str).next() {
        return Err(refused(reason));
    }
    let images = v["images"].as_array().filter(|images| !images.is_empty());
    let images = images.ok_or_else(|| Fault::upstream("Bedrock's answer holds no image"))?;
    out.push(Output::Start(Start::default()));
    for image in images {
        let bytes = image.as_str().and_then(base64_bytes);
        let bytes = bytes.ok_or_else(|| Fault::upstream("a Bedrock image is not base64"))?;
        let mime = sniff(&bytes).filter(|m| m.starts_with("image/"));
        let media = Media::new(
            mime.unwrap_or("image/png"),
            MediaSource::Bytes(Bytes::from(bytes)),
        );
        out.push(Output::Image(Image::new(media)));
    }
    Ok(())
}

/// The fault of a `finish_reasons` entry: a filter's is the client's.
fn refused(reason: &str) -> Fault {
    match reason.starts_with("Filter reason") {
        true => Fault::bad_request(format!("Bedrock filtered the image: {reason}"))
            .with(ErrorCode("content_filter".into())),
        false => Fault::upstream(format!("Bedrock made no image: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::{FaultKind, Native};
    use serde_json::json;

    fn media(bytes: &'static [u8]) -> Media {
        Media::new("image/png", MediaSource::Bytes(Bytes::from_static(bytes)))
    }

    /// An input to `model`, with `native` as an OpenAI client sent it.
    fn input(model: &str, prompt: &str, native: Value) -> Input {
        let mut input = Input::new(prompt);
        input.model = Some(model.to_owned());
        let fields = native.as_object().cloned().unwrap_or_default();
        input.native = OpenAiImages::with_fields(&Native::default(), |f| *f = fields);
        input
    }

    fn sent(input: &Input) -> Value {
        let family = Family::of(input.model().unwrap_or_default()).expect("a family");
        Value::Object(body(family, input).expect("a body"))
    }

    fn refuses(input: &Input) -> bool {
        let family = Family::of(input.model().unwrap_or_default()).expect("a family");
        body(family, input).is_err_and(|f| f.is(FaultKind::Client))
    }

    #[test]
    fn a_generation_is_a_prompt_with_its_aspect_ratio_and_options() {
        let native = json!({"seed": "7", "output_format": "jpeg", "negative_prompt": "blur",
            "response_format": "b64_json", "quality": "high", "user": "u"});
        let mut i = input("stability.stable-image-core-v1:1", "a fox", native);
        i.size = Some("1792x1024".into());
        let want = json!({"prompt": "a fox", "aspect_ratio": "16:9", "seed": 7,
            "output_format": "jpeg", "negative_prompt": "blur"});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn an_edit_of_a_generator_is_an_image_to_image() {
        let mut i = input(
            "stability.sd3-5-large-v1:0",
            "p",
            json!({"strength": "0.5"}),
        );
        i.images = vec![media(b"img")];
        i.size = Some("1024x1024".into());
        let want = json!({"prompt": "p", "image": "aW1n", "mode": "image-to-image",
            "strength": 0.5});
        assert_eq!(sent(&i), want);
        i.model = Some("stability.stable-image-ultra-v1:1".into());
        let want = json!({"prompt": "p", "image": "aW1n", "strength": 0.5});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn inpaint_takes_its_mask_and_fields() {
        let mask = json!({"image_url": "data:image/png;base64,bXNr"});
        let mut i = input(
            "us.stability.stable-image-inpaint-v1:0",
            "sky",
            json!({"mask": mask, "grow_mask": "5"}),
        );
        i.images = vec![media(b"img")];
        let want = json!({"prompt": "sky", "image": "aW1n", "mask": "bXNr", "grow_mask": 5});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn upscales_and_style_transfer_take_what_their_service_does() {
        let mut i = input(
            "us.stability.stable-fast-upscale-v1:0",
            "dropped",
            json!({}),
        );
        i.images = vec![media(b"img")];
        assert_eq!(sent(&i), json!({"image": "aW1n"}));
        i.model = Some("us.stability.stable-creative-upscale-v1:0".into());
        i.native = OpenAiImages::with_fields(&i.native, |f| {
            f.insert("creativity".into(), "0.3".into());
        });
        let want = json!({"prompt": "dropped", "image": "aW1n", "creativity": 0.3});
        assert_eq!(sent(&i), want);
        i.model = Some("us.stability.stable-style-transfer-v1:0".into());
        assert!(refuses(&i), "style transfer takes two images");
        i.images.push(media(b"sty"));
        i.native = Native::default();
        let want = json!({"prompt": "dropped", "init_image": "aW1n", "style_image": "c3R5"});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn what_bedrock_cannot_do_is_refused() {
        let mut i = input("stability.stable-image-ultra-v1:1", "p", json!({}));
        i.count = Some(2);
        assert!(refuses(&i));
        i.count = None;
        i.size = Some("1000x100".into());
        assert!(refuses(&i), "no aspect ratio is near");
        i.size = None;
        i.native = OpenAiImages::with_fields(&i.native, |f| {
            f.insert("response_format".into(), "url".into());
        });
        assert!(refuses(&i));
        let edit = input("us.stability.stable-fast-upscale-v1:0", "", json!({}));
        assert!(refuses(&edit), "an upscale needs an image");
        let mut core = input("stability.stable-image-core-v1:1", "p", json!({}));
        core.images = vec![media(b"img")];
        assert!(refuses(&core), "Core takes no image");
        let mask = json!({"image_url": "data:image/png;base64,bXNr"});
        let mut masked = input(
            "us.stability.stable-outpaint-v1:0",
            "",
            json!({"mask": mask}),
        );
        masked.images = vec![media(b"img")];
        assert!(refuses(&masked), "outpaint takes no mask");
        let mut linked = input("us.stability.stable-outpaint-v1:0", "", json!({}));
        let url = MediaSource::Url("https://a.test/i.png".into());
        linked.images = vec![Media::new("image/png", url)];
        assert!(refuses(&linked), "Bedrock takes no URLs");
        assert!(Family::of("amazon.nova-canvas-v1:0").is_none());
    }

    #[test]
    fn an_answer_is_its_images_or_why_it_has_none() {
        let png = STANDARD.encode(b"\x89PNG....");
        let raw = json!({"images": [png], "finish_reasons": [null], "seeds": [1]});
        let mut out = Vec::new();
        read_answer(Bytes::from(raw.to_string()), &mut out).expect("images");
        assert!(matches!(&out[..], [Output::Start(_), Output::Image(i)]
            if i.media.mime_type == "image/png"));
        let filtered = json!({"finish_reasons": ["Filter reason: prompt"]});
        let fault =
            read_answer(Bytes::from(filtered.to_string()), &mut Vec::new()).expect_err("a fault");
        assert!(fault.is(FaultKind::Client));
        assert_eq!(fault.code(), "content_filter");
        let failed = json!({"images": ["aW1n"], "finish_reasons": ["Inference error"]});
        let fault =
            read_answer(Bytes::from(failed.to_string()), &mut Vec::new()).expect_err("a fault");
        assert!(fault.is(FaultKind::Provider));
    }
}
