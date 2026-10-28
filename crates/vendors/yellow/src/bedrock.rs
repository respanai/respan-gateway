//! Amazon Bedrock, at a region's `bedrock-runtime`, with a Bedrock API key:
//! Claude on Bedrock's Anthropic Messages (`/anthropic/v1/messages`), every
//! other model on Converse ([`BedrockConverse`]), which serves Claude too.
//! An AWS key pair signs calls outside the gateway, by a SigV4 proxy at the
//! base URL, as an HTTP proxy is.
//!
//! Its models are the region's: the control plane (`bedrock.{region}`)
//! lists the foundation models Converse serves whole and streamed, called
//! on demand by their own id, and each active inference profile (`us.`,
//! `global.`, an application's ARN) as the model it routes to.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::bedrock::BedrockConverse;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::{escape, set_path};
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Ask, Body, BoxFuture, Capability, Dialect, Dialected, HttpTransport, KeyIn, Modality, Model,
    Models, Rewrite, Vendor, VendorProvider, built_wrong, http, json, time,
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
    ],
    models: list_models,
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

/// Claude goes to Messages, whatever the client speaks; any other model to
/// Converse.
fn format_for(_: &Ask, model: &Model) -> Option<&'static str> {
    let claude = model.maker.as_deref() == Some("anthropic") || model.id.contains("anthropic.");
    Some(match claude {
        true => AnthropicMessages::NAME,
        false => BedrockConverse::NAME,
    })
}

/// Its models, as the module says. A key that may not list the profiles
/// still lists the models.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let parts = p.parts()?;
        let base = parts
            .first()
            .map_or(BASE_URL, |part| part.base_url.as_str());
        let control = p.client_at(&base.replacen("bedrock-runtime.", "bedrock.", 1))?;
        let models = get(&control, "/foundation-models".to_owned()).await?;
        let profiles = profiles(&control).await.unwrap_or_default();
        Ok(cards(&models, &profiles))
    })
}

/// The answer to `GET path` on the control plane.
async fn get(control: &HttpTransport, path: String) -> Result<Value, Fault> {
    let call = http::Request::get(path).body(Body::default());
    let (answer, _) = control.send(call.map_err(built_wrong)?).await?;
    json(answer).await
}

/// Every inference profile, a page at a time.
async fn profiles(control: &HttpTransport) -> Result<Vec<Value>, Fault> {
    let mut all = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let mut path = "/inference-profiles?maxResults=1000".to_owned();
        if let Some(token) = &next {
            path += &format!("&nextToken={}", escape(token));
        }
        let page = get(control, path).await?;
        let listed = page["inferenceProfileSummaries"].as_array();
        all.extend(listed.into_iter().flatten().cloned());
        next = page["nextToken"].as_str().map(str::to_owned);
        if next.is_none() {
            return Ok(all);
        }
    }
}

/// The chat models a call can name, as the module says; a model only
/// provisioned throughput serves is left out.
fn cards(models: &Value, profiles: &[Value]) -> Vec<Model> {
    let summaries: Vec<&Value> = names_of(&models["modelSummaries"]).collect();
    let on_demand = |m: &Value| names(&m["inferenceTypesSupported"]).any(|t| t == "ON_DEMAND");
    let mut cards: Vec<Model> = (summaries.iter())
        .filter(|m| on_demand(m))
        .filter_map(|m| card(m, m["modelId"].as_str()?))
        .collect();
    for profile in profiles.iter().filter(|p| p["status"] == "ACTIVE") {
        let Some(id) = (profile["inferenceProfileId"].as_str())
            .or_else(|| profile["inferenceProfileArn"].as_str())
        else {
            continue;
        };
        let routed = profile["models"][0]["modelArn"]
            .as_str()
            .unwrap_or_default();
        let routed = routed.rsplit_once("foundation-model/").map(|(_, id)| id);
        let summary = summaries.iter().find(|m| m["modelId"].as_str() == routed);
        if let Some(mut card) = summary.and_then(|m| card(m, id)) {
            if let Some(name) = profile["inferenceProfileName"].as_str() {
                card.display_name = Some(name.to_owned());
            }
            cards.push(card);
        }
    }
    cards
}

/// The card of a foundation model's summary, called as `id`, if it answers
/// in text on Converse, whole and streamed.
fn card(m: &Value, id: &str) -> Option<Model> {
    let served = &m["inferenceAPIsSupported"]["converse"];
    let chats = names(&m["outputModalities"]).any(|o| o == "TEXT")
        && served["sync"] == true
        && served["streaming"] == true;
    if !chats {
        return None;
    }
    let mut model = Model::new(id);
    model.kind = Some("chat".to_owned());
    let model_id = m["modelId"].as_str().unwrap_or(id);
    model.maker = model_id.split_once('.').map(|(maker, _)| maker.to_owned());
    model.display_name = m["modelName"].as_str().map(str::to_owned);
    let lifecycle = &m["modelLifecycle"];
    model.created = lifecycle["startOfLifeTime"].as_str().and_then(time);
    model.retires_at = lifecycle["endOfLifeTime"].as_str().and_then(time);
    for input in names(&m["inputModalities"]) {
        let read = match input {
            "IMAGE" => Modality::Image,
            "AUDIO" | "SPEECH" => Modality::Audio,
            "VIDEO" => Modality::Video,
            _ => continue,
        };
        model.reads(read);
    }
    let converse = &m["converse"];
    if names(&converse["userDocumentTypesSupported"])
        .next()
        .is_some()
    {
        model.reads(Modality::Document);
    }
    model.max_output_tokens = converse["maxTokensMaximum"].as_u64();
    if converse["reasoningSupported"].is_object() {
        model.can(Capability::Reasoning);
    }
    Some(model)
}

/// The values a list holds at `v`.
fn names_of(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

/// The names a list holds at `v`.
fn names(v: &Value) -> impl Iterator<Item = &str> {
    names_of(v).filter_map(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary(id: &str, outputs: Value, converse: Value, types: Value) -> Value {
        json!({
            "modelId": id, "modelName": "Name", "outputModalities": outputs,
            "inputModalities": ["TEXT", "IMAGE"], "inferenceTypesSupported": types,
            "inferenceAPIsSupported": { "converse": converse },
            "converse": { "maxTokensMaximum": 64000, "reasoningSupported": { "embedded": false },
                          "userDocumentTypesSupported": ["pdf"] },
            "modelLifecycle": { "startOfLifeTime": "2025-04-01T00:00:00Z",
                                "endOfLifeTime": "2027-04-01T00:00:00Z" },
        })
    }

    #[test]
    fn a_chat_model_is_one_converse_serves_whole_and_streamed_by_its_id_or_profile() {
        let both = json!({ "sync": true, "streaming": true });
        let on_demand = json!(["ON_DEMAND"]);
        let models = json!({ "modelSummaries": [
            summary("amazon.nova-lite-v1:0", json!(["TEXT"]), both.clone(), on_demand.clone()),
            summary("anthropic.claude-haiku-4-5-20251001-v1:0", json!(["TEXT"]), both.clone(),
                    json!(["INFERENCE_PROFILE"])),
            summary("amazon.nova-2-sonic-v1:0", json!(["SPEECH", "TEXT"]),
                    json!({ "sync": false, "streaming": true }), on_demand.clone()),
            summary("cohere.embed-v4:0", json!(["EMBEDDING"]), json!({}), on_demand),
        ]});
        let profile = |id: &str, model: &str, status: &str| {
            json!({ "inferenceProfileId": id, "inferenceProfileName": format!("US {model}"),
                    "status": status, "models": [{ "modelArn":
                    format!("arn:aws:bedrock:us-east-1::foundation-model/{model}") }] })
        };
        let claude = "anthropic.claude-haiku-4-5-20251001-v1:0";
        let profiles = [
            profile(
                "us.anthropic.claude-haiku-4-5-20251001-v1:0",
                claude,
                "ACTIVE",
            ),
            profile(
                "eu.anthropic.claude-haiku-4-5-20251001-v1:0",
                claude,
                "INACTIVE",
            ),
        ];
        let cards = cards(&models, &profiles);
        let ids: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "amazon.nova-lite-v1:0",
                "us.anthropic.claude-haiku-4-5-20251001-v1:0"
            ]
        );
        let nova = &cards[0];
        assert_eq!(nova.kind.as_deref(), Some("chat"));
        assert_eq!(nova.maker.as_deref(), Some("amazon"));
        assert_eq!(nova.max_output_tokens, Some(64000));
        assert_eq!(nova.capabilities, [Capability::Reasoning]);
        assert_eq!(nova.input_modalities, [Modality::Image, Modality::Document]);
        assert_eq!(nova.retires_at, time("2027-04-01"));
        assert_eq!(cards[1].maker.as_deref(), Some("anthropic"));
        assert_eq!(
            cards[1].display_name.as_deref(),
            Some(&*format!("US {claude}"))
        );
    }

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
        let format = |id: &str, maker: Option<&str>| {
            let mut model = Model::new(id);
            model.maker = maker.map(str::to_owned);
            format_for(&ask, &model)
        };
        let messages = Some(AnthropicMessages::NAME);
        assert_eq!(
            format("global.anthropic.claude-haiku-4-5-20251001-v1:0", None),
            messages
        );
        // An application's profile, whose maker its listing names.
        let arn = "arn:aws:bedrock:us-east-1:1:application-inference-profile/abc";
        assert_eq!(format(arn, Some("anthropic")), messages);
        assert_eq!(
            format("amazon.nova-micro-v1:0", None),
            Some(BedrockConverse::NAME)
        );
    }
}
