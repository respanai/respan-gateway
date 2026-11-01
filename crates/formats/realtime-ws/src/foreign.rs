//! What a format does with another format's natives: it drops them, as what
//! has meaning beyond its format belongs in the kind. A writer to a provider
//! says what it dropped; a writer to a client has nothing to say of it.

use respan_gateway_conversation::Content;
use respan_gateway_core::NativeFields;
use respan_gateway_realtime::{Input, Output, Turns};

/// Says what of another format `writer` goes without.
fn warn(writer: &str, from: &str, what: impl std::fmt::Debug) {
    eprintln!("warning: a session in {writer} drops {from}'s {what:?}");
}

/// `native`, unless it is another format's, which is dropped with a warning.
fn mine(writer: &str, native: &mut Option<NativeFields>) {
    if let Some(n) = native.take_if(|n| *n.format != *writer) {
        warn(writer, &n.format, n.fields.keys().collect::<Vec<_>>());
    }
}

/// `event` for `writer`'s provider, without other formats' natives; `None`
/// if nothing of it is left.
pub(crate) fn admit(writer: &str, event: &Input) -> Option<Input> {
    let mut event = event.clone();
    match &mut event {
        Input::Session(s) => {
            mine(writer, &mut s.native);
            if let Some(Turns::Server(vad)) = &mut s.turns {
                mine(writer, &mut vad.native);
            }
            for tool in s.tools.iter_mut().flatten() {
                mine(writer, &mut tool.native);
            }
        }
        Input::Message { message, native } => {
            mine(writer, &mut message.native);
            mine(writer, native);
            let had = !message.content.is_empty();
            message.content.retain_mut(|part| match part {
                Content::Native(n) if *n.format != *writer => {
                    warn(writer, &n.format, "message part");
                    false
                }
                Content::Text { native, .. } => {
                    mine(writer, native);
                    true
                }
                _ => true,
            });
            if had && message.content.is_empty() {
                return None;
            }
        }
        Input::ToolResult(result) => mine(writer, &mut result.native),
        // Many a second, so unsaid.
        Input::Audio { native, .. } => _ = native.take_if(|n| *n.format != *writer),
        Input::Commit { native } | Input::Respond { native } | Input::Cancel { native } => {
            mine(writer, native);
        }
        Input::Native(n) if *n.format != *writer => {
            warn(writer, &n.format, "event");
            return None;
        }
        _ => {}
    }
    Some(event)
}

/// `event` with every other format's native fields dropped, for `format`'s
/// client; `None` for another format's native event.
pub(crate) fn strip(format: &str, event: &Output) -> Option<Output> {
    let mut event = event.clone();
    let mine = |native: &mut Option<NativeFields>| {
        native.take_if(|n| *n.format != *format);
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
