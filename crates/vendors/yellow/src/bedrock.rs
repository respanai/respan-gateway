//! Amazon Bedrock, at a region's `bedrock-runtime`, with a Bedrock API key:
//! Claude on Bedrock's Anthropic Messages (`/anthropic/v1/messages`), every
//! other model on Converse ([`BedrockConverse`]), which serves Claude too,
//! and Cohere and Titan embeddings on InvokeModel ([`BedrockEmbeddings`]).
//! An AWS key pair signs calls outside the gateway, by a SigV4 proxy at the
//! base URL, as an HTTP proxy is. Its models are its account's: none are
//! listed.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::bedrock::BedrockConverse;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;
use respan_gateway_media_http::bedrock::BedrockEmbeddings;

use respan_gateway_vendor_kit::{
    Ask, BoxFuture, Dialect, Dialected, KeyIn, Model, Models, Rewrite, Vendor, VendorProvider,
};

/// Where a region's runtime is: `AWS_REGION`'s, unless a host says.
const BASE_URL: &str = "https://bedrock-runtime.{AWS_REGION}.amazonaws.com";

/// Where its Anthropic Messages are, which take Anthropic's body as it is.
const MESSAGES_PATH: &str = "/anthropic/v1/messages";

/// Amazon Bedrock: its key a Bedrock API key.
pub const VENDOR: Vendor = Vendor {
    id: "bedrock",
    key_env: &["AWS_BEARER_TOKEN_BEDROCK"],
    formats: &[
        (&BedrockConverse, Some(BASE_URL)),
        (&Dialected::new(AnthropicMessages, Messages), Some(BASE_URL)),
        (&BedrockEmbeddings, Some(BASE_URL)),
    ],
    models: no_models,
    format_for: Some(format_for),
};

/// Anthropic's Messages as Bedrock serves them: under `/anthropic`, the key a
/// bearer token.
#[derive(Debug, Clone, Copy)]
pub struct Messages;

impl Dialect for Messages {
    type Input = Input;
    type Output = Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Bearer)
    }

    fn request(&self, head: &mut RequestHead, _: Option<&Bytes>) -> Result<Option<Rewrite>, Fault> {
        set_path(head, MESSAGES_PATH)?;
        Ok(None)
    }
}

/// Claude (`anthropic.claude-…`, or a profile of it such as `us.anthropic.…`)
/// goes to Messages, whatever the client speaks; any other model to Converse.
fn format_for(ask: &Ask, model: &Model) -> Option<&'static str> {
    if ask.kind != "chat" {
        return None;
    }
    Some(match model.id.contains("anthropic.") {
        true => AnthropicMessages::NAME,
        false => BedrockConverse::NAME,
    })
}

/// Its models are its account's: none are listed.
fn no_models(_: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async { Ok(Vec::new()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_vendor_kit::http;

    #[test]
    fn bedrocks_messages_are_under_anthropic() {
        let call = http::Request::post("/v1/messages")
            .body(())
            .expect("a call");
        let mut head = call.into_parts().0;
        Messages.request(&mut head, None).expect("a path");
        assert_eq!(head.uri.path(), MESSAGES_PATH);
    }

    #[test]
    fn claude_goes_to_messages_and_any_other_model_to_converse() {
        let ask = Ask {
            kind: "chat",
            format: "openai-chat",
            model: None,
            client: None,
        };
        let format = |id: &str| format_for(&ask, &Model::new(id));
        let claude = "global.anthropic.claude-haiku-4-5-20251001-v1:0";
        assert_eq!(format(claude), Some(AnthropicMessages::NAME));
        assert_eq!(
            format("amazon.nova-micro-v1:0"),
            Some(BedrockConverse::NAME)
        );
        // An embedding goes to the one format of its kind.
        let embedding = Ask {
            kind: "embedding",
            ..ask
        };
        assert_eq!(
            format_for(&embedding, &Model::new("cohere.embed-v4:0")),
            None
        );
    }
}
