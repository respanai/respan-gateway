//! MiniMax's media models on its own API, over HTTP: video generation
//! ([`MiniMaxVideos`]) and speech synthesis ([`MiniMaxSpeech`]), provider
//! formats only. A provider's key is a bearer token; its base URL is the
//! API's host for videos, `https://api.minimax.io`, and with the version
//! for speech, `https://api.minimax.io/v1`.

mod speech;
mod videos;

pub use speech::MiniMaxSpeech;
pub use videos::MiniMaxVideos;
