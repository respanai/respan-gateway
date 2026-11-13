//! xAI: Chat Completions and images, as OpenAI serves them, images in its
//! dialect.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_media::image;
use respan_gateway_media_http::Natives as _;
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

/// xAI's images, as they depart from OpenAI's: an image is asked for as
/// `b64_json`, its bytes, unless the request says otherwise, as xAI
/// answers a URL of its storage by default, which an account that keeps
/// no data may not be answered.
#[derive(Debug, Clone, Copy)]
pub struct XaiImages;

impl Dialect for XaiImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(as_bytes))
    }
}

/// `input` asking for its images as bytes.
fn as_bytes(input: &mut image::Input) -> Result<(), Fault> {
    input.native = OpenAiImages::with_fields(&input.native, |fields| {
        fields.entry("response_format").or_insert("b64_json".into());
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_image_is_asked_for_as_bytes_unless_the_request_says() {
        let mut input = image::Input::new("a fox");
        as_bytes(&mut input).expect("asked");
        let fields = OpenAiImages::fields(&input.native);
        assert_eq!(fields["response_format"], json!("b64_json"));
    }
}
