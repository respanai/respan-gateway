//! EmpirioLabs AI's own APIs over HTTP, of the kinds it answers as jobs:
//! [`EmpirioLabsVideos`], a provider of the
//! [`video`](respan_gateway_media::video) kind, and
//! [`EmpirioLabsTranscriptions`], of the
//! [`transcription`](respan_gateway_media::transcription) kind. A
//! provider's base URL includes the version, `https://api.empiriolabs.ai/v1`.
//! A job of either is polled at `GET /jobs/{id}`, the one endpoint of its
//! asynchronous kinds.

mod transcriptions;
mod videos;

pub use transcriptions::EmpirioLabsTranscriptions;
pub use videos::EmpirioLabsVideos;

/// The path a job `id` is polled at.
fn job_path(id: &str) -> String {
    format!("/jobs/{id}")
}

/// Whether `s` is a path segment that every reader reads as it is: of
/// letters, digits, `-`, `_` and `.`, and no `.` or `..`.
fn is_segment(s: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    !s.is_empty() && s != "." && s != ".." && s.chars().all(plain)
}
