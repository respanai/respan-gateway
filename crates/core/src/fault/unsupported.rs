use std::borrow::Cow;
use std::fmt;

use super::{ErrorCode, Fault, FaultKind};

/// What a provider or the gateway does not take, which a fault is made of
/// (`?` or `Fault::from`): its message names what and where, and why it is
/// refused gives the fault's kind and code. The fault keeps it as an
/// extension, so an edge can tell a provider's lasting limit from a gap of
/// the gateway's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported {
    why: Why,
    what: Cow<'static, str>,
    to: Option<Cow<'static, str>>,
    fix: Option<Cow<'static, str>>,
}

/// Why something is [`Unsupported`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Why {
    /// The provider never takes it, as its own docs say: the client's to
    /// change, coded `unsupported`.
    Lasting,
    /// The gateway does not yet write the client's input in the provider's
    /// terms: the client's, coded `untranslatable`.
    Input,
    /// The gateway does not yet write the provider's answer in the client's
    /// terms: the gateway's, coded `untranslatable`.
    Output,
}

impl Unsupported {
    /// `what`, such as "a developer message", refused for `why`.
    pub fn new(why: Why, what: impl Into<Cow<'static, str>>) -> Self {
        Self {
            why,
            what: what.into(),
            to: None,
            fix: None,
        }
    }

    /// The provider or format that does not take it, by its own name.
    pub fn to(mut self, name: impl Into<Cow<'static, str>>) -> Self {
        self.to = Some(name.into());
        self
    }

    /// What the client can send instead.
    pub fn fix(mut self, fix: impl Into<Cow<'static, str>>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// Why it is refused.
    pub fn why(&self) -> Why {
        self.why
    }
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = &self.what;
        match (self.why, &self.to) {
            (Why::Lasting, Some(to)) => write!(f, "{to} does not support {what}")?,
            (Why::Lasting, None) => write!(f, "{what} is not supported")?,
            (_, Some(to)) => write!(f, "{what} cannot be translated to {to} yet")?,
            (Why::Input, None) => write!(f, "{what} cannot be translated to another format yet")?,
            (_, None) => write!(f, "{what} cannot be translated to this format yet")?,
        }
        match &self.fix {
            Some(fix) => write!(f, "; {fix}"),
            None => Ok(()),
        }
    }
}

impl From<Unsupported> for Fault {
    fn from(u: Unsupported) -> Self {
        let (kind, code) = match u.why {
            Why::Lasting => (FaultKind::Client, "unsupported"),
            Why::Input => (FaultKind::Client, "untranslatable"),
            Why::Output => (FaultKind::Gateway, "untranslatable"),
        };
        Fault::of(kind, u.to_string())
            .with(ErrorCode(code.into()))
            .with(u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn why_gives_the_kind_code_and_message() {
        let lasting = Unsupported::new(Why::Lasting, "a developer message")
            .to("OpenAI Realtime")
            .fix("give it the system role");
        let input = Unsupported::new(Why::Input, "top_k").to("openai-chat");
        let output = Unsupported::new(Why::Output, "a tool call");
        for (u, kind, code, message) in [
            (
                lasting,
                FaultKind::Client,
                "unsupported",
                "OpenAI Realtime does not support a developer message; give it the system role",
            ),
            (
                input,
                FaultKind::Client,
                "untranslatable",
                "top_k cannot be translated to openai-chat yet",
            ),
            (
                output,
                FaultKind::Gateway,
                "untranslatable",
                "a tool call cannot be translated to this format yet",
            ),
        ] {
            let why = u.why();
            let fault = Fault::from(u);
            assert_eq!(
                (fault.kind(), fault.code(), fault.message()),
                (kind, code, message)
            );
            assert_eq!(fault.get::<Unsupported>().map(Unsupported::why), Some(why));
        }
    }
}
