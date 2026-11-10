#![warn(missing_docs)]
//! WebSocket sessions on the gateway, as a neutral building block.
//!
//! A WebSocket is an HTTP Upgrade exchange on the HTTP wire: the call's head
//! is the upgrade request, the answer's head is the `101`, and the two bodies
//! are streams of frames, **one body item per text frame**. This integration provides
//! both edges of a session and the contract of a session's close
//! ([`serve`] documents how each end reaches the client and the provider):
//!
//! - [`WsTransport`]: an outbound native HTTP sender
//!   that connects to a `ws://` or `wss://` provider.
//! - [`serve`]: a client's upgrade, through a gateway, for an adapter that
//!   hosts sessions, such as `respan_gateway_serve`.
//! - [`CloseCode`] and [`ProviderClose`]: the code a client is closed with,
//!   and the fault of a provider's Close.
//! - [`Limits`]: what either edge takes of a session, in bytes and time.
//!
//! Frames carry text. [`WsTransport::with_binary_text`] can normalize a
//! provider's binary UTF-8 messages into text; other binary frames are faults.
//! Nothing here reads a provider's events: a session's format is its kind's
//! (`respan_gateway_realtime_ws`).

use std::fmt;
use std::time::Duration;

use respan_gateway_core::{Fault, FaultKind};

mod session;
mod transport;

pub use session::serve;
pub use transport::WsTransport;

/// What an edge of a session takes: the provider edge's ([`WsTransport`])
/// and the client edge's (an adapter's). [`Default`] gives each its
/// default, with its reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest message the edge accepts, and of a provider's refusal of
    /// the upgrade, its body: by default 16 MiB, above the largest event a
    /// provider takes (OpenAI Realtime's `input_audio_buffer.append`, up to
    /// 15 MiB). A larger one is a fault.
    pub max_message: usize,
    /// How long a side waits for the other's part of a close: an edge for
    /// its peer's Close after it sent one, the client edge for the
    /// response's end after its client sent Close, and for a refusal of the
    /// upgrade to be read. By default 5 s: a provider answers a Close
    /// behind the frames it was already sending, such as the rest of a
    /// turn, and one that does not answer is not waited on longer.
    pub close_wait: Duration,
    /// How long a Close is given to leave, to either side: by default 1 s,
    /// as a Close is under 130 bytes, so a peer that has not taken it in
    /// that time has stopped reading, and waiting on it would keep the
    /// session alive past its end.
    pub close_send: Duration,
    /// How long a peer may be quiet, while the edge waits for its next
    /// message, before it is pinged; quiet that long again, it is gone. By
    /// default 30 s: half the 60 s idle timeout common to load balancers
    /// and proxies (AWS's ALB, nginx's `proxy_read_timeout`), so an idle
    /// session's pings keep them from cutting it, and a peer that died
    /// without a word is seen within a minute. Time spent waiting for the
    /// other side to take the peer's frame does not count.
    pub ping_after: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message: 16 << 20,
            close_wait: Duration::from_secs(5),
            close_send: Duration::from_secs(1),
            ping_after: Duration::from_secs(30),
        }
    }
}

/// A WebSocket close code (RFC 6455 7.4), named as the RFC names those the
/// gateway sends, any other by its number: `CloseCode(4000)`.
///
/// As an extension of a fault ([`Fault::with`]), the code an edge sends as
/// its Close in place of its kind's: [`POLICY`](Self::POLICY) for the
/// client's, [`AWAY`](Self::AWAY) for an abandoned one, else
/// [`ERROR`](Self::ERROR). A provider's own code is a [`ProviderClose`],
/// which reaches the client only as this: set by a plugin, or by
/// [`WsTransport`] on a call [`Relayed`](respan_gateway_http::raw::Relayed),
/// as `respan_gateway_plugin_relay::Relay` marks one.
///
/// [`Fault::with`]: respan_gateway_core::Fault::with
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CloseCode(pub u16);

impl CloseCode {
    /// 1000: the session is done.
    pub const NORMAL: Self = Self(1000);
    /// 1001: an end goes away, such as a client that left.
    pub const AWAY: Self = Self(1001);
    /// 1003: a type of data the end cannot take, such as a binary frame.
    pub const UNSUPPORTED: Self = Self(1003);
    /// 1007: data inconsistent with its message's type.
    pub const INVALID: Self = Self(1007);
    /// 1008: a message against the end's policy; the RFC's generic code
    /// when no other fits.
    pub const POLICY: Self = Self(1008);
    /// 1009: a message too big to take.
    pub const TOO_BIG: Self = Self(1009);
    /// 1011: the server met a condition it did not expect.
    pub const ERROR: Self = Self(1011);

    /// This code if a Close frame may carry it (RFC 6455 7.4), else
    /// [`ERROR`](Self::ERROR): 1004 to 1006, 1015 to 2999, and any outside
    /// 1000 to 4999 may not be sent.
    pub(crate) fn sendable(self) -> Self {
        match self.0 {
            1000..=1003 | 1007..=1014 | 3000..=4999 => self,
            _ => Self::ERROR,
        }
    }

    /// Whether it refuses what the closing end was sent (RFC 6455 7.4.1):
    /// data it cannot take, data inconsistent with its type, or a message
    /// too big.
    fn refuses_data(self) -> bool {
        matches!(self, Self::UNSUPPORTED | Self::INVALID | Self::TOO_BIG)
    }
}

impl fmt::Display for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A provider's Close with a code other than [`CloseCode::NORMAL`], which
/// the fault [`WsTransport`] ends the answer with is made of
/// (`Fault::from`), and which it keeps as an extension. Its message names
/// the code and reason.
///
/// A code that refuses the data it was sent (1003, 1007 or 1009) is the
/// client's fault only when the provider took the client's frames as sent
/// (`relayed`). Translated, the frames were the gateway's writing, so a
/// refusal of them may be the gateway's as much as the client's, and stays
/// the provider's to report, as any other code does (1008's policy among
/// them, which can be about the account as well as the input). Whether the
/// client is closed with the code is a plugin's choice ([`CloseCode`]); the
/// reason is never sent to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderClose {
    /// The Close's code.
    pub code: CloseCode,
    /// The Close's reason, empty if it gave none.
    pub reason: String,
    /// Whether the provider was sent the client's frames as the client sent
    /// them ([`Relayed`](respan_gateway_http::raw::Relayed)).
    pub relayed: bool,
}

impl From<ProviderClose> for Fault {
    fn from(close: ProviderClose) -> Self {
        let ProviderClose {
            code,
            reason,
            relayed,
        } = &close;
        let message = match reason.is_empty() {
            true => format!("the provider closed the session with {code}"),
            false => format!("the provider closed the session with {code}: {reason}"),
        };
        let kind = match *relayed && code.refuses_data() {
            true => FaultKind::Client,
            false => FaultKind::Provider,
        };
        Fault::of(kind, message).with(close)
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseCode, Fault, FaultKind, ProviderClose};

    #[test]
    fn a_code_no_close_may_carry_is_sent_as_1011() {
        for bad in [999, 1004, 1005, 1006, 1015, 5000] {
            assert_eq!(CloseCode(bad).sendable(), CloseCode::ERROR, "{bad}");
        }
        for good in [1000, 1003, 1008, 1014, 4001] {
            assert_eq!(CloseCode(good).sendable(), CloseCode(good));
        }
    }

    #[test]
    fn a_refusal_of_data_is_the_clients_only_when_relayed() {
        let close = |code, relayed| ProviderClose {
            code,
            reason: String::new(),
            relayed,
        };
        for (code, relayed, kind) in [
            (CloseCode::INVALID, true, FaultKind::Client),
            (CloseCode::INVALID, false, FaultKind::Provider),
            (CloseCode::POLICY, true, FaultKind::Provider),
        ] {
            assert_eq!(Fault::from(close(code, relayed)).kind(), kind);
        }
    }
}
