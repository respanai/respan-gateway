//! Soniox's real-time speech transcription and translation protocol.

mod input;
mod output;
#[cfg(test)]
mod tests;

use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::raw::Original;
use respan_gateway_http::{ClientIo, Connect, ProviderIo, Reach, Upstream};
use respan_gateway_realtime::{AudioFormat, Input, Output, Purpose, Session};
use respan_gateway_ws::WsTransport;

use crate::foreign::{Drop, Harmless};
use crate::session::{Spec, model_at};
use crate::{SessionModel, requested_model};

const NAME: &str = SPEC.name;

pub(crate) const HARMLESS: Harmless = Harmless {
    session: &[],
    vad: &[],
    message: &[],
    event: &[],
    input: |event| match event["type"].as_str() {
        Some("keepalive") => (Drop::Always, "keepalive".into()),
        _ => (Drop::Never, "Soniox control or audio".into()),
    },
    // Translation cannot silently disappear when crossing formats.
    said: |event| {
        event["tokens"].as_array().is_some_and(|tokens| {
            tokens
                .iter()
                .any(|token| token["translation_status"] == "translation")
        })
    },
};

pub(crate) const SPEC: Spec = Spec {
    name: "soniox-realtime",
    path: "/transcribe-websocket",
    protocol: None,
    binary_text: false,
    transcribes: true,
    harmless: Some(&HARMLESS),
};

/// Soniox real-time transcription and speech-to-text translation at
/// `/transcribe-websocket`. Authenticate with a bearer header, never an
/// `api_key` in the configuration. Audio uses base64 text frames because
/// the gateway's WebSocket wire carries text, including an empty text
/// frame to finish. Binary client audio is not supported.
///
/// The initial configuration's Soniox options, including `translation`,
/// remain native. Final source tokens become typed input transcripts;
/// revisable tokens, translation, timing and speaker information remain
/// native responses. Translation to another client's protocol is refused
/// rather than silently discarded. Configuration is fixed once sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SonioxRealtime;

impl SonioxRealtime {
    /// The format's identity.
    pub const NAME: &'static str = SPEC.name;
    /// The path served and called.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to the Soniox WebSocket origin.
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

impl Upstream for SonioxRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn reach(&self) -> Reach {
        Reach::Session { binary_text: false }
    }
    fn at_origin(&self) -> bool {
        true
    }
    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |io| connect(io))
    }
}

impl Format<ClientIo, Input, Output> for SonioxRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn matches(&self, io: &ClientIo) -> bool {
        SPEC.matches(io)
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let mut client = input::Client::new(model_at(io.request().uri()));
        let read = move |raw: Bytes, out: &mut Vec<Input>| client.read(raw, out);
        let mut client = output::Client::default();
        let write = move |event: &Output, out: &mut Vec<Bytes>| client.write(event, out);
        Ok(SPEC.client(io, Box::new(read), Box::new(write)))
    }
}

impl Format<ProviderIo, Output, Input> for SonioxRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let model = io
            .extensions()
            .get::<SessionModel>()
            .map(|m| m.0.clone())
            .or_else(|| requested_model(io.extensions().get::<Original>()?));
        let (say, said) = crate::session::say();
        let mut opened = Session::default();
        opened.model = model.clone();
        opened.purpose = Some(Purpose::Transcription);
        opened.input_transcription = Some(true);
        opened.input_audio = Some(AudioFormat::pcm16(16_000));
        let _ = say.unbounded_send(Output::Opened(opened));
        Ok(SPEC.provider(
            io,
            None,
            Box::new(input::Provider::new(model)),
            Box::new(output::Provider::default()),
            Some(said),
        ))
    }
}
