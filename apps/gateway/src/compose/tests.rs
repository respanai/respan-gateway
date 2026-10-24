use mockllm::{Answer, FakeAnthropic, FakeOpenAi};
use respan_gateway_http::Body;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;

use super::*;
use crate::App;

/// Hand `req` to `gateway` as an HTTP server does: whole.
async fn serve(
    gateway: &Gateway,
    req: http::Request<Body>,
) -> (
    http::Response<Body>,
    futures::future::BoxFuture<'static, Result<(), Fault>>,
) {
    respan_gateway_http::handle(gateway, req, 32 << 20).await
}

fn post(path: &str, body: impl Into<Bytes>) -> http::Request<Body> {
    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = path.parse().unwrap_or_else(|e| panic!("path {path}: {e}"));
    req
}

async fn read_whole(
    resp: http::Response<Body>,
) -> (StatusCode, http::HeaderMap, Result<Bytes, Fault>) {
    let (parts, body) = resp.into_parts();
    let body = BodyExt::collect(body).await.map(|b| b.to_bytes());
    (parts.status, parts.headers, body)
}

/// The gateway of `text`, its plugins included.
fn resolve(text: &str) -> Result<Gateway, String> {
    Config::parse(text)
        .and_then(|mut config| {
            // A provider sees only the calls a test makes.
            config.plugins.routing.listings.get_or_insert(false);
            App::new(config)
        })
        .map(|app| app.gateway().clone())
        .map_err(|e| e.to_string())
}

/// Build the actual HTTP configuration against two strict local providers.
async fn local(text: &str) -> (Gateway, FakeOpenAi, FakeOpenAi) {
    let a = FakeOpenAi::start().await;
    let b = FakeOpenAi::start().await;
    a.answer(Answer::text("a"));
    b.answer(Answer::text("b"));
    let text = text
        .replace("https://a.test/v1", &a.base_url())
        .replace("https://b.test/v1", &b.base_url());
    let gateway = resolve(&text).unwrap_or_else(|e| panic!("gateway: {e}"));
    (gateway, a, b)
}

const TWO: &str = "providers = { a = { formats = \"openai-chat\", base_url = \"https://a.test/v1\", auth = { type = \"bearer\", key = \"sk-test\" } }, b = { formats = \"openai-chat\", base_url = \"https://b.test/v1\", auth = { type = \"bearer\", key = \"sk-test\" } } }\nformats = [\"openai-chat\"]\n";

async fn request(gateway: &Gateway, model: &str) {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (response, _) = serve(gateway, post("/v1/chat/completions", body.to_string())).await;
    let (status, _, body) = read_whole(response).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(body.is_ok(), "{body:?}");
    gateway.idle().await;
}

/// The model each request to `mock` asked for.
fn models_asked(mock: &FakeOpenAi) -> Vec<String> {
    let model = |r: &mockllm::Request| r.object()?.get("model")?.as_str().map(str::to_owned);
    mock.requests().iter().filter_map(model).collect()
}

#[tokio::test]
async fn the_first_listed_glob_a_model_matches_wins() {
    let text =
        format!("{TWO}[routes.\"claude*\"]\nto = \"a\"\n[routes.\"claude-3*\"]\nto = \"b\"\n");
    let (gateway, a, b) = local(&text).await;
    request(&gateway, "claude-3-x").await;
    assert_eq!(a.requests().len(), 1);
    assert_eq!(a.requests()[0].path, "/v1/chat/completions");
    assert!(b.requests().is_empty());
}

#[tokio::test]
async fn the_default_target_answers_what_no_router_targets() {
    let text = format!(
        "{TWO}plugins = {{ routing = {{ default = \"b\" }} }}\n[routes.\"claude*\"]\nto = \"a\"\n"
    );
    let (gateway, a, b) = local(&text).await;
    request(&gateway, "gpt-x").await;
    assert!(a.requests().is_empty());
    assert_eq!(b.requests().len(), 1);
    assert_eq!(b.requests()[0].path, "/v1/chat/completions");
}

#[tokio::test]
async fn without_a_target_the_first_provider_of_the_kind_answers() {
    let (gateway, a, b) = local(TWO).await;
    request(&gateway, "llama").await;
    assert_eq!(a.requests().len(), 1);
    assert!(b.requests().is_empty());
}

#[tokio::test]
async fn a_provider_prefix_picks_the_provider_and_is_not_sent() {
    let (gateway, a, b) = local(TWO).await;
    request(&gateway, "b/gpt-4.1-mini").await;
    assert!(a.requests().is_empty());
    assert_eq!(models_asked(&b), ["gpt-4.1-mini"]);
}

#[tokio::test]
async fn a_vendor_routes_its_models_with_its_key() {
    let openai = FakeOpenAi::start().await;
    let anthropic = FakeAnthropic::start().await;
    openai.answer(Answer::text("hi"));
    anthropic.answer(Answer::text("hi"));
    let text = format!(
        "providers = {{ openai = {{ vendor = \"openai\", base_url = \"{}\", key = \"sk-openai\" }}, anthropic = {{ vendor = \"anthropic\", base_url = \"{}\", key = \"sk-ant\" }} }}\n",
        openai.base_url(),
        anthropic.base_url()
    );
    let gateway = resolve(&text).unwrap_or_else(|e| panic!("gateway: {e}"));
    request(&gateway, "anthropic/claude-sonnet-4-5").await;
    request(&gateway, "openai/gpt-4.1-mini").await;
    assert_eq!(anthropic.requests().len(), 1);
    assert_eq!(anthropic.requests()[0].header("x-api-key"), Some("sk-ant"));
    assert_eq!(openai.requests().len(), 1);
    let auth = openai.requests()[0]
        .header("authorization")
        .map(str::to_owned);
    assert_eq!(auth.as_deref(), Some("Bearer sk-openai"));
}

#[tokio::test]
async fn the_example_config_routes_by_model_both_ways() {
    let openai = FakeOpenAi::start().await;
    let anthropic = FakeAnthropic::start().await;
    openai.answer(Answer::text("hi"));
    anthropic.answer(Answer::text("hi"));
    // Its vendors, at the fakes.
    let mut config: toml::Value =
        toml::from_str(include_str!("../../../../gateway.example.toml")).expect("example");
    for (vendor, base_url) in [
        ("openai", openai.base_url()),
        ("anthropic", anthropic.base_url()),
    ] {
        config["providers"]
            .as_table_mut()
            .expect("providers")
            .insert(
                vendor.into(),
                toml::Value::Table(toml::map::Map::from_iter([
                    ("vendor".into(), toml::Value::String(vendor.into())),
                    ("base_url".into(), toml::Value::String(base_url)),
                    ("key".into(), toml::Value::String("sk".into())),
                ])),
            );
    }
    // Its tool servers and tool plugins, which this does not exercise.
    let table = config.as_table_mut().expect("example");
    table.remove("tools");
    table.remove("plugins");
    let text = toml::to_string(&config).expect("example");
    let gateway = resolve(&text).unwrap_or_else(|e| panic!("gateway: {e}"));

    let openai_client =
        r#"{"model":"anthropic/claude-x","messages":[{"role":"user","content":"hi"}]}"#;
    let anthropic_client =
        r#"{"model":"openai/gpt-x","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;
    let to_claude = serve(&gateway, post("/v1/chat/completions", openai_client));
    let (_, _, to_claude) = read_whole(to_claude.await.0).await;
    let to_gpt = serve(&gateway, post("/v1/messages", anthropic_client));
    let (_, _, to_gpt) = read_whole(to_gpt.await.0).await;

    gateway.idle().await;
    assert_eq!(anthropic.requests().len(), 1);
    assert_eq!(anthropic.requests()[0].path, "/v1/messages");
    assert_eq!(openai.requests().len(), 1);
    assert_eq!(openai.requests()[0].path, "/v1/chat/completions");
    let json = |b: Result<respan_gateway_http::bytes::Bytes, Fault>| {
        b.ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let to_claude = json(to_claude);
    let to_gpt = json(to_gpt);
    assert_eq!(to_claude["object"], "chat.completion", "{to_claude}");
    assert_eq!(to_gpt["type"], "message", "{to_gpt}");
}

/// The plan of `text`.
fn plan(text: &str) -> Result<Plan, String> {
    let config = Config::parse(text).map_err(|e| e.to_string())?;
    let formats = crate::formats::formats();
    let composed = compose(
        &config,
        &formats,
        1 << 20,
        Default::default(),
        &Default::default(),
    );
    composed
        .map(|(_, plan)| plan)
        .map_err(|e| e.message().to_owned())
}

#[test]
fn a_vendor_is_every_format_it_speaks() {
    let plan = plan(
        "providers = { openai = { vendor = \"openai\", key = \"sk\" }, gemini = { vendor = \"gemini\", key = \"AIza\", formats = { gemini-generate = \"http://g.test/v1beta\", openai-chat = \"http://g.test/v1beta/openai\", gemini-live = \"http://g.test\", openai-images = \"http://g.test/v1beta/openai\", gemini-veo = \"http://g.test/v1beta\", openai-embeddings = \"http://g.test/v1beta/openai\" } } }\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let entries = |i: usize| -> Vec<(String, String, String)> {
        let entries = plan.upstreams[i].entries.iter();
        let entry = |e: &Entry| (e.format.clone(), e.internal.clone(), e.base_url.clone());
        entries.map(entry).collect()
    };
    let entry = |f: &str, i: &str, b: &str| (f.to_owned(), i.to_owned(), b.to_owned());
    assert_eq!(
        entries(0),
        [
            entry("openai-chat", "openai", "https://api.openai.com/v1"),
            entry(
                "openai-responses",
                "openai:openai-responses",
                "https://api.openai.com/v1"
            ),
            entry(
                "openai-realtime",
                "openai:openai-realtime",
                "wss://api.openai.com/v1"
            ),
            entry(
                "responses-ws",
                "openai:responses-ws",
                "wss://api.openai.com/v1"
            ),
            entry(
                "openai-images",
                "openai:openai-images",
                "https://api.openai.com/v1"
            ),
            entry(
                "openai-transcriptions",
                "openai:openai-transcriptions",
                "https://api.openai.com/v1"
            ),
            entry(
                "openai-speech",
                "openai:openai-speech",
                "https://api.openai.com/v1"
            ),
            entry(
                "openai-embeddings",
                "openai:openai-embeddings",
                "https://api.openai.com/v1"
            ),
        ]
    );
    assert_eq!(
        entries(1),
        [
            entry("gemini-generate", "gemini", "http://g.test/v1beta"),
            entry(
                "openai-chat",
                "gemini:openai-chat",
                "http://g.test/v1beta/openai"
            ),
            entry("gemini-live", "gemini:gemini-live", "ws://g.test"),
            entry(
                "openai-images",
                "gemini:openai-images",
                "http://g.test/v1beta/openai"
            ),
            entry("gemini-veo", "gemini:gemini-veo", "http://g.test/v1beta"),
            entry(
                "openai-embeddings",
                "gemini:openai-embeddings",
                "http://g.test/v1beta/openai"
            ),
        ]
    );
    let header = |i: usize, e: usize, name: &str| {
        let parts = plan.upstreams[i].provider.parts().unwrap_or_default();
        let auth = parts.get(e).map(|p| p.headers.clone()).unwrap_or_default();
        auth.get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(header(1, 0, "x-goog-api-key").as_deref(), Some("AIza"));
    assert_eq!(
        header(1, 1, "authorization").as_deref(),
        Some("Bearer AIza")
    );
    assert_eq!(header(1, 4, "x-goog-api-key").as_deref(), Some("AIza"));
    assert!(plan.upstreams.iter().all(|u| u.own_key));
    // Served by default: the chat formats, each chat, media and realtime client
    // format, and Responses over WebSocket.
    assert_eq!(
        plan.serve,
        [
            "openai-chat",
            "anthropic-messages",
            "openai-responses",
            "openai-realtime",
            "openai-images",
            "openai-transcriptions",
            "openai-speech",
            "openai-videos",
            "openai-embeddings",
            "gemini-generate",
            "gemini-images",
            "gemini-live",
            "mistral-realtime",
            "gemini-veo",
            "responses-ws"
        ]
    );
}

#[test]
fn a_list_of_formats_shares_one_base_url_and_key() {
    let plan = plan(
        "providers = { both = { formats = [\"anthropic-messages\", \"passthrough\"], base_url = \"https://x.test/\", key = \"k\" } }\nformats = [\"anthropic-messages\", \"passthrough\"]\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let both = &plan.upstreams[0];
    let internal: Vec<&str> = both.entries.iter().map(|e| e.internal.as_str()).collect();
    assert_eq!(internal, ["both", "both:passthrough"]);
    let parts = both.provider.parts().unwrap_or_default();
    let x_api_key = parts[0].headers.get("x-api-key");
    assert_eq!(x_api_key.and_then(|v| v.to_str().ok()), Some("k"));
    assert!(parts[1].headers.contains_key("authorization"));
    assert_eq!(both.entries[1].base_url, "https://x.test");
}

/// Vendors share `openai-chat`: each takes its key as its own vendor says,
/// and a provider of no vendor as OpenAI's does.
#[test]
fn a_key_goes_where_its_own_vendor_takes_it() {
    let keys = plan(
        "providers = { az = { vendor = \"azure\", base_url = \"https://r.test/openai/v1\", key = \"a\" }, oa = { vendor = \"openai\", key = \"o\" }, plain = { formats = \"openai-chat\", base_url = \"https://p.test/v1\", key = \"p\" } }\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let auth = |i: usize, name: &str| {
        let parts = keys.upstreams[i].provider.parts().unwrap_or_default();
        let auth = parts.first().map(|p| p.headers.clone()).unwrap_or_default();
        auth.get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(auth(0, "api-key").as_deref(), Some("a"));
    assert_eq!(auth(0, "authorization"), None);
    assert_eq!(auth(1, "authorization").as_deref(), Some("Bearer o"));
    assert_eq!(auth(2, "authorization").as_deref(), Some("Bearer p"));
    // A vendor's dialect is no format of its own.
    let dialect = plan(
        "providers = { g = { formats = \"groq-chat\", base_url = \"https://g.test\", key = \"k\" } }\n",
    );
    assert!(dialect.is_err_and(|e: String| e.contains("unknown format 'groq-chat'")));
}

#[test]
fn a_bad_provider_is_an_error() {
    let cases = [
        ("p = {  }", "provider 'p': needs a `vendor` or `formats`"),
        (
            "p = { formats = \"openai-chat\" }",
            "provider 'p': format openai-chat needs a `base_url`",
        ),
        (
            "p = { formats = { openai-chat = \"http://x\" }, base_url = \"http://y\" }",
            "provider 'p': give each format its base URL in `formats`, or one `base_url`, not both",
        ),
        (
            "p = { vendor = \"gemini\", key = \"k\", formats = [\"gemini-generate\", \"openai-chat\"], base_url = \"http://g.test\" }",
            "provider 'p': its formats are at different URLs, so one `base_url` cannot say where each is; give each its own:\nformats = {\n  \"gemini-generate\" = \"https://generativelanguage.googleapis.com/v1beta\",\n  \"openai-chat\" = \"https://generativelanguage.googleapis.com/v1beta/openai\",\n}",
        ),
        (
            "p = { vendor = \"openai\", key = \"k\", auth = { type = \"bearer\", key = \"k\" } }",
            "provider 'p': set `key` or `auth`, not both",
        ),
        (
            "p = { vendor = \"groq\", key = \"env:RESPAN_TEST_UNSET\" }",
            "provider 'p': environment variable RESPAN_TEST_UNSET is not set",
        ),
        (
            "p = { formats = [\"openai-chat\", \"openai-chat\"], base_url = \"http://x\" }",
            "provider 'p': lists a format twice",
        ),
        (
            "p = { vendor = \"cloudflare\", key = \"k\" }",
            "provider 'p': environment variable CLOUDFLARE_ACCOUNT_ID is not set; the base URL of openai-chat needs it, or give it one",
        ),
    ];
    for (provider, expected) in cases {
        let text = format!("providers = {{ {provider} }}");
        assert_eq!(plan(&text).err().as_deref(), Some(expected), "{text}");
    }
    let unknown = plan("providers = { p = { vendor = \"nope\" } }").err();
    let said = "provider 'p': unknown vendor 'nope'; vendors: [";
    assert!(unknown.is_some_and(|e| e.starts_with(said)));
}

#[test]
fn client_keys_are_literal_or_from_the_environment_and_named() {
    let unset = plan("client_keys = [\"literal\", \"env:RESPAN_TEST_UNSET\"]\n");
    let said = "client_keys: environment variable RESPAN_TEST_UNSET is not set";
    assert_eq!(unset.err().as_deref(), Some(said));
    let one = "providers = { p = { formats = \"openai-chat\", base_url = \"http://x.test\" } }\n";
    let set =
        plan(&format!("{one}client_keys = [\"one\", \"two\"]\n")).unwrap_or_else(|e| panic!("{e}"));
    let names = |p: &Plan| {
        p.keys
            .iter()
            .map(|c| (c.name.clone(), c.key.clone()))
            .collect::<Vec<_>>()
    };
    let pair = |n: &str, k: &str| (n.to_owned(), k.to_owned());
    assert_eq!(names(&set), [pair("key-1", "one"), pair("key-2", "two")]);
    let named = "client_keys = [\"plain\", { name = \"app\", key = \"secret\" }, { name = \"ci\", key = \"env:RESPAN_TEST_UNSET\" }]\n";
    let said = "client_keys: environment variable RESPAN_TEST_UNSET is not set";
    assert_eq!(
        plan(&format!("{one}\n{named}")).err().as_deref(),
        Some(said)
    );
    let named = "client_keys = [\"plain\", { name = \"app\", key = \"secret\" }]\n";
    let set = plan(&format!("{one}\n{named}")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(names(&set), [pair("key-1", "plain"), pair("app", "secret")]);
    for (keys, said) in [
        (
            "[{ name = \"a\", key = \"x\" }, { name = \"a\", key = \"y\" }]",
            "client_keys: the name 'a' is used twice",
        ),
        (
            "[{ name = \" \", key = \"x\" }]",
            "client_keys: a name is empty",
        ),
    ] {
        let got = plan(&format!("{one}\nclient_keys = {keys}")).err();
        assert_eq!(got.as_deref(), Some(said), "{keys}");
    }
}

#[test]
fn two_providers_of_a_session_format_no_other_translates_are_an_error() {
    let text = "providers = { openai = { vendor = \"openai\", key = \"k\" }, responses = { formats = \"responses-ws\", base_url = \"wss://x.test/v1\" } }\n";
    assert_eq!(
        plan(text).err().as_deref(),
        Some(
            "formats: responses-ws needs exactly one provider of that format; openai, responses all speak it"
        )
    );
    // A realtime session goes to any provider of the kind.
    let text = "providers = { openai = { vendor = \"openai\", key = \"k\" }, realtime = { formats = \"openai-realtime\", base_url = \"wss://x.test/v1\" } }\nformats = [\"openai-realtime\", \"gemini-live\"]\n";
    assert!(plan(text).is_ok());
}

#[test]
fn bearer_auth_is_a_sensitive_header() {
    let auth = Auth::Bearer { key: "sk".into() };
    let headers = auth_headers("p", &auth).unwrap_or_default();
    let value = headers.get(AUTHORIZATION);
    assert_eq!(value.and_then(|v| v.to_str().ok()), Some("Bearer sk"));
    assert!(value.is_some_and(HeaderValue::is_sensitive));
}

#[test]
fn a_missing_environment_variable_is_an_error() {
    let text = "providers = { p = { formats = \"openai-chat\", base_url = \"http://x\", auth = { type = \"bearer\", key = \"env:RESPAN_TEST_UNSET\" } } }\nformats = []\n";
    assert_eq!(
        resolve(text).err().as_deref(),
        Some("provider 'p': environment variable RESPAN_TEST_UNSET is not set")
    );
}

#[test]
fn a_bad_base_url_or_auth_header_is_an_error() {
    let provider = |p: &str| {
        format!("providers = {{ p = {{ formats = \"openai-chat\", {p} }} }}\nformats = []")
    };
    let url = provider("base_url = \"https://k:s@x.test\"");
    let said = resolve(&url).err().unwrap_or_default();
    assert!(said.starts_with("provider 'p': bad base URL"), "{said}");
    assert!(!said.contains("k:s"), "{said}");
    let host = provider(
        "base_url = \"https://x.test\", auth = { type = \"header\", name = \"host\", key = \"k\" }",
    );
    assert_eq!(
        resolve(&host).err().as_deref(),
        Some("provider 'p': header 'host' is the connection's")
    );
}

#[test]
fn a_bad_plugin_or_default_is_an_error() {
    let router = "default = \"a\"";
    let cases = [
        (
            "[routes.c]\nto = \"nope\"\n".to_owned(),
            "routes: c: to: unknown provider 'nope'; providers: [a, b]",
        ),
        (
            "plugins = { routing = { default = \"nope\" } }\n".to_owned(),
            "plugins: routing: default: unknown provider 'nope'; providers: [a, b]",
        ),
    ];
    for (text, expected) in cases {
        let text = format!("{TWO}{text}");
        assert_eq!(resolve(&text).err().as_deref(), Some(expected), "{text}");
    }
    let named = format!("{TWO}plugins = {{ routing = {{ name = \"x\", {router} }} }}");
    let said = resolve(&named).err().unwrap_or_default();
    assert!(said.contains("unknown field `name`"), "{said}");
}

#[test]
fn a_session_provider_is_websocket_and_its_clients_go_to_it_alone() {
    let realtime =
        "realtime = { formats = \"openai-realtime\", base_url = \"wss://api.openai.com/v1\" }";
    // Its base, given as the vendor's SDKs take it, in its scheme.
    let lone = "providers = { p = { formats = \"gemini-live\", base_url = \"https://x.test\" } }\nformats = []\n";
    assert!(resolve(lone).is_ok(), "{:?}", resolve(lone).err());
    let cases = [
        (
            "providers = { p = { formats = \"openai-chat\", base_url = \"http://x.test\" } }\nformats = [\"responses-ws\"]\n"
                .to_owned(),
            "formats: responses-ws needs exactly one provider of that format",
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(resolve(&text).err().as_deref(), Some(expected), "{text}");
    }
    let both = format!("providers = {{ {realtime} }}\nformats = [\"openai-realtime\"]");
    assert!(resolve(&both).is_ok());
    // A route applies to every kind its provider serves.
    let routed = format!("{both}\n[routes.\"gpt*\"]\nto = \"realtime\"\n");
    assert!(resolve(&routed).is_ok());
}

#[tokio::test]
async fn a_route_applies_to_every_kind_its_provider_serves() {
    let text = "providers = { chat = { formats = \"openai-chat\", base_url = \"https://a.test/v1\", key = \"k\" }, asr = { formats = \"openai-transcriptions\", base_url = \"https://b.test/v1\", key = \"k\" } }\nformats = [\"openai-chat\", \"openai-transcriptions\"]\n[routes.\"voice*\"]\nto = \"asr\"\n[routes.\"voice-chat*\"]\nto = \"chat\"\n";
    let plan = plan(text).unwrap_or_else(|e| panic!("{e}"));
    let route = |kind, format, model| {
        plan.routes
            .route(kind, format, Some(model))
            .map(|r| r.provider)
    };
    // The glob reaches the provider of each kind that serves it; a kind
    // the provider does not serve falls through to the next route.
    let transcription = route(kind::TRANSCRIPTION, "openai-transcriptions", "voice-1");
    assert_eq!(transcription.as_deref(), Some("asr"));
    let chat = route(kind::CHAT, "openai-chat", "voice-1");
    assert_eq!(chat.as_deref(), Some("chat"));
}

#[tokio::test]
async fn responses_are_served_by_default_natively_where_a_provider_speaks_them() {
    let text = "providers = { a = { formats = [\"openai-chat\", \"openai-responses\"], base_url = \"https://a.test/v1\", auth = { type = \"bearer\", key = \"sk-test\" } }, b = { formats = \"openai-chat\", base_url = \"https://b.test/v1\", auth = { type = \"bearer\", key = \"sk-test\" } } }\n[routes.\"gpt*\"]\nto = \"a\"\n[routes.\"llama*\"]\nto = \"b\"\n";
    let (gateway, a, b) = local(text).await;
    let send = |path: &'static str, body: serde_json::Value| {
        let gateway = gateway.clone();
        async move {
            let (response, _) = serve(&gateway, post(path, body.to_string())).await;
            let (status, _, body) = read_whole(response).await;
            gateway.idle().await;
            (status, body.unwrap_or_default())
        }
    };
    // A provider that speaks Responses gets the client's own format.
    let responses = |model: &str| serde_json::json!({ "model": model, "input": "hi" });
    let (status, body) = send("/v1/responses", responses("gpt-4o-mini")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(a.requests()[0].path, "/v1/responses");
    assert!(String::from_utf8_lossy(&body).contains("\"object\":\"response\""));
    // A chat client of the same provider keeps its own.
    let chat = serde_json::json!({ "model": "gpt-4o-mini", "messages": [{ "role": "user", "content": "hi" }] });
    assert_eq!(send("/v1/chat/completions", chat).await.0, StatusCode::OK);
    assert_eq!(a.requests()[1].path, "/v1/chat/completions");
    // One that speaks only chat is translated to.
    let (status, body) = send("/v1/responses", responses("llama-3")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(b.requests()[0].path, "/v1/chat/completions");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    assert_eq!(v["object"], "response");
    assert_eq!(v["output"][0]["content"][0]["text"], "b");
}

#[test]
fn a_format_served_after_passthrough_is_refused() {
    let text = "formats = [\"passthrough\", \"openai-chat\"]\n";
    assert_eq!(
        resolve(text).err().as_deref(),
        Some(
            "formats: 'openai-chat' comes after 'passthrough', which accepts every request first; serve 'passthrough' last"
        )
    );
}

#[test]
fn an_unknown_format_names_the_compiled_in_ones() {
    let text = "formats = [\"openai-chat\", \"nope\"]\n";
    let said = "formats: unknown format 'nope'; compiled-in formats: [";
    assert!(resolve(text).err().is_some_and(|e| e.starts_with(said)));
}

#[test]
fn a_gemini_call_for_an_image_another_provider_draws_is_an_images() {
    let text = "providers = { o = { vendor = \"openai\", key = \"k\" }, g = { vendor = \"gemini\", key = \"k\" } }\n";
    let plan = plan(text).unwrap_or_else(|e| panic!("{e}"));
    assert!(plan.serves("gemini-images"));
    let drawn = |model| crate::formats::drawn_elsewhere(&plan.routes, model);
    assert!(drawn("gpt-image-2"));
    // Gemini's own image model is a chat of Gemini's, as it was.
    assert!(!drawn("g/gemini-3.1-flash-image"));
    assert!(!drawn("gemini-2.5-flash"));
    assert!(!drawn("o/gpt-4o"));
}

#[test]
fn veo_is_served_to_its_clients() {
    let text =
        "providers = { g = { vendor = \"gemini\", key = \"k\" } }\nformats = [\"gemini-veo\"]\n";
    let served = plan(text).map(|p| p.serve);
    assert_eq!(served, Ok(vec!["gemini-veo".to_owned()]));
}

#[test]
fn an_empty_key_is_refused_and_keys_are_trimmed() {
    let one = "providers = { p = { formats = \"openai-chat\", base_url = \"http://x.test\" } }\n";
    for keys in ["['']", "['  ']"] {
        assert_eq!(
            plan(&format!("{one}client_keys = {keys}")).err().as_deref(),
            Some("client_keys: a key is empty"),
            "{keys}"
        );
    }
    let trimmed = plan(&format!("{one}client_keys = [\" gk \"]\n"))
        .map(|p| p.keys.into_iter().map(|c| c.key).collect::<Vec<_>>());
    assert_eq!(trimmed, Ok(vec!["gk".to_owned()]));
    let empty_provider_key = "providers = { p = { formats = \"openai-chat\", base_url = \"http://x.test\", key = \"\" } }\n";
    assert_eq!(
        plan(empty_provider_key).err().as_deref(),
        Some("provider 'p': a key is empty")
    );
    let keyless =
        "providers = { p = { formats = \"openai-chat\", base_url = \"http://x.test\" } }\n";
    assert!(plan(keyless).is_ok_and(|p| !p.upstreams[0].own_key));
    let empty_auth = "providers = { p = { formats = \"openai-chat\", base_url = \"http://x.test\", auth = { type = \"bearer\", key = \" \" } } }\n";
    assert_eq!(
        plan(empty_auth).err().as_deref(),
        Some("provider 'p': a key is empty")
    );
}

#[test]
fn a_config_without_providers_is_refused() {
    assert_eq!(
        plan("providers = {  }\n").err().as_deref(),
        Some(
            "providers: none configured; add one, such as `[providers.openai]` with `vendor = 'openai'`"
        )
    );
}

#[test]
fn a_provider_name_is_letters_digits_and_a_little_punctuation() {
    for name in ["'a/b'", "'a~b'", "'a:b'", "'a b'", "''"] {
        let text = format!(
            "providers = {{ {name} = {{ formats = \"openai-chat\", base_url = \"http://x.test\" }} }}\n"
        );
        let said = plan(&text).err().unwrap_or_default();
        assert!(said.ends_with("(no '/', '~' or ':')"), "{name}: {said}");
    }
    let fine = "providers = { \"my-gw_2.0\" = { formats = \"openai-chat\", base_url = \"http://x.test\" } }\n";
    assert!(plan(fine).is_ok());
}

#[test]
fn a_listed_model_name_with_a_slash_goes_whole_to_its_lister() {
    let plan = plan(
        "providers = { openai = { vendor = \"openai\", key = \"sk\" }, deepseek = { vendor = \"deepseek\", key = \"sk\" }, together = { vendor = \"together\", key = \"sk\" } }\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let listed = ["deepseek-ai/DeepSeek-V3", "openai/gpt-oss-120b"];
    (plan.routes.catalog()).set("together", listed.map(respan_gateway_sdk::Model::new));
    let to = |model: &str| {
        let route = plan.routes.route(kind::CHAT, "openai-chat", Some(model));
        route.map(|r| (r.provider, r.model.unwrap_or_default()))
    };
    let sent = |provider: &str, model: &str| Some((provider.to_owned(), model.to_owned()));
    let whole = "deepseek-ai/DeepSeek-V3";
    assert_eq!(to(whole), sent("together", whole), "not deepseek");
    // A configured provider's name comes first, and is taken off.
    assert_eq!(to("openai/gpt-oss-120b"), sent("openai", "gpt-oss-120b"));
    assert_eq!(
        to("together/openai/gpt-oss-120b"),
        sent("together", "openai/gpt-oss-120b")
    );
    // Unlisted and naming no provider: the default, sent the whole name.
    let unlisted = "meta-llama/Llama-3.3-70B";
    assert_eq!(to(unlisted), sent("openai", unlisted));
}

/// Two providers `a` and `b`, and `route` for `gpt-x`; `a` answers
/// `first`, `b` answers `b`.
async fn routed(route: &str, first: Answer) -> (Gateway, FakeOpenAi, FakeOpenAi) {
    let a = FakeOpenAi::start().await;
    let b = FakeOpenAi::start().await;
    a.answer(first);
    b.answer(Answer::text("b"));
    let text = format!(
        "{}[routes.\"gpt-x\"]\n{route}\n",
        TWO.replace("https://a.test/v1", &a.base_url())
            .replace("https://b.test/v1", &b.base_url())
    );
    let gateway = resolve(&text).unwrap_or_else(|e| panic!("gateway: {e}"));
    (gateway, a, b)
}

#[tokio::test]
async fn a_route_falls_back_to_its_next_candidate() {
    let refused = Answer::error(503).message("overloaded");
    let (gateway, a, b) = routed("to = [\"a\", \"b/gpt-y\"]", refused).await;
    request(&gateway, "gpt-x").await;
    assert_eq!(models_asked(&a), ["gpt-x"]);
    assert_eq!(models_asked(&b), ["gpt-y"], "the candidate's own model");
}

#[tokio::test]
async fn a_route_without_fallback_retries_its_first_alone() {
    let route = "to = [\"a\", \"b\"]\nfallback = false\ntries = 2\nbackoff = \"1ms\"";
    // `a` refuses once, then answers.
    let (gateway, a, b) = routed(route, Answer::error(503)).await;
    request(&gateway, "gpt-x").await;
    assert_eq!((a.requests().len(), b.requests().len()), (2, 0));
}

#[tokio::test]
async fn a_hedged_route_takes_whichever_candidate_answers() {
    let route = "to = [\"a\", \"b\"]\nhedge = \"0s\"";
    let (gateway, _, b) = routed(route, Answer::error(503)).await;
    request(&gateway, "gpt-x").await;
    assert_eq!(b.requests().len(), 1);
}

#[test]
fn bad_routes_and_retired_settings_are_errors() {
    let cases = [
        (
            "to = [\"b\", \"b\"]",
            "routes: gpt-x: to: 'b' is listed twice",
        ),
        ("to = []", "routes: gpt-x: to: names no provider"),
        (
            "to = \"a/\"",
            "routes: gpt-x: to: 'a/' names no model after the '/'",
        ),
        ("to = \"a\"\ntries = 0", "routes: gpt-x: tries: 0; 1 to 10"),
        (
            "to = \"a\"\nbackoff = \"5s\"\nmax_backoff = \"1s\"",
            "routes: gpt-x: backoff: 5s is longer than max_backoff: 1s",
        ),
        (
            "to = \"a\"\nwhen = { clients = [\"ops\"] }",
            "routes: gpt-x: when: clients: unknown client key 'ops'; named keys: []",
        ),
        (
            "to = \"a\"\nwhen = { kinds = [\"chatt\"] }",
            "routes: gpt-x: when: kinds: unknown kind 'chatt'; kinds: [",
        ),
    ];
    for (route, expected) in cases {
        let text = format!("{TWO}[routes.\"gpt-x\"]\n{route}\n");
        let said = resolve(&text).err().unwrap_or_default();
        assert!(said.starts_with(expected), "{text}\n{said}");
    }
    for (plugins, now) in [
        ("routing = { models = [] }", "the top-level `routes`"),
        ("fallback = { after = { a = [\"b\"] } }", "a route's own"),
        ("hedge = { after = { a = [\"b\"] } }", "`hedge = \"200ms\"`"),
    ] {
        let text = format!("{TWO}plugins = {{ {plugins} }}\n");
        let said = resolve(&text).err().unwrap_or_default();
        assert!(said.contains(now), "{text}\n{said}");
    }
    let text = format!("{TWO}[mcp.files]\ncommand = \"npx\"\n");
    let said = resolve(&text).err().unwrap_or_default();
    assert!(said.contains("`[tools.github]`"), "{text}\n{said}");
}

#[tokio::test]
async fn a_vendor_sends_a_chat_to_a_model_its_chat_format_refuses_in_another() {
    let text = "providers = { a = { vendor = \"openai\", base_url = \"https://a.test/v1\", key = \"k\" } }\nformats = [\"openai-chat\"]\n";
    let (gateway, a, _) = local(text).await;
    // gpt-5.4 and later, and a model only Responses serves, whatever the
    // chat carries.
    for (model, path) in [
        ("gpt-5-mini", "/v1/chat/completions"),
        ("gpt-6-luna", "/v1/responses"),
        ("o3-pro", "/v1/responses"),
    ] {
        a.answer(Answer::text("a"));
        let body = serde_json::json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] });
        let (response, _) = serve(&gateway, post("/v1/chat/completions", body.to_string())).await;
        let (status, _, body) = read_whole(response).await;
        gateway.idle().await;
        assert_eq!(status, StatusCode::OK, "{model}: {body:?}");
        let v: serde_json::Value =
            serde_json::from_slice(&body.unwrap_or_default()).unwrap_or_default();
        assert_eq!(
            v["object"], "chat.completion",
            "{model}: answered in the client's format"
        );
        assert_eq!(
            a.requests().last().map(|r| r.path.clone()).as_deref(),
            Some(path),
            "{model}"
        );
    }
}

/// OpenRouter's media, each at its own path: an image at `/images`, a
/// video at `/videos` and its follow-ups by its `gen-vid-` id, beside
/// another provider of videos; decisions at `/systemone`.
#[tokio::test]
async fn openrouter_is_called_at_each_media_kinds_own_path() {
    let text = "providers = { router = { vendor = \"openrouter\", base_url = \"https://a.test/v1\", key = \"k\" }, other = { formats = \"openai-videos\", base_url = \"https://b.test/v1\", key = \"k\" } }\n[routes.\"x-ai/*\"]\nto = \"router\"\n";
    let (gateway, router, other) = local(text).await;
    let call = |method: http::Method, path: &str, body: serde_json::Value| {
        let mut req = post(path, body.to_string());
        *req.method_mut() = method;
        let gateway = gateway.clone();
        async move {
            let (response, _) = serve(&gateway, req).await;
            _ = read_whole(response).await;
            gateway.idle().await;
        }
    };
    let id = "gen-vid-1792778188-abcdefghij0123456789";
    let image = serde_json::json!({ "model": "router/black-forest-labs/flux", "prompt": "a fox" });
    call(http::Method::POST, "/v1/images/generations", image).await;
    let video = serde_json::json!({ "model": "x-ai/grok-imagine-video", "prompt": "a fox", "seconds": "4" });
    call(http::Method::POST, "/v1/videos", video).await;
    call(
        http::Method::GET,
        &format!("/v1/videos/{id}"),
        serde_json::Value::Null,
    )
    .await;
    let content = format!("/v1/videos/{id}/content");
    call(http::Method::GET, &content, serde_json::Value::Null).await;
    call(
        http::Method::GET,
        "/v1/videos/video_1",
        serde_json::Value::Null,
    )
    .await;
    let question = serde_json::json!({ "type": "noul", "instructions": "Is it?", "criteria": { "true": "yes", "false": "no" } });
    let decision = serde_json::json!({ "model": "router/typesafe/jev-1.14", "state": "a fox", "questions": { "q": question } });
    call(http::Method::POST, "/v1/decisions", decision).await;
    let called = |mock: &FakeOpenAi| -> Vec<(String, String)> {
        let at = |r: &mockllm::Request| match r.query.as_str() {
            "" => r.path.clone(),
            query => format!("{}?{query}", r.path),
        };
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), at(r)))
            .collect()
    };
    let to = |method: &str, path: String| (method.to_owned(), path);
    assert_eq!(
        called(&router),
        [
            to("POST", "/v1/images".into()),
            to("POST", "/v1/videos".into()),
            to("GET", format!("/v1/videos/{id}")),
            to("GET", format!("/v1/videos/{id}/content?index=0")),
            to("POST", "/v1/systemone".into()),
        ]
    );
    assert_eq!(called(&other), [to("GET", "/v1/videos/video_1".into())]);
    let video = router.requests()[1].body.clone();
    assert_eq!(
        (&video["model"], &video["duration"]),
        (
            &serde_json::json!("x-ai/grok-imagine-video"),
            &serde_json::json!(4)
        )
    );
}
