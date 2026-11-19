//! xAI: Chat Completions and images, as OpenAI serves them, images in its
//! dialect.

use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation_http::fields::{invalid, media_url};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_media::image;
use respan_gateway_media_http::Natives;
use respan_gateway_media_http::openai::OpenAiImages;

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.x.ai/v1";

/// xAI.
pub const VENDOR: Vendor = Vendor {
    id: "xai",
    key_env: &["XAI_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&Dialected::new(OpenAiImages, XaiImages), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// The aspect ratios xAI draws, as it names them, each a width to a height.
const ASPECT_RATIOS: [(&str, f64, f64); 15] = [
    ("1:1", 1.0, 1.0),
    ("3:4", 3.0, 4.0),
    ("4:3", 4.0, 3.0),
    ("9:16", 9.0, 16.0),
    ("16:9", 16.0, 9.0),
    ("2:3", 2.0, 3.0),
    ("3:2", 3.0, 2.0),
    ("9:19.5", 9.0, 19.5),
    ("19.5:9", 19.5, 9.0),
    ("9:20", 9.0, 20.0),
    ("20:9", 20.0, 9.0),
    ("1:2", 1.0, 2.0),
    ("2:1", 2.0, 1.0),
    ("21:9", 21.0, 9.0),
    ("5:2", 5.0, 2.0),
];

/// xAI's images, as they depart from OpenAI's: it takes no `size`, but an
/// `aspect_ratio` and a `resolution` (`1k`, or `2k` above 1024 pixels on the
/// long side), so a size is sent as those, the nearest ratio it draws within
/// 5%, else refused, 400; `auto` sends none. It takes an edit as JSON alone,
/// refusing a form, so an edit's images are sent as data URLs of their bytes.
#[derive(Debug, Clone, Copy)]
pub struct XaiImages;

impl Dialect for XaiImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(drawn))
    }
}

/// `input` as xAI takes it: its size as a ratio and a resolution, its
/// images as URLs.
fn drawn(input: &mut image::Input) -> Result<(), Fault> {
    for image in &mut input.images {
        if matches!(image.source, MediaSource::Bytes(_)) {
            image.source = MediaSource::Url(media_url(image));
        }
    }
    let Some(size) = input.size.take().filter(|s| s != "auto") else {
        return Ok(());
    };
    let (ratio, resolution) = sized(&size).ok_or_else(|| {
        let ratios = ASPECT_RATIOS.map(|(name, ..)| name).join(", ");
        invalid(
            "size",
            &format!("is not of an aspect ratio xAI draws: {ratios}"),
        )
    })?;
    input.native = OpenAiImages::with_fields(&input.native, |fields| {
        fields.entry("aspect_ratio").or_insert(ratio.into());
        fields.entry("resolution").or_insert(resolution.into());
    });
    Ok(())
}

/// The aspect ratio and resolution of a size `WxH`.
fn sized(size: &str) -> Option<(&'static str, &'static str)> {
    let (w, h) = size.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
    if w == 0 || h == 0 {
        return None;
    }
    let ratio = f64::from(w) / f64::from(h);
    let off = |&&(_, a, b): &&(&str, f64, f64)| (ratio / (a / b) - 1.0).abs();
    let (name, ..) = ASPECT_RATIOS
        .iter()
        .min_by(|x, y| off(x).total_cmp(&off(y)))
        .filter(|best| off(best) <= 0.05)?;
    let resolution = if w.max(h) > 1024 { "2k" } else { "1k" };
    Some((name, resolution))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Media;
    use respan_gateway_http::bytes::Bytes;
    use serde_json::{Value, json};

    #[test]
    fn a_size_is_an_aspect_ratio_and_a_resolution() {
        let mut input = image::Input::new("a fox");
        input.size = Some("1792x1024".into());
        drawn(&mut input).expect("taken");
        assert!(input.size.is_none());
        let fields = Value::Object(OpenAiImages::fields(&input.native).clone());
        assert_eq!(
            fields,
            json!({ "aspect_ratio": "16:9", "resolution": "2k" })
        );
        assert_eq!(sized("1024x1024"), Some(("1:1", "1k")));
        assert_eq!(sized("1080x2340"), Some(("9:19.5", "2k")));
        input.size = Some("1100x1000".into());
        assert!(drawn(&mut input).is_err(), "no ratio xAI draws");
    }

    #[test]
    fn an_edits_bytes_are_sent_as_data_urls() {
        let mut input = image::Input::new("make it green");
        input.images = vec![Media::new(
            "image/png",
            MediaSource::Bytes(Bytes::from_static(b"img")),
        )];
        drawn(&mut input).expect("taken");
        assert_eq!(
            input.images[0].source,
            MediaSource::Url("data:image/png;base64,aW1n".into())
        );
    }
}
