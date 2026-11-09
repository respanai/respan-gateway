//! Phonic's speech-to-speech over WebSocket.

mod input;
mod output;
mod session;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{ClientIo, ProviderIo, raw::Original};
use respan_gateway_http::{Connect, Reach, Upstream};
use respan_gateway_realtime::{AudioFormat, Input, Output, Session};
use respan_gateway_ws::WsTransport;

use crate::session::{self as spec, Spec};
use crate::{SessionModel, requested_model};

const NAME: &str = SPEC.name;

pub(crate) const SPEC: Spec = Spec {
    name: "phonic-sts",
    path: "/sts/ws",
    protocol: None,
    binary_text: false,
    transcribes: false,
};

/// A session's audio format, both ways, until its `config` says another.
const DEFAULT_FORMAT: AudioFormat = AudioFormat::pcm16(44_100);

/// The session its `config` started, as the provider's writer applied it
/// and its reader tells of it.
type Applied = Arc<Mutex<Option<Session>>>;

/// The format of the audio a client asked for: its reader reads it, its
/// writer writes in it.
type Asked = Arc<Mutex<AudioFormat>>;

/// The time now, which the session's duration is billed by.
type Clock = Box<dyn FnMut() -> Instant + Send>;

/// Phonic's speech-to-speech, which its models (`phonic-1`) speak: a `GET`
/// upgrade to a path ending in `/sts/ws`, and a call to it after
/// `wss://api.phonic.ai/v1`, with no query.
///
/// Its `config`, audio, the user's turns and transcript, the model's turns
/// and transcript, interruptions and errors are the kind's; the rest, such
/// as tools and its own `say`, stays native. It bills a conversation by its
/// minutes and says no usage, so the seconds since its conversation was
/// created are [`Output::Seconds`] at each frame but its audio, and at its
/// end. Its client
/// routes by `?model=`, as a Gemini Live client does; a call names the
/// model routing chose, else that one, else the one its `config` names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhonicSts;

impl PhonicSts {
    /// The format's name, `phonic-sts`, which tags its native events.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is served at the end of, and sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

impl Format<ClientIo, Input, Output> for PhonicSts {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        SPEC.matches(io)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let asked = Asked::new(Mutex::new(DEFAULT_FORMAT));
        let mut client = input::Client::new(asked.clone());
        let read = move |raw: Bytes, out: &mut Vec<Input>| client.read(raw, out);
        let mut client = output::Client::new(asked);
        let write = Box::new(move |event: &Output, out: &mut Vec<Bytes>| client.write(event, out));
        Ok(SPEC.client(io, Box::new(read), write))
    }
}

/// Its spec's: a bearer key, as a WebSocket session.
impl Upstream for PhonicSts {
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

impl Format<ProviderIo, Output, Input> for PhonicSts {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let chosen = io.extensions().get::<SessionModel>().map(|m| m.0.clone());
        let wire = io.extensions().get::<Original>();
        let model = chosen.or_else(|| requested_model(wire?));
        let voiced = wire
            .and_then(Original::get)
            .is_some_and(|head| path_ends_with(head.uri(), SPEC.path));
        let applied = Applied::default();
        let (say, said) = spec::say();
        let mut provider = input::Provider::new(model, voiced, applied.clone(), say);
        let write = Box::new(move |event: &Input, out: &mut Vec<Bytes>| provider.write(event, out));
        let read = output::Provider::new(applied, Box::new(Instant::now));
        Ok(SPEC.provider(io, None, write, Box::new(read), Some(said)))
    }
}
