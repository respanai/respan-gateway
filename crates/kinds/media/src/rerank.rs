//! The rerank kind: documents scored by their relevance to a query.
//!
//! A response is a sequence of events: a [`Output::Start`], then a
//! [`Output::Result`] for each document the provider ranks, best first (as
//! many as the request's `top_n` asks, else all), with [`Output::Usage`],
//! [`Output::Searches`] and [`Output::Native`] among them.

use respan_gateway_conversation::Usage;
use respan_gateway_core::{FormatId, NativeFields, NativeValue};
use serde_json::Value;

/// The kind's input: a query, and the documents to rank against it.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// What the documents are ranked against.
    pub query: String,
    /// The texts to rank; a result names one by its place here, from 0.
    pub documents: Vec<String>,
    /// How many of the best to answer, if not all.
    pub top_n: Option<u32>,
    /// Whether each result carries its document's text.
    pub return_documents: Option<bool>,
    /// The request's fields the kind has no place for, such as how long a
    /// document may be before the model cuts it.
    pub native: Option<NativeFields>,
}

impl Input {
    /// A ranking of `documents` against `query`.
    pub fn new(query: impl Into<String>, documents: impl Into<Vec<String>>) -> Self {
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

/// The kind's output: one event of a response.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// Which response the events belong to; first.
    Start(Start),
    /// The score of a document, in the order the provider gave them.
    Result(Ranked),
    /// Token usage the provider reports, as an increment.
    Usage(Usage),
    /// Search units a provider bills, as an increment: what Cohere counts
    /// a query against up to a hundred documents as. How many a call is
    /// worth is the provider's, and what a unit costs the application's.
    Searches(u64),
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
    /// The response's id, if the provider gives one.
    pub id: Option<String>,
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
            ..Self::default()
        }
    }
}

/// The score of a document.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Ranked {
    /// Which document it is of, from 0.
    pub index: u32,
    /// How relevant the document is to the query, higher more so.
    pub relevance_score: f64,
    /// The document's text, if the provider sends it back.
    pub document: Option<String>,
    /// Its fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Ranked {
    /// The score of document `index`.
    pub fn new(index: u32, relevance_score: f64) -> Self {
        Self {
            index,
            relevance_score,
            document: None,
            native: None,
        }
    }
}
