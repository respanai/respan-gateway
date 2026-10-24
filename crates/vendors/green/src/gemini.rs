//! Google's Gemini API: Chat Completions and embeddings at its
//! OpenAI-compatible base, and its own API beside it: `generateContent`,
//! for chats, images, speech and transcriptions, Gemini Live and Veo.

use std::borrow::Cow;

use respan_gateway_conversation::{Content, Input, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_core::{Fault, Filter, NativeFields};
use serde_json::{Map, Value, json};

use respan_gateway_conversation_http::gemini::{GeminiGenerate, needs_signatures};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media::transcription;
use respan_gateway_media_http::dialect::{Dialect as MediaDialect, Dialected as MediaDialected};
use respan_gateway_media_http::gemini::{
    GeminiImages, GeminiSpeech, GeminiTranscriptions, GeminiVeo,
};
use respan_gateway_media_http::openai::OpenAiEmbeddings;
use respan_gateway_realtime_ws::GeminiLive;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Capability, Model, Models, Vendor, VendorProvider, built_wrong, chat, http,
    json as body_json, models, transcription as text_only,
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
        (
            &MediaDialected::new(GeminiTranscriptions, Gemini),
            Some(API),
        ),
        (&GeminiSpeech, Some(API)),
        (&GeminiVeo, Some(API)),
        (&OpenAiEmbeddings, Some(OPENAI_API)),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// Gemini's transcriptions: the text alone, so the client's other fields
/// are refused.
impl MediaDialect<transcription::Input, transcription::Output> for Gemini {
    fn input<'a>(
        &self,
        input: &'a transcription::Input,
    ) -> Result<Cow<'a, transcription::Input>, Fault> {
        text_only::text_only(input)
    }
}

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
    fn input<'a>(&self, input: &'a Input, _: &Call) -> Result<Cow<'a, Input>, Fault> {
        let calls = |m: &respan_gateway_conversation::Message| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|p| matches!(p, Content::ToolCall { .. }))
        };
        if !needs_signatures(input.model.as_deref()) || !input.messages.iter().any(calls) {
            return Ok(Cow::Borrowed(input));
        }
        let mut input = input.clone();
        sign_calls(&mut input);
        Ok(Cow::Owned(input))
    }

    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        chat::read(call, &["extra_content"], false)
    }
}

/// Signs, with the placeholder in its `extra_content`, the first call of
/// each assistant message that has no signature of its own, as Gemini
/// returns one only on the first of a turn's parallel calls.
fn sign_calls(input: &mut Input) {
    // Another format's fields a translation drops anyway, as a client's
    // item id or cache marker, give way to the signature.
    const DROPPED: &[&str] = &["inert", "cache_control", "thoughtSignature"];
    let format = OpenAiChat::NAME;
    let signature = json!({ "google": { "thought_signature": SIGNATURE_PLACEHOLDER } });
    let own = |native: &mut Option<NativeFields>| match native {
        Some(n) if &*n.format != format => {
            let dropped = n.fields.keys().all(|k| DROPPED.contains(&k.as_str()));
            if dropped {
                *native = None;
            }
            dropped
        }
        _ => true,
    };
    for message in input
        .messages
        .iter_mut()
        .filter(|m| m.role == Role::Assistant)
    {
        let first = message.content.iter_mut().find_map(|part| match part {
            Content::ToolCall { native, .. } => Some(native),
            _ => None,
        });
        let Some(native) = first else {
            continue;
        };
        if !own(native) {
            continue;
        }
        let fields = native
            .get_or_insert_with(|| NativeFields::new(format, Map::new()))
            .fields
            .entry("extra_content");
        fields.or_insert_with(|| signature.clone());
    }
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
