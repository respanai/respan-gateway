//! What vendors of decisions share in their dialects: System One's
//! contract served at `/systemone` ([`SystemOne`]).

use respan_gateway_core::Fault;
use respan_gateway_decision::{Input, Output};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;

use crate::{Dialect, Rewrite};

/// Decisions in System One's contract, served at `/systemone`, the name
/// its clients call: a dialect of `decisions`.
#[derive(Debug, Clone, Copy)]
pub struct SystemOne;

impl Dialect for SystemOne {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/systemone")?;
        Ok(None)
    }
}
