//! Amazon Bedrock's InvokeModel for Stability AI images: a provider format
//! of the [`image`](respan_gateway_media::image) kind, [`BedrockImages`].

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{kept, merge_missing, set};
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::{Connect, Decoder, ProviderIo, Upstream, Whole};
use respan_gateway_media::image::{Image, Input, Output, Start};
use serde_json::{Map, Number, Value};

use crate::edge::{bind_provider, escape, sends};
use crate::openai::{OpenAiImages, sniff};

const NAME: &str = "bedrock-images";

/// The aspect ratios Stability takes.
const ASPECT_RATIOS: &[(u32, u32)] = &[
    (16, 9),
    (1, 1),
    (21, 9),
    (2, 3),
    (3, 2),
    (4, 5),
    (5, 4),
    (9, 16),
    (9, 21),
];

/// The native fields Stability takes as whole numbers, and as other
/// numbers: a form's text of one is its number.
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
/// /model/{model}/invoke`, which needs a model: Stable Image Core and
/// Ultra and Stable Diffusion 3.5 Large (`stability.*`) for generations,
/// and the Image Services (`stability.stable-image-inpaint-v1:0`,
/// `stability.stable-outpaint-v1:0`, erase, search and replace, search and
/// recolor, remove background, style transfer, style guide, control sketch
/// and structure, and the fast, creative and conservative upscales) for
/// edits. An inference profile's id (`us.stability.*`) is the model as it
/// is, escaped in the path.
///
/// A generation is `{prompt, negative_prompt?, aspect_ratio?, output_format?}`,
/// the aspect ratio the nearest Stability takes to the request's `size`;
/// Stable Diffusion 3.5 (with `mode`) and Ultra take an edit as
/// image-to-image; the style guide draws from a prompt in the style of its
/// image. Every service takes the edit's first image as `image` (style transfer's
/// `init_image`, with the second as `style_image`) and the inpaint and
/// erase services its mask; the prompt is left out of the services that
/// take none. The request's native fields (`seed`, `negative_prompt`,
/// `search_prompt`, `select_prompt`, `grow_mask`, `control_strength`,
/// `fidelity`, `creativity`, `left`/`right`/`up`/`down`, ...) are the
/// service's own and pass through, a form's numbers as numbers.
///
/// An answer is `{images, finish_reasons, seeds}`: an [`Image`] per image,
/// whose type is read from its bytes. A `finish_reasons` entry that is not
/// null is a fault, not an image: a content filter's is a 400 coded
/// `content_filter`. Bedrock reports no tokens for these models, so the
/// images are the usage. A request for more than one image is refused, as
/// are images by URL, a `size` that is no `WxH` and fields of another
/// format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BedrockImages;

impl BedrockImages {
    /// The format's name, `bedrock-images`.
    pub const NAME: &'static str = NAME;

    /// Whether it draws with `model`, a model's id or an inference
    /// profile's.
    pub fn draws(model: &str) -> bool {
        Family::of(model).is_some()
    }
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
        Ok(bind_provider(
            io,
            NAME,
            |head, input: &Input| {
                let model = input.model.as_deref().unwrap_or_default();
                let body = serde_json::to_vec(&body(input)?)
                    .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
                let path = format!("/model/{}/invoke", escape(model));
                respan_gateway_http::path::set_path(head, &path)?;
                let json = HeaderValue::from_static("application/json");
                head.headers.insert(CONTENT_TYPE, json);
                Ok(sends(Bytes::from(body)))
            },
            |_, _| -> Option<Box<dyn Decoder<Output>>> { Some(Box::new(Whole::new(read_answer))) },
        ))
    }
}

/// A Stability service or model, as its id says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Core,
    Ultra,
    Sd35,
    Inpaint,
    Outpaint,
    Erase,
    SearchReplace,
    SearchRecolor,
    RemoveBackground,
    StyleTransfer,
    StyleGuide,
    ControlSketch,
    ControlStructure,
    FastUpscale,
    CreativeUpscale,
    ConservativeUpscale,
}

impl Family {
    fn of(model: &str) -> Option<Family> {
        let has = |name: &str| model.contains(name);
        Some(match () {
            _ if has("stability.stable-image-core") => Family::Core,
            _ if has("stability.stable-image-ultra") => Family::Ultra,
            _ if has("stability.sd3-5-large") => Family::Sd35,
            _ if has("stability.stable-image-inpaint") => Family::Inpaint,
            _ if has("stability.stable-outpaint") => Family::Outpaint,
            _ if has("stability.stable-image-erase-object") => Family::Erase,
            _ if has("stability.stable-image-search-replace") => Family::SearchReplace,
            _ if has("stability.stable-image-search-recolor") => Family::SearchRecolor,
            _ if has("stability.stable-image-remove-background") => Family::RemoveBackground,
            _ if has("stability.stable-style-transfer") => Family::StyleTransfer,
            _ if has("stability.stable-image-style-guide") => Family::StyleGuide,
            _ if has("stability.stable-image-control-sketch") => Family::ControlSketch,
            _ if has("stability.stable-image-control-structure") => Family::ControlStructure,
            _ if has("stability.stable-fast-upscale") => Family::FastUpscale,
            _ if has("stability.stable-creative-upscale") => Family::CreativeUpscale,
            _ if has("stability.stable-conservative-upscale") => Family::ConservativeUpscale,
            _ => return None,
        })
    }

    /// Whether it draws from a prompt alone.
    fn generates(self) -> bool {
        matches!(self, Family::Core | Family::Ultra | Family::Sd35)
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

    /// Whether it takes a source image: an edit of its own, or an
    /// image-to-image.
    fn takes_image(self) -> bool {
        self != Family::Core
    }
}

/// The body of a call for `input`.
fn body(input: &Input) -> Result<Map<String, Value>, Fault> {
    let model = input.model.as_deref().unwrap_or_default();
    let family = Family::of(model).ok_or_else(|| {
        Fault::bad_request(format!(
            "a Bedrock image needs a Stability AI model or service, not {model:?}"
        ))
    })?;
    if input.count.is_some_and(|n| n > 1) {
        return Err(Fault::bad_request("Bedrock makes one image per call"));
    }
    let mut v = Map::new();
    if family.takes_prompt() && !input.prompt.is_empty() {
        v.insert("prompt".into(), input.prompt.clone().into());
    }
    let edit = input.is_edit();
    if !edit && !family.generates() {
        return Err(Fault::bad_request(format!(
            "Bedrock's {model} edits an image: it needs one"
        )));
    }
    if edit && !family.takes_image() {
        return Err(Fault::bad_request(format!(
            "Bedrock's {model} takes no image"
        )));
    }
    let wanted = if family == Family::StyleTransfer {
        2
    } else {
        1
    };
    let sent = input.images.len();
    let native = kept(OpenAiImages::NAME, &input.native)?;
    let styled = native.contains_key("style_image");
    if edit && sent != wanted && !(sent == 1 && styled) {
        return Err(Fault::bad_request(format!(
            "Bedrock's {model} takes {wanted} image{}, not {sent}",
            if wanted == 1 { "" } else { "s" }
        )));
    }
    let mut images = input.images.iter();
    if let Some(image) = images.next() {
        let key = if family == Family::StyleTransfer {
            "init_image"
        } else {
            "image"
        };
        v.insert(key.into(), encoded(image)?.into());
        if edit && family == Family::Sd35 {
            v.insert("mode".into(), "image-to-image".into());
        }
    }
    if let Some(style) = images.next() {
        v.insert("style_image".into(), encoded(style)?.into());
    }
    match &input.mask {
        Some(mask) if family.takes_mask() => {
            v.insert("mask".into(), encoded(mask)?.into());
        }
        Some(_) => {
            return Err(Fault::bad_request(format!(
                "Bedrock's {model} takes no mask"
            )));
        }
        None => {}
    }
    if (family.generates() && !edit) || family == Family::StyleGuide {
        set(
            &mut v,
            "aspect_ratio",
            input.size.as_deref().map(aspect_ratio).transpose()?,
        );
    }
    set(
        &mut v,
        "output_format",
        input.output_format.as_deref().map(output_format),
    );
    merge_missing(&mut v, &passed(native)?);
    Ok(v)
}

/// The base64 of `media`'s bytes.
fn encoded(media: &Media) -> Result<String, Fault> {
    match &media.source {
        MediaSource::Bytes(bytes) => Ok(STANDARD.encode(bytes)),
        MediaSource::Url(_) => Err(Fault::bad_request(
            "Bedrock takes images as bytes, not URLs",
        )),
    }
}

/// The aspect ratio Stability takes nearest to `size`, `1024x768`.
fn aspect_ratio(size: &str) -> Result<String, Fault> {
    if size == "auto" {
        return Ok("1:1".into());
    }
    let sides = size.split_once('x').and_then(|(w, h)| {
        let (w, h) = (w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?);
        (w > 0.0 && h > 0.0).then_some((w, h))
    });
    let (w, h) = sides.ok_or_else(|| Fault::bad_request(format!("size {size:?} is no WxH")))?;
    let distance = |&(a, b): &(u32, u32)| (w / h).ln() - (f64::from(a) / f64::from(b)).ln();
    let (a, b) = ASPECT_RATIOS
        .iter()
        .min_by(|x, y| distance(x).abs().total_cmp(&distance(y).abs()))
        .copied()
        .unwrap_or((1, 1));
    Ok(format!("{a}:{b}"))
}

/// A file format as Stability names it.
fn output_format(format: &str) -> String {
    match format {
        "jpg" => "jpeg".into(),
        other => other.to_owned(),
    }
}

/// The native fields to send: those of the service, a form's numbers as
/// numbers. A client's `response_format` is the client edge's.
fn passed(mut native: Map<String, Value>) -> Result<Map<String, Value>, Fault> {
    native.remove("response_format");
    for key in INTEGERS.iter().chain(NUMBERS) {
        let Some(Value::String(text)) = native.get(*key) else {
            continue;
        };
        let number = match INTEGERS.contains(key) {
            true => text.trim().parse::<i64>().ok().map(Number::from),
            false => text.trim().parse::<f64>().ok().and_then(Number::from_f64),
        };
        let number =
            number.ok_or_else(|| Fault::bad_request(format!("`{key}` must be a number")))?;
        native.insert((*key).into(), number.into());
    }
    Ok(native)
}

/// The events of an answer: its images, or the fault of a `finish_reasons`
/// entry that is not null.
fn read_answer(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value =
        serde_json::from_slice(&raw).map_err(|_| Fault::upstream("Bedrock answered no JSON"))?;
    for reason in v["finish_reasons"].as_array().into_iter().flatten() {
        if let Some(reason) = reason.as_str() {
            return Err(refused(reason));
        }
    }
    let images = (v["images"].as_array()).filter(|images| !images.is_empty());
    let images = images.ok_or_else(|| Fault::upstream("Bedrock answered no images"))?;
    out.push(Output::Start(Start::new(None)));
    for image in images {
        let bytes = image.as_str().and_then(|b| STANDARD.decode(b.trim()).ok());
        let bytes = bytes.ok_or_else(|| Fault::upstream("a Bedrock image is not base64"))?;
        let mime = sniff(&bytes).filter(|m| m.starts_with("image/"));
        let mime = mime.unwrap_or("image/png");
        out.push(Output::Image(Image::new(Media::new(
            mime,
            MediaSource::Bytes(Bytes::from(bytes)),
        ))));
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
    use respan_gateway_core::{FaultKind, NativeFields};
    use serde_json::json;

    fn media(bytes: &'static [u8]) -> Media {
        Media::new("image/png", MediaSource::Bytes(Bytes::from_static(bytes)))
    }

    fn input(model: &str, prompt: &str) -> Input {
        let mut input = Input::new(prompt);
        input.model = Some(model.to_owned());
        input
    }

    fn native(fields: Value) -> Option<NativeFields> {
        let Value::Object(fields) = fields else {
            return None;
        };
        Some(NativeFields::new(OpenAiImages::NAME, fields))
    }

    fn sent(input: &Input) -> Value {
        Value::Object(body(input).expect("a body"))
    }

    #[test]
    fn a_generation_is_a_prompt_with_its_options() {
        let mut i = input("stability.stable-image-core-v1:1", "a fox");
        i.size = Some("1024x768".into());
        i.output_format = Some("jpg".into());
        i.native =
            native(json!({"seed": "7", "negative_prompt": "blur", "response_format": "b64_json"}));
        let want = json!({
            "prompt": "a fox",
            "aspect_ratio": "5:4",
            "output_format": "jpeg",
            "seed": 7,
            "negative_prompt": "blur",
        });
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn sd35_edits_as_image_to_image() {
        let mut i = input("stability.sd3-5-large-v1:0", "p");
        i.images = vec![media(b"img")];
        i.native = native(json!({"strength": "0.5"}));
        let want =
            json!({"prompt": "p", "image": "aW1n", "mode": "image-to-image", "strength": 0.5});
        assert_eq!(sent(&i), want);
        let mut core = input("stability.stable-image-core-v1:1", "p");
        core.images = vec![media(b"img")];
        assert!(body(&core).is_err_and(|f| f.is(FaultKind::Client)));
    }

    #[test]
    fn inpaint_takes_a_mask_and_its_fields() {
        let mut i = input("us.stability.stable-image-inpaint-v1:0", "sky");
        i.images = vec![media(b"img")];
        i.mask = Some(media(b"msk"));
        i.native = native(json!({"grow_mask": "5"}));
        let want = json!({"prompt": "sky", "image": "aW1n", "mask": "bXNr", "grow_mask": 5});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn a_service_without_a_prompt_or_a_mask_refuses_the_mask() {
        let mut i = input(
            "us.stability.stable-image-remove-background-v1:0",
            "ignored",
        );
        i.images = vec![media(b"img")];
        assert_eq!(sent(&i), json!({"image": "aW1n"}));
        i.mask = Some(media(b"msk"));
        assert!(body(&i).is_err_and(|f| f.is(FaultKind::Client)));
    }

    #[test]
    fn outpaint_and_search_fields_pass_through() {
        let mut i = input("us.stability.stable-outpaint-v1:0", "");
        i.images = vec![media(b"img")];
        i.native = native(json!({"left": "100", "up": 50, "creativity": "0.5"}));
        let want = json!({"image": "aW1n", "left": 100, "up": 50, "creativity": 0.5});
        assert_eq!(sent(&i), want);
        let mut i = input("us.stability.stable-image-search-replace-v1:0", "cat");
        i.images = vec![media(b"img")];
        i.native = native(json!({"search_prompt": "dog"}));
        let want = json!({"prompt": "cat", "image": "aW1n", "search_prompt": "dog"});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn style_transfer_takes_two_images() {
        let mut i = input("us.stability.stable-style-transfer-v1:0", "p");
        i.images = vec![media(b"img")];
        assert!(body(&i).is_err_and(|f| f.is(FaultKind::Client)));
        i.images.push(media(b"sty"));
        let want = json!({"prompt": "p", "init_image": "aW1n", "style_image": "c3R5"});
        assert_eq!(sent(&i), want);
    }

    #[test]
    fn what_bedrock_cannot_do_is_refused() {
        let mut i = input("stability.stable-image-ultra-v1:1", "p");
        i.count = Some(2);
        assert!(body(&i).is_err_and(|f| f.is(FaultKind::Client)));
        let edit = input("us.stability.stable-fast-upscale-v1:0", "");
        assert!(body(&edit).is_err_and(|f| f.is(FaultKind::Client)));
        assert!(body(&input("amazon.nova-canvas-v1:0", "p")).is_err());
        let mut other = input("stability.stable-image-ultra-v1:1", "p");
        other.native = Some(NativeFields::new("other", Map::new()));
        assert!(body(&other).is_ok());
        other.native = Some(NativeFields::new(
            "other",
            json!({"x": 1}).as_object().cloned().unwrap_or_default(),
        ));
        assert!(body(&other).is_err());
    }

    #[test]
    fn an_answer_is_its_images() {
        let png = STANDARD.encode(b"\x89PNG....");
        let jpg = STANDARD.encode(b"\xFF\xD8\xFF\xE0");
        let raw = json!({"images": [png, jpg], "finish_reasons": [null, null], "seeds": [1, 2]});
        let mut out = Vec::new();
        read_answer(Bytes::from(raw.to_string()), &mut out).expect("images");
        let mimes: Vec<_> = (out.iter())
            .filter_map(|e| match e {
                Output::Image(i) => Some(i.media.mime_type.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(mimes, ["image/png", "image/jpeg"]);
    }

    #[test]
    fn a_content_filter_is_a_fault_not_an_image() {
        let raw = json!({"finish_reasons": ["Filter reason: prompt"]});
        let mut out = Vec::new();
        let fault = read_answer(Bytes::from(raw.to_string()), &mut out).expect_err("a fault");
        assert!(fault.is(FaultKind::Client));
        assert_eq!(fault.code(), "content_filter");
        assert!(out.is_empty());
        let raw = json!({"images": ["aW1n"], "finish_reasons": ["Inference error"]});
        let fault = read_answer(Bytes::from(raw.to_string()), &mut out).expect_err("a fault");
        assert!(fault.is(FaultKind::Provider));
    }
}
