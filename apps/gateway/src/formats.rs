//! Every format this binary speaks, and what the rest of the binary needs
//! to know of each: its kind, its endpoints, and how routing reads a
//! request's model. How a provider of it is reached is its vendor's
//! ([`respan_gateway_vendors::VendorProvider`]); every format a vendor
//! speaks is one of these.

use std::sync::Arc;

use respan_gateway_conversation as conversation;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Fault, Format, Gateway};
use respan_gateway_decision as decision;
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::{ClientIo, ProviderIo, raw::Original};
use respan_gateway_media::{embedding, image, speech, transcription, video};
use respan_gateway_media_http::bedrock::{BedrockEmbeddings, BedrockImages};
use respan_gateway_media_http::gemini::{GeminiImages, GeminiVeo};
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions, OpenAiVideos,
};
use respan_gateway_passthrough_http::Passthrough;
use respan_gateway_plugin_routing::{Model, Routes};
use respan_gateway_realtime as realtime;
use respan_gateway_realtime_ws::{
    GeminiLive, MistralRealtime, OpenAiRealtime, ResponsesWs, SessionModel, requested_model,
};

/// The kinds of traffic this binary carries, as the admin API and routing
/// name them.
pub mod kind {
    pub const CHAT: &str = "chat";
    pub const REALTIME: &str = "realtime";
    /// Responses over WebSocket: a session of many requests and replies,
    /// which no other format speaks.
    pub const RESPONSES: &str = "responses";
    pub const PASSTHROUGH: &str = "passthrough";
    pub const IMAGE: &str = "image";
    pub const TRANSCRIPTION: &str = "transcription";
    pub const SPEECH: &str = "speech";
    pub const VIDEO: &str = "video";
    pub const EMBEDDING: &str = "embedding";
    pub const DECISION: &str = "decision";
    /// MCP: tools and resources over streamable HTTP, served by the `mcp`
    /// section.
    pub const MCP: &str = "mcp";
    /// The kinds of one request and its answer, whose client formats are
    /// served by default once a provider speaks the kind.
    pub const ONE_SHOT: [&str; 6] = [IMAGE, TRANSCRIPTION, SPEECH, VIDEO, EMBEDDING, DECISION];
}

/// The first of `items` of each kind, in order.
pub fn first_of_each_kind<T>(
    items: impl IntoIterator<Item = (&'static str, T)>,
) -> Vec<(&'static str, T)> {
    let mut firsts: Vec<(&'static str, T)> = Vec::new();
    for (kind, item) in items {
        if !firsts.iter().any(|(k, _)| *k == kind) {
            firsts.push((kind, item));
        }
    }
    firsts
}

/// Registers a format's clients with the gateway, given the routes.
type Serve = dyn Fn(Gateway, &Arc<Routes>) -> Result<Gateway, Fault> + Send + Sync;

/// Which of the requests a client format matches it takes, by the routes.
type Claims = fn(&Routes, &ClientIo) -> bool;

/// A format compiled into the binary.
pub struct KnownFormat {
    pub name: String,
    pub kind: &'static str,
    /// What its clients send, as `METHOD /path`.
    pub endpoints: Vec<String>,
    pub websocket: bool,
    /// What its video jobs' ids start with, which routes a follow-up.
    pub job_ids: Option<&'static str>,
    /// Whether its clients take only some of the requests it matches,
    /// which another format of their endpoint takes otherwise.
    pub claims: bool,
    /// None for a format only providers speak.
    serve: Option<Box<Serve>>,
    /// How its requests name their model.
    pub model: Model,
}

impl std::fmt::Debug for KnownFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnownFormat")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl KnownFormat {
    /// `format` of `kind`, its clients sending to `endpoints`.
    pub fn of<F, I, O>(kind: &'static str, endpoints: &[&str], format: F) -> Self
    where
        F: Clone + Send + Sync + 'static + Format<ClientIo, I, O> + Format<ProviderIo, O, I>,
        I: Clone + Send + Sync + 'static,
        O: Clone + Send + Sync + 'static,
    {
        let client = format.clone();
        let mut known = Self::provider_only::<F, ProviderIo, I, O>(kind, &format);
        known.endpoints = endpoints.iter().map(|e| (*e).to_owned()).collect();
        known.serve = Some(Box::new(move |gateway, _| {
            gateway.client::<I, O, _>(client.clone())
        }));
        known
    }

    /// `format` of `kind`, its clients sending to `endpoints`, taking of
    /// the requests it matches those `claims` says.
    pub fn claiming<F, I, O>(
        kind: &'static str,
        endpoints: &[&str],
        format: F,
        claims: Claims,
    ) -> Self
    where
        F: Clone + Send + Sync + 'static + Format<ClientIo, I, O> + Format<ProviderIo, O, I>,
        I: Clone + Send + Sync + 'static,
        O: Clone + Send + Sync + 'static,
    {
        let mut known = Self::of(kind, endpoints, format.clone());
        known.claims = true;
        known.serve = Some(Box::new(move |gateway, routes| {
            let routes = routes.clone();
            let claimed = Claimed {
                format: format.clone(),
                routes,
                claims,
            };
            gateway.client::<I, O, _>(claimed)
        }));
        known
    }

    /// `format` of `kind`, which only providers speak, over any transport.
    pub fn provider_only<F, T, I, O>(kind: &'static str, format: &F) -> Self
    where
        F: Format<T, O, I>,
    {
        Self {
            name: format.name().to_owned(),
            kind,
            endpoints: Vec::new(),
            websocket: false,
            job_ids: None,
            claims: false,
            serve: None,
            model: Model::none(),
        }
    }

    /// Registers what it serves with `serve` in place of its client format.
    pub fn serving(
        mut self,
        serve: impl Fn(Gateway) -> Result<Gateway, Fault> + Send + Sync + 'static,
    ) -> Self {
        self.serve = Some(Box::new(move |gateway, _| serve(gateway)));
        self
    }

    fn websocket(mut self) -> Self {
        self.websocket = true;
        self
    }

    pub fn model(mut self, model: Model) -> Self {
        self.model = model;
        self
    }

    fn job_ids(mut self, prefix: &'static str) -> Self {
        self.job_ids = Some(prefix);
        self
    }

    /// Whether clients may speak it.
    pub fn servable(&self) -> bool {
        self.serve.is_some()
    }

    pub fn serve(&self, gateway: Gateway, routes: &Arc<Routes>) -> Result<Gateway, Fault> {
        match &self.serve {
            Some(serve) => serve(gateway, routes),
            None => Err(Fault::config(format!(
                "formats: {} is spoken only by providers; no client sends it",
                self.name
            ))),
        }
    }
}

/// What clients of each format send, as `METHOD /path`.
mod endpoints {
    pub const CHAT_COMPLETIONS: &[&str] = &["POST /v1/chat/completions"];
    pub const RESPONSES: &[&str] = &["POST /v1/responses"];
    pub const MESSAGES: &[&str] = &["POST /v1/messages"];
    pub const GENERATE_CONTENT: &[&str] = &[
        "POST /v1beta/models/{model}:generateContent",
        "POST /v1beta/models/{model}:streamGenerateContent",
    ];
    pub const IMAGES: &[&str] = &["POST /v1/images/generations", "POST /v1/images/edits"];
    pub const TRANSCRIPTIONS: &[&str] = &[
        "POST /v1/audio/transcriptions",
        "POST /v1/audio/translations",
    ];
    pub const SPEECH: &[&str] = &["POST /v1/audio/speech"];
    pub const EMBEDDINGS: &[&str] = &["POST /v1/embeddings"];
    pub const DECISIONS: &[&str] = &["POST /v1/decisions", "POST /v1/systemone"];
    pub const VIDEOS: &[&str] = &[
        "POST /v1/videos",
        "GET /v1/videos",
        "GET /v1/videos/{id}",
        "DELETE /v1/videos/{id}",
        "GET /v1/videos/{id}/content",
        "POST /v1/videos/{id}/remix",
    ];
    pub const VEO: &[&str] = &[
        "POST /v1beta/models/{model}:predictLongRunning",
        "GET /v1beta/{operation}",
        "GET /v1beta/files/{id}:download",
    ];
    pub const PASSTHROUGH: &[&str] = &["ANY /* (every other request)"];
}

/// Every format this binary speaks, each a vendor speaks or passthrough.
pub fn formats() -> Vec<KnownFormat> {
    vec![
        KnownFormat::of(kind::CHAT, endpoints::CHAT_COMPLETIONS, OpenAiChat).model(chat()),
        KnownFormat::of(kind::CHAT, endpoints::RESPONSES, OpenAiResponses).model(chat()),
        session(kind::REALTIME, OpenAiRealtime, OpenAiRealtime::PATH, false).model(realtime()),
        session(kind::RESPONSES, ResponsesWs, ResponsesWs::PATH, false),
        KnownFormat::of(kind::IMAGE, endpoints::IMAGES, OpenAiImages)
            .model(Model::of(image::Input::model, image::Input::set_model)),
        KnownFormat::of(
            kind::TRANSCRIPTION,
            endpoints::TRANSCRIPTIONS,
            OpenAiTranscriptions,
        )
        .model(Model::of(
            transcription::Input::model,
            transcription::Input::set_model,
        )),
        KnownFormat::of(kind::SPEECH, endpoints::SPEECH, OpenAiSpeech)
            .model(Model::of(speech::Input::model, speech::Input::set_model)),
        // Only a `Create` names a model; a follow-up goes by its job's id
        // ([`crate::jobs`]).
        KnownFormat::of(kind::VIDEO, endpoints::VIDEOS, OpenAiVideos)
            .job_ids("video_")
            .model(Model::of(video::Input::model, video::Input::set_model)),
        KnownFormat::of(kind::EMBEDDING, endpoints::EMBEDDINGS, OpenAiEmbeddings).model(Model::of(
            embedding::Input::model,
            embedding::Input::set_model,
        )),
        KnownFormat::of(kind::CHAT, endpoints::MESSAGES, AnthropicMessages).model(chat()),
        KnownFormat::of(kind::CHAT, endpoints::GENERATE_CONTENT, GeminiGenerate).model(chat()),
        KnownFormat::claiming(
            kind::IMAGE,
            endpoints::GENERATE_CONTENT,
            GeminiImages,
            draws,
        )
        .model(Model::of(image::Input::model, image::Input::set_model)),
        session(kind::REALTIME, GeminiLive, GeminiLive::PATH, true).model(realtime()),
        session(
            kind::REALTIME,
            MistralRealtime,
            MistralRealtime::PATH,
            false,
        )
        .model(realtime()),
        KnownFormat::of(kind::VIDEO, endpoints::VEO, GeminiVeo)
            .job_ids("veo_")
            .model(Model::of(video::Input::model, video::Input::set_model)),
        KnownFormat::provider_only(kind::EMBEDDING, &BedrockEmbeddings),
        KnownFormat::provider_only(kind::IMAGE, &BedrockImages),
        KnownFormat::of(kind::DECISION, endpoints::DECISIONS, Decisions).model(Model::of(
            decision::Input::model,
            decision::Input::set_model,
        )),
        KnownFormat::of(kind::PASSTHROUGH, endpoints::PASSTHROUGH, Passthrough),
    ]
}

/// Whether a `generateContent` call is for an image ([`drawn_elsewhere`]).
fn draws(routes: &Routes, io: &ClientIo) -> bool {
    let model = GeminiImages::model(io.request().uri());
    model.is_some_and(|model| drawn_elsewhere(routes, &model))
}

/// Whether routing sends `model`, an image model, to a provider that cannot
/// take a call of it as `generateContent`. Gemini's own image models are
/// asked as they were, as chats, which keep a conversation and its text.
pub(crate) fn drawn_elsewhere(routes: &Routes, model: &str) -> bool {
    let Some(to) = routes.route(kind::IMAGE, GeminiImages::NAME, Some(model)) else {
        return false;
    };
    let model = to.model.as_deref().unwrap_or(model);
    let card = routes.catalog().get(&to.provider, model);
    let kind = card.and_then(|c| c.kind);
    let kind = kind
        .as_deref()
        .unwrap_or_else(|| crate::models::kind_of(model));
    kind == kind::IMAGE && !routes.speaks(&to.provider, GeminiGenerate.name())
}

/// A client format taking only the requests it matches that `claims` says.
#[derive(Clone)]
struct Claimed<F> {
    format: F,
    routes: Arc<Routes>,
    claims: Claims,
}

impl<F, I, O> Format<ClientIo, I, O> for Claimed<F>
where
    F: Format<ClientIo, I, O>,
{
    fn name(&self) -> &str {
        self.format.name()
    }

    fn matches(&self, io: &ClientIo) -> bool {
        self.format.matches(io) && (self.claims)(&self.routes, io)
    }

    fn bind(&self, io: ClientIo) -> Result<respan_gateway_core::Connection<I, O>, Fault> {
        self.format.bind(io)
    }
}

/// How the model of a chat request is named.
fn chat() -> Model {
    Model::of(conversation::Input::model, conversation::Input::set_model)
}

/// How a realtime session names its model: at `?model=` on its handshake,
/// as its first input comes only once it is routed (a Gemini Live client
/// without it goes to the default). The model routing chose is dialed
/// ([`SessionModel`]) and asked for by the session's opening input.
fn realtime() -> Model {
    Model::new(
        |cx| requested_model(cx.extensions().get::<Original>()?),
        |cx, model| {
            cx.extensions_mut().insert(SessionModel(model.clone()));
            cx.filter(realtime::set_model(model))
        },
    )
}

/// A session `format` of `kind`, its clients sending to its `path`: after
/// `/v1`, as an SDK adds it to the gateway's base URL, unless `at_origin`.
fn session<F>(kind: &'static str, format: F, path: &str, at_origin: bool) -> KnownFormat
where
    F: Clone
        + Send
        + Sync
        + 'static
        + Format<ClientIo, realtime::Input, realtime::Output>
        + Format<ProviderIo, realtime::Output, realtime::Input>,
{
    let endpoint = match at_origin {
        true => format!("GET {path} (WebSocket)"),
        false => format!("GET /v1{path} (WebSocket)"),
    };
    KnownFormat::of(kind, &[&endpoint], format).websocket()
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_vendors::ALL;

    #[test]
    fn every_format_a_vendor_speaks_is_known() {
        let known = formats();
        for vendor in ALL {
            for (format, _) in vendor.formats {
                let name = format.name();
                let found = known.iter().find(|k| k.name == name);
                let found = found.unwrap_or_else(|| panic!("{}: {name} is unknown", vendor.id));
                let session = matches!(
                    format.reach(),
                    respan_gateway_vendors::Reach::Session { .. }
                );
                assert_eq!(found.websocket, session, "{name}");
            }
        }
    }
}
