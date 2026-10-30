//! Respan: its gateway, one key for many vendors' models, in each spec it
//! serves: Chat Completions, Responses, Anthropic Messages and Gemini's
//! `generateContent`; and decisions, for its own decision models.

use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_decision_http::Decisions;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Ask, Body, BoxFuture, Capability, Modality, Model, Models, Price, Vendor, VendorProvider,
    built_wrong, http, json, time,
};

use crate::openai::responses_only;

/// Where its API is: Chat Completions and Responses at its root, the
/// others under their vendor's name.
const BASE_URL: &str = "https://api.respan.ai/api";
const ANTHROPIC: &str = "https://api.respan.ai/api/anthropic";
const GEMINI: &str = "https://api.respan.ai/api/google/gemini/v1beta";
/// Decisions, at `/v1/decisions`, are under the version.
const DECISIONS: &str = "https://api.respan.ai/api/v1";

/// Its public catalog of the models its gateway serves, beside its API:
/// `GET {CATALOG}/list`.
const CATALOG: &str = "https://www.respan.ai/api/gateway-models";

/// Respan. Its Chat Completions serves every model it lists; each of its
/// other formats only its own vendor's (`format_for`).
pub const VENDOR: Vendor = Vendor {
    id: "respan",
    key_env: &["RESPAN_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&AnthropicMessages, Some(ANTHROPIC)),
        (&GeminiGenerate, Some(GEMINI)),
        (&Decisions, Some(DECISIONS)),
    ],
    models: list_models,
    format_for: Some(format_for),
    ..Vendor::DEFAULT
};

/// The format a request for `model` goes in: the client's own if it is
/// the native one of the model's maker, else Chat Completions, which serves
/// any; an OpenAI model only Responses serves well, as OpenAI's, Responses.
fn format_for(ask: &Ask, model: &Model) -> Option<&'static str> {
    let native = match model.maker.as_deref().unwrap_or_default() {
        "openai" if responses_only(&model.id) => return Some(OpenAiResponses::NAME),
        "openai" => OpenAiResponses::NAME,
        "anthropic" => AnthropicMessages::NAME,
        "google" => GeminiGenerate::NAME,
        _ => OpenAiChat::NAME,
    };
    (ask.format != native).then_some(OpenAiChat::NAME)
}

/// Its models: its catalog, a page at a time, which needs no key, so is
/// sent none. A provider at another base URL lists at
/// `{base}/gateway-models/list`.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let catalog = match p.base_url_of("openai-chat") {
            Some(base) if base.trim_end_matches('/') != BASE_URL => {
                format!("{}/gateway-models", base.trim_end_matches('/'))
            }
            _ => CATALOG.to_owned(),
        };
        let client = p.keyless_client_at(&catalog)?;
        let mut models = Vec::new();
        for page in 1.. {
            let call = http::Request::get(format!("/list?page={page}&page_size=1000"));
            let call = call.body(Body::default()).map_err(built_wrong)?;
            let (answer, _) = client.send(call).await?;
            let answer = json(answer).await?;
            let listed = answer["models"].as_array().into_iter().flatten();
            models.extend(listed.filter_map(card));
            if answer["hasNext"] != true || page == 20 {
                break;
            }
        }
        Ok(models)
    })
}

/// One entry of its catalog: prices per million tokens; a model only its
/// batches serve (`<model>:batch`) is not one a call names.
fn card(m: &Value) -> Option<Model> {
    let id = m["model_name"]
        .as_str()
        .filter(|id| !id.ends_with(":batch"))?;
    let mut model = Model::new(id);
    let text = |v: &Value| v.as_str().filter(|t| !t.is_empty()).map(str::to_owned);
    model.display_name = text(&m["display_name"]).filter(|n| n != id);
    model.description = text(&m["model_description"]);
    model.maker = text(&m["provider"]["provider_id"]).map(|maker| match maker.as_str() {
        "google_gemini_ai" => "google".to_owned(),
        _ => maker,
    });
    model.context_window = m["max_context_window"].as_u64();
    model.created = m["metadata"]["release_date"].as_str().and_then(time);
    let can = &m["capability"];
    model.max_output_tokens = [
        "max_completion_tokens",
        "max_output_tokens",
        "max_output_length",
    ]
    .iter()
    .find_map(|k| can[k].as_u64());
    let on = |v: &Value| *v == true || v["supported"] == true;
    if on(&can["function_call"]) || on(&can["tool_use"]) {
        model.can(Capability::Tools);
    }
    if on(&can["reasoning_support"]) || on(&can["reasoning"]) || on(&can["thinking"]) {
        model.can(Capability::Reasoning);
    }
    let said = ["supported_parameters", "supported_features"].iter();
    let names = said.flat_map(|k| can[k].as_array().into_iter().flatten());
    names
        .filter_map(Value::as_str)
        .filter_map(Capability::parse)
        .for_each(|c| model.can(c));
    let image = can["modality"]
        .as_str()
        .is_some_and(|m| m.contains("image->"));
    if on(&can["images"]) || on(&can["image_input"]) || image {
        model.reads(Modality::Image);
    }
    let cost = |k: &str| m[k].as_f64();
    model.price = match (cost("input_cost"), cost("output_cost")) {
        (Some(input), Some(output)) if input > 0.0 || output > 0.0 => Some(Price {
            cache_write: cost("cache_creation_input_cost"),
            ..Price::new(input, output, cost("cache_hit_input_cost"))
        }),
        _ => None,
    };
    Some(model)
}
