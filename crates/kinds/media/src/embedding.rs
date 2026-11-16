//! The embedding kind: vectors for texts, images and videos.
//!
//! A response is a sequence of events: a [`Output::Start`], then an
//! [`Output::Embedding`] for each item of the request, in order (as many as
//! the model answers for an [`Item::Native`]), with
//! [`Output::Usage`], [`Output::InputTokens`] and [`Output::Native`] among
//! them.

use respan_gateway_conversation::{Media, Usage};
use respan_gateway_core::Native;

/// The kind's input: items to embed.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// What to embed, one vector each.
    pub items: Vec<Item>,
    /// How many numbers each vector holds, if the model can shorten them.
    pub dimensions: Option<u32>,
    /// The request's fields the kind has no place for, such as how the
    /// vectors are encoded or the end user's id.
    pub native: Native,
}

impl Input {
    /// An embedding of each of `items`.
    pub fn new(items: impl Into<Vec<Item>>) -> Self {
        Self {
            items: items.into(),
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
}

/// An item to embed.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Item {
    /// A text.
    Text(String),
    /// Parts (texts, images, audio, videos, files) embedded together as one
    /// vector.
    Parts(Parts),
    /// An item the kind has no form for, such as token ids; only a call in
    /// its format can send it.
    Native(Native),
}

/// The parts of an [`Item::Parts`].
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Parts {
    /// The parts, in order.
    pub parts: Vec<Part>,
    /// The item as its format wrote it, to write back as it was.
    pub native: Native,
}

impl Parts {
    /// Parts with no native form.
    pub fn new(parts: impl Into<Vec<Part>>) -> Self {
        Self {
            parts: parts.into(),
            native: Native::default(),
        }
    }
}

/// A part of an [`Item::Parts`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Part {
    /// A text.
    Text(String),
    /// Media: its type, and its bytes or its URL.
    Media(Media),
}

/// What a count of input tokens is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Modality {
    /// Text.
    Text,
    /// Images.
    Image,
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// Documents, such as PDFs.
    Document,
}

/// The kind's output: one event of a response.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// Which response the events belong to; first.
    Start(Start),
    /// The vector of an item, in the order the provider gave them.
    Embedding(Embedding),
    /// Usage the provider reports, as an increment.
    Usage(Usage),
    /// Of the usage's input tokens, those of a modality, as an increment.
    InputTokens(Modality, u64),
    /// An event the kind has no form for, as its format has it. Sent only in
    /// that format.
    Native(Native),
}

/// Which response the events belong to.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Start {
    /// The model that made it, as the provider names it.
    pub model: Option<String>,
    /// The response's fields the kind has no place for.
    pub native: Native,
}

impl Start {
    /// The start of a response of `model`.
    pub fn new(model: Option<String>) -> Self {
        Self {
            model,
            native: Native::default(),
        }
    }
}

/// The vector of an item.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Embedding {
    /// Which item it is of, from 0.
    pub index: u32,
    /// The vector.
    pub vector: Vec<f32>,
    /// Its fields the kind has no place for.
    pub native: Native,
}

impl Embedding {
    /// The vector of item `index`.
    pub fn new(index: u32, vector: Vec<f32>) -> Self {
        Self {
            index,
            vector,
            native: Native::default(),
        }
    }
}
