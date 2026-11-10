#![warn(missing_docs)]
//! [`AvifToPng`]: AVIF images in a decision request become PNG before the
//! call, for a provider that reads PNG and JPEG but not AVIF, such as
//! span-02. Register it on that provider alone:
//!
//! ```ignore
//! let span_02 = Decisions.provider("span-02".into(), connect).plugin(AvifToPng::default());
//! ```
//!
//! An image is AVIF by its bytes (an `ftyp` box naming `avif`), whatever its
//! label. Read as images: a `data:` URL with a base64 body anywhere in the
//! request (its state, its questions, its fields the kind has no place for,
//! such as System One's `images`); a bare base64 item of a request's
//! `images`; and the `data_base64` of an object, whose `format` becomes
//! `png`. Each becomes the same with a PNG in its place. Anything else, an
//! image at a URL included, passes as sent.
//!
//! Decoding is in Rust (rav1d, the port of dav1d), off the async runtime.
//! A still image is read: one AV1 frame or a grid of them, with its alpha,
//! rotation and mirror, any bit depth and chroma layout, converted by the
//! matrix and range it states. An AVIF it cannot read (an image sequence,
//! a damaged file, one over the pixel limit) refuses the request as a bad
//! request saying why, rather than reaching the provider unreadable.

mod av1;
mod container;
mod image;

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use respan_gateway_core::{Fault, Plugin, ProviderContext, Result, async_trait};
use respan_gateway_decision::{Input, Question};
use serde_json::Value;

pub use image::avif_to_png;

/// The most pixels an image, and each of its frames, may have.
pub const MAX_PIXELS: u32 = 40_000_000;

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const PNG_DATA_URL: &str = "data:image/png;base64,";

/// Turns AVIF images of a decision call into PNG. See the [crate] docs.
#[derive(Debug, Clone, Copy)]
pub struct AvifToPng {
    max_pixels: u32,
}

impl Default for AvifToPng {
    fn default() -> Self {
        Self {
            max_pixels: MAX_PIXELS,
        }
    }
}

impl AvifToPng {
    /// Refuses an image, or a frame of one, of more than `max_pixels`.
    pub fn with_max_pixels(max_pixels: u32) -> Self {
        Self { max_pixels }
    }
}

#[async_trait]
impl Plugin for AvifToPng {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        let found = avif_strings(px.input::<Input>()?);
        if found.is_empty() {
            return Ok(());
        }
        let max_pixels = self.max_pixels;
        let converted = tokio::task::spawn_blocking(move || convert(found, max_pixels))
            .await
            .map_err(|e| Fault::internal(format!("AVIF conversion stopped: {e}")))??;
        let converted = Arc::new(converted);
        px.filter(move |input: &mut Input| {
            rewrite(input, &converted);
            Ok(())
        })
    }
}

/// How an image string carries its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carrier {
    /// `data:<type>;base64,<body>`.
    DataUrl,
    /// The base64 alone.
    Bare,
}

/// The string's base64 body, if it is a base64 `data:` URL.
fn data_url_body(s: &str) -> Option<&str> {
    let rest = s.strip_prefix("data:")?;
    let (head, body) = rest.split_once(',')?;
    head.rsplit(';')
        .next()
        .filter(|p| p.eq_ignore_ascii_case("base64"))?;
    Some(body)
}

/// Whether `body` decodes to an AVIF file, by its first bytes.
fn starts_avif(body: &str) -> bool {
    let body = body.trim_start();
    let head: String = body
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .take(128)
        .collect();
    if !head.is_ascii() {
        return false;
    }
    let head = &head[..head.len() - head.len() % 4];
    BASE64
        .decode(head)
        .is_ok_and(|bytes| container::is_avif(&bytes))
}

/// Calls `each` on every image string of `input`, with its carrier, and
/// lets it replace the string.
fn visit(input: &mut Input, each: &mut dyn FnMut(&mut String, Carrier) -> bool) {
    walk(&mut input.state, false, each);
    for Question {
        instructions,
        native,
        ..
    } in &mut input.questions
    {
        if let Some(v) = instructions {
            walk(v, false, each);
        }
        if let Some(n) = native {
            fields(&mut n.fields, each);
        }
    }
    if let Some(n) = &mut input.native {
        fields(&mut n.fields, each);
    }
}

fn fields(
    map: &mut serde_json::Map<String, Value>,
    each: &mut dyn FnMut(&mut String, Carrier) -> bool,
) {
    for (key, v) in map.iter_mut() {
        walk(v, key == "images", each);
    }
}

/// `images`: `v` is a request's `images`, whose items may be bare base64.
fn walk(v: &mut Value, images: bool, each: &mut dyn FnMut(&mut String, Carrier) -> bool) {
    match v {
        Value::String(s) => {
            if data_url_body(s).is_some() {
                each(s, Carrier::DataUrl);
            } else if images {
                each(s, Carrier::Bare);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, images, each);
            }
        }
        Value::Object(map) => {
            let mut bare = false;
            if let Some(Value::String(s)) = map.get_mut("data_base64") {
                bare = data_url_body(s).is_none() && each(s, Carrier::Bare);
            }
            if bare && map.contains_key("format") {
                map.insert("format".into(), Value::String("png".into()));
            }
            for (key, item) in map.iter_mut() {
                if key != "data_base64" {
                    walk(item, key == "images", each);
                }
            }
        }
        _ => {}
    }
}

/// Every distinct AVIF image string of `input`.
fn avif_strings(input: &Input) -> Vec<(String, Carrier)> {
    let mut input = input.clone();
    let mut found: Vec<(String, Carrier)> = Vec::new();
    visit(&mut input, &mut |s, carrier| {
        let body = match carrier {
            Carrier::DataUrl => data_url_body(s).unwrap_or_default(),
            Carrier::Bare => s.as_str(),
        };
        let is = starts_avif(body);
        if is && !found.iter().any(|(f, _)| f == s) {
            found.push((s.clone(), carrier));
        }
        is
    });
    found
}

/// Each AVIF string as its PNG string.
fn convert(found: Vec<(String, Carrier)>, max_pixels: u32) -> Result<HashMap<String, String>> {
    let mut out = HashMap::with_capacity(found.len());
    for (i, (s, carrier)) in found.into_iter().enumerate() {
        let body = match carrier {
            Carrier::DataUrl => data_url_body(&s).unwrap_or_default(),
            Carrier::Bare => s.as_str(),
        };
        let compact: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let refuse = |why: String| {
            Fault::bad_request(format!(
                "AVIF image {} could not be converted to PNG: {why}",
                i + 1
            ))
        };
        let bytes = BASE64
            .decode(compact)
            .map_err(|e| refuse(format!("its base64 is invalid ({e})")))?;
        let png = BASE64.encode(avif_to_png(&bytes, max_pixels).map_err(refuse)?);
        let png = match carrier {
            Carrier::DataUrl => format!("{PNG_DATA_URL}{png}"),
            Carrier::Bare => png,
        };
        out.insert(s, png);
    }
    Ok(out)
}

/// `input` with each converted string in place.
fn rewrite(input: &mut Input, converted: &HashMap<String, String>) {
    visit(input, &mut |s, _| match converted.get(s.as_str()) {
        Some(png) => {
            *s = png.clone();
            true
        }
        None => false,
    });
}

#[cfg(test)]
mod tests;
