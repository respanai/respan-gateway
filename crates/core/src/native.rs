//! What only its own format reads: a value of a type private to that format.

use std::any::Any;
use std::fmt::{self, Debug};
use std::sync::Arc;

/// What a value holds that only its own format reads: fields the kind has no
/// place for, or a whole event or part it has no form for, as a value of a
/// type private to that format. No other format can name the type, so none
/// can read it: what has meaning across formats belongs in the kind.
///
/// It is never edited in place: a writer builds a new one in its place.
/// Two are equal if both are empty, or both are the one value.
#[derive(Clone, Default)]
pub struct Native(Option<Arc<dyn Any + Send + Sync>>);

impl Native {
    /// `value`, as its type's format holds it.
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Native(Some(Arc::new(value)))
    }

    /// The value it holds, if it is a `T`.
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.0.as_deref()?.downcast_ref()
    }

    /// Whether it holds nothing, of any format.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

impl Debug for Native {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_empty() {
            "Native"
        } else {
            "Native(..)"
        })
    }
}

impl PartialEq for Native {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for Native {}
