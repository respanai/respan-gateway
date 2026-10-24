//! Amazon Bedrock: Cohere Embed, Titan Embeddings, Nova Multimodal
//! Embeddings and Marengo on InvokeModel, at a region's runtime, with a
//! Bedrock API key.

use respan_gateway_core::Fault;
use respan_gateway_media_http::bedrock::BedrockEmbeddings;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Capability, HttpTransport, Modality, Model, Models, Vendor, VendorProvider,
    built_wrong, http, json, time,
};

/// Its region's runtime.
const BASE_URL: &str = "https://bedrock-runtime.{AWS_REGION}.amazonaws.com";

/// Amazon Bedrock.
pub const VENDOR: Vendor = Vendor {
    id: "bedrock",
    key_env: &["AWS_BEARER_TOKEN_BEDROCK"],
    formats: &[(&BedrockEmbeddings, Some(BASE_URL))],
    models: list_models,
    ..Vendor::DEFAULT
};

/// Its models: the region's control plane, its runtime's host without
/// `-runtime`, lists its foundation models and the inference profiles that
/// route to them, which take the same key.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let parts = p.parts()?;
        let base = parts
            .first()
            .map_or(BASE_URL, |part| part.base_url.as_str());
        let control = p.client_at(&base.replacen("bedrock-runtime.", "bedrock.", 1))?;
        let models = get(&control, "/foundation-models".to_owned()).await?;
        // A key that may not list the profiles still lists the models.
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
            let token: String = form_urlencoded::byte_serialize(token.as_bytes()).collect();
            path += &format!("&nextToken={token}");
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

/// The models a call can name: each foundation model of a kind served here
/// that is called on demand by its own id, and each active inference
/// profile (`us.`, `global.`, an application's ARN) as the model it routes
/// to, named as the profile. A model only provisioned throughput serves is
/// left out.
fn cards(models: &Value, profiles: &[Value]) -> Vec<Model> {
    let summaries: Vec<&Value> = models["modelSummaries"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
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

/// The card of a foundation model's summary, called as `id`, if it is of a
/// kind served here.
fn card(m: &Value, id: &str) -> Option<Model> {
    let mut model = Model::new(id);
    model.kind = Some(kind_of(m, id)?.to_owned());
    let model_id = m["modelId"].as_str().unwrap_or(id);
    model.maker = model_id.split_once('.').map(|(maker, _)| maker.to_owned());
    model.display_name = m["modelName"].as_str().map(str::to_owned);
    model.description = m["description"]["fullDescription"]
        .as_str()
        .map(str::to_owned);
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

/// The kind of a model, called as `id`, that a format here serves: an
/// embedding model whose family it embeds with.
fn kind_of(m: &Value, id: &str) -> Option<&'static str> {
    let outputs = || names(&m["outputModalities"]);
    if outputs().any(|o| o == "EMBEDDING") && BedrockEmbeddings::embeds(id) {
        return Some("embedding");
    }
    None
}

/// The names a list holds at `v`.
fn names(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary(id: &str, outputs: Value, types: Value) -> Value {
        json!({
            "modelId": id, "modelName": "Name", "outputModalities": outputs,
            "inputModalities": ["TEXT", "IMAGE"], "inferenceTypesSupported": types,
            "modelLifecycle": {"status": "ACTIVE", "startOfLifeTime": "2025-04-01T00:00:00Z",
                               "endOfLifeTime": "2027-04-01T00:00:00Z"},
            "description": {"fullDescription": "Embeds."},
        })
    }

    #[test]
    fn a_listed_model_is_called_by_its_id_on_demand_or_by_its_profiles() {
        let models = json!({"modelSummaries": [
            summary("cohere.embed-v4:0", json!(["EMBEDDING"]), json!(["ON_DEMAND", "INFERENCE_PROFILE"])),
            summary("cohere.embed-english-v3:0:512", json!(["EMBEDDING"]), json!(["PROVISIONED"])),
            summary("twelvelabs.marengo-embed-3-0-v1:0", json!(["EMBEDDING"]), json!(["INFERENCE_PROFILE"])),
            summary("twelvelabs.marengo-embed-2-7-v1:0", json!(["EMBEDDING"]), json!(["ON_DEMAND"])),
            summary("cohere.rerank-v3-5:0", json!(["TEXT"]), json!(["ON_DEMAND"])),
        ]});
        let profile = |id: &str, model: &str, status: &str| {
            json!({"inferenceProfileId": id, "inferenceProfileName": format!("US {model}"), "status": status,
                   "models": [{"modelArn": format!("arn:aws:bedrock:us-east-1::foundation-model/{model}")}]})
        };
        let profiles = [
            profile("us.cohere.embed-v4:0", "cohere.embed-v4:0", "ACTIVE"),
            profile(
                "us.twelvelabs.marengo-embed-3-0-v1:0",
                "twelvelabs.marengo-embed-3-0-v1:0",
                "ACTIVE",
            ),
            profile("eu.cohere.embed-v4:0", "cohere.embed-v4:0", "INACTIVE"),
        ];
        let cards = cards(&models, &profiles);
        let ids: Vec<&str> = cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "cohere.embed-v4:0",
                "us.cohere.embed-v4:0",
                "us.twelvelabs.marengo-embed-3-0-v1:0"
            ]
        );
        let v4 = &cards[0];
        assert_eq!(v4.kind.as_deref(), Some("embedding"));
        assert_eq!(v4.maker.as_deref(), Some("cohere"));
        assert_eq!(v4.display_name.as_deref(), Some("Name"));
        assert_eq!(v4.description.as_deref(), Some("Embeds."));
        assert_eq!(v4.input_modalities, [Modality::Image]);
        assert_eq!(v4.retires_at, time("2027-04-01"));
        assert_eq!(
            cards[1].display_name.as_deref(),
            Some("US cohere.embed-v4:0")
        );
    }
}
