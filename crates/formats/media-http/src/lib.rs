#![warn(missing_docs)]
//! The media kinds over HTTP:
//!
//! - [`openai`]: OpenAI's media endpoints, as OpenAI and compatible vendors
//!   serve them, one format per kind.
//! - [`gemini`]: Google's Veo and image models, as the Gemini API serves
//!   them, of the video and image kinds.
//! - [`bedrock`]: Amazon Bedrock's InvokeModel, a provider of the embedding
//!   kind.
//!
//! A vendor's departures from one of them are its dialect of it, laid over
//! it ([`respan_gateway_http::dialect`]).

pub mod bedrock;
mod edge;
pub mod gemini;
pub mod openai;
