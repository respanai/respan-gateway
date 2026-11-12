//! Typed media used in conversation content.

use bytes::Bytes;
use respan_gateway_core::Native;

/// Media content with its representation separated from its meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Media {
    /// MIME type, such as `image/png`, `audio/wav`, or `application/pdf`.
    pub mime_type: String,
    /// Where the media bytes are stored.
    pub source: MediaSource,
    /// Optional display name or filename.
    pub name: Option<String>,
    /// Provider-specific semantic fields, such as image detail or cache hints.
    pub native: Native,
}

impl Media {
    /// Media with no optional name or native fields.
    pub fn new(mime_type: impl Into<String>, source: MediaSource) -> Self {
        Self {
            mime_type: mime_type.into(),
            source,
            name: None,
            native: Native::default(),
        }
    }
}

/// The source of a media value. Wire encoders choose base64 or URL syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MediaSource {
    /// A remote resource.
    Url(String),
    /// Owned media bytes, cheaply shared when cloned.
    Bytes(Bytes),
}
