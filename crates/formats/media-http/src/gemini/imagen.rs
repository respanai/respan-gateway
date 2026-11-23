//! Gemini's Imagen models through `predict`: a format of the
//! [`image`](respan_gateway_media::image) kind, [`GeminiImagen`].

use super::{is_segment, refusal};
use crate::edge::{bind_provider, named_aspect, sends};
use base64::{Engine, engine::general_purpose::STANDARD};
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{base64_bytes, invalid};
use respan_gateway_conversation_http::gemini::method_of;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    HeaderValue, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{All, ClientIo, Encoder, ProviderIo, Whole, native::Json};
use respan_gateway_http::{Connect, KeyIn, Upstream};
use respan_gateway_media::image::{Image, Input, Output, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-imagen";

/// The method of an Imagen call: `.../models/{model}:predict`.
const PREDICT: &str = "predict";

type Key = crate::Own<GeminiImagen>;

/// Gemini's Imagen models, as the Gemini API serves them: a `POST` to
/// `.../models/{model}:predict`, one instance of the prompt, its answer's
/// predictions the images.
///
/// A call is one instance, `{"prompt"}`; its `count` is `sampleCount`, and
/// its `size` the nearest `aspectRatio` Imagen draws (within 5%, else 400),
/// with an `imageSize` of `2K` for a size above 1K. Imagen on the Gemini
/// API draws from a prompt alone, so images to edit are refused, 400. Its
/// native fields go in the body, `parameters` merged with what it says. An
/// answer's images are its predictions' `bytesBase64Encoded` (or `gcsUri`);
/// one with no image is refused, 400 `no_image`, saying why if a prediction
/// has a `raiFilteredReason`. Imagen reports no usage.
///
/// A client speaks it too, of any provider of the kind: a `POST` to
/// `.../models/{model}:predict` with one instance of a prompt. Its
/// `parameters`' `sampleCount` is the count, and its `aspectRatio` (with an
/// `imageSize` of `1K` or `2K`) a size; the rest of them, and its other
/// fields, are this format's native fields. The answer is the images as
/// `predictions`, each `bytesBase64Encoded` (or `gcsUri`, for an image a
/// provider keeps at a URL) with its `mimeType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiImagen;

impl GeminiImagen {
    /// The format's name, `gemini-imagen`.
    pub const NAME: &'static str = NAME;

    /// The model a client's call at `uri` names, if it is one of this
    /// format's.
    pub fn model(uri: &Uri) -> Option<String> {
        method_of(uri)
            .filter(|(_, method)| *method == PREDICT)
            .map(|(model, _)| model)
    }
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiImagen {
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

impl Format<ProviderIo, Output, Input> for GeminiImagen {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, call, |_, _| {
            Some(Box::new(Whole::new(read_response)))
        }))
    }
}

/// Imagen's aspect ratios, as ratios.
const RATIOS: [(&str, u32, u32); 5] = [
    ("1:1", 1, 1),
    ("3:4", 3, 4),
    ("4:3", 4, 3),
    ("9:16", 9, 16),
    ("16:9", 16, 9),
];

/// Imagen's aspect ratios, and the size of each at 1K, which is of its
/// ratio only roughly (4:3 is 1280x896).
const ASPECTS: [(&str, u32, u32); 5] = [
    ("1:1", 1024, 1024),
    ("3:4", 896, 1280),
    ("4:3", 1280, 896),
    ("9:16", 768, 1408),
    ("16:9", 1408, 768),
];

/// The call of `input`: one instance of its prompt.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input.model.as_deref().unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    if !is_segment(model) {
        return Err(Fault::bad_request(
            "an Imagen image needs a model, such as imagen-4.0-generate-001",
        ));
    }
    if !input.images.is_empty() {
        return Err(invalid(
            "image",
            "is not taken: Imagen on the Gemini API draws from a prompt alone",
        ));
    }
    head.uri = format!("/models/{model}:{PREDICT}")
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    let body = serde_json::to_vec(&body_of(input)?)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// A call's body: its instance, its `parameters`, and its native fields.
fn body_of(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut native = input.native.fields::<Key>().clone();
    let mut parameters = match native.remove("parameters") {
        Some(Value::Object(parameters)) => parameters,
        None => Map::new(),
        Some(_) => return Err(invalid("parameters", "must be an object")),
    };
    if let Some(count) = input.count {
        parameters.insert("sampleCount".into(), count.into());
    }
    if let Some((aspect, size)) = input.size.as_deref().map(aspect_of).transpose()?.flatten() {
        parameters.insert("aspectRatio".into(), aspect.into());
        if let Some(size) = size {
            parameters.insert("imageSize".into(), size.into());
        }
    }
    let mut v = Map::new();
    v.insert("instances".into(), json!([{ "prompt": input.prompt }]));
    if !parameters.is_empty() {
        v.insert("parameters".into(), parameters.into());
    }
    for (key, value) in native {
        v.entry(key).or_insert(value);
    }
    Ok(v)
}

/// Imagen's aspect ratio of a size `WxH`, and `2K` if it is above 1K;
/// none for `auto`.
fn aspect_of(size: &str) -> Result<Option<(&'static str, Option<&'static str>)>, Fault> {
    // Of a ratio, or of one of Imagen's own sizes.
    let found = match named_aspect(size, &RATIOS, "Imagen") {
        Ok(found) => found,
        Err(refused) => named_aspect(size, &ASPECTS, "Imagen").map_err(|_| refused)?,
    };
    let Some(((aspect, ..), long)) = found else {
        return Ok(None);
    };
    let Some((_, w, h)) = ASPECTS.iter().find(|(a, ..)| *a == aspect) else {
        return Ok(None);
    };
    let above_1k = f64::from(long) / f64::from(*w.max(h)) > 1.5;
    Ok(Some((aspect, above_1k.then_some("2K"))))
}

/// The size, `WxH`, of an image of `aspect` and `size`, 1K if it says
/// none; none if Imagen's aspect ratios or sizes have no such one.
fn size_of(aspect: &str, size: Option<&str>) -> Option<String> {
    let (_, w, h) = ASPECTS.iter().find(|(a, ..)| *a == aspect)?;
    let times = match size {
        None | Some("1K") => 1,
        Some("2K") => 2,
        Some(_) => return None,
    };
    Some(format!("{}x{}", w * times, h * times))
}

/// The events of an answer: its images. One with no image is refused,
/// saying why if it can.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} response is not JSON")))?;
    out.push(Output::Start(Start::default()));
    let (mut images, mut why) = (0, None);
    for prediction in v["predictions"].as_array().into_iter().flatten() {
        match image_of(prediction) {
            Some(media) => {
                out.push(Output::Image(Image::new(media)));
                images += 1;
            }
            None => why = why.or(prediction["raiFilteredReason"].as_str()),
        }
    }
    if images > 0 {
        return Ok(());
    }
    let mut message = String::from("Imagen made no image");
    if let Some(why) = why {
        message.push_str(&format!(": {why}"));
    }
    Err(Fault::bad_request(message).with(ErrorCode("no_image".into())))
}

/// The image of a prediction, if it holds one: its bytes, or where Google
/// Cloud Storage keeps it.
fn image_of(prediction: &Value) -> Option<Media> {
    let mime = prediction["mimeType"].as_str().unwrap_or("image/png");
    if let Some(data) = prediction["bytesBase64Encoded"].as_str() {
        let bytes = base64_bytes(data)?;
        return Some(Media::new(mime, MediaSource::Bytes(bytes.into())));
    }
    let uri = prediction["gcsUri"].as_str()?;
    Some(Media::new(mime, MediaSource::Url(uri.to_owned())))
}

// --- A client of Imagen: its requests read, the kind's answers written.

impl Format<ClientIo, Input, Output> for GeminiImagen {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && GeminiImagen::model(request.uri()).is_some()
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
                let encoder = response_encoder(&mut head);
                Ok(reply.write(head, encoder))
            },
        ))
    }
}

/// A client's call: its model, its one instance, and its `parameters`.
fn read_request(head: &RequestHead, raw: &[u8]) -> Result<Input, Fault> {
    let model = GeminiImagen::model(&head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no predict endpoint"))?;
    let mut v: Map<String, Value> = serde_json::from_slice(raw)
        .map_err(|_| Fault::bad_request("the request body is not a JSON object"))?;
    let mut input = Input::new(read_instances(v.remove("instances"))?);
    input.model = Some(model);
    match v.remove("parameters") {
        Some(Value::Object(parameters)) => {
            if let Some(rest) = read_parameters(parameters, &mut input)? {
                v.insert("parameters".into(), rest.into());
            }
        }
        None => {}
        Some(_) => return Err(invalid("parameters", "must be an object")),
    }
    input.native = Native::json::<Key>(v);
    Ok(input)
}

/// The prompt of `instances`: one instance, of a prompt alone.
fn read_instances(instances: Option<Value>) -> Result<String, Fault> {
    let one = || invalid("instances", "must be one instance: a call draws one prompt");
    let mut instances = match instances {
        Some(Value::Array(instances)) => instances,
        _ => return Err(one()),
    };
    let (Some(instance), true) = (instances.pop(), instances.is_empty()) else {
        return Err(one());
    };
    let Some(fields) = instance.as_object() else {
        return Err(one());
    };
    if let Some(other) = fields.keys().find(|k| *k != "prompt") {
        let why = "is not taken: Imagen on the Gemini API draws from a prompt alone";
        return Err(invalid(&format!("instances[0].{other}"), why));
    }
    match fields.get("prompt").and_then(Value::as_str) {
        Some(prompt) if !prompt.is_empty() => Ok(prompt.to_owned()),
        _ => Err(invalid("instances[0].prompt", "must say what to draw")),
    }
}

/// Reads `parameters` into `input`: its `sampleCount`, and its
/// `aspectRatio` and `imageSize` if a size can say them; the rest, if any,
/// is given back.
fn read_parameters(
    mut parameters: Map<String, Value>,
    input: &mut Input,
) -> Result<Option<Map<String, Value>>, Fault> {
    input.count = match parameters.remove("sampleCount") {
        None => None,
        Some(n) => Some(
            n.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("parameters.sampleCount", "must be a count"))?,
        ),
    };
    // Imagen draws a square when no aspect ratio is asked for.
    let aspect = parameters.get("aspectRatio").map(Value::as_str);
    let size = match (aspect, parameters.get("imageSize")) {
        (None, None) => None,
        (aspect, size) => {
            let aspect = aspect.unwrap_or(Some("1:1"));
            let size = size.map(Value::as_str);
            match (aspect, size) {
                (Some(aspect), None) => size_of(aspect, None),
                (Some(aspect), Some(Some(size))) => size_of(aspect, Some(size)),
                _ => None,
            }
        }
    };
    // A size the kind can say goes as one; another stays Imagen's own.
    if let Some(size) = size {
        input.size = Some(size);
        parameters.remove("aspectRatio");
        parameters.remove("imageSize");
    }
    Ok((!parameters.is_empty()).then_some(parameters))
}

/// The encoder of an answer: the images as `predictions`, whole.
fn response_encoder(head: &mut ResponseHead) -> Box<dyn Encoder<Output>> {
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Box::new(All::new(move |events: &[Output]| {
        serde_json::to_string(&response_of(events))
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))
    }))
}

/// A response of `events`: a prediction of each image. What a response has
/// no place for, such as usage, is dropped.
fn response_of(events: &[Output]) -> Value {
    let predictions: Vec<Value> = events
        .iter()
        .filter_map(|event| match event {
            Output::Image(image) => Some(prediction_of(&image.media)),
            _ => None,
        })
        .collect();
    json!({ "predictions": predictions })
}

/// A prediction of `media`: its bytes in base64, or its URL.
fn prediction_of(media: &Media) -> Value {
    match &media.source {
        MediaSource::Bytes(bytes) => {
            json!({ "mimeType": media.mime_type, "bytesBase64Encoded": STANDARD.encode(bytes) })
        }
        MediaSource::Url(url) => json!({ "mimeType": media.mime_type, "gcsUri": url }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;
    use respan_gateway_http::http;

    fn request(path: &str, body: Value) -> Result<Input, Fault> {
        let head = http::Request::post(path).body(()).unwrap_or_default();
        read_request(&head.into_parts().0, body.to_string().as_bytes())
    }

    fn png(data: &'static [u8]) -> Media {
        Media::new("image/png", MediaSource::Bytes(Bytes::from_static(data)))
    }

    #[test]
    fn a_clients_instance_is_the_prompt_and_its_parameters_a_count_and_size() {
        let body = json!({
            "instances": [{ "prompt": "a robot" }],
            "parameters": {
                "sampleCount": 2,
                "aspectRatio": "16:9",
                "imageSize": "2K",
                "personGeneration": "allow_adult",
            },
        });
        let input =
            request("/v1beta/models/imagen-4.0-generate-001:predict", body).expect("a request");
        assert_eq!(input.model.as_deref(), Some("imagen-4.0-generate-001"));
        assert_eq!(input.prompt, "a robot");
        assert_eq!(input.count, Some(2));
        assert_eq!(input.size.as_deref(), Some("2816x1536"));
        let native = Value::Object(input.native.fields::<Key>().clone());
        assert_eq!(
            native,
            json!({ "parameters": { "personGeneration": "allow_adult" } })
        );
        assert!(
            GeminiImagen::model(
                &"/v1beta/models/m:predictLongRunning"
                    .parse()
                    .unwrap_or_default()
            )
            .is_none()
        );
    }

    #[test]
    fn a_client_without_one_instance_of_a_prompt_is_refused() {
        let path = "/v1beta/models/m:predict";
        for body in [
            json!({}),
            json!({ "instances": [] }),
            json!({ "instances": [{ "prompt": "a" }, { "prompt": "b" }] }),
            json!({ "instances": [{ "prompt": "" }] }),
            json!({ "instances": [{ "prompt": "a", "image": { "bytesBase64Encoded": "YQ==" } }] }),
            json!({ "instances": [{ "prompt": "a" }], "parameters": [] }),
        ] {
            let refused = request(path, body.clone());
            assert!(refused.is_err_and(|f| f.is(FaultKind::Client)), "{body}");
        }
    }

    #[test]
    fn a_size_is_the_nearest_aspect_ratio_and_back() {
        for (size, aspect, image_size) in [
            ("1024x1024", "1:1", None),
            ("1280x960", "4:3", None),
            ("1024x768", "4:3", None),
            ("1280x896", "4:3", None),
            ("2560x1792", "4:3", Some("2K")),
            ("1024x1792", "9:16", None),
            ("1792x1024", "16:9", None),
            ("2048x2048", "1:1", Some("2K")),
        ] {
            assert_eq!(
                aspect_of(size).ok(),
                Some(Some((aspect, image_size))),
                "{size}"
            );
        }
        assert_eq!(aspect_of("auto").ok(), Some(None));
        // 3:2 is none of Imagen's, nor within 5% of one.
        for size in ["1536x1024", "1000x100"] {
            assert!(aspect_of(size).is_err(), "{size}");
        }
        assert_eq!(size_of("3:4", None).as_deref(), Some("896x1280"));
        assert_eq!(size_of("1:1", Some("2K")).as_deref(), Some("2048x2048"));
        assert_eq!(size_of("2:3", None), None);
        assert_eq!(size_of("1:1", Some("4K")), None);
        // Only a size, as Imagen draws a square without an aspect ratio.
        let mut input = Input::new("a");
        let rest = read_parameters(
            json!({ "imageSize": "2K" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            &mut input,
        );
        assert_eq!(rest.ok(), Some(None));
        assert_eq!(input.size.as_deref(), Some("2048x2048"));
    }

    #[test]
    fn a_call_is_one_instance_and_its_parameters() {
        let mut input = Input::new("a robot");
        input.model = Some("models/imagen-4.0-generate-001".into());
        input.count = Some(3);
        input.size = Some("1792x1024".into());
        input.native =
            Native::json::<Key>(json!({ "parameters": { "personGeneration": "dont_allow" } }));
        let mut head = http::Request::new(()).into_parts().0;
        let encoder = call(&mut head, &input).expect("a call");
        assert_eq!(head.uri.path(), "/models/imagen-4.0-generate-001:predict");
        let written = {
            let mut encoder = encoder;
            encoder.encode_all(&[input])
        };
        let written: Value =
            serde_json::from_slice(&written.unwrap_or_default()).unwrap_or_default();
        let want = json!({
            "instances": [{ "prompt": "a robot" }],
            "parameters": { "personGeneration": "dont_allow", "sampleCount": 3, "aspectRatio": "16:9" },
        });
        assert_eq!(written, want);
    }

    #[test]
    fn a_call_with_images_to_edit_is_refused() {
        let mut input = Input::new("make it blue");
        input.model = Some("imagen-4.0-generate-001".into());
        input.images = vec![png(b"a")];
        let mut head = http::Request::new(()).into_parts().0;
        let refused = call(&mut head, &input).err();
        assert!(refused.is_some_and(|f| f.is(FaultKind::Client)));
    }

    #[test]
    fn an_answers_images_are_its_predictions() {
        let answer = json!({ "predictions": [
            { "mimeType": "image/png", "bytesBase64Encoded": "YQ==" },
            { "mimeType": "image/jpeg", "gcsUri": "gs://b/i.jpg" },
        ]});
        let mut out = Vec::new();
        read_response(Bytes::from(answer.to_string()), &mut out).expect("an answer");
        let gcs = Media::new("image/jpeg", MediaSource::Url("gs://b/i.jpg".into()));
        let want = [
            Output::Start(Start::default()),
            Output::Image(Image::new(png(b"a"))),
            Output::Image(Image::new(gcs)),
        ];
        assert_eq!(out, want);
    }

    #[test]
    fn an_answer_without_an_image_says_why() {
        let answer =
            json!({ "predictions": [{ "raiFilteredReason": "blocked by the safety filter" }] });
        let mut out = Vec::new();
        let refused =
            read_response(Bytes::from(answer.to_string()), &mut out).expect_err("no image");
        assert!(refused.is(FaultKind::Client));
        assert!(
            refused.message().contains("safety filter"),
            "{}",
            refused.message()
        );
        let mut out = Vec::new();
        assert!(read_response(Bytes::from("{}"), &mut out).is_err());
    }

    #[test]
    fn a_response_is_the_images_as_predictions() {
        let events = [
            Output::Start(Start::default()),
            Output::Image(Image::new(png(b"a"))),
            Output::Image(Image::new(Media::new(
                "image/png",
                MediaSource::Url("https://a.example/i.png".into()),
            ))),
        ];
        let mut head = http::Response::new(()).into_parts().0;
        let mut encoder = response_encoder(&mut head);
        let body = encoder.encode_all(&events).unwrap_or_default();
        assert_eq!(head.headers[CONTENT_TYPE], "application/json");
        let want = json!({ "predictions": [
            { "mimeType": "image/png", "bytesBase64Encoded": "YQ==" },
            { "mimeType": "image/png", "gcsUri": "https://a.example/i.png" },
        ]});
        assert_eq!(serde_json::from_slice::<Value>(&body).ok(), Some(want));
    }
}
