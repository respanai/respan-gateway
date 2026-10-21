//! TypeSafe: System One, its decision models' API.

use respan_gateway_decision_http::Decisions;
use respan_gateway_decision_http::dialect::{Dialect, Dialected};

use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Model, Models, Vendor, VendorProvider, built_wrong, http, json,
};

/// Where its API is.
const BASE_URL: &str = "https://api.typesafe.ai/v1";

/// TypeSafe.
pub const VENDOR: Vendor = Vendor {
    id: "typesafe",
    key_env: &["TYPESAFE_API_KEY"],
    formats: &[(&Dialected::new(Decisions, TypeSafe), Some(BASE_URL))],
    models: list_models,
    ..Vendor::DEFAULT
};

/// TypeSafe's decisions: it serves them as System One, at `/systemone`.
#[derive(Debug, Clone, Copy)]
pub struct TypeSafe;

impl Dialect for TypeSafe {
    fn path(&self) -> Option<&'static str> {
        Some("/systemone")
    }
}

/// Its models: `{"models": [{"name", ..}]}`, each model or alias by the
/// name a call takes.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        let answer = json(answer).await?;
        let models = answer["models"].as_array().into_iter().flatten();
        let model = |m: &Value| {
            let mut model = Model::new(m["name"].as_str()?);
            model.description = m["description"].as_str().map(str::to_owned);
            Some(model)
        };
        Ok(models.filter_map(model).collect())
    })
}
