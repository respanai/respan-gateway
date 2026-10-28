#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use bytes::{Buf, BufMut, Bytes, BytesMut};
use respan_gateway_core::Fault;
use respan_gateway_http_faults::over_limit;

/// The bytes of a message that are not its headers or payload: the prelude
/// (12) and the message CRC (4).
const OVERHEAD: usize = 16;

/// The CRC-32 (IEEE) lookup table.
const TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// The CRC-32 (IEEE) of `bytes`.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0, |crc, &b| {
        TABLE[usize::from((crc as u8) ^ b)] ^ (crc >> 8)
    })
}

/// A header's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Header {
    /// A boolean.
    Bool(bool),
    /// A signed byte.
    Byte(i8),
    /// A signed 16-bit integer.
    Short(i16),
    /// A signed 32-bit integer.
    Int(i32),
    /// A signed 64-bit integer.
    Long(i64),
    /// Bytes.
    Bytes(Bytes),
    /// UTF-8 text.
    String(String),
    /// Milliseconds since the Unix epoch.
    Timestamp(i64),
    /// A UUID.
    Uuid([u8; 16]),
}

/// One message of the stream: its headers, in order, and its payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    /// The headers, by name.
    pub headers: Vec<(String, Header)>,
    /// The payload.
    pub payload: Bytes,
}

impl Message {
    /// The text of the header `name`, if it is one.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(n, v)| match v {
            Header::String(text) if n == name => Some(text.as_str()),
            _ => None,
        })
    }

    /// The message written as a service sends it. A name over 255 bytes and
    /// a value over 65535 are cut short.
    pub fn encode(&self) -> Bytes {
        let mut headers = BytesMut::new();
        for (name, value) in &self.headers {
            let name = &name.as_bytes()[..name.len().min(255)];
            headers.put_u8(name.len() as u8);
            headers.put_slice(name);
            match value {
                Header::Bool(v) => headers.put_u8(if *v { 0 } else { 1 }),
                Header::Byte(v) => {
                    headers.put_u8(2);
                    headers.put_i8(*v);
                }
                Header::Short(v) => {
                    headers.put_u8(3);
                    headers.put_i16(*v);
                }
                Header::Int(v) => {
                    headers.put_u8(4);
                    headers.put_i32(*v);
                }
                Header::Long(v) => {
                    headers.put_u8(5);
                    headers.put_i64(*v);
                }
                Header::Bytes(v) => sized(&mut headers, 6, v),
                Header::String(v) => sized(&mut headers, 7, v.as_bytes()),
                Header::Timestamp(v) => {
                    headers.put_u8(8);
                    headers.put_i64(*v);
                }
                Header::Uuid(v) => {
                    headers.put_u8(9);
                    headers.put_slice(v);
                }
            }
        }
        let total = OVERHEAD + headers.len() + self.payload.len();
        let mut out = BytesMut::with_capacity(total);
        out.put_u32(total as u32);
        out.put_u32(headers.len() as u32);
        out.put_u32(crc32(&out));
        out.put_slice(&headers);
        out.put_slice(&self.payload);
        out.put_u32(crc32(&out));
        out.freeze()
    }
}

/// A header value of `kind` that has a 16-bit length.
fn sized(headers: &mut BytesMut, kind: u8, value: &[u8]) {
    let value = &value[..value.len().min(usize::from(u16::MAX))];
    headers.put_u8(kind);
    headers.put_u16(value.len() as u16);
    headers.put_slice(value);
}

/// The fault of a part of a message whose CRC does not match.
fn damaged(part: &str) -> Fault {
    Fault::upstream(format!(
        "an event stream message's {part} fails its checksum"
    ))
}

/// The fault of a message that cannot be read.
fn unreadable(what: &str) -> Fault {
    Fault::upstream(format!("an event stream message's {what} cannot be read"))
}

/// The big-endian `u32` at the start of `bytes`.
fn word(bytes: &[u8]) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[..4]);
    u32::from_be_bytes(word)
}

/// Splits a body into messages. A message is held whole until its last byte
/// comes.
#[derive(Debug, Default)]
pub struct Parser {
    buf: BytesMut,
    /// The most bytes of one message it accepts.
    limit: Option<usize>,
}

impl Parser {
    /// A splitter that accepts messages of up to `limit` bytes, or of any
    /// size for `None`.
    pub fn new(limit: Option<usize>) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// Accepts messages of up to `limit` bytes from now on, or of any size
    /// for `None`.
    pub fn limit(&mut self, limit: Option<usize>) {
        self.limit = limit;
    }

    /// The messages `bytes` completes. `Err` with `upstream` for a message
    /// over the limit, a prelude or message that fails its CRC, or one
    /// whose lengths or headers cannot be read.
    pub fn push(&mut self, bytes: Bytes) -> Result<Vec<Message>, Fault> {
        self.buf.extend_from_slice(&bytes);
        let mut out = Vec::new();
        while let Some(message) = self.next()? {
            out.push(message);
        }
        Ok(out)
    }

    /// The body ended: `Err` with `truncated` if it ended inside a message.
    pub fn finish(&self) -> Result<(), Fault> {
        match self.buf.is_empty() {
            true => Ok(()),
            false => Err(Fault::truncated()),
        }
    }

    fn next(&mut self) -> Result<Option<Message>, Fault> {
        if self.buf.len() < 12 {
            return Ok(None);
        }
        if crc32(&self.buf[..8]) != word(&self.buf[8..]) {
            return Err(damaged("prelude"));
        }
        let total = word(&self.buf) as usize;
        let headers = word(&self.buf[4..]) as usize;
        if total < OVERHEAD || headers > total - OVERHEAD {
            return Err(unreadable("lengths"));
        }
        if let Some(limit) = self.limit.filter(|&limit| total > limit) {
            return Err(over_limit(limit));
        }
        if self.buf.len() < total {
            return Ok(None);
        }
        let whole = self.buf.split_to(total).freeze();
        if crc32(&whole[..total - 4]) != word(&whole[total - 4..]) {
            return Err(damaged("body"));
        }
        let split = 12 + headers;
        Ok(Some(Message {
            headers: read_headers(whole.slice(12..split))?,
            payload: whole.slice(split..total - 4),
        }))
    }
}

/// The headers `block` holds.
fn read_headers(mut block: Bytes) -> Result<Vec<(String, Header)>, Fault> {
    let mut out = Vec::new();
    while block.has_remaining() {
        let len = usize::from(take(&mut block, 1, "header")?[0]);
        let name = take(&mut block, len, "header")?;
        let name = String::from_utf8(name.to_vec()).map_err(|_| unreadable("header name"))?;
        let kind = take(&mut block, 1, "header")?[0];
        let fixed = |block: &mut Bytes, n: usize| take(block, n, "header");
        let value = match kind {
            0 => Header::Bool(true),
            1 => Header::Bool(false),
            2 => Header::Byte(fixed(&mut block, 1)?.get_i8()),
            3 => Header::Short(fixed(&mut block, 2)?.get_i16()),
            4 => Header::Int(fixed(&mut block, 4)?.get_i32()),
            5 => Header::Long(fixed(&mut block, 8)?.get_i64()),
            8 => Header::Timestamp(fixed(&mut block, 8)?.get_i64()),
            9 => {
                let mut id = [0; 16];
                id.copy_from_slice(&fixed(&mut block, 16)?);
                Header::Uuid(id)
            }
            6 | 7 => {
                let len = usize::from(fixed(&mut block, 2)?.get_u16());
                let bytes = fixed(&mut block, len)?;
                match kind {
                    6 => Header::Bytes(bytes),
                    _ => Header::String(
                        String::from_utf8(bytes.to_vec()).map_err(|_| unreadable("header"))?,
                    ),
                }
            }
            _ => return Err(unreadable("header type")),
        };
        out.push((name, value));
    }
    Ok(out)
}

/// The next `n` bytes of `block`, or the fault of a header cut short.
fn take(block: &mut Bytes, n: usize, what: &str) -> Result<Bytes, Fault> {
    match block.len() >= n {
        true => Ok(block.split_to(n)),
        false => Err(unreadable(what)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, payload: &str) -> Message {
        Message {
            headers: vec![
                (":event-type".into(), Header::String(kind.into())),
                (":message-type".into(), Header::String("event".into())),
                (
                    ":content-type".into(),
                    Header::String("application/json".into()),
                ),
            ],
            payload: Bytes::from(payload.to_owned()),
        }
    }

    #[test]
    fn the_crc_is_the_ieee_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn a_message_round_trips_with_every_header_type() {
        let message = Message {
            headers: vec![
                ("a".into(), Header::Bool(true)),
                ("b".into(), Header::Bool(false)),
                ("c".into(), Header::Byte(-1)),
                ("d".into(), Header::Short(-2)),
                ("e".into(), Header::Int(-3)),
                ("f".into(), Header::Long(-4)),
                ("g".into(), Header::Bytes(Bytes::from_static(b"xy"))),
                ("h".into(), Header::String("text".into())),
                ("i".into(), Header::Timestamp(5)),
                ("j".into(), Header::Uuid([7; 16])),
            ],
            payload: Bytes::from_static(b"{}"),
        };
        let mut parser = Parser::new(None);
        assert_eq!(parser.push(message.encode()).ok(), Some(vec![message]));
        assert!(parser.finish().is_ok());
    }

    #[test]
    fn a_stream_split_at_any_byte_decodes_every_message_once() {
        let messages = [
            event("one", "{\"a\":1}"),
            event("two", ""),
            event("three", "{}"),
        ];
        let stream: Vec<u8> = messages.iter().flat_map(|m| m.encode().to_vec()).collect();
        for cut in 0..=stream.len() {
            let mut parser = Parser::new(None);
            let mut out = Vec::new();
            for part in [&stream[..cut], &stream[cut..]] {
                out.extend(
                    parser
                        .push(Bytes::copy_from_slice(part))
                        .unwrap_or_default(),
                );
            }
            assert_eq!(out, messages, "cut at {cut}");
        }
    }

    /// A ConverseStream body as Bedrock sent it (Nova Micro saying hello).
    const CONVERSE_STREAM: &[u8] = include_bytes!("../testdata/converse-stream.bin");

    #[test]
    fn bedrocks_converse_stream_splits_at_any_byte_and_writes_back_as_sent() {
        let kinds = [
            "messageStart",
            "contentBlockDelta",
            "contentBlockDelta",
            "contentBlockStop",
            "messageStop",
            "metadata",
        ];
        for cut in 0..=CONVERSE_STREAM.len() {
            let mut parser = Parser::new(Some(64 * 1024));
            let mut out = Vec::new();
            for part in [&CONVERSE_STREAM[..cut], &CONVERSE_STREAM[cut..]] {
                let messages = parser.push(Bytes::copy_from_slice(part));
                out.extend(messages.unwrap_or_default());
            }
            assert!(parser.finish().is_ok(), "cut at {cut}");
            let said: Vec<_> = out.iter().map(|m| m.text(":event-type")).collect();
            assert_eq!(said, kinds.map(Some), "cut at {cut}");
            let written: Vec<u8> = out.iter().flat_map(|m| m.encode().to_vec()).collect();
            assert_eq!(written, CONVERSE_STREAM, "cut at {cut}");
        }
    }

    #[test]
    fn a_damaged_message_or_prelude_fails_its_checksum() {
        let whole = event("one", "{\"a\":1}").encode().to_vec();
        for at in [9, whole.len() - 6] {
            let mut damaged = whole.clone();
            damaged[at] ^= 1;
            let err = Parser::new(None).push(Bytes::from(damaged)).err();
            assert!(
                err.is_some_and(|f| f.message().contains("checksum")),
                "byte {at}"
            );
        }
    }

    #[test]
    fn a_body_that_ends_inside_a_message_is_truncated() {
        let whole = event("one", "{}").encode();
        let mut parser = Parser::new(None);
        assert!(
            parser
                .push(whole.slice(..whole.len() - 1))
                .is_ok_and(|m| m.is_empty())
        );
        assert!(parser.finish().is_err_and(|f| f.code() == "truncated"));
    }

    #[test]
    fn a_message_is_held_up_to_the_limit() {
        let whole = event("one", "{}").encode();
        let err = Parser::new(Some(whole.len() - 1)).push(whole.clone()).err();
        assert!(err.is_some_and(|f| f.message().contains("limit")));
        assert!(Parser::new(Some(whole.len())).push(whole).is_ok());
    }

    #[test]
    fn lengths_that_cannot_be_are_refused() {
        let mut prelude = BytesMut::new();
        prelude.put_u32(8);
        prelude.put_u32(0);
        prelude.put_u32(crc32(&prelude));
        assert!(Parser::new(None).push(prelude.freeze()).is_err());
    }
}
