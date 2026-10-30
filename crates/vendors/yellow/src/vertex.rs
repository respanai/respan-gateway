//! Google Vertex AI: Gemini's `generateContent` at a project and location,
//! with a Google access token. And its express mode ([`EXPRESS`]): Gemini's
//! `generateContent` under no project, with an API key.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Dialect, Dialected, KeyIn, Model, Models, Vendor, VendorProvider, built_wrong,
    http, json as body_json, located, models,
};

/// A project's location, under which each of its APIs is.
macro_rules! location {
    () => {
        "https://{GOOGLE_CLOUD_LOCATION}-aiplatform.googleapis.com/v1/projects/{GOOGLE_CLOUD_PROJECT}/locations/{GOOGLE_CLOUD_LOCATION}"
    };
}

/// Google's models, in Gemini's own API.
const API: &str = concat!(location!(), "/publishers/google");

/// Where express mode's Gemini is: under no project, at the global host.
const EXPRESS_API: &str = "https://aiplatform.googleapis.com/v1/publishers/google";

/// Where Google's models are listed, after its origin, a page of at most
/// 300 at a time.
const MODELS: &str = "/v1beta1/publishers/google/models?pageSize=300";

/// Google Vertex AI. The key is a Google access token, such as `gcloud
/// auth print-access-token` prints, which expires within the hour: the
/// gateway sends the one it is given, so the host gives it a fresh one.
/// The `global` location is at the host of no region.
pub const VENDOR: Vendor = Vendor {
    id: "vertex",
    key_env: &["GOOGLE_VERTEX_ACCESS_TOKEN", "CLOUDSDK_AUTH_ACCESS_TOKEN"],
    formats: &[(&Dialected::new(GeminiGenerate, Vertex), Some(API))],
    models: list_models,
    locate,
    ..Vendor::DEFAULT
};

/// Vertex AI in express mode: Gemini's `generateContent` under no project,
/// with an API key in `x-goog-api-key`, as Gemini's API takes it. It lists
/// no models, as its API serves `generateContent`, `streamGenerateContent`
/// and `countTokens` alone.
pub const EXPRESS: Vendor = Vendor {
    id: "vertex-express",
    key_env: &["GOOGLE_VERTEX_API_KEY"],
    formats: &[(&GeminiGenerate, Some(EXPRESS_API))],
    models: models::none,
    ..Vendor::DEFAULT
};

/// `url` located, its `global` location at the host of no region,
/// `aiplatform.googleapis.com`, as Google has it: no host is
/// `global-aiplatform.googleapis.com`.
fn locate(url: &str) -> Result<String, &str> {
    let url = located(url)?;
    Ok(match url.strip_prefix("https://global-aiplatform.") {
        Some(rest) => format!("https://aiplatform.{rest}"),
        None => url,
    })
}

/// Vertex's `generateContent`: its key a bearer token, not Gemini's
/// `x-goog-api-key`.
#[derive(Debug, Clone, Copy)]
pub struct Vertex;

impl Dialect for Vertex {
    type Input = Input;
    type Output = Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Bearer)
    }
}

/// Its models: Google's list ([`MODELS`]) at its API's origin (its base
/// before `/v1/`), as the list is under no project; as OpenAI lists them
/// for a provider given no base of `generateContent`. A list says no kind,
/// limits or prices, so a model has its id and maker alone.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let Some(base) = p.base_url_of(GeminiGenerate::NAME) else {
            return models::openai(p).await;
        };
        let origin = base.split_once("/v1/").map_or(base, |(origin, _)| origin);
        let client = p.client_at(origin)?;
        let (mut models, mut at) = (Vec::new(), MODELS.to_owned());
        loop {
            let call = http::Request::get(&at).body(Body::default());
            let (answer, _) = client.send(call.map_err(built_wrong)?).await?;
            let answer = body_json(answer).await?;
            models.extend(listed(&answer));
            match answer["nextPageToken"].as_str() {
                Some(next) if !next.is_empty() => at = format!("{MODELS}&pageToken={next}"),
                _ => return Ok(models),
            }
        }
    })
}

/// The models of Google's list that a call reaches: those Vertex gives a
/// resource, none that must be deployed first (`deploy`), and none of its
/// open checkpoints, which are notebooks.
fn listed(answer: &Value) -> Vec<Model> {
    let models = answer["publisherModels"].as_array().into_iter().flatten();
    let model = |m: &Value| {
        let actions = m["supportedActions"].as_object();
        let deploy = actions.is_some_and(|a| a.contains_key("deploy"));
        let open = m["openSourceCategory"].as_str().unwrap_or_default();
        let resource = m.get("publisherModelTemplate").is_some();
        let served = resource && !deploy && !open.contains("OSS");
        let name = m["name"].as_str().filter(|_| served)?;
        let id = name
            .strip_prefix("publishers/google/models/")
            .unwrap_or(name);
        let mut model = Model::new(id);
        model.maker = Some("google".to_owned());
        Some(model)
    };
    models.filter_map(model).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_global_location_is_at_the_host_of_no_region() {
        let url = "https://global-aiplatform.googleapis.com/v1/projects/p/locations/global";
        assert_eq!(
            locate(url).as_deref(),
            Ok("https://aiplatform.googleapis.com/v1/projects/p/locations/global")
        );
        let regional = "https://us-east5-aiplatform.googleapis.com/v1";
        assert_eq!(locate(regional).as_deref(), Ok(regional));
    }

    #[test]
    fn its_models_are_those_a_call_reaches() {
        let answer = json!({"publisherModels": [
            {"name": "publishers/google/models/gemini-3.5-flash",
             "publisherModelTemplate": "projects/{project}/..",
             "supportedActions": {"openGenerationAiStudio": {}}},
            {"name": "publishers/google/models/gemma3",
             "supportedActions": {"deploy": {}, "multiDeployVertex": {}}},
            {"name": "publishers/google/models/occupancy-analytics",
             "supportedActions": {"createApplication": {}}},
            {"name": "publishers/google/models/weather-next-v2",
             "openSourceCategory": "PROPRIETARY",
             "supportedActions": {"requestAccess": {}}},
            {"name": "publishers/google/models/imageclassification-proprietary-vit",
             "openSourceCategory": "THIRD_PARTY_OWNED_OSS",
             "publisherModelTemplate": "projects/{project}/.."},
        ]});
        let ids: Vec<_> = listed(&answer)
            .into_iter()
            .map(|m| (m.id, m.maker))
            .collect();
        assert_eq!(ids, [("gemini-3.5-flash".into(), Some("google".into()))]);
    }
}
