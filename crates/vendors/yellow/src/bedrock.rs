//! Amazon Bedrock, at a region's `bedrock-runtime`, authenticated by a
//! Bedrock API key or signed (SigV4) by an AWS key pair: Claude on
//! Bedrock's Anthropic Messages (`/anthropic/v1/messages`), every other
//! model on Converse ([`BedrockConverse`]), which serves Claude too.
//!
//! Its models are the region's: the control plane (`bedrock.{region}`)
//! lists the foundation models Converse serves whole and streamed, called
//! on demand by their own id, and each active inference profile (`us.`,
//! `global.`, an application's ARN) as the model it routes to.

use std::sync::Arc;
use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::bedrock::BedrockConverse;
use respan_gateway_core::Fault;
use respan_gateway_http::http::header::{HeaderName, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::escape;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Ask, Body, BoxFuture, Capability, Dialect, Dialected, HttpTransport, KeyIn, Modality, Model,
    Models, Sign, Vendor, VendorProvider, built_wrong, http, json, time,
};

/// Where a region's runtime is: `AWS_REGION`'s, unless a host says.
const BASE_URL: &str = "https://bedrock-runtime.{AWS_REGION}.amazonaws.com";

/// Its Anthropic Messages, which take Anthropic's body as it is.
const MESSAGES_URL: &str = "https://bedrock-runtime.{AWS_REGION}.amazonaws.com/anthropic";

/// The name Bedrock signs as, not its host's `bedrock-runtime`.
const SIGNING_NAME: &str = "bedrock";

/// Amazon Bedrock: its key a Bedrock API key, else each call signed by the
/// environment's AWS key pair.
pub const VENDOR: Vendor = Vendor {
    id: "bedrock",
    key_env: &["AWS_BEARER_TOKEN_BEDROCK"],
    formats: &[
        (&BedrockConverse, Some(BASE_URL)),
        (
            &Dialected::new(AnthropicMessages, Bearer),
            Some(MESSAGES_URL),
        ),
    ],
    models: list_models,
    format_for: Some(format_for),
    signer: Some(environment_signer),
};

/// Anthropic's Messages with the key as a bearer token, as Bedrock takes it.
#[derive(Debug, Clone, Copy)]
pub struct Bearer;

impl Dialect for Bearer {
    type Input = Input;
    type Output = Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Bearer)
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

/// The environment's AWS key pair (`AWS_ACCESS_KEY_ID`,
/// `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) in `AWS_REGION`, if set.
fn environment_signer() -> Option<Arc<dyn Sign>> {
    let env = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    Some(signer(
        env("AWS_ACCESS_KEY_ID")?,
        env("AWS_SECRET_ACCESS_KEY")?,
        env("AWS_SESSION_TOKEN"),
        env("AWS_REGION")?,
    ))
}

/// The signer of Bedrock calls in `region` by an AWS key pair, for a host
/// with a tenant's own (`CallSigner`).
pub fn signer(
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    region: String,
) -> Arc<dyn Sign> {
    let credentials = Credentials::new(
        access_key_id,
        secret_access_key,
        session_token,
        None,
        SIGNING_NAME,
    );
    Arc::new(SigV4 {
        identity: credentials.into(),
        region,
    })
}

/// SigV4 by an AWS key pair, in a region.
struct SigV4 {
    identity: Identity,
    region: String,
}

impl Sign for SigV4 {
    fn sign(&self, head: &mut RequestHead, body: &[u8]) -> Result<(), Fault> {
        let refused = |e: &dyn std::fmt::Display| Fault::internal(format!("SigV4: {e}"));
        let params = v4::SigningParams::builder()
            .identity(&self.identity)
            .region(&self.region)
            .name(SIGNING_NAME)
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| refused(&e))?
            .into();
        let headers = (head.headers.iter())
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
        let uri = head.uri.to_string();
        let request = SignableRequest::new(
            head.method.as_str(),
            uri.as_str(),
            headers,
            SignableBody::Bytes(body),
        )
        .map_err(|e| refused(&e))?;
        let (signed, _) = sign(request, &params)
            .map_err(|e| refused(&e))?
            .into_parts();
        for (name, value) in signed.headers() {
            let name = HeaderName::try_from(name).map_err(|e| refused(&e))?;
            let mut value = HeaderValue::try_from(value).map_err(|e| refused(&e))?;
            value.set_sensitive(true);
            head.headers.insert(name, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::bytes::Bytes;
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

    fn call() -> (RequestHead, Bytes) {
        let head = http::Request::post("/anthropic/v1/messages")
            .body(())
            .expect("a call");
        (head.into_parts().0, Bytes::from_static(b"{}"))
    }

    #[test]
    fn a_key_pair_signs_the_call_as_sent() {
        let (mut head, body) = call();
        head.uri = "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1/messages"
            .parse()
            .expect("a URL");
        let signer = signer(
            "AKID".into(),
            "secret".into(),
            Some("token".into()),
            "us-east-1".into(),
        );
        signer.sign(&mut head, &body).expect("signed");
        let auth = head.headers["authorization"].to_str().expect("text");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"),
            "{auth}"
        );
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
        assert!(head.headers.contains_key("x-amz-date"));
        assert_eq!(head.headers["x-amz-security-token"], "token");
    }
}
