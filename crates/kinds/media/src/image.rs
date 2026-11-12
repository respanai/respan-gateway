//! The image kind: images generated from a prompt, or edits of source
//! images. A request with source images is an edit.
//!
//! A response is a sequence of events, the same for a stream and a whole
//! body: a [`Output::Start`], then an [`Output::Image`] for each image, with
//! [`Output::Usage`] and [`Output::Native`] among them.

use respan_gateway_conversation::{Media, Usage};
use respan_gateway_core::{FormatId, NativeFields, NativeValue};
use serde_json::Value;

/// The kind's input: a request for images.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any; without one, the provider's default.
    pub model: Option<String>,
    /// What to draw, or how to change the source images.
    pub prompt: String,
    /// The source images of an edit; none for a generation.
    pub images: Vec<Media>,
    /// Where an edit may change its first image: the transparent areas.
    pub mask: Option<Media>,
    /// How many images, if not one.
    pub count: Option<u32>,
    /// Their size, as the provider names it, such as `1024x1024`.
    pub size: Option<String>,
    /// The request's fields the kind has no place for, such as a quality.
    pub native: Option<NativeFields>,
}

impl Input {
    /// A generation of `prompt` alone.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Self::default()
        }
    }

    /// The model asked for, if any.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Asks for `model`.
    pub fn set_model(&mut self, model: String) {
        self.model = Some(model);
    }

    /// Whether it is an edit: it has source images.
    pub fn is_edit(&self) -> bool {
        !self.images.is_empty()
    }
}

/// The kind's output: one event of a response, streamed or not.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// The response begins.
    Start(Start),
    /// An image, in the order the provider gave them.
    Image(Image),
    /// Usage the provider reports, as an increment.
    Usage(Usage),
    /// An event the kind has no form for, as `format` has it. Sent only in
    /// that format.
    Native(NativeValue),
}

impl Output {
    /// An event of `format` the kind has no form for.
    pub fn native(format: impl Into<FormatId>, raw: impl Into<Value>) -> Self {
        Output::Native(NativeValue::new(format, raw))
    }
}

/// Which response the events belong to.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Start {
    /// The response's fields the kind has no place for, such as when it was
    /// made, or the size or background the provider chose.
    pub native: Option<NativeFields>,
}

/// An image a provider made.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Image {
    /// The image: its bytes, or a URL the provider keeps it at.
    pub media: Media,
    /// Its fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Image {
    /// The image `media`.
    pub fn new(media: Media) -> Self {
        Self {
            media,
            native: None,
        }
    }
}
