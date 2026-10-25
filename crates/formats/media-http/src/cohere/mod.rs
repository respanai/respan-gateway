//! Cohere's media endpoints over HTTP, as Cohere and compatible vendors
//! serve them: [`CohereRerank`]. A provider's base URL includes the version
//! (`https://api.cohere.com/v2`), and its key is a bearer token.

mod rerank;

pub use rerank::CohereRerank;
