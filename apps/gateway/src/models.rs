//! `GET /v1/models`: every provider's models in one OpenAI-shaped list,
//! each named as a client asks for it: as it is if routing sends that name
//! to its provider, else as `provider/model`. `GET /v1beta/models` is the
//! same list in Gemini's shape, for Gemini SDKs; a request to `/v1/models`
//! with `anthropic-version` gets Anthropic's shape.

use parking_lot::Mutex;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use respan_gateway_core::Fault;
use respan_gateway_core::futures::future::join_all;
use respan_gateway_http::http::{HeaderMap, StatusCode};
use respan_gateway_http_faults::Status;
use serde_json::{Value, json};

use crate::auth;
use crate::compose::Upstream;
use crate::formats::{first_of_each_kind, kind};
use crate::requests::rfc3339;
use respan_gateway_plugin_routing::Routes;
use respan_gateway_vendors::{Model, VendorProvider};

/// How long a provider may take to list its models.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long a merged list is reused.
const FRESH: Duration = Duration::from_secs(60);

/// One provider, which lists its models.
#[derive(Debug)]
struct Listing {
    provider: String,
    /// Where its models come from.
    source: VendorProvider,
    /// A format of each kind it serves.
    kinds: Vec<(&'static str, String)>,
    /// The models the config declares of it, laid over its list's.
    declared: Vec<Model>,
}

/// Why a provider gave no list, and the status it answered with, if any.
struct Refusal {
    status: Option<u16>,
    why: String,
}

/// How a provider answered a call made with its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Reachable,
    /// It refused the key, answering with this status.
    Unauthorized(u16),
    /// It did not answer, or not with a list; why.
    Unreachable(String),
}

/// A provider's [`Reach`] and how long it took to answer.
#[derive(Debug, Clone)]
pub struct Probe {
    pub provider: String,
    pub reach: Reach,
    pub took: Duration,
}

#[derive(Debug)]
pub struct Models {
    listings: Vec<Listing>,
    routes: Arc<Routes>,
    cached: Mutex<Option<(Instant, Value)>>,
    /// Whether every provider has answered its first listing, or failed to.
    listed: tokio::sync::watch::Sender<bool>,
}

impl Models {
    pub fn new(upstreams: &[Upstream], routes: Arc<Routes>) -> Self {
        let listings = upstreams.iter().map(|u| Listing {
            provider: u.name.clone(),
            source: u.provider.clone(),
            kinds: first_of_each_kind(u.entries.iter().map(|e| (e.kind, e.format.clone()))),
            declared: u.models.clone(),
        });
        Self {
            listings: listings.collect(),
            routes,
            cached: Mutex::default(),
            listed: tokio::sync::watch::channel(false).0,
        }
    }

    /// Becomes `true` once every provider has answered its first listing, or
    /// failed to: the catalog then holds what their lists say, such as the
    /// prices calls are charged at.
    pub fn listed(&self) -> tokio::sync::watch::Receiver<bool> {
        self.listed.subscribe()
    }

    /// Lists every provider's models now and every `every` after, while
    /// anything but this task holds `models`, so routes that pick by price
    /// or capability know them before a client lists.
    pub fn refresh(models: &Arc<Self>, every: Duration) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let models: Weak<Self> = Arc::downgrade(models);
        runtime.spawn(async move {
            loop {
                let Some(held) = models.upgrade() else { return };
                let _ = held.collect().await;
                held.listed.send_replace(true);
                drop(held);
                tokio::time::sleep(every).await;
            }
        });
    }

    /// The merged list, fetched again once a minute old; not kept while a
    /// provider fails to list. `Err` says why, when every provider failed.
    pub async fn list(&self) -> Result<Value, String> {
        let fresh = |at: &Instant| at.elapsed() < FRESH;
        if let Some((_, list)) = self.lock().as_ref().filter(|(at, _)| fresh(at)) {
            return Ok(list.clone());
        }
        let (data, failed) = self.collect().await;
        if data.is_empty() && !failed.is_empty() {
            return Err(format!(
                "no provider listed its models: {}",
                failed.join("; ")
            ));
        }
        let list = json!({ "object": "list", "data": data });
        if failed.is_empty() {
            *self.lock() = Some((Instant::now(), list.clone()));
        }
        Ok(list)
    }

    /// Every provider's models now, and why each provider that did not
    /// list is missing.
    pub async fn collect(&self) -> (Vec<Value>, Vec<String>) {
        let fetched = join_all(self.listings.iter().map(|l| self.fetch(l))).await;
        let (listed, failed): (Vec<_>, Vec<_>) = fetched.into_iter().partition(Result::is_ok);
        let failed = failed.into_iter().filter_map(Result::err).collect();
        let listed = listed.into_iter().flatten().flatten();
        (self.routes().chain(listed).collect(), failed)
    }

    /// The routes named for one model, as models a client may ask for:
    /// owned by `routes`, of the kind their first candidate's model is.
    fn routes(&self) -> impl Iterator<Item = Value> + '_ {
        let rules = self.routes.rules().iter().filter(|r| !r.is_glob());
        rules.map(|rule| {
            let first = rule.to.first().and_then(|c| c.model_for(Some(&rule.name)));
            let kind = kind_of(first.as_deref().unwrap_or(&rule.name));
            let to: Vec<String> = rule.to.iter().map(ToString::to_string).collect();
            json!({
                "id": rule.name,
                "object": "model",
                "owned_by": "routes",
                "kind": kind,
                "route": { "pick": rule.pick.name(), "to": to },
            })
        })
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Option<(Instant, Value)>> {
        self.cached.lock()
    }

    /// One provider's models, or why it gave none; `None` if it lists none.
    async fn get(&self, listing: &Listing) -> Option<Result<Vec<Model>, Refusal>> {
        let listed = tokio::time::timeout(TIMEOUT, listing.source.models()).await;
        let refused = |fault: Fault| match fault.get::<Status>() {
            Some(Status(status)) => Refusal {
                status: Some(status.as_u16()),
                why: format!("answered {}", status.as_u16()),
            },
            None => Refusal {
                status: None,
                why: fault.message().to_owned(),
            },
        };
        match listed {
            Ok(listed) => Some(listed?.map_err(refused)),
            Err(_) => Some(Err(Refusal {
                status: None,
                why: format!("no answer within {}s", TIMEOUT.as_secs()),
            })),
        }
    }

    /// One call to each provider that lists its models, with its key: how
    /// each answered and how long it took. A provider that lists nothing
    /// is not called.
    pub async fn probe(&self) -> Vec<Probe> {
        let probes = join_all(self.listings.iter().map(|l| self.probe_one(l))).await;
        probes.into_iter().flatten().collect()
    }

    async fn probe_one(&self, listing: &Listing) -> Option<Probe> {
        let started = Instant::now();
        let reach = match self.get(listing).await? {
            Ok(_) => Reach::Reachable,
            Err(Refusal {
                status: Some(s @ (401 | 403)),
                ..
            }) => Reach::Unauthorized(s),
            Err(Refusal { why, .. }) => Reach::Unreachable(why),
        };
        Some(Probe {
            provider: listing.provider.clone(),
            reach,
            took: started.elapsed(),
        })
    }

    /// One provider's models, or why it did not say.
    async fn fetch(&self, listing: &Listing) -> Result<Vec<Value>, String> {
        let Some(listed) = self.get(listing).await else {
            return Ok(Vec::new());
        };
        let models = listed.map_err(|r| format!("{}: {}", listing.provider, r.why))?;
        // The list's models, each of the kind the list says, else its name,
        // with the config's declaration laid over it; then the models it
        // declares that the list does not name.
        let mut cards: Vec<Model> = models;
        for card in &mut cards {
            card.provider.clone_from(&listing.provider);
            card.kind
                .get_or_insert_with(|| kind_of(&card.id).to_owned());
            if let Some(declared) = listing.declared.iter().find(|d| d.id == card.id) {
                card.overlay(declared);
            }
        }
        let unlisted = (listing.declared.iter()).filter(|d| !cards.iter().any(|c| c.id == d.id));
        cards.extend(unlisted.cloned().collect::<Vec<_>>());
        self.routes
            .catalog()
            .set(&listing.provider, cards.iter().cloned());
        let models = cards
            .iter()
            .filter_map(|card| {
                let kind = card.kind.as_deref().unwrap_or_else(|| kind_of(&card.id));
                let (kind, format) = listing.kinds.iter().find(|(k, _)| *k == kind)?;
                let mut model = serde_json::to_value(card).ok()?;
                model["id"] = self.public_id(listing, kind, format, &card.id).into();
                model["object"] = "model".into();
                Some(model)
            })
            .collect();
        Ok(models)
    }

    fn public_id(&self, listing: &Listing, kind: &'static str, format: &str, id: &str) -> String {
        let route = self.routes.route(kind, format, Some(id));
        match route {
            Some(route)
                if route.provider == listing.provider && route.model.as_deref() == Some(id) =>
            {
                id.to_owned()
            }
            _ => format!("{}/{id}", listing.provider),
        }
    }
}

/// What a model does, by its name as vendors spell them; chat unless the
/// name says otherwise.
pub(crate) fn kind_of(id: &str) -> &'static str {
    let id = id.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| id.contains(w));
    match () {
        _ if has(&["embed"]) => kind::EMBEDDING,
        _ if has(&["rerank"]) => kind::RERANK,
        _ if has(&[
            "dall-e",
            "gpt-image",
            "imagen",
            "-image",
            "nano-banana",
            "flux",
        ]) =>
        {
            kind::IMAGE
        }
        // Before transcription: a realtime transcriber is a session.
        _ if has(&["realtime", "-live", "live-", "native-audio"]) => kind::REALTIME,
        _ if has(&["whisper", "transcribe"]) => kind::TRANSCRIPTION,
        _ if has(&["tts"]) => kind::SPEECH,
        _ if has(&["sora", "veo"]) => kind::VIDEO,
        _ if has(&["jev", "clef", "span-02", "decision", "systemone"]) => kind::DECISION,
        _ => kind::CHAT,
    }
}

/// `GET /v1/models`: the merged list in OpenAI's shape, or in Anthropic's
/// for a request that carries `anthropic-version`, as Anthropic's SDKs send.
pub async fn list(State(models): State<Arc<Models>>, headers: HeaderMap) -> Response {
    let anthropic = headers.contains_key("anthropic-version");
    match (models.list().await, anthropic) {
        (Ok(list), false) => Json(list).into_response(),
        (Ok(list), true) => Json(anthropic_list(&list)).into_response(),
        (Err(why), false) => auth::error(StatusCode::BAD_GATEWAY, "upstream_error", &why),
        (Err(why), true) => {
            let error = json!({"type": "error", "error": {"type": "api_error", "message": why}});
            (StatusCode::BAD_GATEWAY, Json(error)).into_response()
        }
    }
}

/// The chat models of an OpenAI-shaped `list` as Anthropic lists them, in
/// one page.
fn anthropic_list(list: &Value) -> Value {
    let models = list["data"].as_array().into_iter().flatten();
    let data: Vec<Value> = models
        .filter(|m| m["kind"] == "chat")
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let created = m["created"].as_u64().unwrap_or_default();
            let created = SystemTime::UNIX_EPOCH + Duration::from_secs(created);
            let mut model = json!({
                "type": "model",
                "id": id,
                "display_name": m["display_name"].as_str().unwrap_or(id),
                "created_at": rfc3339(created),
            });
            if let Some(n) = m["context_window"].as_u64() {
                model["max_input_tokens"] = n.into();
            }
            if let Some(n) = m["max_output_tokens"].as_u64() {
                model["max_tokens"] = n.into();
            }
            Some(model)
        })
        .collect();
    json!({
        "first_id": data.first().map(|m| &m["id"]),
        "last_id": data.last().map(|m| &m["id"]),
        "has_more": false,
        "data": data,
    })
}

/// A Gemini-shaped error body.
fn gemini_error(status: StatusCode, name: &str, message: &str) -> Response {
    let body = json!({"error": {"code": status.as_u16(), "message": message, "status": name}});
    (status, Json(body)).into_response()
}

/// The chat models of an OpenAI-shaped `list` as Gemini lists them: the ones
/// `generateContent` serves.
fn gemini_models(list: &Value) -> Vec<Value> {
    let models = list["data"].as_array().into_iter().flatten();
    models
        .filter(|m| m["kind"] == "chat")
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let mut model = json!({
                "name": format!("models/{id}"),
                "baseModelId": id,
                "displayName": m["display_name"].as_str().unwrap_or(id),
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            });
            let fields = [
                ("description", "description"),
                ("context_window", "inputTokenLimit"),
                ("max_output_tokens", "outputTokenLimit"),
            ];
            for (from, to) in fields {
                if !m[from].is_null() {
                    model[to] = m[from].clone();
                }
            }
            Some(model)
        })
        .collect()
}

/// `GET /v1beta/models`: the merged list in Gemini's shape.
pub async fn list_gemini(State(models): State<Arc<Models>>) -> Response {
    match models.list().await {
        Ok(list) => Json(json!({ "models": gemini_models(&list) })).into_response(),
        Err(why) => gemini_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE", &why),
    }
}

/// `GET /v1beta/models/{model}`, or `.../models/models/{model}`: one of the
/// merged list.
pub async fn get_gemini(State(models): State<Arc<Models>>, Path(model): Path<String>) -> Response {
    let list = match models.list().await {
        Ok(list) => list,
        Err(why) => return gemini_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE", &why),
    };
    let wanted = format!("models/{model}");
    match gemini_models(&list)
        .into_iter()
        .find(|m| m["name"] == wanted)
    {
        Some(found) => Json(found).into_response(),
        None => gemini_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("{wanted} is not found"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_models_kind_is_read_from_its_name() {
        let kind = kind_of;
        assert_eq!(kind("gpt-4o-mini"), "chat");
        assert_eq!(kind("gpt-image-1"), "image");
        assert_eq!(kind("imagen-4.0-generate-001"), "image");
        assert_eq!(kind("gemini-3.1-flash-image"), "image");
        assert_eq!(kind("nano-banana-pro-preview"), "image");
        assert_eq!(kind("whisper-large-v3"), "transcription");
        assert_eq!(kind("gpt-4o-mini-transcribe"), "transcription");
        assert_eq!(kind("gpt-4o-mini-tts"), "speech");
        assert_eq!(kind("sora-2"), "video");
        assert_eq!(kind("veo-3.0-generate-001"), "video");
        assert_eq!(kind("gpt-realtime"), "realtime");
        assert_eq!(kind("voxtral-mini-transcribe-realtime-2602"), "realtime");
        assert_eq!(kind("gemini-live-2.5-flash-preview"), "realtime");
        assert_eq!(kind("gemini-2.0-flash-live-001"), "realtime");
        assert_eq!(
            kind("gemini-2.5-flash-native-audio-preview-09-2025"),
            "realtime"
        );
        assert_eq!(kind("gemini-2.5-flash"), "chat");
        assert_eq!(kind("text-embedding-3-small"), "embedding");
        assert_eq!(kind("gemini-embedding-001"), "embedding");
        assert_eq!(kind("rerank-v3.5"), "rerank");
        assert_eq!(kind("jev-latest"), "decision");
        assert_eq!(kind("span-02"), "decision");
        assert_eq!(kind("gpt-6-luna-decisions"), "decision");
    }
}
