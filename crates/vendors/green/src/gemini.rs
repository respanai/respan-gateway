//! Google's Gemini API: Chat Completions and embeddings at its
//! OpenAI-compatible base, and its own API beside it: `generateContent`,
//! for chats and images, Gemini Live and Veo.

use respan_gateway_conversation::{Content, Input, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_core::{Fault, Filter, Native};
use serde_json::{Value, json};

use respan_gateway_conversation_http::gemini::{GeminiGenerate, needs_signatures};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media_http::gemini::{GeminiImages, GeminiVeo};
use respan_gateway_media_http::openai::OpenAiEmbeddings;
use respan_gateway_realtime_ws::GeminiLive;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Capability, Dialect, Dialected, Model, Models, Vendor, VendorProvider,
    built_wrong, http, json as body_json, models,
};

/// Its own API: `generateContent` and Veo.
const API: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Where Live is: its origin, as Live's path is whole.
const LIVE: &str = "https://generativelanguage.googleapis.com";

/// Its OpenAI-compatible API, beside its own.
const OPENAI_API: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// Google's Gemini API: its own, and an OpenAI-compatible one beside it.
/// Its own `generateContent` comes first, so a client of another format is
/// translated to it, which says what Chat Completions cannot, such as
/// reasoning; a client of Chat Completions is sent to its
/// OpenAI-compatible endpoint as it is.
pub const VENDOR: Vendor = Vendor {
    id: "gemini",
    key_env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    formats: &[
        (&GeminiGenerate, Some(API)),
        (&Dialected::new(OpenAiChat, Gemini), Some(OPENAI_API)),
        (&GeminiLive, Some(LIVE)),
        (&GeminiImages, Some(API)),
        (&GeminiVeo, Some(API)),
        (&OpenAiEmbeddings, Some(OPENAI_API)),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// What a Gemini 3 model accepts as the signature of a function call made
/// elsewhere, as Google documents it.
pub use respan_gateway_conversation_http::gemini::SIGNATURE_PLACEHOLDER;

/// Gemini's Chat Completions: a Gemini 3 model refuses a function call in
/// the history without its thought signature, which Chat Completions
/// carries in the call's `extra_content`; and a reply's `extra_content` is
/// Gemini's alone. Its own format signs its calls itself.
#[derive(Debug, Clone, Copy)]
pub struct Gemini;

impl Dialect for Gemini {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(signed))
    }
}

/// `input` as Google's Chat Completions takes it: each call's and text's
/// signature in its `extra_content`, and, if its model needs them signed,
/// the first call of each assistant message without one signed with the
/// placeholder, as Gemini returns one only on the first of a turn's
/// parallel calls.
fn signed(input: &mut Input) -> Result<(), Fault> {
    let needs = needs_signatures(input.model.as_deref());
    for message in input
        .messages
        .iter_mut()
        .filter(|m| m.role == Role::Assistant)
    {
        let mut first = needs;
        for at in 0..message.content.len() {
            let signature = match message.content.get(at + 1) {
                Some(Content::Signature(s)) => Some(s.clone()),
                _ => None,
            };
            let Content::ToolCall { native, .. } = &mut message.content[at] else {
                continue;
            };
            let placeholder = std::mem::take(&mut first).then(|| SIGNATURE_PLACEHOLDER.into());
            if let Some(signature) = signature.or(placeholder) {
                sign(native, signature);
            }
        }
        // A text's signature is the message's: its last text's.
        let text = message.content.windows(2).rev().find_map(|w| match w {
            [Content::Text { .. }, Content::Signature(s)] => Some(s.clone()),
            _ => None,
        });
        if let Some(signature) = text {
            sign(&mut message.native, signature);
        }
    }
    Ok(())
}

/// `native` with `signature` in its `extra_content`, as Google's Chat takes it.
fn sign(native: &mut Native, signature: String) {
    let fields = OpenAiChat::fields_mut(native);
    let extra = fields.entry("extra_content").or_insert_with(|| json!({}));
    extra["google"]["thought_signature"] = signature.into();
}

/// Its models, as its own API lists them (`{"models": [{"name", ..}]}`),
/// each id without the `models/` it puts before it; as OpenAI lists them for
/// a provider of its OpenAI-compatible formats alone.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        // Its own API's base and key, as either format of it takes them.
        let own = match p.client_of(GeminiGenerate::NAME)? {
            Some(own) => Some(own),
            None => p.client_of(GeminiVeo::NAME)?,
        };
        let Some(own) = own else {
            return models::openai(p).await;
        };
        let call = http::Request::get("/models?pageSize=1000").body(Body::default());
        let (answer, _) = own.send(call.map_err(built_wrong)?).await?;
        let answer = body_json(answer).await?;
        let models = answer["models"].as_array().into_iter().flatten();
        let model = |m: &Value| {
            let name = m["name"].as_str()?;
            let mut model = Model::new(name.strip_prefix("models/").unwrap_or(name));
            model.display_name = m["displayName"].as_str().map(str::to_owned);
            model.description = m["description"].as_str().map(str::to_owned);
            model.context_window = m["inputTokenLimit"].as_u64();
            model.max_output_tokens = m["outputTokenLimit"].as_u64();
            // Its list says only whether one thinks; every model that
            // generates content calls tools.
            let methods = m["supportedGenerationMethods"].as_array();
            if methods.is_some_and(|ms| ms.iter().any(|m| m == "generateContent")) {
                model.can(Capability::Tools);
            }
            if m["thinking"] == true {
                model.can(Capability::Reasoning);
            }
            Some(model)
        };
        Ok(models.filter_map(model).collect())
    })
}
