//! Qwen, on Alibaba Cloud Model Studio's international site: Chat
//! Completions as OpenAI serves them, speech, transcriptions and videos on
//! DashScope's own API, and Realtime in OpenAI's beta protocol, which its
//! Omni Realtime models speak.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;
use respan_gateway_media_http::dashscope::{
    DashScopeSpeech, DashScopeTranscriptions, DashScopeVideos,
};
use respan_gateway_realtime as realtime;
use respan_gateway_realtime_ws::OpenAiRealtimeBeta;

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor};

/// Where its OpenAI-compatible API is.
const BASE_URL: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";

/// Where DashScope's own API is.
const DASHSCOPE_URL: &str = "https://dashscope-intl.aliyuncs.com/api/v1";

/// Where its Realtime is.
const REALTIME_URL: &str = "wss://dashscope-intl.aliyuncs.com/api-ws/v1";

/// The rate its Realtime hears PCM at, and no other, in Hz.
const HEARD: u32 = 16_000;

/// Qwen.
pub const VENDOR: Vendor = Vendor {
    id: "qwen",
    key_env: &["DASHSCOPE_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&DashScopeSpeech, Some(DASHSCOPE_URL)),
        (&DashScopeTranscriptions, Some(DASHSCOPE_URL)),
        (&DashScopeVideos, Some(DASHSCOPE_URL)),
        (
            &Dialected::new(OpenAiRealtimeBeta::hearing(HEARD), QwenRealtime),
            Some(REALTIME_URL),
        ),
    ],
    ..Vendor::DEFAULT
};

/// Its Realtime: it hears PCM at 16 kHz and speaks it at 24 kHz, and no
/// other, so a session names neither format, which it spells its own way,
/// and the user's audio is converted to the rate it hears.
#[derive(Debug, Clone, Copy)]
pub struct QwenRealtime;

impl Dialect for QwenRealtime {
    type Input = realtime::Input;
    type Output = realtime::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<realtime::Input>>> {
        Some(Box::new(unnamed_audio))
    }
}

/// A session as Qwen takes it: its audio formats unnamed.
fn unnamed_audio(event: &mut realtime::Input) -> Result<(), respan_gateway_core::Fault> {
    if let realtime::Input::Session(session) = event {
        session.input_audio = None;
        session.output_audio = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_realtime::{AudioFormat, Input, Session};

    #[test]
    fn a_session_names_no_audio_format() {
        let mut session = Session::default();
        session.input_audio = Some(AudioFormat::pcm16(24_000));
        session.output_audio = Some(AudioFormat::pcm16(24_000));
        let mut event = Input::Session(session);
        unnamed_audio(&mut event).expect("edited");
        let Input::Session(session) = event else {
            panic!("a session");
        };
        assert_eq!(session.input_audio, None);
        assert_eq!(session.output_audio, None);
    }
}
