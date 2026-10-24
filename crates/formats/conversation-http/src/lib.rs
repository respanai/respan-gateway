#![warn(missing_docs)]
//! The conversation kind over HTTP: JSON requests, JSON or SSE responses.
//! A format supplies its [`Schema`], its names and its JSON both ways; the
//! HTTP around it, on either edge, is shared.
//!
//! - [`openai`]: OpenAI Chat Completions.
//! - [`anthropic`]: Anthropic Messages.
//! - [`responses`]: OpenAI Responses.
//! - [`gemini`]: Google Gemini's `generateContent`.
//!
//! A format is its spec: what a provider sends beyond it is kept native, at
//! the place it appeared, for a plugin to read (each module says where), or
//! for a vendor's dialect of it ([`respan_gateway_http::dialect`]). What is
//! native to one format reaches only a client of that format: another
//! drops it from a response.

pub mod fields;
mod schema;
pub use schema::{Asked, Schema, StreamResponse, merge_details, read_provider_error};

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod responses;
