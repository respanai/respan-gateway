//! The speech kind: text to speech.
//!
//! A response is a sequence of events, the same for raw audio and a stream
//! of events: the audio's bytes in [`Output::Audio`] chunks, in order, with
//! [`Output::Usage`] and [`Output::Native`] among them.

use bytes::Bytes;
use respan_gateway_conversation::Usage;
use respan_gateway_core::Native;

/// The kind's input: text to speak.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// The text to speak.
    pub text: String,
    /// The voice, as the provider names it.
    pub voice: Option<String>,
    /// How to speak it, such as a tone.
    pub instructions: Option<String>,
    /// How fast, 1 being the voice's own pace.
    pub speed: Option<f64>,
    /// The audio's file format, as the provider names it, such as `mp3`.
    pub audio_format: Option<String>,
    /// The request's fields the kind has no place for.
    pub native: Native,
}

impl Input {
    /// Speech of `text`.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
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

/// The kind's output: one event of a response, streamed or not.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// The next bytes of the audio.
    Audio(Bytes),
    /// Usage the provider reports, as an increment.
    Usage(Usage),
    /// An event the kind has no form for, as its format has it. Sent only in
    /// that format.
    Native(Native),
}
