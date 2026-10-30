//! What a Responses request says: its input items, tools and choice, text
//! format, and media. What the kind has no form for, such as a tool the
//! provider runs or an item it stored, stays in the `native` of the request,
//! the message, the tool or the part.
//!
//! The input's items are the conversation's messages: consecutive items an
//! assistant produced (messages, function calls, reasoning) are one assistant
//! message, and consecutive function call outputs are one user message of
//! results. A message item's `id` and `status` are inert (kept under
//! [`INERT`], dropped by any other format); on an assistant message the item's
//! other fields are under `message`, on the first text part, which opens a new
//! message item when it carries them. An item the kind has no form for is a
//! message of the native role `item`, holding the item as a native part.

mod read;
mod settings;
mod write;

pub(super) use read::read_input;
pub(super) use settings::{
    decode_part, encode_part, read_format, read_reasoning, read_tools, write_format,
    write_reasoning, write_tools,
};
pub(super) use write::{INSTRUCTIONS, write_input};

use super::RESPONSES;
use crate::schema::Names;
use respan_gateway_conversation::Role;

/// Roles an input message can have.
const ROLES: Names<Role> = Names(
    &[
        ("system", Role::System),
        ("developer", Role::Developer),
        ("user", Role::User),
        ("assistant", Role::Assistant),
    ],
    RESPONSES,
);

/// The role of a message that holds an item the kind has no form for.
const ITEM_ROLE: &str = "item";

/// The key of an assistant message item's fields beyond its inert ones, on
/// its first text part.
const MESSAGE: &str = "message";
