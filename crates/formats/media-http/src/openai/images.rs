//! OpenAI Images: `POST /images/generations` (JSON) and `POST
//! /images/edits` (a form, or JSON naming its images by URL), answered
//! whole in JSON or, if asked, as a stream of events.

use super::multipart::{self, Writer};
use super::{
    Fields, Form, as_usage_extras, b64, bind_client, bytes_of, event_json, file_name, foreign,
    invalid, is_true, json_bytes, kept, merge_missing, mime_of, missing, native_of, post_to,
    read_usage, rest_of, sends, set, sniff, sse_event, streams, take_count, take_string, unb64,
    unix_now, unreadable, usage_extras, whole_json, write_field, write_usage,
};
use crate::dialect::{self, Spec};
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, NativeFields, NativeValue};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::content;
use respan_gateway_http::http::{self, Method, request::Parts as RequestHead};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Sse, Whole};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::image::{Image, Input, Output, Partial, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "openai-images";
const GENERATIONS: &str = "/images/generations";
const EDITS: &str = "/images/edits";

/// OpenAI Images, as OpenAI or a compatible vendor serves it: a `POST` to
/// a path ending in `/images/generations` or `/images/edits`.
///
/// An edit's images and mask are files of a form (`image`, or `image[]`
/// for several, and `mask`), or, in JSON, URLs (`images: [{"image_url"}]`);
/// an image a client names by file id cannot be relayed and is refused. A
/// call writes an edit, a request with images, as a form, or as JSON if one
/// is a URL, which a form cannot hold.
///
/// A whole response decodes into a [`Start`] of its `created` and its other
/// fields, an [`Image`] per item of `data` (its `b64_json` decoded, or its
/// `url`), and its usage; an image is of the answer's `output_format`, else
/// of the type its bytes say, else PNG. A stream decodes into a [`Start`] of its first
/// event's fields, a [`Partial`] per partial image and an [`Image`] per
/// completed one, with its usage; a stream that ends before its image fails
/// `truncated`. Either is written to a client in the framing it asked for;
/// a whole body has no partial images.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiImages;

impl OpenAiImages {
    /// The format's name, `openai-images`, which tags its native fields.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for OpenAiImages {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST
            && [GENERATIONS, EDITS]
                .iter()
                .any(|path| path_ends_with(request.uri(), path))
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(bind_client(io, decode_request, response_encoder))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiImages {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiImages {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(dialect::bind(*self, io))
    }
}

/// Its spec's parts, which a vendor's dialect runs around.
impl Spec for OpenAiImages {
    type Input = Input;
    type Output = Output;
    const NAME: &'static str = NAME;

    fn streams(&self, io: &ProviderIo) -> bool {
        streams(io, &[GENERATIONS, EDITS], |f| is_true(f.get("stream")))
    }

    fn post(
        &self,
        head: &mut RequestHead,
        input: &Input,
        stream: bool,
    ) -> Result<Box<dyn Encoder<Input>>, Fault> {
        post(head, input, stream)
    }

    fn read_response(
        &self,
        head: &http::response::Parts,
        _: &http::Request<Bytes>,
    ) -> Option<Box<dyn Decoder<Output>>> {
        read_response(head)
    }
}

/// A client's request, an edit if to `/images/edits`.
fn decode_request(head: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let edit = path_ends_with(&head.uri, EDITS);
    let Fields { mut map, files } = form.fields()?;
    let prompt = take_string(&mut map, "prompt")?.ok_or_else(|| missing("prompt"))?;
    let mut input = Input::new(prompt);
    input.model = take_string(&mut map, "model")?;
    input.count = take_count(&mut map, "n")?;
    input.size = take_string(&mut map, "size")?;
    input.quality = take_string(&mut map, "quality")?;
    input.output_format = take_string(&mut map, "output_format")?;
    input.user = take_string(&mut map, "user")?;
    map.remove("stream");
    for (name, media) in files {
        match name.as_str() {
            "image" if edit => input.images.push(media),
            "mask" if edit && input.mask.is_none() => input.mask = Some(media),
            _ => return Err(invalid(&name, "is not a file this endpoint takes")),
        }
    }
    if edit {
        if let Some(images) = map.remove("images") {
            let images = images
                .as_array()
                .ok_or_else(|| invalid("images", "must be a list"))?;
            for image in images {
                input.images.push(linked(image, "images")?);
            }
        }
        if let Some(mask) = map.remove("mask") {
            input.mask = Some(linked(&mask, "mask")?);
        }
        if input.images.is_empty() {
            return Err(missing("image"));
        }
    }
    input.native = native_of(NAME, map);
    Ok(input)
}

/// An image a JSON edit names by URL, `{"image_url": ..}`; a data URL is
/// its bytes.
fn linked(v: &Value, param: &str) -> Result<Media, Fault> {
    if v.get("file_id").is_some() {
        return Err(invalid(param, "names a file id, which cannot be relayed"));
    }
    let url = v["image_url"]
        .as_str()
        .ok_or_else(|| invalid(param, "must name an image_url"))?;
    if let Some((mime, bytes)) = data_url(url) {
        return Ok(Media::new(mime, MediaSource::Bytes(bytes)));
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let mime = mime_of(None, path, &[]);
    Ok(Media::new(mime, MediaSource::Url(url.to_owned())))
}

/// The type and bytes of a base64 data URL.
fn data_url(url: &str) -> Option<(String, Bytes)> {
    let (head, data) = url.strip_prefix("data:")?.split_once(',')?;
    let mime = head.strip_suffix(";base64")?;
    Some((mime.to_owned(), unb64(data)?))
}

/// `media` as a data URL.
fn to_data_url(media: &Media, bytes: &[u8]) -> String {
    format!("data:{};base64,{}", media.mime_type, b64(bytes))
}

/// The call of `input`: a generation in JSON; an edit as a form, or in
/// JSON if an image is a URL.
fn post(
    head: &mut RequestHead,
    input: &Input,
    stream: bool,
) -> Result<Box<dyn Encoder<Input>>, Fault> {
    if !input.is_edit() {
        if input.mask.is_some() {
            return Err(Fault::bad_request("a mask needs an image to edit"));
        }
        post_to(head, GENERATIONS, "application/json")?;
        return Ok(sends(json_bytes(&fields(input, stream)?)?));
    }
    let mut files = input.images.iter().chain(&input.mask);
    if !files.all(|media| bytes_of(media).is_some()) {
        post_to(head, EDITS, "application/json")?;
        return Ok(sends(json_bytes(&linked_edit(input, stream)?)?));
    }
    let mut form = Writer::with_boundary(multipart::random_boundary());
    post_to(head, EDITS, &form.content_type())?;
    for (name, value) in &fields(input, stream)? {
        write_field(&mut form, name, value);
    }
    let name = match input.images.len() {
        1 => "image",
        _ => "image[]",
    };
    let files = input.images.iter().map(|media| (name, media));
    for (name, media) in files.chain(input.mask.iter().map(|mask| ("mask", mask))) {
        let bytes = bytes_of(media).cloned().unwrap_or_default();
        form.file(name, &file_name(media, "image"), &media.mime_type, &bytes);
    }
    Ok(sends(form.finish()))
}

/// The fields of a call but its images: its typed fields, then its native
/// ones.
fn fields(input: &Input, stream: bool) -> Result<Map<String, Value>, Fault> {
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("prompt".into(), input.prompt.clone().into());
    set(&mut v, "n", input.count);
    set(&mut v, "size", input.size.clone());
    set(&mut v, "quality", input.quality.clone());
    set(&mut v, "output_format", input.output_format.clone());
    set(&mut v, "user", input.user.clone());
    merge_missing(&mut v, &kept(NAME, &input.native)?);
    if stream {
        v.insert("stream".into(), true.into());
    }
    Ok(v)
}

/// A JSON edit, its images named by URL.
fn linked_edit(input: &Input, stream: bool) -> Result<Map<String, Value>, Fault> {
    let link = |media: &Media| {
        let url = match &media.source {
            MediaSource::Url(url) => url.clone(),
            MediaSource::Bytes(bytes) => to_data_url(media, bytes),
        };
        json!({ "image_url": url })
    };
    let mut v = fields(input, stream)?;
    v.insert(
        "images".into(),
        input.images.iter().map(link).collect::<Vec<_>>().into(),
    );
    set(&mut v, "mask", input.mask.as_ref().map(link));
    Ok(v)
}

/// The decoder of a provider's answer of `head`: a stream of events, or one
/// JSON body.
fn read_response(head: &http::response::Parts) -> Option<Box<dyn Decoder<Output>>> {
    Some(match content::is_stream(head) {
        true => Box::new(Sse::new(Streamed::default())),
        false => Box::new(Whole::new(decode_whole)),
    })
}

impl Decoder<Output> for Streamed {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.event(&raw, out)
    }

    fn end(&mut self, end: Result<(), Fault>, _: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        Streamed::end(self)
    }
}

/// The events of a whole JSON response.
fn decode_whole(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = whole_json(NAME, raw, |v| v["data"].is_array())?;
    let format = v["output_format"].as_str();
    let mut start = Start::new(v["created"].as_u64());
    start.native = native_of(NAME, rest_of(&v, &["created", "data", "usage"]));
    out.push(Output::Start(start));
    for item in v["data"].as_array().into_iter().flatten() {
        let mut image = Image::new(media_of(item, format)?);
        image.revised_prompt = item["revised_prompt"].as_str().map(Into::into);
        image.native = native_of(NAME, rest_of(item, &["b64_json", "url", "revised_prompt"]));
        out.push(Output::Image(image));
    }
    read_usage_into(&v["usage"], out)
}

/// The events of a usage object, if `u` is one.
fn read_usage_into(u: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
    if u.is_object() {
        let (usage, extras) = read_usage(u, &[])?;
        out.push(Output::Usage(usage));
        out.extend(usage_extras(NAME, extras).map(Output::Native));
    }
    Ok(())
}

/// The image of an item or event `v`, of file format `format`; of none,
/// the type its bytes say, else PNG.
fn media_of(v: &Value, format: Option<&str>) -> Result<Media, Fault> {
    let typed = |format: &str| match format {
        "jpg" | "jpeg" => "image/jpeg".to_owned(),
        other => format!("image/{other}"),
    };
    let mime = |bytes: &[u8]| {
        let sniffed = || {
            sniff(bytes)
                .filter(|m| m.starts_with("image/"))
                .map(Into::into)
        };
        format
            .map(typed)
            .or_else(sniffed)
            .unwrap_or_else(|| typed("png"))
    };
    match (v["b64_json"].as_str(), v["url"].as_str()) {
        (Some(data), _) => {
            let bytes =
                unb64(data).ok_or_else(|| unreadable(NAME, "holds an image not in base64"))?;
            Ok(Media::new(mime(&bytes), MediaSource::Bytes(bytes)))
        }
        (None, Some(url)) => Ok(Media::new(mime(&[]), MediaSource::Url(url.to_owned()))),
        (None, None) => Err(unreadable(NAME, "holds an image with no b64_json or url")),
    }
}

/// The fields of a stream's image event that say the image itself.
const EVENT_FIELDS: &[&str] = &[
    "type",
    "b64_json",
    "url",
    "created_at",
    "partial_image_index",
    "revised_prompt",
    "usage",
];

/// Reads a stream: its first event's other fields are its [`Start`]'s, and
/// a later event keeps those it says otherwise.
#[derive(Debug, Default)]
struct Streamed {
    shared: Option<Map<String, Value>>,
    done: bool,
}

impl Streamed {
    fn event(&mut self, raw: &[u8], out: &mut Vec<Output>) -> Result<(), Fault> {
        let Some(v) = event_json(NAME, raw)? else {
            return Ok(());
        };
        let kind = v["type"].as_str().unwrap_or_default();
        let (_, step) = kind.split_once('.').unwrap_or_default();
        let ours = kind.starts_with("image_generation.") || kind.starts_with("image_edit.");
        if !ours || !matches!(step, "partial_image" | "completed") {
            out.push(Output::Native(NativeValue::new(NAME, v)));
            return Ok(());
        }
        let mut fields = rest_of(&v, EVENT_FIELDS);
        match &self.shared {
            None => {
                let mut start = Start::new(v["created_at"].as_u64());
                start.native = native_of(NAME, std::mem::take(&mut fields));
                self.shared = Some(
                    start
                        .native
                        .as_ref()
                        .map(|n| n.fields.clone())
                        .unwrap_or_default(),
                );
                out.push(Output::Start(start));
            }
            Some(shared) => fields.retain(|k, value| shared.get(k) != Some(value)),
        }
        let media = media_of(&v, v["output_format"].as_str())?;
        if step == "partial_image" {
            let index = v["partial_image_index"].as_u64().unwrap_or_default();
            let mut partial = Partial::new(u32::try_from(index).unwrap_or(u32::MAX), media);
            partial.native = native_of(NAME, fields);
            out.push(Output::Partial(partial));
            return Ok(());
        }
        let mut image = Image::new(media);
        image.revised_prompt = v["revised_prompt"].as_str().map(Into::into);
        image.native = native_of(NAME, fields);
        out.push(Output::Image(image));
        self.done = true;
        read_usage_into(&v["usage"], out)
    }

    fn end(&mut self) -> Result<(), Fault> {
        match self.done {
            true => Ok(()),
            false => Err(Fault::truncated()),
        }
    }
}

/// The encoder of a response to a client's `request`, in the framing it
/// asked for.
fn response_encoder(
    request: &http::Request<Bytes>,
    asked: &Map<String, Value>,
    head: &mut http::response::Parts,
) -> Box<dyn Encoder<Output>> {
    if !is_true(asked.get("stream")) {
        super::content_type(head, "application/json");
        return Box::new(All::new(encode_whole));
    }
    super::content_type(head, "text/event-stream");
    let prefix = match path_ends_with(request.uri(), EDITS) {
        true => "image_edit",
        false => "image_generation",
    };
    Box::new(StreamEncoder {
        prefix,
        created: None,
        shared: Map::new(),
        pending: None,
        usage: None,
        extras: Map::new(),
    })
}

/// An image's data item or event fields: its bytes, or its URL.
fn image_fields(media: &Media, native: &Option<NativeFields>) -> Result<Map<String, Value>, Fault> {
    let mut v = Map::new();
    match &media.source {
        MediaSource::Bytes(bytes) => v.insert("b64_json".into(), b64(bytes).into()),
        MediaSource::Url(url) => v.insert("url".into(), url.clone().into()),
    };
    merge_missing(&mut v, &kept(NAME, native)?);
    Ok(v)
}

/// A whole response of `events`.
fn encode_whole(events: &[Output]) -> Result<Bytes, Fault> {
    let (mut start, mut data, mut usage, mut extras) = (None, Vec::new(), None, Map::new());
    for event in events {
        match event {
            Output::Start(s) => start = Some(s),
            Output::Partial(_) => {}
            Output::Image(image) => {
                let mut item = image_fields(&image.media, &image.native)?;
                set(&mut item, "revised_prompt", image.revised_prompt.clone());
                data.push(Value::Object(item));
            }
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            // A body has no place for another event.
            Output::Native(n) => {
                if let Some(more) = as_usage_extras(&n.value) {
                    merge_missing(&mut extras, more);
                }
            }
            _ => {}
        }
    }
    let mut body = Map::new();
    let created = start.and_then(|s| s.created).unwrap_or_else(unix_now);
    body.insert("created".into(), created.into());
    body.insert("data".into(), data.into());
    if let Some(usage) = usage {
        let mut u = write_usage(&usage);
        merge_missing(&mut u, &extras);
        body.insert("usage".into(), u.into());
    }
    if let Some(start) = start {
        merge_missing(&mut body, &kept(NAME, &start.native)?);
    }
    json_bytes(&body)
}

/// Writes a stream of events in order: a partial image at once, and a
/// completed one when the next event that is not its usage comes or the
/// stream ends, with the usage reported until then.
struct StreamEncoder {
    prefix: &'static str,
    created: Option<u64>,
    shared: Map<String, Value>,
    pending: Option<Map<String, Value>>,
    usage: Option<Usage>,
    extras: Map<String, Value>,
}

impl StreamEncoder {
    fn event(
        &mut self,
        step: &str,
        media: &Media,
        native: &Option<NativeFields>,
    ) -> Result<Map<String, Value>, Fault> {
        let mut v = Map::new();
        v.insert("type".into(), format!("{}.{step}", self.prefix).into());
        merge_missing(&mut v, &image_fields(media, native)?);
        let created = *self.created.get_or_insert_with(unix_now);
        v.insert("created_at".into(), created.into());
        merge_missing(&mut v, &self.shared);
        Ok(v)
    }

    fn flush(&mut self, out: &mut BytesMut) {
        let Some(mut v) = self.pending.take() else {
            return;
        };
        if let Some(usage) = self.usage.take() {
            let mut u = write_usage(&usage);
            merge_missing(&mut u, &std::mem::take(&mut self.extras));
            v.insert("usage".into(), u.into());
        }
        out.extend_from_slice(sse_event(&Value::Object(v)).as_bytes());
    }
}

impl StreamEncoder {
    fn encode_into(&mut self, event: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        match event {
            Output::Start(start) => {
                self.created = start.created.or(self.created);
                self.shared = kept(NAME, &start.native)?;
            }
            Output::Partial(partial) => {
                self.flush(out);
                let mut v = self.event("partial_image", &partial.media, &partial.native)?;
                v.insert("partial_image_index".into(), partial.index.into());
                out.extend_from_slice(sse_event(&Value::Object(v)).as_bytes());
            }
            Output::Image(image) => {
                self.flush(out);
                let mut v = self.event("completed", &image.media, &image.native)?;
                set(&mut v, "revised_prompt", image.revised_prompt.clone());
                self.pending = Some(v);
            }
            Output::Usage(u) => *self.usage.get_or_insert_with(Usage::default) += u,
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            Output::Native(n) => match as_usage_extras(&n.value) {
                Some(more) => merge_missing(&mut self.extras, more),
                None => {
                    self.flush(out);
                    out.extend_from_slice(sse_event(&n.value).as_bytes());
                }
            },
            _ => {}
        }
        Ok(())
    }

    fn end_into(&mut self, end: &Result<(), Fault>, out: &mut BytesMut) -> Result<(), Fault> {
        self.flush(out);
        end.clone()
    }
}

impl Encoder<Output> for StreamEncoder {
    fn encode(&mut self, event: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::edge::frame(frames, |out| self.encode_into(event, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::edge::frame(frames, |out| self.end_into(&end, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    fn edit(body: Value) -> Result<Input, Fault> {
        let head = http::Request::post(EDITS).body(()).unwrap_or_default();
        let (head, raw) = (head.into_parts().0, Bytes::from(body.to_string()));
        let form = Form {
            headers: &head.headers,
            raw: &raw,
            original: None,
        };
        decode_request(&head, &form)
    }

    #[test]
    fn an_image_of_no_output_format_is_of_the_type_its_bytes_say() {
        let mut out = Vec::new();
        let webp = r#"{"created": 1, "data": [{"b64_json": "UklGRvoKAABXRUJQVlA4"}]}"#;
        decode_whole(Bytes::from_static(webp.as_bytes()), &mut out).expect("read");
        let typed = |out: &[Output]| match out.get(1) {
            Some(Output::Image(image)) => image.media.mime_type.clone(),
            _ => String::new(),
        };
        assert_eq!(typed(&out), "image/webp");
        let said = r#"{"created": 1, "output_format": "jpeg", "data": [{"b64_json": "UklGRvoKAABXRUJQVlA4"}]}"#;
        out.clear();
        decode_whole(Bytes::from_static(said.as_bytes()), &mut out).expect("read");
        assert_eq!(typed(&out), "image/jpeg", "as the answer says");
    }

    #[test]
    fn a_json_edit_names_its_images_by_url_and_is_written_so() {
        let body = json!({
            "prompt": "p",
            "images": [
                {"image_url": "data:image/png;base64,iVBO"},
                {"image_url": "https://x.test/a.webp?s=1"},
            ],
            "mask": {"image_url": "data:image/png;base64,bWFzaw=="},
        });
        let input = edit(body.clone()).expect("an edit");
        let first = MediaSource::Bytes(Bytes::from_static(b"\x89PN"));
        assert_eq!(input.images[0].source, first);
        assert_eq!(input.images[1].mime_type, "image/webp");
        assert_eq!(input.native, None);
        let mut head = http::Request::new(()).into_parts().0;
        let encoder = post(&mut head, &input, false).expect("a call");
        assert_eq!(head.uri.path(), EDITS);
        assert_eq!(head.headers[http::header::CONTENT_TYPE], "application/json");
        let written = {
            let mut encoder = encoder;
            encoder.encode_all(&[input])
        }
        .expect("a body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(written, body);
    }

    #[test]
    fn a_stream_of_two_images_is_written_in_order() {
        let image = |data: &'static [u8]| {
            let media = Media::new("image/png", MediaSource::Bytes(Bytes::from_static(data)));
            (Image::new(media.clone()), Partial::new(0, media))
        };
        let ((a, partial_a), (b, partial_b)) = (image(b"a"), image(b"b"));
        let events = [
            Output::Start(Start::new(Some(1))),
            Output::Partial(partial_a),
            Output::Image(a),
            Output::Usage(Usage::new(1, 2)),
            Output::Partial(partial_b),
            Output::Image(b),
            Output::Usage(Usage::new(3, 4)),
        ];
        let encoder = Box::new(StreamEncoder {
            prefix: "image_generation",
            created: None,
            shared: Map::new(),
            pending: None,
            usage: None,
            extras: Map::new(),
        });
        let written = {
            let mut encoder: Box<dyn Encoder<Output>> = encoder;
            encoder.encode_all(&events)
        }
        .expect("a stream");
        let written = String::from_utf8_lossy(&written);
        let seen: Vec<(String, u64)> = written
            .split("\n\n")
            .filter_map(|e| serde_json::from_str::<Value>(e.strip_prefix("data: ")?).ok())
            .map(|v| {
                let kind = v["type"].as_str().unwrap_or_default().to_owned();
                (kind, v["usage"]["input_tokens"].as_u64().unwrap_or(0))
            })
            .collect();
        let want = [
            ("image_generation.partial_image", 0),
            ("image_generation.completed", 1),
            ("image_generation.partial_image", 0),
            ("image_generation.completed", 3),
        ];
        assert_eq!(seen, want.map(|(k, n)| (k.to_owned(), n)));
    }

    #[test]
    fn an_image_named_by_file_id_is_refused() {
        let refused = edit(json!({"prompt": "p", "images": [{"file_id": "file-1"}]}));
        assert!(refused.is_err_and(|f| f.is(FaultKind::Client)));
    }
}
