#![warn(missing_docs)]
//! The realtime kind: a live session with a model, in both directions at once.
//! Formats translate through these types, and plugins read any session
//! through them: `cx.filter(|i: &mut Input| ..)` for what the client says,
//! `cx.filter(|o: &mut Output| ..)` for what the model says, one event each.
//!
//! A session is configured ([`Input::Session`], answered by
//! [`Output::Updated`]), then fed audio ([`Input::Audio`]) or messages
//! ([`Input::Message`]). Each answer is a response: [`Output::ResponseStart`],
//! its [`Output::Audio`], [`Output::Text`], [`Output::Transcript`] and
//! [`Output::ToolCall`]s, then [`Output::ResponseDone`] with how it ended,
//! [`Output::Usage`] among them; a provider that bills by duration says
//! [`Output::Seconds`] instead. A user who speaks over the model barges in:
//! [`Output::SpeechStarted`], and the response ends [`Status::Interrupted`].
//!
//! What the kind has no form for stays as its format has it ([`Native`],
//! [`Input::Native`], [`Output::Native`]). Only that format reads or sends it.
//! Crossing to another format, it is dropped.
//!
//! The crate is the kind's types alone, with no I/O; its formats over
//! WebSocket are `respan-gateway-realtime-ws`'s.

use std::time::Duration;

use bytes::Bytes;
use respan_gateway_core::{Fault, Native};

pub use respan_gateway_conversation::{Message, Tool};

/// One event the client sends the provider.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Input {
    /// Configure the session; a field left `None` is left as it is. The
    /// first event of every session, as its format decodes it (one opening
    /// with no frame of its own carries what the handshake said, such as
    /// a model in the URL's query).
    Session(Session),
    /// The next chunk of the user's audio, in its own format.
    Audio {
        /// The audio.
        audio: Audio,
        /// The event's fields the kind has no place for, such as an event id.
        native: Native,
    },
    /// The user's audio so far is a turn, an explicit end of the user's activity. Only
    /// meaningful with [`Turns::Manual`].
    Commit {
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// A whole message added to the conversation, such as the user's typed
    /// text.
    // WARN: mistral-realtime, openai-live and phonic-sts have none.
    Message {
        /// The message: its role and content.
        message: Message,
        /// The event's fields the kind has no place for, such as an item id
        /// or where it goes in the history.
        native: Native,
    },
    /// What a tool answered the model's call `id`.
    // WARN: mistral-realtime, openai-live and phonic-sts have none.
    ToolResult(ToolResult),
    /// Answer now. Some specs imply it instead, by a complete turn, the
    /// end of an activity, or a tool's answer.
    // WARN: mistral-realtime, openai-live and phonic-sts have none.
    Respond {
        /// Fields the kind has no place for, such as a response's own
        /// instructions or an out-of-band response.
        native: Native,
    },
    /// Stop the response being made.
    // WARN: mistral-realtime, openai-live and phonic-sts have none; gemini-live
    // emulates it.
    Cancel {
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// An event the kind has no form for, as its format has it, such as
    /// a spec's own history edit. Sent only in that format.
    Native(Native),
}

impl Input {
    /// The model a [`Input::Session`] asks for; `None` for any other event.
    pub fn model(&self) -> Option<&str> {
        match self {
            Input::Session(session) => session.model.as_deref(),
            _ => None,
        }
    }

    /// Asks for `model` if it is an [`Input::Session`]; other events are
    /// left as they are.
    pub fn set_model(&mut self, model: String) {
        if let Input::Session(session) = self {
            session.model = Some(model);
        }
    }
}

/// A filter that asks for `model`, such as `cx.filter(set_model(m))`.
pub fn set_model(
    model: impl Into<String>,
) -> impl FnMut(&mut Input) -> Result<(), Fault> + Clone + Send + 'static {
    let model = model.into();
    move |input: &mut Input| {
        input.set_model(model.clone());
        Ok(())
    }
}

/// A session's configuration. `None` leaves a field as it is (an update), or
/// as the provider's default (the first).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Session {
    /// What the session is for: a conversation unless it says.
    // WARN: openai-live and phonic-sts cannot transcribe alone, nor
    // mistral-realtime converse.
    pub purpose: Option<Purpose>,
    /// The model, as its provider names it, without any path prefix a spec adds.
    pub model: Option<String>,
    /// Instructions for the model.
    // WARN: mistral-realtime has none.
    pub instructions: Option<String>,
    /// What the model answers in.
    // WARN: mistral-realtime has none, and openai-live and phonic-sts
    // answer in audio alone.
    pub output: Option<Modality>,
    /// The voice, as the provider names it.
    // WARN: mistral-realtime has none.
    pub voice: Option<String>,
    /// The format of the user's audio, where the spec declares it per
    /// session; each [`Audio`] says its own.
    pub input_audio: Option<AudioFormat>,
    /// The format of the model's audio.
    // WARN: mistral-realtime has none.
    pub output_audio: Option<AudioFormat>,
    /// Who decides when the user's turn ends.
    pub turns: Option<Turns>,
    /// Whether the user's audio is transcribed ([`Output::InputTranscript`]).
    // WARN: only openai-realtime and gemini-live can turn it off.
    pub input_transcription: Option<bool>,
    /// Whether the model's audio is transcribed ([`Output::Transcript`]).
    // WARN: only gemini-live can turn it off, and mistral-realtime has no
    // audio of the model's.
    pub output_transcription: Option<bool>,
    /// The tools the model may call; `Some(vec![])` takes them all away.
    // WARN: mistral-realtime, openai-live and phonic-sts have none.
    pub tools: Option<Vec<Tool>>,
    /// The fields the kind has no place for, such as noise
    /// reduction or context window compression.
    pub native: Native,
}

/// What a session is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Purpose {
    /// The model answers the user.
    Conversation,
    /// The user's audio is transcribed ([`Output::InputTranscript`]), and
    /// nothing answers.
    Transcription,
}

/// What the model answers in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Modality {
    /// Speech, which may also be transcribed.
    Audio,
    /// Text alone.
    Text,
}

/// Who decides when the user's turn ends.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Turns {
    /// The provider, by voice activity detection.
    Server(Vad),
    /// The client, with [`Input::Commit`].
    Manual,
}

/// How the provider detects the user's turns.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Vad {
    /// Silence that ends a turn, in milliseconds.
    // WARN: openai-live and phonic-sts have none.
    pub silence_ms: Option<u32>,
    /// Audio kept from before speech was detected, in milliseconds.
    // WARN: openai-live and phonic-sts have none.
    pub prefix_ms: Option<u32>,
    /// Tuning the kind has no place for, such as thresholds, detector types
    /// and sensitivities.
    pub native: Native,
}

/// Audio: bytes in a format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    /// The samples, as `format` encodes them; never base64.
    pub data: Bytes,
    /// Their format.
    pub format: AudioFormat,
}

impl Audio {
    /// How long it plays, from its length and format.
    pub fn duration(&self) -> Duration {
        let samples = (self.data.len() / self.format.sample_bytes()) as u64;
        match self.format.rate {
            0 => Duration::ZERO,
            rate => Duration::from_nanos(samples * 1_000_000_000 / u64::from(rate)),
        }
    }
}

/// How audio is encoded: always mono.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    /// The sample encoding.
    pub encoding: Encoding,
    /// Samples a second.
    pub rate: u32,
}

impl AudioFormat {
    /// The bytes of one sample.
    pub const fn sample_bytes(self) -> usize {
        match self.encoding {
            Encoding::Pcm16 => 2,
            Encoding::Mulaw | Encoding::Alaw => 1,
        }
    }

    /// 16-bit little-endian PCM at `rate`.
    pub const fn pcm16(rate: u32) -> Self {
        Self {
            encoding: Encoding::Pcm16,
            rate,
        }
    }

    /// G.711 μ-law, 8 kHz.
    pub const MULAW: Self = Self {
        encoding: Encoding::Mulaw,
        rate: 8000,
    };

    /// G.711 A-law, 8 kHz.
    pub const ALAW: Self = Self {
        encoding: Encoding::Alaw,
        rate: 8000,
    };
}

/// A sample encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Encoding {
    /// 16-bit signed little-endian PCM.
    Pcm16,
    /// G.711 μ-law.
    Mulaw,
    /// G.711 A-law.
    Alaw,
}

/// What a tool answered.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ToolResult {
    /// The id of the call it answers.
    pub id: String,
    /// What the tool said: JSON text, or plain text.
    pub output: String,
    /// The fields the kind has no place for, such as when the model should act on it.
    pub native: Native,
}

impl ToolResult {
    /// What a tool answered the call `id`.
    pub fn new(id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            output: output.into(),
            native: Native::default(),
        }
    }
}

/// One event the provider sends the client.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// The session began, configured so.
    Opened(Session),
    /// A configuration took effect.
    Updated(Session),
    /// The user began speaking: a client stops playing the model's audio.
    // WARN: mistral-realtime and openai-live have none.
    SpeechStarted {
        /// Where in the user's audio, in milliseconds, if the provider says.
        // WARN: phonic-sts has none.
        at_ms: Option<u64>,
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// The user stopped speaking.
    // WARN: mistral-realtime and openai-live have none.
    SpeechStopped {
        /// Where in the user's audio, in milliseconds, if the provider says.
        // WARN: phonic-sts has none.
        at_ms: Option<u64>,
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// What the user said, as the provider transcribes it.
    InputTranscript {
        /// The text: the next part, or with `complete` the whole turn's.
        text: String,
        /// Whether `text` is the whole transcript of the user's turn.
        complete: bool,
        /// The event's fields the kind has no place for, such as its item.
        native: Native,
    },
    /// A response begins.
    // WARN: mistral-realtime and openai-live have none.
    ResponseStart {
        /// The event's fields the kind has no place for, such as its id.
        native: Native,
    },
    /// The next chunk of the model's audio.
    // WARN: mistral-realtime has none.
    Audio {
        /// The audio.
        audio: Audio,
        /// The event's fields the kind has no place for, such as its item.
        native: Native,
    },
    /// The next text of the model's answer, in [`Modality::Text`].
    // WARN: mistral-realtime, openai-live and phonic-sts have none.
    Text {
        /// The text.
        text: String,
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// The next text of the model's audio, transcribed.
    // WARN: mistral-realtime has none.
    Transcript {
        /// The text.
        text: String,
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// The model calls tool `name`; its answer is an [`Input::ToolResult`]
    /// naming `id`.
    // WARN: mistral-realtime, openai-live and phonic-sts have none.
    ToolCall {
        /// The call's id.
        id: String,
        /// The tool's name.
        name: String,
        /// Its arguments, as JSON text, whole.
        arguments: String,
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// Usage the provider reports for a response, as an increment.
    // WARN: openai-live and phonic-sts bill by duration instead.
    Usage(Usage),
    /// Seconds of the session the provider bills, as an increment, where it
    /// bills by duration rather than by token.
    // WARN: openai-realtime, gemini-live and mistral-realtime bill by token
    // instead.
    Seconds(f64),
    /// The response ended.
    // WARN: mistral-realtime and openai-live have none.
    ResponseDone {
        /// How.
        status: Status,
        /// The event's fields the kind has no place for.
        native: Native,
    },
    /// The provider refused something the client sent; the session goes on
    /// where the format allows it.
    Error {
        /// Why, as any format reads it.
        cause: Cause,
        /// What it said.
        message: String,
        /// The event's fields the kind has no place for, such as the
        /// provider's own code and the id of the event it refuses.
        native: Native,
    },
    /// An event the kind has no form for, as its format has it. Sent only in
    /// that format.
    Native(Native),
}

/// Why a provider answered an [`Output::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cause {
    /// What the client sent is invalid: sent again as it is, it is
    /// refused again.
    Invalid,
    /// A cancel found no response being made, as when one just ended: no
    /// one's fault, and nothing changed.
    NothingToCancel,
    /// The provider failed.
    Provider,
}

/// How a response ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// It was complete, tool calls included.
    Completed,
    /// The client cancelled it ([`Input::Cancel`]).
    // WARN: phonic-sts has none.
    Cancelled,
    /// The user spoke over it.
    Interrupted,
    /// A safety filter.
    // WARN: phonic-sts has none.
    Filtered,
    /// The provider failed.
    // WARN: phonic-sts has none.
    Failed,
    /// A reason the kind has no form for, as its format names it; another
    /// format writes `fallback`.
    Native {
        /// The reason, as its format names it.
        native: Native,
        /// What another format writes.
        fallback: Box<Status>,
    },
}

/// Token counts of a response, as an increment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// The counts every kind reports: input, output, cached, reasoning.
    pub tokens: respan_gateway_conversation::Usage,
    /// Of `tokens.input_tokens`, those of audio.
    pub input_audio_tokens: u64,
    /// Of `tokens.output_tokens`, those of audio.
    pub output_audio_tokens: u64,
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, other: &Usage) {
        self.tokens += &other.tokens;
        self.input_audio_tokens = self
            .input_audio_tokens
            .saturating_add(other.input_audio_tokens);
        self.output_audio_tokens = self
            .output_audio_tokens
            .saturating_add(other.output_audio_tokens);
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Usage) {
        *self += &other;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_plays_for_its_samples_at_its_rate() {
        let second = |format: AudioFormat, bytes: usize| Audio {
            data: Bytes::from(vec![0; bytes]),
            format,
        };
        let one = Duration::from_secs(1);
        assert_eq!(second(AudioFormat::pcm16(24_000), 48_000).duration(), one);
        assert_eq!(second(AudioFormat::pcm16(16_000), 32_000).duration(), one);
        assert_eq!(second(AudioFormat::MULAW, 8_000).duration(), one);
        assert_eq!(second(AudioFormat::ALAW, 4_000).duration(), one / 2);
        // An odd byte is no sample.
        let odd = second(AudioFormat::pcm16(8_000), 3).duration();
        assert_eq!(odd, Duration::from_micros(125));
        assert_eq!(second(AudioFormat::pcm16(0), 2).duration(), Duration::ZERO);
    }

    #[test]
    fn only_a_session_names_a_model() {
        let mut session = Input::Session(Session::default());
        assert_eq!(session.model(), None);
        set_model("model-a")(&mut session).expect("set");
        assert_eq!(session.model(), Some("model-a"));

        let mut commit = Input::Commit {
            native: Native::default(),
        };
        commit.set_model("model-a".into());
        assert_eq!(
            commit,
            Input::Commit {
                native: Native::default()
            }
        );
        assert_eq!(commit.model(), None);
        let native = Input::Native(Native::json::<()>(serde_json::json!({"type": "x"})));
        assert_eq!(native.model(), None);
    }
}
