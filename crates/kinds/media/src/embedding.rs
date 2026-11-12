//! The embedding kind: vectors for texts, images and videos.
//!
//! A response is a sequence of events: a [`Output::Start`], then an
//! [`Output::Embedding`] for each item of the request, in order (as many as
//! the model answers for an [`Item::Native`]), with
//! [`Output::Usage`] and [`Output::Native`] among them.

use respan_gateway_conversation::Usage;
use respan_gateway_core::{FormatId, NativeFields, NativeValue};
use serde_json::Value;

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
    pub native: Option<NativeFields>,
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
    /// An item the kind has no form for, such as token ids or parts (texts,
    /// images, videos) the model answers one vector for, or one each; only a
    /// call in its format can send it.
    Native(NativeValue),
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
    /// The model that made it, as the provider names it.
    pub model: Option<String>,
    /// The response's fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Start {
    /// The start of a response of `model`.
    pub fn new(model: Option<String>) -> Self {
        Self {
            model,
            native: None,
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
    pub native: Option<NativeFields>,
}

impl Embedding {
    /// The vector of item `index`.
    pub fn new(index: u32, vector: Vec<f32>) -> Self {
        Self {
            index,
            vector,
            native: None,
        }
    }
}
