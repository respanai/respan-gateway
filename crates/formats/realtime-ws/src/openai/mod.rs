//! OpenAI Realtime over WebSocket.

mod input;
mod output;
mod session;
#[cfg(test)]
mod tests;

use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::Uri;
use respan_gateway_http::{ClientIo, ProviderIo, raw::Original};
use respan_gateway_http::{Connect, Reach, Upstream};
use respan_gateway_realtime::{Input, Output, Purpose};
use respan_gateway_ws::WsTransport;

use crate::foreign::{Drop, Harmless};
use crate::session::{Spec, model_at, model_query, transcribes_at};
use crate::{SessionModel, requested_model};

const NAME: &str = SPEC.name;

/// The model OpenAI transcribes a session's input with, unless the session
/// names one: it transcribes only with a named model, so a session that
/// turns transcription on without one asks for this one.
const TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";

/// What of OpenAI Realtime's own natives another format may drop: its
/// event and item ids, its session's noise reduction, speed,
/// transcription settings, tracing, includes, context truncation and output
/// cap, and its voice detection's tuning, as each format manages its
/// context, and detects voices, itself; its truncation of what was heard
/// and clearing of what is played, as a format keeps only what it sent, and
/// its clearing of the user's audio if there is none. Its MCP events tell
/// of what the model said.
pub(crate) const HARMLESS: Harmless = Harmless {
    session: &[
        "event_id",
        "session.audio.input.noise_reduction",
        "session.audio.input.transcription",
        "session.audio.output.speed",
        "session.include",
        "session.tracing",
        "session.truncation",
        "session.max_output_tokens",
    ],
    vad: &["type", "threshold", "eagerness"],
    message: &["id"],
    event: &["event_id", "item.id", "response_id"],
    input: |event| {
        let kind = event["type"].as_str().unwrap_or_default().to_owned();
        let drop = match kind.as_str() {
            "conversation.item.truncate" | "output_audio_buffer.clear" => Drop::Always,
            "input_audio_buffer.clear" => Drop::Unheard,
            _ => Drop::Never,
        };
        (drop, kind)
    },
    said: |event| event["type"].as_str().is_some_and(|t| t.contains("mcp")),
};

/// A handshake's query asking for a transcription, as [`purpose_at`] reads
/// it.
const TRANSCRIPTION_QUERY: &str = "intent=transcription";

/// What a client's handshake asks its session to be for: a transcription
/// at `?intent=transcription`, which OpenAI fixes at connect.
fn purpose_at(uri: &Uri) -> Option<Purpose> {
    let query = uri.query()?.as_bytes();
    form_urlencoded::parse(query)
        .any(|(name, intent)| name == "intent" && intent == "transcription")
        .then_some(Purpose::Transcription)
}

pub(crate) const SPEC: Spec = Spec {
    name: "openai-realtime",
    path: "/realtime",
    protocol: Some("realtime"),
    binary_text: false,
    transcribes: false,
    harmless: Some(&HARMLESS),
};

/// OpenAI Realtime: a `GET` upgrade to a path ending in `/realtime`, and a
/// call to `/realtime` after the base URL its SDKs take
/// (`wss://api.openai.com/v1`). It speaks the subprotocol `realtime`, which
/// its browser clients offer beside their key: a call offers it beside the
/// client's own offers, and a client that offered it gets it back.
///
/// Its GA events are the kind's where the kind has a form for them: a
/// session's configuration, audio, messages, tools' calls and answers, and
/// a response's start, content, usage and end. The rest, such as its items
/// and their content parts, stays native, so a relay to OpenAI keeps every
/// event. A session's events begin with the session its handshake asked
/// for, naming the model at `?model=`; OpenAI takes its model at connect,
/// so a session that says nothing more is no frame.
///
/// A call dials the model routing chose ([`SessionModel`](crate::SessionModel)),
/// else the one at the client's `?model=`, and its own query alone; with
/// neither it is refused, `bad_request`.
///
/// A response of another format has none of OpenAI's items: its client is
/// told of them as OpenAI does, with ids made up for them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiRealtime;

impl OpenAiRealtime {
    /// The format's name, `openai-realtime`, which tags its native events.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is served at the end of, and sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }

    /// `input` as its writer takes it: the fields of other formats' natives
    /// that OpenAI can do without are dropped, and one that could change
    /// the answer is refused. A plugin that adds OpenAI's own fields to an
    /// input from another format's client admits it first.
    pub fn admit(&self, input: &Input) -> Result<Input, Fault> {
        input::admit(input)
    }
}

impl Format<ClientIo, Input, Output> for OpenAiRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        SPEC.matches(io)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let uri = io.request().uri();
        let mut client = input::Client::new(model_at(uri), purpose_at(uri));
        let read = move |raw: Bytes, out: &mut Vec<Input>| client.read(raw, out);
        let mut client = output::Client::default();
        let write = Box::new(move |event: &Output, out: &mut Vec<Bytes>| client.write(event, out));
        Ok(SPEC.client(io, Box::new(read), write))
    }
}

/// Its spec's: a bearer key, as a WebSocket session.
impl Upstream for OpenAiRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn reach(&self) -> Reach {
        Reach::Session {
            binary_text: SPEC.binary_text,
        }
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        // The model routing chose, else the one the client's handshake asks for.
        let wire = io.extensions().get::<Original>();
        let chosen = io.extensions().get::<SessionModel>().map(|m| m.0.clone());
        let asked = || requested_model(wire?);
        let model = chosen.or_else(asked).ok_or_else(|| {
            Fault::bad_request(
                "OpenAI Realtime takes its model at connect: name it with ?model= on the session's URL",
            )
        })?;
        // OpenAI opens a transcription at its intent, with no model: the
        // model is the one that transcribes.
        let uri = wire.and_then(|w| Some(w.get()?.uri().clone()));
        let transcribing = uri.is_some_and(|uri| {
            purpose_at(&uri) == Some(Purpose::Transcription) || transcribes_at(&uri)
        });
        let query = match transcribing {
            true => TRANSCRIPTION_QUERY.to_owned(),
            false => model_query(&model),
        };
        let mut provider = input::Provider::new(model, transcribing);
        let write = Box::new(move |event: &Input, out: &mut Vec<Bytes>| provider.write(event, out));
        let mut provider = output::Provider::default();
        let read = move |raw: Bytes, out: &mut Vec<Output>| provider.read(raw, out);
        Ok(SPEC.provider(io, Some(query), write, Box::new(read), None))
    }
}
