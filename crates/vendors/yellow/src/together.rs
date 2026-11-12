//! Together: Chat Completions, images and embeddings, as OpenAI serves them,
//! images in its dialect.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_media::image;
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiImages};

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.together.xyz/v1";

/// Together.
pub const VENDOR: Vendor = Vendor {
    id: "together",
    key_env: &["TOGETHER_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiImages, TogetherImages),
            Some(BASE_URL),
        ),
        (&OpenAiEmbeddings, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Together's images, as they depart from OpenAI's: an image's size is its
/// `width` and `height` in pixels, not a `size`. A `size` of a width by a
/// height, such as `1024x768`, is sent as the two; any other is sent as it
/// is. A request's own `width` or `height` is kept.
#[derive(Debug, Clone, Copy)]
pub struct TogetherImages;

impl Dialect for TogetherImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(sized))
    }
}

/// `input` with a `size` of a width by a height as its `width` and `height`.
fn sized(input: &mut image::Input) -> Result<(), Fault> {
    let Some((width, height)) = input.size.as_deref().and_then(sides) else {
        return Ok(());
    };
    let fields = OpenAiImages::fields_mut(&mut input.native);
    fields.entry("width").or_insert(width.into());
    fields.entry("height").or_insert(height.into());
    input.size = None;
    Ok(())
}

/// The width and height a `size` such as `1024x768` names, in pixels.
fn sides(size: &str) -> Option<(u32, u32)> {
    let (width, height) = size.split_once('x')?;
    let side = |digits: &str| digits.parse().ok().filter(|side| *side > 0);
    Some((side(width)?, side(height)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_size_of_a_width_by_a_height_is_sent_as_the_two() {
        let mut input = image::Input::new("a fox");
        input.size = Some("1024x768".into());
        sized(&mut input).expect("sized");
        assert_eq!(input.size, None);
        let fields = OpenAiImages::fields(&input.native);
        assert_eq!(fields["width"], json!(1024));
        assert_eq!(fields["height"], json!(768));
    }

    #[test]
    fn a_requests_own_width_and_height_are_kept() {
        let mut input = image::Input::new("a fox");
        input.size = Some("1024x768".into());
        input.native = OpenAiImages::native(json!({ "width": 512, "height": 512 }));
        sized(&mut input).expect("sized");
        let fields = OpenAiImages::fields(&input.native);
        assert_eq!(fields["width"], json!(512));
        assert_eq!(fields["height"], json!(512));
    }

    #[test]
    fn any_other_size_is_sent_as_it_is() {
        for size in ["auto", "1024", "0x768", "1024x"] {
            let mut input = image::Input::new("a fox");
            input.size = Some(size.into());
            sized(&mut input).expect("sized");
            assert_eq!(input.size.as_deref(), Some(size));
            assert!(OpenAiImages::fields(&input.native).is_empty());
        }
    }
}
