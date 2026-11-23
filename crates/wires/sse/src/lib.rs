#![warn(missing_docs)]
//! Server-sent events as bytes, for any format that streams them: an
//! incremental [`Parser`] that splits a body into events, [`event`] and
//! [`comment`] to write them so no value can end its event or start a
//! field, and [`event_data`] and [`event_json`] to read one. It knows no
//! HTTP: reading a response body as SSE is `respan_gateway_http`'s.

use bytes::{Bytes, BytesMut};
use respan_gateway_core::Fault;
use respan_gateway_http_faults::over_limit;
use serde_json::Value;

/// The byte order mark a stream may start with, which is no part of its
/// first line (HTML, 9.2.6).
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Splits a body into events. A byte order mark that starts it is dropped.
/// At the end, a last event of whole lines is kept without its blank line;
/// one cut mid-line is dropped, as clients do.
#[derive(Debug, Default)]
pub struct Parser {
    buf: BytesMut,
    /// Where to resume searching `buf` for the end of an event.
    scanned: usize,
    /// The most bytes of one event it holds.
    limit: Option<usize>,
    /// Whether it is past where a byte order mark may be.
    started: bool,
}

impl Parser {
    /// Create an incremental synchronous event splitter that holds up to
    /// `limit` bytes of an event not yet ended, such as its transport's
    /// (`respan_gateway_http::connect`), or any number for `None`.
    pub fn new(limit: Option<usize>) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// Split complete events from one wire frame. `Err` with `upstream` if
    /// an event not yet ended is over the limit.
    pub fn push(&mut self, bytes: Bytes) -> Result<Vec<Bytes>, Fault> {
        let mut events = Vec::new();
        self.push_into(bytes, &mut events)?;
        Ok(events)
    }

    /// Append complete events from one wire frame to `out`. If an event
    /// not yet ended is over the limit, return `upstream` after appending
    /// the complete events before it. A reader may finish on one of those
    /// events without reading the oversized tail.
    pub fn push_into(&mut self, bytes: Bytes, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.split(&bytes, false, out);
        match self.limit.filter(|&limit| self.buf.len() > limit) {
            Some(limit) => Err(over_limit(limit)),
            None => Ok(()),
        }
    }
    /// Finish the body, retaining a trailing event of whole lines only.
    pub fn finish(&mut self) -> Vec<Bytes> {
        let mut events = Vec::new();
        self.split(&[], true, &mut events);
        events
    }

    /// Split off the first event ended by a blank line; at the end, a last
    /// `\r` ends its line.
    fn next_event(&mut self, at_end: bool) -> Option<Bytes> {
        let mut i = self.scanned;
        while let Some(first) = line_end(&self.buf[i..], at_end) {
            if first == 0 {
                i += 1;
                continue;
            }
            match line_end(&self.buf[i + first..], at_end) {
                None => break,
                Some(0) => i += first,
                Some(second) => {
                    self.scanned = 0;
                    return Some(self.buf.split_to(i + first + second).freeze());
                }
            }
        }
        self.scanned = i;
        None
    }

    /// The events of `bytes`, then, `at_end`, the last one left.
    fn split(&mut self, bytes: &[u8], at_end: bool, events: &mut Vec<Bytes>) {
        self.buf.extend_from_slice(bytes);
        if !self.started {
            if !at_end && self.buf.len() < BOM.len() && BOM.starts_with(&self.buf) {
                return;
            }
            if self.buf.starts_with(BOM) {
                let _ = self.buf.split_to(BOM.len());
            }
            self.started = true;
        }
        while let Some(event) = self.next_event(at_end) {
            events.push(event);
        }
        if at_end {
            if self.buf.ends_with(b"\n") || self.buf.ends_with(b"\r") {
                events.push(self.buf.split().freeze());
            }
            self.buf.clear();
        }
    }
}

/// The length of the line ending at the start of `b`: `\r\n`, `\n` or `\r`,
/// or 0 for none. `None` if more bytes could change the answer.
fn line_end(b: &[u8], at_end: bool) -> Option<usize> {
    match b {
        [] => None,
        [b'\r'] => at_end.then_some(1),
        [b'\r', b'\n', ..] => Some(2),
        [b'\r' | b'\n', ..] => Some(1),
        _ => Some(0),
    }
}

/// A comment, which clients skip, such as a quiet stream's opening bytes
/// (`respan_gateway_http::Reply::open`): a `:` line for each line of
/// `text`, so no text can start a field or end its event.
pub fn comment(text: &str) -> String {
    let mut out = String::new();
    field(&mut out, "", text);
    out.push('\n');
    out
}

/// An event of `data`, named `name` if given: each line of either, however
/// it ends, a line of its field, so no value can end its event or start
/// another field. Data of several lines is one value to a reader; a name of
/// several, its last line.
pub fn event(name: Option<&str>, data: impl std::fmt::Display) -> String {
    let mut out = String::new();
    if let Some(name) = name {
        field(&mut out, "event", name);
    }
    field(&mut out, "data", &data.to_string());
    out.push('\n');
    out
}

/// Write `value` as lines of the field `name`, one per line it holds.
fn field(out: &mut String, name: &str, value: &str) {
    for line in value.split("\r\n").flat_map(|l| l.split(['\n', '\r'])) {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(line);
        out.push('\n');
    }
}

/// An event's `data:` as JSON.
pub fn event_json(raw: &[u8]) -> Option<Value> {
    serde_json::from_str(&event_data(raw)?).ok()
}

/// The `data:` of an event, lines joined, if it has any.
pub fn event_data(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let data: Vec<&str> = text
        .split(['\n', '\r'])
        .filter_map(|l| l.strip_prefix("data:").or((l == "data").then_some("")))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    (!data.is_empty()).then(|| data.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Splitter = Parser;

    fn frames() -> Splitter {
        Parser::new(None)
    }

    fn splitter(stream: &[u8]) -> Splitter {
        let mut d = frames();
        d.buf.extend_from_slice(stream);
        d
    }

    #[test]
    fn every_legal_line_ending_ends_an_event() {
        for (stream, first) in [
            (&b"data: a\n\ndata: b"[..], &b"data: a\n\n"[..]),
            (b"data: a\r\n\r\ndata: b", b"data: a\r\n\r\n"),
            (b"data: a\r\rdata: b", b"data: a\r\r"),
            (b"data: a\n\r\ndata: b", b"data: a\n\r\n"),
        ] {
            let mut d = splitter(stream);
            assert_eq!(d.next_event(false).as_deref(), Some(first));
            assert_eq!(d.next_event(false), None);
        }
    }

    #[test]
    fn a_last_bare_cr_ends_its_event_when_the_body_ends() {
        let mut d = Parser::new(None);
        let mut out = Vec::new();
        out.extend(
            d.push(Bytes::from_static(b"data: a\r\rdata: b\r\r"))
                .unwrap_or_default(),
        );
        assert_eq!(out.len(), 1);
        out.extend(d.finish());
        assert_eq!(&out[1][..], b"data: b\r\r");
    }

    #[test]
    fn a_last_event_of_whole_lines_is_kept_and_a_cut_one_dropped() {
        for (body, kept) in [
            (&b"data: [DONE]\n"[..], Some(&b"data: [DONE]\n"[..])),
            (b"data: [DO", None),
        ] {
            let mut d = Parser::new(None);
            let mut out = Vec::new();
            out.extend(d.push(Bytes::from_static(body)).unwrap_or_default());
            out.extend(d.finish());
            assert_eq!(out.first().map(|b| &b[..]), kept);
        }
    }

    #[test]
    fn an_event_is_named_if_asked_and_its_data_reads_as_json() {
        assert_eq!(event(Some("message"), 1), "event: message\ndata: 1\n\n");
        assert_eq!(
            event_json(event(None, "{\"a\":2}").as_bytes()),
            Some(serde_json::json!({"a": 2}))
        );
    }

    #[test]
    fn a_bare_data_line_is_empty_data() {
        assert_eq!(event_data(b"data\n\n").as_deref(), Some(""));
    }

    // A byte order mark that starts a stream, however its frames split it,
    // is no part of its first line; one later is data.
    #[test]
    fn a_leading_byte_order_mark_is_dropped() {
        let stream = b"\xEF\xBB\xBFdata: a\n\n\xEF\xBB\xBFdata: b\n\n";
        for cut in 0..=stream.len() {
            let mut d = Parser::new(None);
            let mut out = Vec::new();
            for part in [&stream[..cut], &stream[cut..]] {
                out.extend(d.push(Bytes::copy_from_slice(part)).unwrap_or_default());
            }
            let data: Vec<_> = out.iter().map(|e| event_data(e)).collect();
            assert_eq!(data, [Some("a".to_owned()), None], "cut at {cut}");
            let second = String::from_utf8_lossy(&out[1]);
            assert_eq!(second.trim_end(), "\u{feff}data: b", "cut at {cut}");
        }
        // A body that only begins like one keeps its bytes.
        let mut d = Parser::new(None);
        assert!(
            d.push(Bytes::from_static(b"\xEF\xBB"))
                .is_ok_and(|e| e.is_empty())
        );
        assert!(
            d.push(Bytes::from_static(b"x\n\n"))
                .is_ok_and(|e| e.len() == 1)
        );
    }

    // No name, data or comment can end its event or start a field.
    #[test]
    fn a_value_of_several_lines_starts_no_field() {
        let injected = "x\r\ndata: forged\rid: 9\n";
        assert_eq!(
            event(Some(injected), 1),
            "event: x\nevent: data: forged\nevent: id: 9\nevent: \ndata: 1\n\n"
        );
        assert_eq!(comment(injected), ": x\n: data: forged\n: id: 9\n: \n\n");
        let mut d = Parser::new(None);
        let mut events = d
            .push(Bytes::from(event(None, injected) + &comment(injected)))
            .unwrap_or_default();
        events.extend(d.finish());
        assert_eq!(events.len(), 2);
        assert_eq!(
            event_data(&events[0]).as_deref(),
            Some("x\ndata: forged\nid: 9\n")
        );
        assert_eq!(event_data(&events[1]), None);
    }

    #[test]
    fn data_of_several_lines_is_an_event_of_several_data_lines() {
        assert_eq!(event(None, "not\njson"), "data: not\ndata: json\n\n");
        let again = event(None, "a\n\ndata: b");
        assert_eq!(again, "data: a\ndata: \ndata: data: b\n\n");
        assert_eq!(
            event_data(event(None, "a\r\nb").as_bytes()).as_deref(),
            Some("a\nb")
        );
    }

    #[test]
    fn a_split_crlf_waits_for_its_second_half() {
        let mut d = splitter(b"data: a\r\n\r");
        assert_eq!(d.next_event(false), None);
        d.buf.extend_from_slice(b"\n");
        assert_eq!(
            d.next_event(false).as_deref(),
            Some(&b"data: a\r\n\r\n"[..])
        );
    }

    #[test]
    fn a_stream_split_at_any_byte_decodes_every_event_once() {
        let stream = b"data: {\"a\":1}\n\nevent: x\r\ndata: {\"b\":2}\r\n\r\ndata: [DONE]\n\n";
        for cut in 0..=stream.len() {
            let mut d = Parser::new(None);
            let mut out = Vec::new();
            for part in [&stream[..cut], &stream[cut..]] {
                out.extend(d.push(Bytes::copy_from_slice(part)).unwrap_or_default());
            }
            let decoded: Vec<u8> = out.iter().flat_map(|b| b.to_vec()).collect();
            assert_eq!((out.len(), &decoded[..]), (3, &stream[..]), "cut at {cut}");
        }
    }

    #[test]
    fn an_event_is_held_up_to_the_limit() {
        let mut d = Parser::new(Some(8));
        assert_eq!(
            d.push(Bytes::from_static(b"data: a\n\n"))
                .map(|e| e.len())
                .ok(),
            Some(1)
        );
        assert!(d.push(Bytes::from_static(b"data: 1")).is_ok());
        let over = d.push(Bytes::from_static(b"23")).err();
        assert!(over.is_some_and(|f| f.is(respan_gateway_core::FaultKind::Provider)));
    }

    #[test]
    fn complete_events_are_appended_before_a_tail_limit_error() {
        let mut d = Parser::new(Some(8));
        let mut out = vec![Bytes::from_static(b"earlier")];
        let result = d.push_into(Bytes::from_static(b"data: a\n\ndata: 123"), &mut out);
        assert!(result.is_err());
        assert_eq!(
            out,
            [
                Bytes::from_static(b"earlier"),
                Bytes::from_static(b"data: a\n\n")
            ]
        );
    }

    #[test]
    fn appending_events_keeps_the_same_framing_at_every_byte_boundary() {
        let wire = b"\xEF\xBB\xBFdata: a\n\ndata: b\r\n\r\ndata: c\n";
        let expected = [
            Bytes::from_static(b"data: a\n\n"),
            Bytes::from_static(b"data: b\r\n\r\n"),
            Bytes::from_static(b"data: c\n"),
        ];
        for cut in 0..=wire.len() {
            let mut appended = Parser::new(None);
            let mut actual = Vec::new();
            for frame in [&wire[..cut], &wire[cut..]] {
                appended
                    .push_into(Bytes::copy_from_slice(frame), &mut actual)
                    .expect("append");
            }
            actual.extend(appended.finish());
            assert_eq!(actual, expected, "cut {cut}");
        }
    }
}
