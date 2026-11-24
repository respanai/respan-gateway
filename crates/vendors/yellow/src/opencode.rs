//! OpenCode Zen: Chat Completions, Responses and Messages.
//!
//! The dialect carries Zen's allowlisted session headers across formats.
//! Either identity header (`x-opencode-session` or `x-session-id`) takes
//! precedence over a generated ID. The other headers carry session lineage,
//! affinity, project or client metadata and are forwarded independently.
//! With no identity header, Zen gets an ID derived from the remaining client
//! credentials, or a fresh UUID when none are available. The upstream key
//! configured on the provider is not used to derive the ID.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Extensions, Fault};
use respan_gateway_http::Credential;
use respan_gateway_http::http::header::HeaderValue;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::raw::Original;
use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const CHAT_URL: &str = "https://opencode.ai/zen/v1";
// Messages adds `/v1/messages` to this base.
const MESSAGES_URL: &str = "https://opencode.ai/inference/anthropic";

/// OpenCode Zen, with session affinity on each of its conversation formats.
pub const VENDOR: Vendor = Vendor {
    id: "opencode",
    key_env: &["OPENCODE_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenCode), Some(CHAT_URL)),
        (&Dialected::new(OpenAiResponses, OpenCode), Some(CHAT_URL)),
        (
            &Dialected::new(AnthropicMessages, OpenCode),
            Some(MESSAGES_URL),
        ),
    ],
    ..Vendor::DEFAULT
};

const SESSION: &str = "x-opencode-session";
const IDENTITY_HEADERS: &[&str] = &[SESSION, "x-session-id"];
const COMPANION_HEADERS: &[&str] = &[
    "x-opencode-session-id",
    "x-opencode-parent-session-id",
    "x-parent-session-id",
    "x-session-affinity",
    "x-opencode-project",
    "x-opencode-client",
];

/// OpenCode Zen's session headers, added after the format builds the request.
#[derive(Debug, Clone, Copy)]
pub struct OpenCode;

impl Dialect for OpenCode {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        // The HTTP request hook receives the call's core extensions as a value.
        let call = head.extensions.get::<Extensions>();
        let original = call
            .and_then(Extensions::get::<Original>)
            .and_then(Original::get);
        let named = original.is_some_and(|request| {
            IDENTITY_HEADERS
                .iter()
                .any(|name| request.headers().contains_key(*name))
        });
        if let Some(request) = original {
            for &name in IDENTITY_HEADERS.iter().chain(COMPANION_HEADERS) {
                for value in request.headers().get_all(name) {
                    head.headers.append(name, value.clone());
                }
            }
        }
        if !named {
            let session = call
                .and_then(Extensions::get::<Credential>)
                .map_or_else(|| Uuid::new_v4().to_string(), session_of);
            let value = HeaderValue::from_str(&session)
                .map_err(|err| Fault::internal("invalid OpenCode session ID").with_source(err))?;
            head.headers.insert(SESSION, value);
        }
        Ok(None)
    }
}

fn session_of(credential: &Credential) -> String {
    // Header map and query ordering do not affect the ID. Lengths and tags
    // keep header names, values and query parameters distinct in the hash.
    let mut headers: Vec<_> = credential
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect();
    headers.sort_unstable();
    let mut query: Vec<_> = credential.query().collect();
    query.sort_unstable();
    let mut digest = Sha256::new();
    digest.update(b"opencode-session\0");
    let mut field = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    };
    for (name, value) in headers {
        field(b"header");
        field(name.as_bytes());
        field(value);
    }
    for param in query {
        field(b"query");
        field(param.as_bytes());
    }
    format!("gw-{:x}", digest.finalize())
}
