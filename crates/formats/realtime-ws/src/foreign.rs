//! What a format does with another format's natives: it cannot read them, so
//! it drops them, as what has meaning beyond its format belongs in the kind.
//! A format's own natives are those of its key `K`; another's native fields
//! read as none, and its native events and parts are dropped here. A writer to
//! a provider says what it dropped; a writer to a client has nothing to say of
//! it.

use respan_gateway_conversation::Content;
use respan_gateway_core::{Key, Native};
use respan_gateway_realtime::{Input, Output};

/// Whether `native` is another format's than the one of key `K`.
fn foreign<K: Key>(native: &Native) -> bool {
    native.value::<K>().is_none()
}

/// Says what of another format `writer` goes without.
fn warn(writer: &str, what: &str) {
    eprintln!("warning: a session in {writer} drops another format's {what}");
}

/// `event` for `writer`'s provider, without other formats' native events and
/// parts; `None` if nothing of it is left.
// WARN: other formats' native fields are dropped by the writer, silently.
pub(crate) fn admit<K: Key>(writer: &str, event: &Input) -> Option<Input> {
    let mut event = event.clone();
    match &mut event {
        Input::Message { message, .. } => {
            let had = !message.content.is_empty();
            message.content.retain(|part| match part {
                Content::Native(n) if foreign::<K>(n) => {
                    warn(writer, "message part");
                    false
                }
                _ => true,
            });
            if had && message.content.is_empty() {
                return None;
            }
        }
        Input::Native(n) if foreign::<K>(n) => {
            warn(writer, "event");
            return None;
        }
        _ => {}
    }
    Some(event)
}

/// `event` for the client of the format of key `K`: `None` for another
/// format's native event. Another's native fields its writer drops.
pub(crate) fn strip<K: Key>(event: &Output) -> Option<Output> {
    match event {
        Output::Native(native) if foreign::<K>(native) => None,
        _ => Some(event.clone()),
    }
}
