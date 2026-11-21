//! The transcription kind: speech to text, in the language spoken or
//! translated to English.
//!
//! A response is a sequence of events, the same for a stream and a whole
//! body: a stream's [`Output::Delta`]s, then the [`Output::Transcript`]; or,
//! for a transcript the provider rendered as text (plain text, SubRip or
//! WebVTT), an [`Output::Rendered`]. [`Output::Usage`], [`Output::Seconds`]
//! and [`Output::Native`] come among them.

use respan_gateway_conversation::{Media, Usage};
use respan_gateway_core::Native;

/// What a request asks of the audio.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Task {
    /// The text spoken, in its language.
    #[default]
    Transcribe,
    /// The text spoken, translated to English.
    Translate,
}

/// The kind's input: audio to transcribe.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// The audio.
    pub audio: Media,
    /// Whether to transcribe or translate it.
    pub task: Task,
    /// The language spoken, as ISO-639-1, if the client knows it.
    pub language: Option<String>,
    /// Text to guide the model, such as the words before the audio.
    pub prompt: Option<String>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// The request's fields the kind has no place for, such as how the
    /// transcript is rendered.
    pub native: Native,
}

impl Input {
    /// A transcription of `audio`.
    pub fn new(audio: Media) -> Self {
        Self {
            model: None,
            audio,
            task: Task::Transcribe,
            language: None,
            prompt: None,
            temperature: None,
            native: Native::default(),
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

/// The kind's output: one event of a response, streamed or not.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// The next text of a streamed transcript.
    Delta(Delta),
    /// The whole transcript.
    Transcript(Transcript),
    /// The transcript as the provider rendered it in a text format, such as
    /// SubRip, kept as it came.
    Rendered(String),
    /// Token usage the provider reports, as an increment.
    Usage(Usage),
    /// Seconds of audio the provider bills, as an increment.
    Seconds(f64),
    /// An event the kind has no form for, as its format has it. Sent only in
    /// that format.
    Native(Native),
}

impl Output {
    /// The next `text` of a streamed transcript.
    pub fn delta(text: impl Into<String>) -> Self {
        Output::Delta(Delta {
            text: text.into(),
            native: Native::default(),
        })
    }
}

/// The next text of a streamed transcript.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Delta {
    /// The text.
    pub text: String,
    /// Its fields the kind has no place for, such as log probabilities.
    pub native: Native,
}

impl Delta {
    /// The next `text`.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            native: Native::default(),
        }
    }
}

/// A whole transcript.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Transcript {
    /// All of its text.
    pub text: String,
    /// The language spoken, as the provider names it.
    pub language: Option<String>,
    /// The audio's length, in seconds.
    pub duration: Option<f64>,
    /// Its text in timed segments, if the client asked for them.
    pub segments: Vec<Segment>,
    /// Its words, timed, if the client asked for them.
    pub words: Vec<Word>,
    /// Its fields the kind has no place for, such as log probabilities.
    pub native: Native,
}

impl Transcript {
    /// A transcript of `text` alone.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

/// A timed segment of a transcript.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Segment {
    /// Where it starts, in seconds.
    pub start: f64,
    /// Where it ends, in seconds.
    pub end: f64,
    /// Its text.
    pub text: String,
    /// Who spoke it, if the provider told speakers apart.
    pub speaker: Option<String>,
    /// Its fields the kind has no place for, such as its tokens.
    pub native: Native,
}

impl Segment {
    /// The segment of `text` from `start` to `end`.
    pub fn new(start: f64, end: f64, text: impl Into<String>) -> Self {
        Self {
            start,
            end,
            text: text.into(),
            ..Self::default()
        }
    }
}

/// A timed word of a transcript.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Word {
    /// The word.
    pub word: String,
    /// Where it starts, in seconds.
    pub start: f64,
    /// Where it ends, in seconds.
    pub end: f64,
    /// Its fields the kind has no place for.
    pub native: Native,
}

impl Word {
    /// The `word` from `start` to `end`.
    pub fn new(word: impl Into<String>, start: f64, end: f64) -> Self {
        Self {
            word: word.into(),
            start,
            end,
            native: Native::default(),
        }
    }
}
