//! What every format's session shares: its handshake at both edges, and its
//! frames read and written by the format's own codec.

use respan_gateway_core::futures::channel::mpsc;
use respan_gateway_core::futures::stream::{self, PollNext};
use respan_gateway_core::futures::{FutureExt, StreamExt, future};
use respan_gateway_core::{Connection, EventStream, Fault, FaultKind, Native};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{SEC_WEBSOCKET_PROTOCOL, UPGRADE};
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri};
use respan_gateway_http::{self as h, ClientIo, Decoder, Encoder, ProviderIo, Reach};
use respan_gateway_realtime::{Cause, Input, Output};
use respan_gateway_ws::{CloseCode, ProviderClose, WsTransport};

/// Reads one frame as zero or more events.
pub(crate) type Read<T> = Box<dyn Decoder<T>>;

/// Where a provider's writer says what answers an input rather than a
/// frame, such as an update it need not send: its events reach the client
/// before the frames' next, until the frames end.
pub(crate) type Say = mpsc::UnboundedSender<Output>;

/// What a [`Say`] says, read beside the frames.
pub(crate) type Said = mpsc::UnboundedReceiver<Output>;

/// A channel for what a provider's writer says itself.
pub(crate) fn say() -> (Say, Said) {
    mpsc::unbounded()
}

/// Every spec of the crate: what a client's upgrade request is matched
/// against.
pub(crate) const SPECS: [&Spec; 5] = [
    &crate::openai::SPEC,
    &crate::gemini::SPEC,
    &crate::mistral::SPEC,
    &crate::live::SPEC,
    &crate::phonic::SPEC,
];

/// What a spec says of its session.
#[derive(Debug)]
pub(crate) struct Spec {
    /// The format's name.
    pub(crate) name: &'static str,
    /// The path it is served at the end of, and sent to.
    pub(crate) path: &'static str,
    /// The subprotocol it speaks, if any.
    pub(crate) protocol: Option<&'static str>,
    /// Whether its provider sends JSON in binary frames.
    pub(crate) binary_text: bool,
    /// Whether its sessions transcribe alone: one opened at its path is a
    /// transcription from its connect, whoever provides it.
    pub(crate) transcribes: bool,
}

impl Spec {
    /// Whether a client's request is an upgrade to this session: at a path
    /// ending in its own, but not in another spec's that ends in it, as
    /// Mistral's `/audio/transcriptions/realtime` does in OpenAI's
    /// `/realtime`, whatever order they are served in.
    pub(crate) fn matches(&self, io: &ClientIo) -> bool {
        let head = io.request();
        let at = |path| respan_gateway_http::path::path_ends_with(head.uri(), path);
        let longer =
            |other: &Spec| other.path.len() > self.path.len() && other.path.ends_with(self.path);
        head.method() == Method::GET
            && respan_gateway_http::rules::header_tokens(head.headers(), UPGRADE)
                .any(|v| v.eq_ignore_ascii_case("websocket"))
            && at(self.path)
            && !SPECS.iter().any(|other| longer(other) && at(other.path))
    }

    /// How its provider is reached: as a session, in binary frames if the
    /// spec sends them.
    pub(crate) fn reach(&self) -> Reach {
        Reach::Session {
            binary_text: self.binary_text,
        }
    }

    /// A transport to its provider at `base_url`, taking binary text if the
    /// spec sends binary frames.
    pub(crate) fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        Ok(WsTransport::new(base_url)?.with_binary_text(self.binary_text))
    }

    /// A client's session: its frames read by `read`, the provider's events
    /// written by `write`. Its `101` gives back the subprotocol if the
    /// client offered it.
    pub(crate) fn client(
        &self,
        io: ClientIo,
        read: Read<Input>,
        write: Box<dyn Encoder<Output>>,
    ) -> Connection<Input, Output> {
        let write = Box::new(Told { write, said: None });
        let protocol = self.protocol.map(HeaderValue::from_static);
        io.with_error(refusal).bind(
            move |_| read,
            move |reply| {
                let mut head = reply.head();
                if head.status == StatusCode::OK {
                    head.status = StatusCode::SWITCHING_PROTOCOLS;
                    head.headers
                        .insert(UPGRADE, HeaderValue::from_static("websocket"));
                }
                if let Some(protocol) = protocol {
                    head.headers.remove(SEC_WEBSOCKET_PROTOCOL);
                    let mut offers = respan_gateway_http::rules::header_tokens(
                        reply.request().headers(),
                        SEC_WEBSOCKET_PROTOCOL,
                    );
                    if offers.any(|v| v.as_bytes() == protocol.as_bytes()) {
                        head.headers.insert(SEC_WEBSOCKET_PROTOCOL, protocol);
                    }
                }
                Ok(reply.open(head, write))
            },
        )
    }

    /// A session with the provider: an upgrade to the spec's path and
    /// `query`, offering its subprotocol beside the client's own offers;
    /// the client's events written by `write`, the provider's frames read
    /// by `read`, and what `write` says itself, if anything, among them.
    pub(crate) fn provider(
        &self,
        io: ProviderIo,
        query: Option<String>,
        write: Box<dyn Encoder<Input>>,
        read: Read<Output>,
        said: Option<Said>,
    ) -> Connection<Output, Input> {
        let read = Box::new(Explained { read, error: None });
        let path = self.path;
        let protocol = self.protocol;
        let wire = io.extensions().get::<h::raw::Original>().cloned();
        // Only the handshake's 101 is a success: a 2xx is no session.
        let upgraded =
            |answer: &ResponseHead, _: &_| answer.status == StatusCode::SWITCHING_PROTOCOLS;
        let (opening, sink) = io.with_success(upgraded).bind(
            move |head, input: Option<&Input>| {
                if input.is_some() {
                    return Err(Fault::config(
                        "a session's events flow in a WebSocket session, not in a call with one input",
                    ));
                }
                head.method = Method::GET;
                let at = match &query {
                    Some(query) => format!("{path}?{query}"),
                    None => path.to_owned(),
                };
                head.uri = at.parse().map_err(|e| {
                    Fault::internal("a session's URL could not be written").with_source(e)
                })?;
                head.headers
                    .insert(UPGRADE, HeaderValue::from_static("websocket"));
                if let Some(protocol) = protocol {
                    let client = wire.as_ref().and_then(h::raw::Original::get);
                    offer_beside_the_clients(&mut head.headers, client.map(Request::headers), protocol)?;
                }
                Ok(write)
            },
            move |_, _| Some(read),
        );
        // What the writer says itself reaches the client as soon as said,
        // not with the provider's next frame.
        let opening = match said {
            Some(said) => opening
                .map(|opened| opened.map(|frames| among(said, frames)))
                .boxed(),
            None => opening,
        };
        (opening, sink)
    }
}

/// The model a call dials: the one routing chose, else the one its
/// client's handshake asks for, if either names one.
pub(crate) fn dialled_model(io: &ProviderIo) -> Option<String> {
    let chosen = io.extensions().get::<crate::SessionModel>();
    let asked = || crate::requested_model(io.extensions().get::<h::raw::Original>()?);
    chosen.map(|m| m.0.clone()).or_else(asked)
}

/// The model at `?model=` of a client's handshake, unescaped, if it names one.
pub(crate) fn model_at(uri: &Uri) -> Option<String> {
    let query = uri.query()?.as_bytes();
    form_urlencoded::parse(query)
        .find(|(name, model)| name == "model" && !model.is_empty())
        .map(|(_, model)| model.into_owned())
}

/// Whether a session opened at `uri` is a transcription from its connect,
/// as one at the path of a spec that transcribes alone is.
pub(crate) fn transcribes_at(uri: &Uri) -> bool {
    let at = |path| respan_gateway_http::path::path_ends_with(uri, path);
    SPECS.iter().any(|spec| spec.transcribes && at(spec.path))
}

/// A handshake's query naming `model`, escaped, as [`model_at`] reads it.
pub(crate) fn model_query(model: &str) -> String {
    form_urlencoded::Serializer::new(String::new())
        .append_pair("model", model)
        .finish()
}

/// A client's writer that tells the client why its session fails: the
/// fault as an [`Output::Error`], which the format writes as its error
/// event, before the Close. A format with no error event writes none, and
/// the Close's reason says it. An error the provider just sent is not told
/// twice.
struct Told {
    write: Box<dyn Encoder<Output>>,
    said: Option<String>,
}

impl Encoder<Output> for Told {
    fn start(&mut self, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.write.start(out)
    }

    fn encode(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.said = match event {
            Output::Error { message, .. } => Some(message.clone()),
            _ => None,
        };
        self.write.encode(event, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Err(fault) = &end else {
            return self.write.end(end, out);
        };
        if fault.is(FaultKind::Abandoned) || self.said.as_deref() == Some(fault.message()) {
            return end;
        }
        // A provider's Close that refused what it was sent is a 400, though upstream.
        let cause = match respan_gateway_http_faults::status(fault).is_client_error() {
            true => Cause::Invalid,
            false => Cause::Provider,
        };
        let error = Output::Error {
            cause,
            message: fault.message().to_owned(),
            native: Native::default(),
        };
        let mut frames = Vec::new();
        match self.write.encode(&error, &mut frames) {
            Ok(()) if !frames.is_empty() => {
                out.extend(frames);
                Ok(())
            }
            _ => end,
        }
    }
}

/// A provider's reader that explains a close by the error the provider
/// sent just before it: the last event it read, if an [`Output::Error`], is
/// what its abnormal Close is for, as one that refused what it was sent
/// (`bad_request`) or its failure (`upstream`), saying the provider's words.
struct Explained {
    read: Read<Output>,
    error: Option<(Cause, String)>,
}

impl Decoder<Output> for Explained {
    fn start(&mut self, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.read.start(out)
    }

    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let before = out.len();
        let read = self.read.decode(raw, out);
        if out.len() > before {
            self.error = match out.last() {
                Some(Output::Error { cause, message, .. }) if *cause != Cause::NothingToCancel => {
                    Some((*cause, message.clone()))
                }
                _ => None,
            };
        }
        read
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        let end = self.read.end(end, out);
        match (end, self.error.take()) {
            (Err(closed), Some((cause, message))) if closed.get::<ProviderClose>().is_some() => {
                let fault = match cause {
                    Cause::Invalid => Fault::bad_request(message),
                    _ => Fault::upstream(message),
                };
                Err(carry(fault, &closed))
            }
            (end, _) => end,
        }
    }

    fn limit(&mut self, bytes: Option<usize>) {
        self.read.limit(bytes);
    }

    fn done(&self) -> bool {
        self.read.done()
    }
}

/// `fault`, explaining the provider's Close `closed`: with its code, and
/// the code the client is closed with if the call relayed it.
pub(crate) fn carry(fault: Fault, closed: &Fault) -> Fault {
    let fault = match closed.get::<ProviderClose>() {
        Some(code) => fault.with(*code),
        None => fault,
    };
    match closed.get::<CloseCode>() {
        Some(code) => fault.with(*code),
        None => fault,
    }
}

/// The events of `frames`, with what was `said` first whenever there is
/// some, until the frames end.
fn among(said: Said, frames: EventStream<Output>) -> EventStream<Output> {
    let said = said.map(|event| Some(Ok(event)));
    let frames = frames.map(Some).chain(stream::once(future::ready(None)));
    let first = |_: &mut ()| PollNext::Left;
    let events = stream::select_with_strategy(said, frames, first)
        .take_while(|event| future::ready(event.is_some()))
        .filter_map(future::ready);
    EventStream::stream(events)
}

/// A refused handshake's answer: text/plain, with the fault's message.
fn refusal(fault: &Fault, _request: &Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    respan_gateway_http::rules::plain_text(head, fault.message())
}

/// Offer `protocol` in `headers` beside the subprotocols the client
/// offered, which are kept, if the client did not offer it.
fn offer_beside_the_clients(
    headers: &mut HeaderMap,
    client: Option<&HeaderMap>,
    protocol: &str,
) -> Result<(), Fault> {
    let offered = client.into_iter().flat_map(|client| {
        respan_gateway_http::rules::header_tokens(client, SEC_WEBSOCKET_PROTOCOL)
    });
    let mut offers: Vec<&str> = offered.filter(|offer| !offer.is_empty()).collect();
    if !offers.contains(&protocol) {
        offers.push(protocol);
    }
    let all = HeaderValue::from_str(&offers.join(", "))
        .map_err(|_| Fault::config("a subprotocol must be a token"))?;
    headers.insert(SEC_WEBSOCKET_PROTOCOL, all);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a model was put in the query as it was, so `a b` broke
    /// the URL and `x&foo=bar` added a parameter.
    #[test]
    fn a_model_is_one_value_of_the_query() {
        for model in ["gpt-realtime", "a b", "x&foo=bar", "100%"] {
            let uri: Uri = format!("/v1/realtime?{}", model_query(model))
                .parse()
                .expect("a URI");
            assert_eq!(model_at(&uri).as_deref(), Some(model));
            assert_eq!(uri.query().map(|q| q.matches('=').count()), Some(1));
        }
    }

    /// Writes each error event as its cause and message.
    fn errors() -> Box<dyn Encoder<Output>> {
        Box::new(|event: &Output, out: &mut Vec<Bytes>| {
            if let Output::Error { cause, message, .. } = event {
                out.push(Bytes::from(format!("{cause:?} {message}")));
            }
            Ok(())
        })
    }

    #[test]
    fn a_failed_session_is_told_to_its_client_once() {
        let refused = || {
            Fault::upstream("refused")
                .with(respan_gateway_http_faults::Status(StatusCode::BAD_REQUEST))
        };
        let mut told = Told {
            write: errors(),
            said: None,
        };
        let mut out = Vec::new();
        assert!(told.end(Err(refused()), &mut out).is_ok());
        assert_eq!(out, [Bytes::from_static(b"Invalid refused")]);
        // The provider said it already: not again, and the end stays a failure.
        let said = Output::Error {
            cause: Cause::Provider,
            message: "refused".into(),
            native: Native::default(),
        };
        let mut out = Vec::new();
        assert!(told.encode(&said, &mut out).is_ok());
        assert!(told.end(Err(refused()), &mut Vec::new()).is_err());
    }

    #[test]
    fn a_providers_close_is_the_error_it_sent_before_it() {
        let read: Read<Output> = Box::new(|raw: Bytes, out: &mut Vec<Output>| {
            out.push(Output::Error {
                cause: Cause::Invalid,
                message: String::from_utf8_lossy(&raw).into_owned(),
                native: Native::default(),
            });
            Ok(())
        });
        let mut explained = Explained { read, error: None };
        let mut out = Vec::new();
        assert!(
            explained
                .decode(Bytes::from_static(b"no such voice"), &mut out)
                .is_ok()
        );
        let closed = Fault::upstream("the provider closed the session 1008")
            .with(ProviderClose(1008))
            .with(CloseCode(1008));
        let fault = explained.end(Err(closed), &mut out).expect_err("a failure");
        assert_eq!(
            (fault.code(), fault.message()),
            ("bad_request", "no such voice")
        );
        assert_eq!(fault.get::<CloseCode>().copied(), Some(CloseCode(1008)));
    }
}
