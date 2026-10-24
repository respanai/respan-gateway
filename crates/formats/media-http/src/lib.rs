#![warn(missing_docs)]
//! The media kinds over HTTP:
//!
//! - [`openai`]: OpenAI's media endpoints, as OpenAI and compatible vendors
//!   serve them, one format per kind.
//! - [`gemini`]: Google's Veo and image models, as the Gemini API serves
//!   them, of the video and image kinds.
//! - [`openrouter`]: OpenRouter's video generation, of the video kind.
//! - [`dialect`]: a format as a vendor speaks it, its departures from the
//!   spec in the vendor's [`Dialect`](dialect::Dialect).

pub mod dialect;
mod edge;
pub mod gemini;
pub mod openai;
pub mod openrouter;
