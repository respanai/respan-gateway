//! Nace (Drex): decisions in System One's and OpenAI's contracts.

use respan_gateway_core::Fault;
use respan_gateway_decision::{Input, Output};
use respan_gateway_decision_http::{Decisions, OpenAiDecisions};
use respan_gateway_http::http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_http::path::set_path;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Dialect, Dialected, Modality, Model, Models, Price, Rewrite, Vendor,
    VendorProvider, built_wrong, http, json,
};

/// Where its API is.
const BASE_URL: &str = "https://console.nace.ai/v1";

/// Nace.
pub const VENDOR: Vendor = Vendor {
    id: "nace",
    key_env: &["NACE_API_KEY"],
    client_header: Some("x-nace-client"),
    formats: &[
        (
            &Dialected::new(Decisions, Nace { systemone: true }),
            Some(BASE_URL),
        ),
        (
            &Dialected::new(OpenAiDecisions::DEFAULT, Nace { systemone: false }),
            Some(BASE_URL),
        ),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// Drex serves native decisions at `/systemone`, with an object legend
/// when asked; OpenAI decisions stay at `/decisions`.
#[derive(Debug, Clone, Copy)]
pub struct Nace {
    /// Whether this is System One's contract.
    pub systemone: bool,
}

impl Dialect for Nace {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        if self.systemone {
            set_path(head, "/systemone")?;
            head.headers
                .insert("x-nace-legend", http::HeaderValue::from_static("object"));
        }
        Ok(None)
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        // Drex overloads with 529. The shared retry policy recognizes 503;
        // Retry-After determines the delay; retry-after-ms is passed through.
        if head.status.as_u16() == 529 {
            head.status = http::StatusCode::SERVICE_UNAVAILABLE;
        }
        None
    }
}

/// Read the public priced catalog at the API's origin without credentials,
/// then add authenticated names and aliases. Either list can stand alone.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let parts = p.parts()?;
        let base = &parts
            .first()
            .ok_or_else(|| Fault::config("Nace needs a format"))?
            .base_url;
        let uri: http::Uri = base
            .parse()
            .map_err(|e: http::uri::InvalidUri| Fault::config(e.to_string()))?;
        let origin = format!(
            "{}://{}",
            uri.scheme_str().unwrap_or("https"),
            uri.authority()
                .ok_or_else(|| Fault::config("Nace needs an origin"))?
        );
        let public = async {
            let call = http::Request::get("/models")
                .body(Body::default())
                .map_err(built_wrong)?;
            let (answer, _) = p.keyless_client_at(&origin)?.send(call).await?;
            Ok::<_, Fault>(listed(&json(answer).await?))
        }
        .await;
        let names = async {
            let call = http::Request::get("/models")
                .body(Body::default())
                .map_err(built_wrong)?;
            let (answer, _) = p.client()?.send(call).await?;
            json(answer).await
        }
        .await;
        match (public, names) {
            (Ok(public), Ok(names)) => Ok(merge_models(public, &names)),
            (Ok(public), Err(_)) => Ok(public),
            (Err(_), Ok(names)) => Ok(merge_models(Vec::new(), &names)),
            (Err(_), Err(fault)) => Err(fault),
        }
    })
}

/// Drex's OpenRouter 2.4 list puts limits and per-token pricing on input
/// modalities. It bills prompt tokens only, with no output charge.
fn listed(answer: &Value) -> Vec<Model> {
    answer["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let mut model = decision_model(entry["id"].as_str()?);
            model.display_name = entry["name"].as_str().map(str::to_owned);
            model.description = entry["description"].as_str().map(str::to_owned);
            for input in entry["input_modalities"].as_array().into_iter().flatten() {
                if let Some(modality) = input["type"].as_str().and_then(Modality::parse) {
                    model.reads(modality);
                }
                if input["type"] != "text" {
                    continue;
                }
                model.context_window =
                    input["supported_inputs"]["max_context_length"]["value"].as_u64();
                let prompt = input["pricing"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|p| p["type"] == "prompt" && p["unit"] == "token");
                let cost = prompt.and_then(|p| p["cost_usd"].as_str()?.parse::<f64>().ok());
                model.price = cost
                    .filter(|p| p.is_finite() && *p >= 0.0)
                    // Per million, rounded to a nano-dollar against float noise.
                    .map(|p| Price::new((p * 1e15).round() / 1e9, 0.0, None));
            }
            Some(model)
        })
        .collect()
}

/// A model of the decision kind, which its name does not always say.
fn decision_model(id: &str) -> Model {
    let mut model = Model::new(id);
    model.kind = Some("decision".to_owned());
    model
}

/// An alias carries its target's card, including its price and modalities.
fn merge_models(mut public: Vec<Model>, names: &Value) -> Vec<Model> {
    let entries = names["models"].as_array().into_iter().flatten();
    for entry in entries {
        let Some(name) = entry["name"].as_str() else {
            continue;
        };
        if public.iter().any(|m| m.id == name) {
            continue;
        }
        let target = entry["alias_for"]
            .as_str()
            .and_then(|id| public.iter().find(|m| m.id == id));
        let mut model = target.cloned().unwrap_or_else(|| decision_model(name));
        model.id = name.to_owned();
        if let Some(description) = entry["description"].as_str() {
            model.description = Some(description.to_owned());
        }
        public.push(model);
    }
    public
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_overload_is_read_as_transient() {
        let (sent, _) = http::Request::new(()).into_parts();
        for status in [529, 422, 429, 500] {
            let (mut head, _) = http::Response::builder()
                .status(status)
                .body(())
                .expect("response")
                .into_parts();
            assert!(
                Nace { systemone: true }
                    .response(&sent, &mut head)
                    .is_none()
            );
            assert_eq!(
                head.status.as_u16(),
                if status == 529 { 503 } else { status }
            );
        }
    }

    #[test]
    fn only_native_calls_use_systemone_and_ask_for_an_object_legend() {
        for native in [true, false] {
            let (mut head, _) = http::Request::builder()
                .uri("/decisions")
                .body(())
                .expect("request")
                .into_parts();
            Nace { systemone: native }
                .request(&mut head)
                .expect("dialect");
            assert_eq!(
                head.uri.path(),
                if native { "/systemone" } else { "/decisions" }
            );
            assert_eq!(
                head.headers.get("x-nace-legend").map(|v| v.as_bytes()),
                native.then_some(b"object".as_slice())
            );
        }
    }

    #[test]
    fn models_are_merged_with_prices_limits_modalities_and_aliases() {
        // Trimmed from the live schema 2.4 feed; readiness is not availability.
        let public = listed(&json!({"data":[
            {"schema_version":"2.4", "id":"drex-v2.0", "name":"Drex 2.0",
             "description":"Drex 2.0 decision model, a mixture of experts with 3B active parameters.",
             "input_modalities":[{"type":"text",
                "supported_inputs":{"max_context_length":{"value":262144,"unit":"token"}},
                "pricing":[{"type":"prompt","unit":"token","cost_usd":"0.000000060"}]}],
             "output_modalities":[{"type":"decisions"}], "is_ready":false},
            {"schema_version":"2.4", "id":"drex-max", "name":"Drex Max",
             "description":"Drex Max multimodal decision model, the most accurate Drex.",
             "input_modalities":[{"type":"text",
                "supported_inputs":{"max_context_length":{"value":262144,"unit":"token"}},
                "pricing":[{"type":"prompt","unit":"token","cost_usd":"0.000000030"}]},
                {"type":"image"}], "output_modalities":[{"type":"decisions"}], "is_ready":false}
        ]}));
        let names = json!({"models":[
            {"name":"drex-latest", "alias_for":"drex-v2.0"},
            {"name":"drex-v1.0", "alias_for":"drex-max"},
            {"name":"drex-v1.1", "alias_for":"drex-v1.5"},
            {"name":"drex-v2.0"}, {"name":"drex-max"}
        ]});
        let merged = merge_models(public, &names);
        assert_eq!(merged.len(), 5);
        for (target, alias, price, modalities) in
            [(0, 2, 0.06, vec![]), (1, 3, 0.03, vec![Modality::Image])]
        {
            let model = &merged[target];
            let priced = model.price.expect("prompt price");
            assert!((priced.input - price).abs() < 1e-12);
            assert_eq!(priced.output, 0.0);
            assert_eq!(model.context_window, Some(262144));
            assert_eq!(model.input_modalities, modalities);
            let mut expected = model.clone();
            expected.id.clone_from(&merged[alias].id);
            assert_eq!(
                merged[alias], expected,
                "an alias keeps the whole target card"
            );
        }
        assert_eq!(merged[2].id, "drex-latest");
        assert_eq!(merged[3].id, "drex-v1.0");
        assert_eq!(merged[4].price, None, "unknown target");
        let fallback = merge_models(Vec::new(), &names);
        assert_eq!(fallback.len(), 5);
        for model in merged.iter().chain(&fallback) {
            assert_eq!(model.kind.as_deref(), Some("decision"), "{}", model.id);
        }
    }
}
