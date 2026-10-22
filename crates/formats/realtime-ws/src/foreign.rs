//! What a format does with another format's natives: its provider's writer
//! drops those that cannot change the answer and refuses the rest; its
//! client's writer drops them all, as what the model said is said, but
//! refuses an event that tells of what the model said in a way it has no
//! place for.
//!
//! Which natives cannot change the answer, or tell of what the model said,
//! is for the format they are of to say, of its own alone ([`Harmless`]):
//! no format knows another's fields. A format that says nothing has every
//! native input refused and every native output dropped.

use respan_gateway_core::{Fault, NativeFields, NativeValue};
use respan_gateway_realtime::{Input, Output, Session, Turns};
use serde_json::{Map, Value};

use crate::json::{untranslatable_input, untranslatable_output};

/// Whether a format's own native input event may be dropped by a writer of
/// another, which has no place for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drop {
    /// It cannot change the answer.
    Always,
    /// It cannot unless user audio was sent since the user's last turn,
    /// such as a clearing of that audio.
    Unheard,
    /// It could.
    Never,
}

/// What a format says of its own natives that another may drop: each a
/// path, or the start of one, such as `session.tracing`; a null field is
/// always dropped.
#[derive(Debug)]
pub(crate) struct Harmless {
    /// Of a session's native.
    pub(crate) session: &'static [&'static str],
    /// Of its voice detection's.
    pub(crate) vad: &'static [&'static str],
    /// Of a message's own.
    pub(crate) message: &'static [&'static str],
    /// Of any other event's.
    pub(crate) event: &'static [&'static str],
    /// Whether one of its native input events may be dropped, and its name,
    /// as a refusal says it.
    pub(crate) input: fn(&Value) -> (Drop, String),
    /// Whether one of its native output events tells of what the model said
    /// (as a provider-run tool's), which a client of another format is
    /// refused rather than not told.
    pub(crate) said: fn(&Value) -> bool,
}

/// What `format` says of its natives, if it is one of this crate's.
fn harmless(format: &str) -> Option<&'static Harmless> {
    crate::session::SPECS
        .iter()
        .find(|spec| spec.name == format)
        .and_then(|spec| spec.harmless)
}

/// `event` for `writer`'s provider with other formats' native fields
/// dropped where their format says they cannot change the answer; refused,
/// naming the first, if one could. A tool's are all refused, and a native
/// event is [`native_input`]'s.
pub(crate) fn admit(writer: &'static str, event: &Input) -> Result<Input, Fault> {
    let mut event = event.clone();
    let drop = |native: &mut Option<NativeFields>,
                field: fn(&Harmless) -> &'static [&'static str]| {
        drop(writer, native, field)
    };
    match &mut event {
        Input::Session(s) => session(writer, s)?,
        Input::Message { message, native } => {
            drop(&mut message.native, |h| h.message)?;
            drop(native, |h| h.event)?;
        }
        Input::ToolResult(result) => drop(&mut result.native, |h| h.event)?,
        Input::Audio { native, .. }
        | Input::Commit { native }
        | Input::Respond { native }
        | Input::Cancel { native } => drop(native, |h| h.event)?,
        _ => {}
    }
    Ok(event)
}

/// Another format's native input event, which `writer` has no place for:
/// `Ok` if it is dropped, as its format says it may be; `heard` says
/// whether user audio was sent since the user's last turn.
pub(crate) fn native_input(
    writer: &'static str,
    event: &NativeValue,
    heard: bool,
) -> Result<(), Fault> {
    let (drop, name) = match harmless(&event.format) {
        Some(h) => (h.input)(&event.value),
        None => (Drop::Never, "another format's event".to_owned()),
    };
    match drop {
        Drop::Always => Ok(()),
        Drop::Unheard if !heard => Ok(()),
        _ => Err(untranslatable_input(writer, &format!("`{name}`"))),
    }
}

fn session(writer: &'static str, s: &mut Session) -> Result<(), Fault> {
    drop(writer, &mut s.native, |h| h.session)?;
    if let Some(Turns::Server(vad)) = &mut s.turns {
        drop(writer, &mut vad.native, |h| h.vad)?;
    }
    for tool in s.tools.iter_mut().flatten() {
        drop(writer, &mut tool.native, |_| &[])?;
    }
    Ok(())
}

fn drop(
    writer: &'static str,
    native: &mut Option<NativeFields>,
    field: fn(&Harmless) -> &'static [&'static str],
) -> Result<(), Fault> {
    let Some(fields) = native.as_ref().filter(|n| *n.format != *writer) else {
        return Ok(());
    };
    let droppable = harmless(&fields.format).map_or(&[][..], field);
    if let Some(path) = kept(&fields.fields, "", droppable) {
        return Err(untranslatable_input(writer, &format!("`{path}`")));
    }
    *native = None;
    Ok(())
}

/// The path of the first field of `fields`, under `at`, that is not null
/// and that `droppable` does not name: the innermost, as a refusal names it.
fn kept(fields: &Map<String, Value>, at: &str, droppable: &[&str]) -> Option<String> {
    fields.iter().find_map(|(key, value)| {
        let path = format!("{at}{key}");
        if value.is_null() || droppable.contains(&path.as_str()) {
            return None;
        }
        match value {
            Value::Object(fields) if !fields.is_empty() => {
                kept(fields, &format!("{path}."), droppable)
            }
            _ => Some(path),
        }
    })
}

/// `event` with every other format's native fields dropped, for `format`'s
/// client; `None` for another format's native event, refused if its format
/// says it tells of what the model said.
pub(crate) fn strip(format: &'static str, event: &Output) -> Result<Option<Output>, Fault> {
    if let Output::Native(native) = event
        && *native.format != *format
        && harmless(&native.format).is_some_and(|h| (h.said)(&native.value))
    {
        return Err(untranslatable_output(
            format,
            "another format's account of what the model said",
        ));
    }
    Ok(strip_fields(format, event))
}

fn strip_fields(format: &str, event: &Output) -> Option<Output> {
    let mut event = event.clone();
    let mine = |native: &mut Option<NativeFields>| {
        if native.as_ref().is_some_and(|n| *n.format != *format) {
            *native = None;
        }
    };
    match &mut event {
        Output::Opened(s) | Output::Updated(s) => {
            mine(&mut s.native);
            if let Some(Turns::Server(vad)) = &mut s.turns {
                mine(&mut vad.native);
            }
            for tool in s.tools.iter_mut().flatten() {
                mine(&mut tool.native);
            }
        }
        Output::SpeechStarted { native, .. }
        | Output::SpeechStopped { native, .. }
        | Output::InputTranscript { native, .. }
        | Output::ResponseStart { native, .. }
        | Output::Audio { native, .. }
        | Output::Text { native, .. }
        | Output::Transcript { native, .. }
        | Output::ToolCall { native, .. }
        | Output::ResponseDone { native, .. }
        | Output::Error { native, .. } => mine(native),
        Output::Native(native) if *native.format != *format => return None,
        _ => {}
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_field_is_dropped_where_named_or_null_and_else_refused() {
        let fields = |v: Value| v.as_object().cloned().expect("object");
        let droppable = ["event_id", "session.tracing"];
        let kept = |v| kept(&fields(v), "", &droppable);
        assert_eq!(
            kept(json!({"event_id": "e", "session": {"tracing": "auto"}})),
            None
        );
        assert_eq!(kept(json!({"session": {"prompt": null}})), None);
        assert_eq!(
            kept(json!({"session": {"tracing": 1, "prompt": {"id": "p"}}})),
            Some("session.prompt.id".into())
        );
        assert_eq!(kept(json!({"response": {}})), Some("response".into()));
    }
}
