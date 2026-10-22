//! Gemini Live over WebSocket.

mod input;
mod output;
mod setup;
#[cfg(test)]
mod tests;

use parking_lot::Mutex;
use respan_gateway_core::Provider;
use respan_gateway_http::{Connect, KeyIn, Reach, Upstream};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::{ClientIo, ProviderIo};
use respan_gateway_realtime::{AudioFormat, Input, Modality, Output, Session};
use respan_gateway_ws::WsTransport;

use serde_json::Value;

use crate::foreign::{Drop, Harmless};
use crate::session::{self, Spec};

const NAME: &str = SPEC.name;

/// What of Gemini Live's own natives another format may drop: its
/// transcription settings, context compression and voice detection's
/// sensitivities, and the shape of the message a client's event came in;
/// the start of the user's activity, which a format's voice detection or
/// its next commit implies, and the end of its audio stream.
pub(crate) const HARMLESS: Harmless = Harmless {
    session: &[
        "setup.contextWindowCompression",
        "setup.inputAudioTranscription",
        "setup.outputAudioTranscription",
    ],
    vad: &[
        "startOfSpeechSensitivity",
        "endOfSpeechSensitivity",
        "disabled",
    ],
    message: &[],
    event: &["clientContent", "realtimeInput.activityStart", "name"],
    input: |event| {
        let first = |v: &Value| v.as_object()?.keys().next().cloned();
        match event.get("realtimeInput").and_then(first) {
            Some(field) if field == "activityStart" || field == "audioStreamEnd" => {
                (Drop::Always, format!("realtimeInput.{field}"))
            }
            Some(field) => (Drop::Never, format!("realtimeInput.{field}")),
            None => (Drop::Never, first(event).unwrap_or_default()),
        }
    },
    said: |_| false,
};

/// What a session's writer to its provider and reader of it share.
#[derive(Debug, Default)]
struct State {
    /// The setup written, which the provider's events say took effect.
    applied: Option<Session>,
    /// Whether a response is being made.
    open: bool,
    /// Whether the client cancelled it: the rest of its turn is dropped.
    cancelled: bool,
    /// The tools' names by the id of their calls.
    calls: HashMap<String, String>,
    /// The calls the provider cancelled, whose answers it takes none of.
    withdrawn: HashSet<String>,
}

type Shared = Arc<Mutex<State>>;

/// Whether a client is being answered: its writer says, its reader reads.
type Answering = Arc<AtomicBool>;

pub(crate) const SPEC: Spec = Spec {
    name: "gemini-live",
    path: "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent",
    protocol: None,
    binary_text: true,
    transcribes: false,
    harmless: Some(&HARMLESS),
};

/// Gemini Live: a `GET` upgrade to a path ending in the Live API's endpoint,
/// and a call to it after the origin Google's SDK takes
/// (`wss://generativelanguage.googleapis.com`). Its server sends each JSON
/// event in a binary frame, which its [`transport`](Self::transport) takes
/// as text.
///
/// Its messages are the kind's events where the kind has a form for them:
/// the `setup` (its model without `models/`), audio, text, turns, tools'
/// calls and answers, transcripts, interruptions and usage. A client's
/// `setup` comes first and once, and each of its messages holds one field,
/// as the Live API takes them; else the session closes 1007. What the kind
/// has no form for, such as a session's resumption or a warning that it
/// ends, stays native, so a relay to Gemini keeps what every message says.
///
/// To a client of another format it says what the Live API cannot: a
/// session opened before its setup, an update that changes nothing, and
/// the end of a response the client cancelled, whose rest it drops.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiLive;

impl GeminiLive {
    /// The format's name, `gemini-live`, which tags its native events.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is served at the end of, and sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]),
    /// taking binary frames as text ([`WsTransport::with_binary_text`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

impl Format<ClientIo, Input, Output> for GeminiLive {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        SPEC.matches(io)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let answering = Answering::default();
        let mut client = input::Client::new(answering.clone());
        let read = move |raw: Bytes, out: &mut Vec<Input>| client.read(raw, out);
        let mut client = output::Client::new(answering);
        let write = Box::new(move |event: &Output, out: &mut Vec<Bytes>| client.write(event, out));
        Ok(SPEC.client(io, Box::new(read), write))
    }
}

/// Its spec's: the key in `x-goog-api-key`, as a WebSocket session at
/// its base URL's origin, as its path is whole.
impl Upstream for GeminiLive {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn reach(&self) -> Reach {
        Reach::Session {
            binary_text: SPEC.binary_text,
        }
    }

    fn at_origin(&self) -> bool {
        true
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiLive {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let shared = Shared::default();
        let (say, said) = session::say();
        // The session is open before its setup, as a client of OpenAI
        // waits to hear.
        let mut opened = Session::default();
        opened.output = Some(Modality::Audio);
        opened.output_audio = Some(AudioFormat::pcm16(24_000));
        let _ = say.unbounded_send(Output::Opened(opened));
        let mut provider = input::Provider::new(shared.clone(), say);
        let write = Box::new(move |event: &Input, out: &mut Vec<Bytes>| provider.write(event, out));
        let mut provider = output::Provider::new(shared);
        let read = move |raw: Bytes, out: &mut Vec<Output>| provider.read(raw, out);
        Ok(SPEC.provider(io, None, write, Box::new(read), Some(said)))
    }
}
