//! xAI: Chat Completions, images and Realtime, as OpenAI serves them,
//! images in its dialect; its Grok Voice models speak OpenAI Realtime at
//! `wss://api.x.ai/v1/realtime`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_media::image;
use respan_gateway_media_http::openai::OpenAiImages;
use respan_gateway_media_http::{Natives, named_aspect};
use respan_gateway_realtime_ws::OpenAiRealtime;

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
        (&OpenAiRealtime, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// The aspect ratios xAI draws, as it names them, each a width to a height.
const ASPECT_RATIOS: [(&str, u32, u32); 15] = [
    ("1:1", 1, 1),
    ("3:4", 3, 4),
    ("4:3", 4, 3),
    ("9:16", 9, 16),
    ("16:9", 16, 9),
    ("2:3", 2, 3),
    ("3:2", 3, 2),
    ("9:19.5", 18, 39),
    ("19.5:9", 39, 18),
    ("9:20", 9, 20),
    ("20:9", 20, 9),
    ("1:2", 1, 2),
    ("2:1", 2, 1),
    ("21:9", 21, 9),
    ("5:2", 5, 2),
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
    crate::as_urls(&mut input.images);
    let size = input.size.take();
    let Some((ratio, resolution)) = size.as_deref().map(sized).transpose()?.flatten() else {
        return Ok(());
    };
    input.native = OpenAiImages::with_fields(&input.native, |fields| {
        fields.entry("aspect_ratio").or_insert(ratio.into());
        fields.entry("resolution").or_insert(resolution.into());
    });
    Ok(())
}

/// The aspect ratio and resolution of a size `WxH`; none for `auto`.
fn sized(size: &str) -> Result<Option<(&'static str, &'static str)>, Fault> {
    let aspect = named_aspect(size, &ASPECT_RATIOS, "xAI")?;
    Ok(aspect.map(|((ratio, ..), long)| (ratio, if long > 1024 { "2k" } else { "1k" })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Media, MediaSource};
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
        assert_eq!(sized("1024x1024").expect("drawn"), Some(("1:1", "1k")));
        assert_eq!(sized("1080x2340").expect("drawn"), Some(("9:19.5", "2k")));
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
