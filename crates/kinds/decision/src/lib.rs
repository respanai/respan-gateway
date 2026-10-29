#![warn(missing_docs)]
//! The decision kind: typed questions about a state, each answered with
//! probabilities rather than prose, as TypeSafe's System One answers them.
//!
//! A request is an [`Input`]: a state and named [`Question`]s, each a
//! yes-or-no, a choice of options, or a score on levels. A response is a
//! sequence of events: a [`Output::Start`], then an [`Output::Answer`] for
//! each question, in order, with [`Output::Usage`] and [`Output::Native`]
//! among them.
//!
//! What the kind has no form for stays as the format that decoded it has it
//! ([`NativeFields`], [`NativeValue`]), and only that format writes it. The
//! crate is the kind's types alone, with no I/O; its formats over HTTP are
//! `respan-gateway-decision-http`'s.

use respan_gateway_core::{FormatId, NativeFields, NativeValue};
use serde_json::Value;

pub use respan_gateway_conversation::Usage;

/// The kind's input: questions about a state.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// What the questions are about: a text, or JSON.
    pub state: Value,
    /// The questions, in order, each answered by its id.
    pub questions: Vec<Question>,
    /// The request's fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Input {
    /// `questions` about `state`.
    pub fn new(state: impl Into<Value>, questions: impl Into<Vec<Question>>) -> Self {
        Self {
            state: state.into(),
            questions: questions.into(),
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

/// A question about the state.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Question {
    /// Its id, which its answer names; chosen by the client.
    pub id: String,
    /// What to ask, as text or JSON, if more than its type says.
    pub instructions: Option<Value>,
    /// What it asks for.
    pub ask: Ask,
    /// Its fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Question {
    /// Question `id`, asking `ask`.
    pub fn new(id: impl Into<String>, ask: Ask) -> Self {
        Self {
            id: id.into(),
            instructions: None,
            ask,
            native: None,
        }
    }
}

/// A typed choice value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceValue {
    /// A string value.
    String(String),
    /// A boolean value.
    Boolean(bool),
}

/// What a question asks for.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Ask {
    /// Yes or no: the probability of yes. Each side may say what it means.
    Noul {
        /// What yes means.
        yes: Option<Value>,
        /// What no means.
        no: Option<Value>,
    },
    /// One of `options`, each named, in order, with what it means if more
    /// than its name says.
    Choice {
        /// The options.
        options: Vec<(ChoiceValue, Option<Value>)>,
    },
    /// A level of `levels`, lowest first, each a text or JSON.
    Score {
        /// The levels.
        levels: Vec<Value>,
    },
    /// A number, such as a count or an amount.
    Number,
    /// A question of a type the kind has no form for, whole, as `format`
    /// has it. Asked only in that format.
    Native(NativeValue),
}

/// The kind's output: one event of a response.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// Which response the events belong to; first.
    Start(Start),
    /// The answer to a question.
    Answer(Answer),
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
    /// The model that answered, as the provider names it.
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

/// The answer to a question.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Answer {
    /// The id of the question it answers.
    pub id: String,
    /// What it says.
    pub verdict: Verdict,
    /// How sure the model is, from 0 to 1, if it says.
    pub confidence: Option<f64>,
    /// Its fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Answer {
    /// The answer to question `id`.
    pub fn new(id: impl Into<String>, verdict: Verdict) -> Self {
        Self {
            id: id.into(),
            verdict,
            confidence: None,
            native: None,
        }
    }
}

/// What an answer says, by its question's [`Ask`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Verdict {
    /// The probability of yes.
    Noul(f64),
    /// The option chosen, and each option's probability, in order.
    Choice {
        /// The option chosen.
        choice: ChoiceValue,
        /// Each option's probability.
        probabilities: Vec<(ChoiceValue, f64)>,
    },
    /// The provider declined to answer.
    Refusal,
    /// The expected level, from 0 for the lowest, and each level's
    /// probability, lowest first.
    Score {
        /// The probability-weighted level.
        score: f64,
        /// Each level's probability.
        probabilities: Vec<f64>,
    },
    /// The number, and the text it was read from, if the provider says.
    Number {
        /// The number.
        number: f64,
        /// The text it was read from.
        text: Option<String>,
    },
}
