//! StepFun: Chat Completions, speech and transcriptions, as OpenAI serves
//! them, and Realtime in OpenAI's beta protocol.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media_http::openai::{OpenAiSpeech, OpenAiTranscriptions};
use respan_gateway_realtime_ws::OpenAiRealtimeBeta;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.stepfun.ai/v1";

/// Where its Realtime is.
const REALTIME_URL: &str = "wss://api.stepfun.ai/v1";

/// StepFun.
pub const VENDOR: Vendor = Vendor {
    id: "stepfun",
    key_env: &["STEPFUN_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiSpeech, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (&OpenAiRealtimeBeta::DEFAULT, Some(REALTIME_URL)),
    ],
    ..Vendor::DEFAULT
};
