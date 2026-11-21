//! The rerank kind: documents ordered by how relevant each is to a query.
//!
//! A response is a sequence of events: a [`Output::Start`], then a
//! [`Output::Ranked`] for each document the provider answers, most relevant
//! first, with [`Output::Usage`] and [`Output::Native`] among them.

use respan_gateway_conversation::Usage;
use respan_gateway_core::Native;

/// The kind's input: a query, and the documents to rank by it.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// What the documents are ranked by.
    pub query: String,
    /// The documents to rank, each answered by its index, from 0.
    pub documents: Vec<Document>,
    /// How many of the most relevant to answer; all if none.
    pub top_n: Option<u32>,
    /// The request's fields the kind has no place for, such as whether the
    /// answer repeats the documents or how long one may be.
    pub native: Native,
}

impl Input {
    /// A ranking of `documents` by `query`.
    pub fn new(query: impl Into<String>, documents: impl Into<Vec<Document>>) -> Self {
        Self {
            query: query.into(),
            documents: documents.into(),
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

/// A document to rank.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Document {
    /// A text.
    Text(String),
    /// A document the kind has no form for, such as an image or an object
    /// of fields; only a call in its format can send it.
    Native(Native),
}

/// The kind's output: one event of a response.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// Which response the events belong to; first.
    Start(Start),
    /// A document's place, in the order the provider gave them.
    Ranked(Ranked),
    /// Usage the provider reports, as an increment.
    Usage(Usage),
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
    /// The response's fields the kind has no place for, such as its id or
    /// the units a provider bills.
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

/// How relevant a document is to the query.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Ranked {
    /// Which document it is, its index in the request, from 0.
    pub index: u32,
    /// How relevant it is, as the model scores it: higher is more.
    pub score: f64,
    /// The document's text, if the provider repeats it.
    pub text: Option<String>,
    /// Its fields the kind has no place for.
    pub native: Native,
}

impl Ranked {
    /// Document `index`, scored `score`.
    pub fn new(index: u32, score: f64) -> Self {
        Self {
            index,
            score,
            text: None,
            native: Native::default(),
        }
    }
}
