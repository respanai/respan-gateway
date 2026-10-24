#![warn(missing_docs)]
//! The media kinds over HTTP:
//!
//! - [`openai`]: OpenAI's media endpoints, as OpenAI and compatible vendors
//!   serve them, one format per kind.
//! - [`gemini`]: Google's Veo and image models, as the Gemini API serves
//!   them, of the video and image kinds.
//! - [`empiriolabs`]: EmpirioLabs AI's video and transcription APIs, which
//!   answer as jobs, providers of those kinds.
//! - [`dialect`]: a format as a vendor speaks it, its departures from the
//!   spec in the vendor's [`Dialect`](dialect::Dialect).

pub mod dialect;
mod edge;
pub mod empiriolabs;
pub mod gemini;
pub mod openai;
