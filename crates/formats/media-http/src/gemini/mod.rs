//! Google's media models as the Gemini API serves them, over HTTP: Veo
//! ([`GeminiVeo`]), and its image models through `generateContent`
//! ([`GeminiImages`]), speech ([`GeminiSpeech`]) and transcriptions
//! ([`GeminiTranscriptions`]); and Veo
//! as Vertex AI serves it ([`VertexVeo`]). A Gemini provider's base URL includes the version,
//! `https://generativelanguage.googleapis.com/v1beta`, and its transport the
//! key, `x-goog-api-key`.

mod images;
mod speech;
mod transcriptions;
mod veo;
mod vertex_veo;

pub use images::GeminiImages;
pub use speech::GeminiSpeech;
pub use transcriptions::GeminiTranscriptions;
pub use veo::GeminiVeo;
pub use vertex_veo::VertexVeo;

use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, response::Parts as ResponseHead};

/// Whether `s` is a path segment that every reader reads as it is.
fn is_segment(s: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    !s.is_empty() && s != "." && s != ".." && s.chars().all(plain)
}

/// A client's refusal, as Gemini's `generateContent` writes it:
/// `{"error": {code, message, status}}`.
fn refusal(fault: &Fault, request: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    GeminiGenerate.refusal(fault, request, head)
}
