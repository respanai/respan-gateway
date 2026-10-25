//! The configuration: where it comes from (a TOML file, the environment or
//! `proxy`) and what it says. [`App::new`](crate::App::new) checks and
//! composes it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
pub use respan_gateway_plugin_routing::Pick;
use serde::{Deserialize, Deserializer};

use crate::Error;
use crate::logging::LogFormat;
use crate::mcp::McpConfig;
use crate::network::{NetworkConfig, Proxy};
use crate::vendors;
pub use respan_gateway_sdk::{Model, Price};

/// A client key in the config: the key alone, or with the name its requests
/// are logged under and what it may do.
#[derive(Clone, PartialEq, Eq)]
pub enum ClientKey {
    Key(String),
    Named(NamedKey),
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedKey {
    pub name: String,
    pub key: String,
    /// Whether the key reaches the admin API. When no key sets it, every
    /// key does; once one sets `true`, only those do.
    #[serde(default)]
    pub admin: Option<bool>,
    /// The tool loop and aggregate tools (`server__tool` globs) the key's
    /// requests may use, within the global `allow` and `deny`; all if unset,
    /// none if empty. A key that sets it reaches MCP servers at `/mcp`
    /// alone, not each at `/mcp/{name}`, which offers all of a server's.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

// A key is a secret: a debug print shows its name alone.
impl std::fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(_) => f.write_str("ClientKey(..)"),
            Self::Named(named) => write!(f, "ClientKey({})", named.name),
        }
    }
}

impl std::fmt::Debug for NamedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedKey")
            .field("name", &self.name)
            .field("admin", &self.admin)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for ClientKey {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(de)?;
        match value {
            serde_json::Value::String(key) => Ok(Self::Key(key)),
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Named)
                .map_err(|e| D::Error::custom(format!("a client key entry: {e}"))),
            _ => Err(D::Error::custom(
                "a client key is a string, or `{ name, key, admin?, tools? }`",
            )),
        }
    }
}

impl ClientKey {
    pub fn key(&self) -> &str {
        match self {
            Self::Key(key) => key,
            Self::Named(named) => &named.key,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Key(_) => None,
            Self::Named(named) => Some(&named.name),
        }
    }

    /// What the entry says of admin access, if it does.
    pub fn admin(&self) -> Option<bool> {
        match self {
            Self::Key(_) => None,
            Self::Named(named) => named.admin,
        }
    }

    pub fn tools(&self) -> Option<&[String]> {
        match self {
            Self::Key(_) => None,
            Self::Named(named) => named.tools.as_deref(),
        }
    }
}

impl From<&str> for ClientKey {
    fn from(key: &str) -> Self {
        Self::Key(key.to_owned())
    }
}

/// A provider's formats: by name, each at its vendor's base URL or the
/// provider's `base_url`; or each at a base URL of its own.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Formats {
    /// One format, by name.
    One(String),
    /// Several, by name.
    Many(Vec<String>),
    /// Each format's base URL, by the format's name.
    At(IndexMap<String, String>),
}

impl Formats {
    /// Its formats' names, in order.
    pub fn names(&self) -> Vec<&str> {
        match self {
            Formats::One(name) => vec![name.as_str()],
            Formats::Many(names) => names.iter().map(String::as_str).collect(),
            Formats::At(urls) => urls.keys().map(String::as_str).collect(),
        }
    }

    /// The base URL it gives format `name`, if it gives one.
    pub fn url_of(&self, name: &str) -> Option<&str> {
        match self {
            Formats::At(urls) => urls.get(name).map(String::as_str),
            _ => None,
        }
    }

    /// It, with what a URL may hold of a secret redacted.
    fn redacted(&self) -> Formats {
        match self {
            Formats::At(urls) => Formats::At(
                urls.iter()
                    .map(|(format, url)| (format.clone(), crate::redact::url(url)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

// Secrets stay out of a debug print: a key, `env:NAME` or not, is not shown.
impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("vendor", &self.vendor)
            .field("formats", &self.formats.as_ref().map(Formats::redacted))
            .field(
                "base_url",
                &self.base_url.as_deref().map(crate::redact::url),
            )
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer { .. } => f.write_str("Auth::Bearer(..)"),
            Self::Header { name, .. } => write!(f, "Auth::Header({name}: ..)"),
        }
    }
}

/// A gateway's configuration, as `gateway.toml` writes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where to listen, as `host:port` (a name is resolved), `:port` or a
    /// bare port (on 127.0.0.1); `127.0.0.1:8080` if unset.
    #[serde(default, deserialize_with = "listen_address")]
    pub listen: Option<SocketAddr>,
    /// The most bytes of a client's request, and of a provider's answer
    /// held at once, past which the request fails 413 or the answer 502; a
    /// number of bytes or a size such as `32MiB`; 32 MiB if unset.
    #[serde(default, deserialize_with = "size")]
    pub body_limit: Option<usize>,
    /// The largest WebSocket message of a session, from the client or the
    /// provider, past which the session ends (close 1009); a number of
    /// bytes or a size such as `16MiB`; 16 MiB if unset.
    #[serde(default, deserialize_with = "size")]
    pub ws_message_limit: Option<usize>,
    /// Models declared by `provider/model`, each a card (`pricing`,
    /// `display_name`, `context_window`, `capabilities`, ...) laid over what
    /// the provider's list says of it, or the whole of what is known of a
    /// model it does not list. A call's cost is its model's `pricing`.
    #[serde(default, deserialize_with = "models")]
    pub models: IndexMap<String, Model>,
    /// Retired: `models`, each with its `pricing`. Refused with how.
    #[serde(default)]
    pub prices: Option<serde::de::IgnoredAny>,
    /// How long open responses and end hooks get, together, after a stop
    /// signal, before they are cut: seconds, or a duration such as `30s`,
    /// `2m` or `500ms`; 30 s if unset.
    #[serde(default, deserialize_with = "span")]
    pub shutdown_grace: Option<Duration>,
    /// Client keys, literal or `env:NAME`, each alone or as
    /// `{ name, key }`. When set, every request needs one; it is consumed
    /// by the gateway and never sent to a provider. A request is logged
    /// under its key's name (`key-1`, `key-2`... for a key without one).
    #[serde(default)]
    pub client_keys: Vec<ClientKey>,
    /// How each finished request is printed to stderr: `text`, `json` or
    /// `off`.
    #[serde(default)]
    pub log: LogFormat,
    /// Whether the web UI and its admin API are served under `/ui/`.
    #[serde(default = "enabled")]
    pub ui: bool,
    /// Providers by name, in order: the first serving a kind is that
    /// kind's default.
    #[serde(default)]
    pub providers: IndexMap<String, ProviderConfig>,
    /// The formats served to clients, by name, tried in order. Unset:
    /// `openai-chat`, `anthropic-messages`, and every format of another
    /// kind some provider speaks, except `passthrough`. Serve
    /// `passthrough` last to relay every other request unread to the
    /// provider of format `passthrough`, whose `base_url` is then the
    /// vendor's origin. Clients of a format no other translates,
    /// `passthrough` or a session's, go to the one provider of that format.
    pub formats: Option<Vec<String>>,
    /// Model routes by the model name clients ask for, or a glob of names
    /// such as `llama-*`: where each goes, and how its candidates are
    /// picked, retried and raced. An exact name is tried first, then a
    /// `provider/model` name, then the globs in order, then each vendor's
    /// own model names, then the kind's default.
    #[serde(default)]
    pub routes: IndexMap<String, RouteConfig>,
    /// Tool servers by name, each served to clients over MCP at
    /// `/mcp/{name}`: an MCP server at a streamable-HTTP `url` or run as a
    /// `command`, or a tool `vendor`'s HTTP API. Name `mcp` in `formats` if
    /// that lists formats.
    #[serde(default)]
    pub tools: IndexMap<String, McpConfig>,
    /// Retired: `tools`. Refused with how.
    #[serde(default)]
    pub mcp: Option<serde::de::IgnoredAny>,
    /// Which web pages may call the gateway from a browser; none if unset.
    pub cors: Option<CorsConfig>,
    /// How the gateway reaches providers and tool servers: through which
    /// proxy, trusting which certificates besides the system's.
    #[serde(default)]
    pub network: NetworkConfig,
    /// The compiled-in plugins, each configured under its name. The gateway
    /// runs them in an order of its own, whatever the order here.
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(skip)]
    pub(crate) source: Source,
    /// Relay each request to one vendor, with the client's own credentials
    /// (`proxy`).
    #[serde(skip)]
    pub(crate) relay: bool,
}

fn enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: None,
            body_limit: None,
            ws_message_limit: None,
            models: IndexMap::new(),
            prices: None,
            shutdown_grace: None,
            client_keys: Vec::new(),
            log: LogFormat::Text,
            ui: true,
            providers: IndexMap::new(),
            formats: None,
            routes: IndexMap::new(),
            tools: IndexMap::new(),
            mcp: None,
            plugins: PluginsConfig::default(),
            cors: None,
            network: NetworkConfig::default(),
            source: Source::Inline,
            relay: false,
        }
    }
}

/// One provider: a vendor, explicit formats, or a vendor with some
/// of its settings replaced.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// A vendor: `openai`, `anthropic`, `gemini`, `groq`, ...
    pub vendor: Option<String>,
    /// Its format, or several: by name, or a table of each format's base
    /// URL by its name, which a vendor whose formats sit at different URLs
    /// (Gemini) needs to be moved; a vendor's are every format it speaks
    /// compiled in, each at its own URL.
    #[serde(default)]
    pub formats: Option<Formats>,
    /// The base URL its formats share, its API's, in place of a table's
    /// or its vendor's; a format of whole paths, such as `passthrough`,
    /// takes its origin.
    pub base_url: Option<String>,
    /// Its key, literal or `env:NAME`, sent as its formats expect; a
    /// vendor's is `env:<VENDOR>_API_KEY`.
    pub key: Option<String>,
    /// Its credentials as one header, in place of `key`. A provider with
    /// neither, and no vendor key, passes on the client's.
    pub auth: Option<Auth>,
    /// Its proxy, in place of `network`'s: a URL, literal or `env:NAME`, or
    /// `false` for none.
    pub proxy: Option<Proxy>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    /// `Authorization: Bearer <key>`
    Bearer { key: String },
    /// `<name>: <key>`
    Header { name: String, key: String },
}

/// The compiled-in plugins. Each is configured under its own name, and the
/// gateway registers them in a fixed order: the request log, routing, the
/// MCP aggregate, video jobs, the tool loop, then the dispatch of routes. Video
/// jobs are always on; a vendor's quirks are in the formats it speaks, not
/// plugins.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Local Ark checks; enabled only in builds with the ark feature.
    #[cfg(feature = "ark")]
    pub ark: Option<respan_gateway_plugin_ark::Config>,
    /// Which provider each request goes to; always on. Its routes are
    /// the top-level `routes`.
    #[serde(default)]
    pub routing: RoutingConfig,
    /// Retired: a route's `to`, `fallback` and `tries`. Refused with how.
    #[serde(default)]
    pub fallback: Option<serde::de::IgnoredAny>,
    /// Retired: a route's `hedge`. Refused with how.
    #[serde(default)]
    pub hedge: Option<serde::de::IgnoredAny>,
    /// Run tools of tool servers inside chat requests; off if unset.
    pub tool_loop: Option<ToolLoopConfig>,
    /// Keep the conversation state a client or provider cannot: Responses
    /// translated to another provider's format, so a client's
    /// `previous_response_id` works with it, and a Chat client's reasoning
    /// between tool calls with OpenAI's Responses. On unless
    /// `enabled = false`.
    #[serde(default)]
    pub responses: ResponsesConfig,
    /// Send each request to Respan as a trace; off if unset.
    pub respan: Option<RespanConfig>,
    /// The request history the web UI and admin API read; always on.
    #[serde(default)]
    pub request_log: RequestLogConfig,
}

/// Respan observability: each request a trace, sent to Respan.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespanConfig {
    /// The Respan API key, literal or `env:NAME`.
    pub key: String,
    /// The OTLP trace endpoint; Respan's if unset.
    pub url: Option<String>,
    /// Whether a model's messages and reply are sent; true if unset.
    pub content: Option<bool>,
    /// The service the traces are of; `respan-gateway` if unset.
    pub service: Option<String>,
}

// Its key is a secret: not printed.
impl std::fmt::Debug for RespanConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RespanConfig")
            .field("content", &self.content)
            .finish_non_exhaustive()
    }
}

/// The conversation state the gateway keeps
/// (`respan_gateway_plugin_responses`), in its memory: not across a
/// restart, nor shared between gateways, so a client whose requests reach
/// several gateways needs them to reach the one that kept its conversation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesConfig {
    /// Whether it is kept; it is unless `false`.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// How long what is kept is kept after it was last read; an hour if
    /// unset.
    #[serde(default, deserialize_with = "span")]
    pub keep: Option<Duration>,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: None,
        }
    }
}

/// Routing: which provider each request goes to.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// Retired: the top-level `routes`. Refused with how.
    #[serde(default)]
    pub models: Option<serde::de::IgnoredAny>,
    /// The provider of each kind it serves for requests nothing else
    /// routes; the first provider serving the kind if unset.
    pub default: Option<String>,
    /// Whether each provider's model list is read at start and every ten
    /// minutes, so a model one lists goes to it, and routes can pick by
    /// price and capability; true if unset.
    pub listings: Option<bool>,
}

/// One route: the candidates a model name goes to, and how they are
/// picked, retried and raced.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Where it goes: `provider`, `provider/model` or `provider/{model}`
    /// (`{model}` the name asked for), or a list of them, each alone or
    /// as `{ target = "..", weight = 3 }`.
    #[serde(deserialize_with = "route_targets")]
    pub to: Vec<RouteTarget>,
    /// How the candidates are ordered: `first` (as listed), `cheapest`
    /// (by each model's `pricing`: input plus three times output), `fastest` (by the gateway's own record of each one's time
    /// to a response), `weighted` or `random`. `first` if unset.
    #[serde(default)]
    pub pick: Pick,
    /// What a candidate's model must be able to do, as its provider's
    /// listing says, such as `tools` or `vision`; one the listing says
    /// nothing of is kept.
    #[serde(default)]
    pub needs: Vec<String>,
    /// Which requests it applies to; every request if unset.
    #[serde(default)]
    pub when: RouteWhen,
    /// Whether a failed candidate falls back to the next; true if unset.
    pub fallback: Option<bool>,
    /// Races the candidates instead, each started this long after the one
    /// before, such as `200ms`; off if unset.
    #[serde(default, deserialize_with = "span")]
    pub hedge: Option<Duration>,
    /// Tries per candidate, retries included; 1 if unset.
    pub tries: Option<u32>,
    /// The first wait before a retry, doubled after each; 250 ms if unset.
    #[serde(default, deserialize_with = "span")]
    pub backoff: Option<Duration>,
    /// The longest wait before a retry; a `Retry-After` longer than it means
    /// no retry. 10 s if unset.
    #[serde(default, deserialize_with = "span")]
    pub max_backoff: Option<Duration>,
    /// How far an answer is read before it counts as a success.
    #[serde(default)]
    pub until: Commit,
}

/// A route's candidate, alone or weighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    /// `provider`, `provider/model` or `provider/{model}`.
    pub target: String,
    /// Its share of requests for `pick = "weighted"`; 1 if unset.
    pub weight: u32,
}

fn route_targets<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<RouteTarget>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Weighted {
        target: String,
        weight: Option<u32>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum One {
        Plain(String),
        Weighted(Weighted),
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Targets {
        One(One),
        Many(Vec<One>),
    }
    let shape = "routes: to: a target such as \"openai/gpt-4o\", \
                 `{ target = \"..\", weight = 3 }`, or a list of them";
    let targets = Targets::deserialize(d).map_err(|_| serde::de::Error::custom(shape))?;
    let list = match targets {
        Targets::One(one) => vec![one],
        Targets::Many(many) => many,
    };
    Ok(list
        .into_iter()
        .map(|one| match one {
            One::Plain(target) => RouteTarget { target, weight: 1 },
            One::Weighted(w) => RouteTarget {
                target: w.target,
                weight: w.weight.unwrap_or(1),
            },
        })
        .collect())
}

/// Which requests a route applies to.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteWhen {
    /// The client keys, by name; any if empty.
    #[serde(default)]
    pub clients: Vec<String>,
    /// The kinds, such as `chat` or `embedding`; any if empty.
    #[serde(default)]
    pub kinds: Vec<String>,
}

/// How far an attempt's answer is read before it counts as a success, and
/// nothing is retried after.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Commit {
    /// Its head: a success status. The default.
    #[default]
    Head,
    /// Its whole answer, held until it ends cleanly.
    End,
}

/// The request log: the history of finished requests.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestLogConfig {
    /// How many finished requests the in-memory history (the web UI's)
    /// keeps; 1000 if unset.
    #[serde(default, deserialize_with = "request_log_history")]
    pub history: Option<usize>,
}

/// Browser access: the origins whose pages may call the gateway.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Origins such as `https://app.example.com` (scheme, host and port
    /// alone), or `*` for any, which needs `client_keys`. Their preflight
    /// requests are answered without a key; their other requests need the
    /// gateway's usual one.
    pub origins: Vec<String>,
}

/// The tool loop: the gateway offers the tools of tool servers to the
/// model and runs the ones it calls, so a client that knows nothing of MCP
/// gets answers that used them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLoopConfig {
    /// The tool servers whose tools the gateway offers and runs, as
    /// `<server>__<tool>`; every one that takes no client's credentials
    /// if unset, as `/mcp` fronts them.
    pub servers: Option<Vec<String>>,
    /// Which of their tools are offered, as the model names them
    /// (`<server>__<tool>`), `*` standing for any characters, such as
    /// `github__search_*`; all of them if unset or empty.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools never offered, named as `allow` does; it wins over `allow`.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Model name prefixes the loop applies to, as the provider is sent
    /// them; every chat model if unset.
    #[serde(default)]
    pub models: Vec<String>,
    /// The most calls to the model for one request; 8 if unset.
    pub max_steps: Option<u32>,
    /// The wall-clock limit of one request's loop; 5 minutes if unset.
    #[serde(default, deserialize_with = "span")]
    pub timeout: Option<Duration>,
    /// The limit of one tool call; 60 s if unset.
    #[serde(default, deserialize_with = "span")]
    pub tool_timeout: Option<Duration>,
    /// How long a server's tool list is kept; 5 minutes if unset.
    #[serde(default, deserialize_with = "span")]
    pub tool_list_ttl: Option<Duration>,
    /// The most tool calls of one step that run at once; 4 if unset.
    pub parallel: Option<usize>,
    /// The most bytes of one tool's answer the model sees; 64 KiB if unset.
    /// A tool's output is untrusted input to the model.
    #[serde(default, deserialize_with = "size")]
    pub max_result: Option<usize>,
    /// The most bytes of tool answers the model sees in one request, over
    /// all its steps; 256 KiB if unset.
    #[serde(default, deserialize_with = "size")]
    pub max_results: Option<usize>,
}

/// Where a configuration came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Environment,
    Proxy,
    #[default]
    Inline,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(path) => write!(f, "{}", path.display()),
            Source::Environment => f.write_str("environment"),
            Source::Proxy => f.write_str("proxy"),
            Source::Inline => f.write_str("inline"),
        }
    }
}

/// The vendor `proxy` relays to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyVendor {
    Anthropic,
    OpenAi,
}

/// The file read when none is named.
pub const DEFAULT_FILE: &str = "gateway.toml";

impl Config {
    /// A configuration from TOML text. Errors omit source excerpts, which may
    /// contain credentials, while retaining the line and column.
    pub fn parse(text: &str) -> Result<Config, Error> {
        let read = serde_path_to_error::deserialize(toml::Deserializer::new(text));
        read.map_err(|e| {
            let path = e.path().to_string();
            let said = toml_error(text, e.inner());
            // A message that names its key already, as most of this
            // module's do, is not named twice.
            let top = path.split(['.', '[']).next().unwrap_or_default();
            match path.as_str() {
                "." => Error::new(said),
                _ if said.starts_with(top) => Error::new(said),
                _ => Error::new(format!("{path}: {said}")),
            }
        })
    }

    /// Structured configuration for language bindings, with secret-safe errors.
    pub fn from_value(value: serde_json::Value) -> Result<Config, Error> {
        serde_json::from_value(value).map_err(|e| Error::new(redacted(&e.to_string())))
    }

    /// The configuration in the file at `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, Error> {
        let path = path.as_ref();
        let in_file = |e: String| Error::new(format!("{}: {e}", path.display()));
        let text = std::fs::read_to_string(path).map_err(|e| in_file(e.to_string()))?;
        let mut config = Config::parse(&text).map_err(|e| in_file(e.to_string()))?;
        config.source = Source::File(path.to_owned());
        Ok(config)
    }

    /// A configuration with no file: a provider for every vendor whose key
    /// is in the environment (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...;
    /// Ollama if `OLLAMA_HOST` is set). `RESPAN_<VENDOR>_BASE_URL` moves a
    /// vendor's base URL. `Err` if there is none. Each tool vendor whose
    /// key is there (`LINKUP_API_KEY`, ...) is a tool server of its name,
    /// and `/mcp` fronts them; the tool loop stays off.
    pub fn from_env() -> Result<Config, Error> {
        let detected = vendors::detected();
        if detected.is_empty() {
            return Err(Error::new(no_keys_message()));
        }
        let providers = detected
            .into_iter()
            .map(|(vendor, key, base_url)| {
                let provider = ProviderConfig {
                    vendor: Some(vendor.id.to_owned()),
                    key,
                    base_url,
                    ..ProviderConfig::default()
                };
                (vendor.id.to_owned(), provider)
            })
            .collect::<IndexMap<String, ProviderConfig>>();
        // Each tool vendor with its key in the environment, a tool server
        // of its name, unless a provider has the name.
        let tools: IndexMap<String, McpConfig> = (respan_gateway_tool_vendors::ALL.iter())
            .filter(|v| v.key_from_env().is_some() && !providers.contains_key(v.id))
            .map(|v| (v.id.to_owned(), McpConfig::tool_vendor(v.id)))
            .collect();
        Ok(Config {
            providers,
            tools,
            source: Source::Environment,
            ..Config::default()
        })
    }

    /// `path` if given, else `./gateway.toml`, else
    /// [`from_env`](Self::from_env).
    pub fn discover(path: Option<&Path>) -> Result<Config, Error> {
        match path {
            Some(path) => Config::load(path),
            None if Path::new(DEFAULT_FILE).exists() => Config::load(DEFAULT_FILE),
            None => Config::from_env(),
        }
    }

    /// The configuration of `proxy`: one vendor at `url` (its public API if
    /// `None`), no keys of its own, its chat format and every other request
    /// relayed, with each client's credentials: the chat format writes the
    /// client's request in its own spec, and passthrough relays the rest
    /// byte for byte.
    pub fn proxy(vendor: ProxyVendor, url: Option<&str>) -> Config {
        let (name, format, default) = match vendor {
            ProxyVendor::Anthropic => (
                "anthropic",
                "anthropic-messages",
                "https://api.anthropic.com",
            ),
            ProxyVendor::OpenAi => ("openai", "openai-chat", "https://api.openai.com/v1"),
        };
        let url = url.unwrap_or(default).trim_end_matches('/');
        // The chat format writes its own path after the vendor's API base;
        // passthrough relays the client's path at the vendor's origin.
        let provider = ProviderConfig {
            formats: Some(Formats::Many(vec![
                format.to_owned(),
                "passthrough".to_owned(),
            ])),
            base_url: Some(url.to_owned()),
            ..ProviderConfig::default()
        };
        Config {
            providers: IndexMap::from([(name.to_owned(), provider)]),
            formats: Some(vec![format.to_owned(), "passthrough".to_owned()]),
            source: Source::Proxy,
            relay: true,
            ..Config::default()
        }
    }

    /// Where it came from.
    pub fn source(&self) -> &Source {
        &self.source
    }
}

pub(crate) fn toml_error(text: &str, error: &toml::de::Error) -> String {
    let message = redacted(error.message());
    match error.span() {
        Some(span) => {
            let prefix = &text[..span.start.min(text.len())];
            let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
            let column = prefix
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .chars()
                .count()
                + 1;
            format!("{message} at line {line} column {column}")
        }
        None => message,
    }
}

/// `message` with the values serde echoes in it left out: a string
/// (`invalid type: string "..."`), and a variant name (`unknown variant
/// `...``), which a misplaced secret would be.
fn redacted(message: &str) -> String {
    let mut out = message.to_owned();
    for (start, end) in [
        ("string \"", "\", expected"),
        ("unknown variant `", "`, expected"),
    ] {
        let mut from = 0;
        while let Some(at) = out[from..].find(start) {
            let value = from + at + start.len();
            let Some(len) = out[value..].rfind(end) else {
                break;
            };
            out.replace_range(value..value + len, "...");
            from = value + 3;
        }
    }
    out
}

/// What to do when there is no config file and no key in the environment.
pub fn no_keys_message() -> String {
    format!(
        "no configuration found.\n\n\
         Create a config:\n  respan-gateway init\n\n\
         Or start with a provider key:\n  export OPENAI_API_KEY=sk-...\n  respan-gateway\n\n\
         Config file: {DEFAULT_FILE}\n\
         More options: respan-gateway --help"
    )
}

/// Where `text` says to listen: `host:port`, a name resolved, preferring
/// IPv4; `:port` or a bare port, on 127.0.0.1.
pub fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
    let text = text.trim();
    let port = text.strip_prefix(':').unwrap_or(text);
    if let Ok(port) = port.parse::<u16>() {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    if let Ok(addr) = text.parse() {
        return Ok(addr);
    }
    let usage = || format!("'{text}' is not host:port, :port or a port");
    if !text.contains(':') {
        return Err(usage());
    }
    let addrs: Vec<SocketAddr> = text
        .to_socket_addrs()
        .map_err(|e| format!("{}: {e}", usage()))?
        .collect();
    let v4 = addrs.iter().find(|a| a.is_ipv4());
    v4.or(addrs.first())
        .copied()
        .ok_or_else(|| format!("'{text}' resolves to no address"))
}

fn listen_address<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SocketAddr>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Address {
        Text(String),
        Port(u16),
    }
    let text = match Option::<Address>::deserialize(d)? {
        None => return Ok(None),
        Some(Address::Text(text)) => text,
        Some(Address::Port(port)) => port.to_string(),
    };
    parse_listen(&text)
        .map(Some)
        .map_err(|e| serde::de::Error::custom(format!("listen: {e}")))
}

/// A number of bytes, or a size such as `32MiB`, `512 KB` or `1GiB`
/// ([`bytesize`]); at least one byte.
fn size<'de, D: Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
    let Some(text) = Option::<Number>::deserialize(d)? else {
        return Ok(None);
    };
    let bytes = match text {
        Number::Count(bytes) => bytes,
        Number::Text(text) => text
            .trim()
            .parse::<bytesize::ByteSize>()
            .map(|size| size.as_u64())
            .map_err(|_| {
                serde::de::Error::custom(format!("'{text}' is not a size such as 32MiB"))
            })?,
    };
    match usize::try_from(bytes) {
        Ok(0) => Err(serde::de::Error::custom("a size must be above 0")),
        Ok(bytes) => Ok(Some(bytes)),
        Err(_) => Err(serde::de::Error::custom("a size this large cannot be held")),
    }
}

/// A count, or text that says one with a unit.
#[derive(Deserialize)]
#[serde(untagged)]
enum Number {
    Count(u64),
    Text(String),
}

/// Each declared model, its id and provider those its key names.
fn models<'de, D: Deserializer<'de>>(d: D) -> Result<IndexMap<String, Model>, D::Error> {
    let models = IndexMap::<String, Model>::deserialize(d)?;
    let bad = |key: &str, why: &str| serde::de::Error::custom(format!("models: {key}: {why}"));
    let mut declared = IndexMap::new();
    for (key, mut model) in models {
        let Some((provider, id)) = key
            .split_once('/')
            .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        else {
            return Err(bad(&key, "name it as `provider/model`"));
        };
        if let Some(p) = &model.price {
            let rates = [
                Some(p.input),
                Some(p.output),
                p.cached_input,
                p.cache_write,
                p.per_second,
            ];
            if rates.iter().flatten().any(|r| !r.is_finite() || *r < 0.0) {
                return Err(bad(&key, "a price must be a number of dollars, 0 or more"));
            }
        }
        (model.id, model.provider) = (id.to_owned(), provider.to_owned());
        declared.insert(key, model);
    }
    Ok(declared)
}

fn request_log_history<'de, D: Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
    match Option::<usize>::deserialize(d)
        .map_err(|e| serde::de::Error::custom(format!("plugins.request_log.history: {e}")))?
    {
        Some(0) => Err(serde::de::Error::custom(
            "plugins: request_log: history: must be above 0",
        )),
        kept => Ok(kept),
    }
}

/// A number of seconds, or a duration such as `30s`, `2m`, `1h` or `500ms`
/// ([`humantime`]).
fn span<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    Ok(match Option::<Number>::deserialize(d)? {
        None => None,
        Some(Number::Count(seconds)) => Some(Duration::from_secs(seconds)),
        Some(Number::Text(text)) => Some(parse_duration(&text).map_err(serde::de::Error::custom)?),
    })
}

/// The time `text` says, as [`humantime`] reads it (`30s`, `2m`, `1h 30m`),
/// or a number of seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    match text.parse::<u64>() {
        Ok(seconds) => Ok(Duration::from_secs(seconds)),
        Err(_) => humantime::parse_duration(text)
            .map_err(|_| format!("'{text}' is not a duration such as 30s or 2m")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_preserves_provider_order_and_reads_readable_values() {
        let config = Config::parse(
            r#"
listen = 8080
body_limit = "2MiB"
shutdown_grace = "500ms"
client_keys = [{ name = "ops", key = "env:OPS_KEY", admin = true }]
[providers.z]
vendor = "openai"
[providers.a]
vendor = "anthropic"
"#,
        )
        .expect("TOML config");
        assert_eq!(
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert_eq!(config.listen.expect("listen").port(), 8080);
        assert_eq!(config.body_limit, Some(2 << 20));
        assert_eq!(config.shutdown_grace, Some(Duration::from_millis(500)));
        assert_eq!(config.client_keys[0].name(), Some("ops"));
    }

    #[test]
    fn toml_diagnostics_have_locations_without_source_secrets() {
        for text in [
            "log = 'sk-secret-1'",
            "client_keys = 'sk-secret-1'",
            "[providers.p]\nkey = 'sk-secret-1'\nbogus = true",
            "[providers.p]\nkey = \"sk-secret-1",
        ] {
            let error = Config::parse(text).expect_err("invalid").to_string();
            assert!(error.contains("line"), "{error}");
            assert!(!error.contains("sk-secret-1"), "{error}");
        }
    }

    #[test]
    fn toml_example_and_generated_starter_parse() {
        Config::parse(include_str!("../../../gateway.example.toml")).expect("example");
        let picks = [crate::init::Chosen::parse("openai=key\"with\ncontrols").expect("vendor")];
        let text = crate::init::starter(&picks, Some("client-secret"));
        let config = Config::parse(&text).expect("starter");
        assert_eq!(
            config.providers["openai"].key.as_deref(),
            Some("key\"with\ncontrols")
        );
        assert_eq!(config.client_keys[0].key(), "client-secret");
    }

    #[test]
    fn debug_shows_no_secret() {
        let config = Config::parse(
            "client_keys = [\"bare-secret\", { name = \"ops\", key = \"named-secret\", admin = true }]\nproviders = { a = { vendor = \"openai\", key = \"prov-secret\", base_url = \"https://user:url-secret@h.test/v1?k=q-secret\" }, b = { formats = \"openai-chat\", base_url = \"http://b.test\", auth = { type = \"bearer\", key = \"auth-secret\" } } }\ntools = { m = { command = \"npx\", args = [\"--token\", \"arg-secret\"], env = { T = \"env-secret\" } }, h = { url = \"https://h.test/mcp/tok-in-path\", headers = { x-k = \"mcp-secret\" }, auth = { oauth = { token_url = \"https://o.test/t\", client_id = \"cid\", client_secret = \"oauth-secret\" } } }, b = { url = \"https://b.test/mcp\", auth = { bearer = \"bearer-secret\" } } }\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let shown = format!("{config:?}");
        for secret in [
            "bare-secret",
            "named-secret",
            "prov-secret",
            "url-secret",
            "q-secret",
            "auth-secret",
            "arg-secret",
            "env-secret",
            "mcp-secret",
            "bearer-secret",
            "oauth-secret",
            "tok-in-path",
        ] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
    }

    #[test]
    fn a_client_key_entry_with_a_typo_is_named() {
        let said = |text: &str| Config::parse(text).err().map(|e| e.to_string());
        let bad = said("client_keys = [{ name = \"ci\", ky = \"x\" }]\n").unwrap_or_default();
        assert!(bad.contains("ky") && bad.contains("client key"), "{bad}");
        assert!(
            said(
                "client_keys = [{ name = \"ci\", key = \"x\", admin = true, tools = [\"a__*\"] }]\n"
            )
            .is_none()
        );
        assert!(said("client_keys = [3]\n").is_some());
    }

    #[test]
    fn a_listen_address_is_a_socket_address() {
        let bad = Config::parse("listen = \"127.0.0.1\"\n").err();
        assert!(bad.is_some_and(|e| e.to_string().contains("listen")));
        let good = Config::parse("listen = \"127.0.0.1:8080\"\n");
        assert!(good.is_ok_and(|c| c.listen.is_some_and(|a| a.port() == 8080)));
        let named = Config::parse("listen = \"localhost:8081\"\n");
        assert!(named.is_ok_and(|c| c.listen.is_some_and(|a| a.port() == 8081)));
    }

    #[test]
    fn a_listen_address_may_be_a_name_a_port_or_colon_port() {
        let local = |port| Ok(SocketAddr::from(([127, 0, 0, 1], port)));
        assert_eq!(parse_listen("8080"), local(8080));
        assert_eq!(parse_listen(":8080"), local(8080));
        assert_eq!(parse_listen("localhost:8080").map(|a| a.port()), Ok(8080));
        assert!(parse_listen("localhost:8080").is_ok_and(|a| a.ip().is_loopback()));
        assert_eq!(
            parse_listen("[::1]:9"),
            Ok(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 9)))
        );
        assert!(parse_listen("0.0.0.0:80").is_ok_and(|a| a.ip().is_unspecified()));
        assert!(parse_listen("localhost").is_err());
        assert!(parse_listen("99999").is_err());
    }

    #[test]
    fn vendors_lists_keys_and_switches_parse() {
        let text = "client_keys = [\"env:GATEWAY_KEY\", \"sk-local\"]\nlog = \"off\"\nui = false\nproviders = { openai = { vendor = \"openai\" }, claude = { vendor = \"anthropic\", key = \"env:MY_KEY\", base_url = \"http://x.test\" }, both = { formats = [\"openai-chat\", \"anthropic-messages\"], base_url = \"http://y.test\" }, one = { formats = \"openai-chat\", base_url = \"http://z.test\" } }\n";
        let config = Config::parse(text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            config.client_keys,
            [
                ClientKey::from("env:GATEWAY_KEY"),
                ClientKey::from("sk-local")
            ]
        );
        assert!(config.log == LogFormat::Off && !config.ui);
        assert!(config.formats.is_none());
        let names: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["openai", "claude", "both", "one"],
            "in config order"
        );
        let both = &config.providers["both"];
        assert_eq!(
            both.formats.as_ref().map(Formats::names),
            Some(vec!["openai-chat", "anthropic-messages"])
        );
        assert_eq!(
            config.providers["one"].formats.as_ref().map(Formats::names),
            Some(vec!["openai-chat"])
        );
        assert_eq!(
            config.providers["claude"].key.as_deref(),
            Some("env:MY_KEY")
        );
        assert_eq!(config.providers["openai"].vendor.as_deref(), Some("openai"));
    }

    #[test]
    fn a_body_limit_is_bytes_or_a_size() {
        let limit = |text: &str| Config::parse(text).map(|c| c.body_limit);
        assert_eq!(limit("body_limit = 1000\n"), Ok(Some(1000)));
        assert_eq!(limit("body_limit = \"32MiB\"\n"), Ok(Some(32 << 20)));
        assert_eq!(limit("body_limit = \"2 kb\"\n"), Ok(Some(2000)));
        let bad = limit("body_limit = \"lots\"\n")
            .err()
            .map(|e| e.to_string());
        assert!(bad.is_some_and(|e| e.contains("body_limit")));
    }

    #[test]
    fn limits_are_sizes_counts_and_durations() {
        let config = Config::parse(
            "ws_message_limit = \"1MiB\"\nplugins = { request_log = { history = 50 } }\nshutdown_grace = \"2m\"\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(config.ws_message_limit, Some(1 << 20));
        assert_eq!(config.plugins.request_log.history, Some(50));
        assert_eq!(config.shutdown_grace, Some(Duration::from_secs(120)));
        let grace = |text: &str| Config::parse(text).map(|c| c.shutdown_grace);
        assert_eq!(
            grace("shutdown_grace = \"30s\"\n"),
            Ok(Some(Duration::from_secs(30)))
        );
        assert_eq!(
            grace("shutdown_grace = 45\n"),
            Ok(Some(Duration::from_secs(45)))
        );
        assert_eq!(
            grace("shutdown_grace = \"500ms\"\n"),
            Ok(Some(Duration::from_millis(500)))
        );
        let defaults = Config::parse("").unwrap_or_else(|e| panic!("{e}"));
        assert!(
            defaults.plugins.request_log.history.is_none() && defaults.shutdown_grace.is_none()
        );
        for (text, key) in [
            ("shutdown_grace = \"soon\"\n", "shutdown_grace"),
            ("shutdown_grace = \"5 parsecs\"\n", "shutdown_grace"),
            ("plugins = { request_log = { history = 0 } }\n", "history"),
            (
                "plugins = { request_log = { history = \"many\" } }\n",
                "history",
            ),
            (
                "[routes.x]\nto = \"p\"\nbackoff = \"soon\"\n",
                "routes.x.backoff",
            ),
            ("ws_message_limit = 0\n", "ws_message_limit"),
            ("ws_message_limit = \"big\"\n", "ws_message_limit"),
            ("body_limit = 0\n", "body_limit"),
        ] {
            let said = Config::parse(text).err().map(|e| e.to_string());
            assert!(said.is_some_and(|e| e.contains(key)), "{text}");
        }
    }

    #[test]
    fn old_names_are_refused() {
        let old = [
            "serve = []\n",
            "target = \"p\"\n",
            "keys = []\n",
            "providers = { p = { format = \"x\" } }\n",
            "request_history = 10\n",
            "tool_loop = { servers = [\"m\"] }\n",
            "mcp_aggregate = {  }\n",
            "default = \"p\"\n",
            "plugins = [{ models = [] }]\n",
            "plugins = { quirks = false }\n",
            "plugins = { video_jobs = false }\n",
        ];
        for text in old {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn defaults_serve_the_ui_and_log() {
        let config = Config::parse("").unwrap_or_else(|e| panic!("{e}"));
        assert!(config.log == LogFormat::Text && config.ui && config.client_keys.is_empty());
        assert_eq!(config.source(), &Source::Inline);
    }

    #[test]
    fn an_error_does_not_echo_a_value_that_may_be_a_secret() {
        for text in [
            "client_keys = \"sk-secret-1\"\n",
            "providers = { p = { vendor = \"openai\", key = [\"sk-secret-1\"] } }\n",
            "providers = { p = { vendor = \"openai\", auth = \"sk-secret-1\" } }\n",
            "tools = { m = { command = \"x\", env = \"sk-secret-1\" } }\n",
            "tools = { m = { command = \"x\", args = \"sk-secret-1\" } }\n",
            "tools = { m = { url = \"http://h.test\", headers = \"sk-secret-1\" } }\n",
            "tools = { m = { url = \"http://h.test\", auth = \"sk-secret-1\" } }\n",
            "tools = { m = { url = \"http://h.test\", auth = { bearer = [\"sk-secret-1\"] } } }\n",
            "log = \"sk-secret-1\"\n",
            "providers = { p = { vendor = \"openai\", auth = { type = \"sk-secret-1\", key = \"k\" } } }\n",
        ] {
            let said = Config::parse(text)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(!said.is_empty(), "{text}");
            assert!(!said.contains("sk-secret-1"), "{text}: {said}");
        }
        let said = Config::parse("client_keys = \"sk-secret-1\"\n")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(said.contains("string \"...\""), "{said}");
    }

    #[test]
    fn tool_loop_and_cors_keys_parse() {
        let text = "plugins = { tool_loop = { servers = [\"github\"], allow = [\"github__*\"], deny = [\"github__delete_*\"], tool_list_ttl = \"1m\", parallel = 2, max_result = \"8KiB\", max_results = \"64KiB\" } }\ncors = { origins = [\"https://app.example.com\"] }\n";
        let config = Config::parse(text).unwrap_or_else(|e| panic!("{e}"));
        let tool_loop = config
            .plugins
            .tool_loop
            .unwrap_or_else(|| panic!("tool_loop"));
        assert_eq!(tool_loop.servers, Some(vec!["github".to_owned()]));
        assert_eq!(tool_loop.allow, ["github__*"]);
        assert_eq!(tool_loop.deny, ["github__delete_*"]);
        assert_eq!(tool_loop.tool_list_ttl, Some(Duration::from_secs(60)));
        assert_eq!(tool_loop.parallel, Some(2));
        assert_eq!(tool_loop.max_results, Some(64 << 10));
        assert!(config.cors.is_some_and(|c| c.origins.len() == 1));
    }

    #[test]
    fn routes_parse_by_model_name() {
        let text = "plugins = { routing = { default = \"openai\" } }\n\
                    [routes.smart]\n\
                    to = [\"anthropic/claude\", { target = \"openai/gpt-5\", weight = 3 }]\n\
                    pick = \"weighted\"\n\
                    needs = [\"tools\"]\n\
                    when = { clients = [\"ops\"], kinds = [\"chat\"] }\n\
                    tries = 2\n\
                    backoff = \"100ms\"\n\
                    until = \"end\"\n\
                    [routes.\"llama-*\"]\n\
                    to = \"groq\"\n\
                    hedge = \"300ms\"\n";
        let config = Config::parse(text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(config.plugins.routing.default.as_deref(), Some("openai"));
        let smart = &config.routes["smart"];
        assert_eq!(
            smart.to,
            [
                RouteTarget {
                    target: "anthropic/claude".into(),
                    weight: 1
                },
                RouteTarget {
                    target: "openai/gpt-5".into(),
                    weight: 3
                },
            ]
        );
        assert_eq!(smart.pick, Pick::Weighted);
        assert_eq!(smart.needs, ["tools"]);
        assert_eq!(smart.when.clients, ["ops"]);
        assert_eq!(smart.tries, Some(2));
        assert_eq!(smart.backoff, Some(Duration::from_millis(100)));
        assert_eq!(smart.until, Commit::End);
        let llama = &config.routes["llama-*"];
        assert_eq!(llama.to[0].target, "groq");
        assert_eq!(llama.hedge, Some(Duration::from_millis(300)));
        assert_eq!(llama.pick, Pick::First);
        let said = Config::parse("[routes.x]\nto = 3\n")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(said.contains("routes: to:"), "{said}");
    }

    #[test]
    fn unknown_fields_are_refused() {
        let said = Config::parse("providers = { p = { vendor = \"openai\", bogus = 1 } }\n")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(said.contains("bogus"), "{said}");
    }

    #[test]
    fn a_provider_s_formats_are_names_or_each_at_its_url() {
        let config = Config::parse(
            "providers = { g = { vendor = \"gemini\", formats = { gemini-generate = \"http://g.test/v1beta\", openai-chat = \"http://g.test/v1beta/openai\" } } }\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let formats = config.providers["g"].formats.as_ref();
        assert_eq!(
            formats.map(Formats::names),
            Some(vec!["gemini-generate", "openai-chat"])
        );
        let url = formats.and_then(|f| f.url_of("openai-chat"));
        assert_eq!(url, Some("http://g.test/v1beta/openai"));
    }

    #[test]
    fn proxy_is_one_provider_of_the_chat_format_and_passthrough_at_its_api_base() {
        let config = Config::proxy(ProxyVendor::OpenAi, Some("http://h.test/v1/"));
        let provider = &config.providers["openai"];
        assert_eq!(provider.base_url.as_deref(), Some("http://h.test/v1"));
        assert_eq!(
            provider.formats.as_ref().map(Formats::names),
            Some(vec!["openai-chat", "passthrough"])
        );
        assert!(config.relay);
        assert_eq!(config.source().to_string(), "proxy");
    }
}
