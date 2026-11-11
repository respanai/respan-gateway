//! Neon AI Gateway: Chat Completions, Responses, Messages and embeddings,
//! for many vendors' models, and decisions, at a branch's own host
//! (`NEON_AI_GATEWAY_BASE_URL`).

use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::Fault;
use respan_gateway_decision_http::Decisions;
use respan_gateway_media_http::openai::OpenAiEmbeddings;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Ask, Body, BoxFuture, Dialected, Model, Models, Vendor, VendorProvider, built_wrong, http,
    json, models,
};

use crate::openai::in_responses;
use crate::typesafe::TypeSafe;

/// Where its APIs are, under a branch's host: Chat Completions and
/// embeddings at `/v1`, the others under their vendor's name.
const CHAT: &str = "{NEON_AI_GATEWAY_BASE_URL}/v1";
const RESPONSES: &str = "{NEON_AI_GATEWAY_BASE_URL}/openai/v1";
/// Messages' base leaves out the `/v1` its format adds.
const MESSAGES: &str = "{NEON_AI_GATEWAY_BASE_URL}/anthropic";
/// Decisions, at `/typesafe/v1/systemone`, as TypeSafe serves them.
const DECISIONS: &str = "{NEON_AI_GATEWAY_BASE_URL}/typesafe/v1";

/// Neon AI Gateway. Responses serves only OpenAI's models, Messages only
/// Anthropic's (`format_for`).
pub const VENDOR: Vendor = Vendor {
    id: "neon",
    key_env: &["NEON_AI_GATEWAY_TOKEN"],
    formats: &[
        (&OpenAiChat, Some(CHAT)),
        (&OpenAiResponses, Some(RESPONSES)),
        (&AnthropicMessages, Some(MESSAGES)),
        (&OpenAiEmbeddings, Some(CHAT)),
        (&Dialected::new(Decisions, TypeSafe), Some(DECISIONS)),
    ],
    models: list_models,
    format_for: Some(format_for),
};

/// The format a chat with `model` goes in: the client's own if it is the
/// native one of the model's maker, else Chat Completions, which serves
/// any; a chat OpenAI's Chat Completions does not serve well, Responses
/// ([`in_responses`]). OpenAI's open-weight models are served at Chat
/// Completions alone.
fn format_for(ask: &Ask, model: &Model) -> Option<&'static str> {
    if ask.kind != "chat" {
        return None;
    }
    let native = match model.maker.as_deref().unwrap_or_default() {
        "openai" if model.id.starts_with("gpt-oss") => OpenAiChat::NAME,
        "openai" if in_responses(&model.id, ask.tools) => return Some(OpenAiResponses::NAME),
        "openai" => OpenAiResponses::NAME,
        "anthropic" => AnthropicMessages::NAME,
        _ => OpenAiChat::NAME,
    };
    (ask.format != native).then_some(OpenAiChat::NAME)
}

/// Its models: `GET /v1/models`, in OpenRouter's form, each by its maker
/// (`owned_by`); and its decision models, `GET /typesafe/v1/models`, which
/// the first lists as chats. A failed list of decision models leaves them
/// chats.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        let answer = json(answer).await?;
        let decisions = match p.base_url_of(Decisions::NAME) {
            Some(base) => decision_models(p, base).await.unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(listed(&answer, &decisions))
    })
}

/// The names of its decision models, `{"models": [{"name"}]}`.
async fn decision_models(p: &VendorProvider, base: &str) -> Result<Vec<String>, Fault> {
    let call = http::Request::get("/models").body(Body::default());
    let (answer, _) = p.client_at(base)?.send(call.map_err(built_wrong)?).await?;
    let answer = json(answer).await?;
    let names = answer["models"].as_array().into_iter().flatten();
    Ok(names
        .filter_map(|m| m["name"].as_str().map(str::to_owned))
        .collect())
}

/// The models of its list, each with its maker and kind: a decision model
/// if `decisions` names it, an embedding model if it writes embeddings,
/// else a chat. Its `created` is always 0, so says nothing. Gemini's are
/// left out.
fn listed(answer: &Value, decisions: &[String]) -> Vec<Model> {
    let entries = answer["data"].as_array().into_iter().flatten();
    entries
        .filter(|m| !m["id"].as_str().unwrap_or_default().starts_with("gemini"))
        .filter_map(|m| {
            let mut model = models::listed(&Value::Array(vec![m.clone()])).pop()?;
            model.created = None;
            model.maker = m["owned_by"].as_str().map(str::to_owned);
            let writes = m["architecture"]["modality"].as_str().unwrap_or_default();
            let kind = match () {
                _ if decisions.contains(&model.id) => "decision",
                _ if writes.ends_with("->embeddings") => "embedding",
                _ => "chat",
            };
            model.kind = Some(kind.to_owned());
            Some(model)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ask<'a>(format: &'a str, tools: bool) -> Ask<'a> {
        Ask {
            kind: "chat",
            format,
            model: None,
            client: None,
            tools,
        }
    }

    fn card(id: &str, maker: &str) -> Model {
        let mut model = Model::new(id);
        model.maker = Some(maker.to_owned());
        model
    }

    #[test]
    fn its_formats_are_under_a_branchs_host() {
        let bases: Vec<_> = VENDOR
            .formats
            .iter()
            .map(|(f, base)| (f.name(), base.expect("a base")))
            .collect();
        assert_eq!(
            bases,
            [
                (OpenAiChat::NAME, CHAT),
                (OpenAiResponses::NAME, RESPONSES),
                (AnthropicMessages::NAME, MESSAGES),
                (OpenAiEmbeddings::NAME, CHAT),
                (Decisions::NAME, DECISIONS),
            ]
        );
        assert!(
            bases
                .iter()
                .all(|(_, b)| b.starts_with("{NEON_AI_GATEWAY_BASE_URL}/"))
        );
    }

    #[test]
    fn a_chat_goes_native_for_its_maker_else_to_chat_completions() {
        let messages = AnthropicMessages::NAME;
        let responses = OpenAiResponses::NAME;
        let chat = OpenAiChat::NAME;
        let claude = card("claude-haiku-5-5", "anthropic");
        let gpt = card("gpt-5-mini", "openai");
        // A client of the maker's own format stays in it.
        assert_eq!(format_for(&ask(messages, false), &claude), None);
        assert_eq!(format_for(&ask(responses, false), &gpt), None);
        // A client of another goes to Chat Completions.
        assert_eq!(format_for(&ask(messages, false), &gpt), Some(chat));
        assert_eq!(format_for(&ask(responses, false), &claude), Some(chat));
        assert_eq!(format_for(&ask(chat, false), &claude), Some(chat));
        // Models only Responses serves go there, whatever the client.
        let pro = card("gpt-5-5-pro", "openai");
        assert_eq!(format_for(&ask(messages, false), &pro), Some(responses));
        // OpenAI's open-weight models are at Chat Completions alone.
        let oss = card("gpt-oss-120b", "openai");
        assert_eq!(format_for(&ask(responses, false), &oss), Some(chat));
        assert_eq!(format_for(&ask(chat, false), &oss), None);
        // Others' are Chat Completions'.
        let kimi = card("kimi-k3", "moonshotai");
        assert_eq!(format_for(&ask(chat, false), &kimi), None);
        assert_eq!(format_for(&ask(messages, false), &kimi), Some(chat));
    }

    #[test]
    fn a_request_of_another_kind_is_the_usual() {
        let embed = Ask {
            kind: "embedding",
            ..ask(OpenAiEmbeddings::NAME, false)
        };
        assert_eq!(format_for(&embed, &card("bge-large-en", "baai")), None);
    }

    #[test]
    fn its_list_says_each_models_maker_and_kind_and_leaves_out_gemini() {
        let entry = |id: &str, owner: &str, modality: &str| {
            json!({ "id": id, "name": id, "object": "model", "owned_by": owner, "created": 0,
                "enabled": true, "context_length": null,
                "architecture": { "modality": modality, "input_modalities": ["text"] },
                "pricing": null })
        };
        let answer = json!({ "object": "list", "data": [
            entry("claude-haiku-5-5", "anthropic", "text+image+file->text"),
            entry("gemini-3-flash", "google", "text+image->text"),
            entry("gemma-3-12b", "google", "text+image->text"),
            entry("bge-large-en", "baai", "text->embeddings"),
            entry("typesafeai-jev-1-14-0", "typesafe", "text+image->text"),
        ]});
        let models = listed(&answer, &["typesafeai-jev-1-14-0".to_owned()]);
        let seen: Vec<_> = models
            .iter()
            .map(|m| {
                (
                    m.id.as_str(),
                    m.maker.as_deref(),
                    m.kind.as_deref(),
                    m.created,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("claude-haiku-5-5", Some("anthropic"), Some("chat"), None),
                ("gemma-3-12b", Some("google"), Some("chat"), None),
                ("bge-large-en", Some("baai"), Some("embedding"), None),
                (
                    "typesafeai-jev-1-14-0",
                    Some("typesafe"),
                    Some("decision"),
                    None
                ),
            ]
        );
    }
}
