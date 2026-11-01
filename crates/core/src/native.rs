//! Explicitly format-specific data carried through the gateway.

use std::sync::Arc;

use serde_json::{Map, Value};

/// Identity of a wire format, rather than a provider or deployment.
pub type FormatId = Arc<str>;

/// Additional semantic fields understood only by their originating format.
/// A foreign encoder drops them: a field whose meaning other formats share
/// belongs in the kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeFields {
    /// The wire format that defines these fields.
    pub format: FormatId,
    /// Provider-format fields, excluding transport and decoder bookkeeping.
    pub fields: Map<String, Value>,
}

impl NativeFields {
    /// Fields belonging to a known wire format.
    pub fn new(format: impl Into<FormatId>, fields: Map<String, Value>) -> Self {
        Self {
            format: format.into(),
            fields,
        }
    }

    /// Whether no native fields remain.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// An entire value whose meaning belongs to one wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeValue {
    /// The wire format that understands the value.
    pub format: FormatId,
    /// The value, shared when requests or events are cloned.
    pub value: Arc<Value>,
}

impl NativeValue {
    /// Preserve a value together with the identity needed to encode it safely.
    pub fn new(format: impl Into<FormatId>, value: impl Into<Value>) -> Self {
        Self {
            format: format.into(),
            value: Arc::new(value.into()),
        }
    }
}
