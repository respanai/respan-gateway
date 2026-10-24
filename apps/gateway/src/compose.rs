//! A checked configuration's gateway: its providers, one core provider per
//! format, and the formats it serves; and the plan the rest of the binary
//! reads, of who serves what.

use std::collections::BTreeSet;
use std::sync::Arc;

use respan_gateway_core::{Fault, Gateway};
use respan_gateway_http::http::HeaderMap;
use respan_gateway_http::http::header::{AUTHORIZATION, HeaderName, HeaderValue};
use respan_gateway_ws::Limits;

use crate::auth::Client;
use crate::config::{Auth, ClientKey, Config, Formats, ProviderConfig};
use crate::formats::{KnownFormat, kind};
use crate::vendors;
use respan_gateway_plugin_routing::{Routes, Rule, Speaks};
use respan_gateway_sdk::{Catalog, Model};
use respan_gateway_vendors::{Locate, Network, VendorProvider};

/// The name of the format that relays what no other served format accepts.
pub const PASSTHROUGH: &str = "passthrough";

/// A provider as configured, with the core provider of each of its formats.
#[derive(Debug)]
pub struct Upstream {
    pub name: String,
    /// What reaches it: a provider of its vendor, at its base URL with its key.
    pub provider: VendorProvider,
    pub entries: Vec<Entry>,
    /// Whether it sends a key of its own, rather than the client's.
    pub own_key: bool,
    /// Where its key comes from, such as `env OPENAI_API_KEY` or `config`;
    /// never the key.
    pub key_from: String,
    /// The models the config declares of it.
    pub models: Vec<Model>,
}

/// One format of a provider: the core provider registered for it.
pub struct Entry {
    pub format: String,
    pub kind: &'static str,
    /// The core provider's name: the provider's for its first format,
    /// `name:format` for the others.
    pub internal: String,
    pub base_url: String,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("format", &self.format)
            .field("internal", &self.internal)
            .field("base_url", &crate::redact::url(&self.base_url))
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct Plan {
    pub upstreams: Vec<Upstream>,
    pub serve: Vec<String>,
    pub routes: Arc<Routes>,
    pub keys: Vec<Client>,
    /// The MCP servers, set by the app that built their format.
    pub mcp: Vec<crate::mcp::McpServer>,
}

impl Plan {
    pub fn serves(&self, format: &str) -> bool {
        self.serve.iter().any(|s| s == format)
    }
}

/// The gateway `config` says, its providers reading up to `limit` bytes of
/// an answer at once, their sessions taking `sessions`, and connecting
/// through `network` but for a `proxy` of their own.
pub fn compose(
    config: &Config,
    formats: &[KnownFormat],
    limit: usize,
    sessions: Limits,
    network: &Network,
) -> Result<(Gateway, Plan), Fault> {
    let (gateway, mut upstreams) = providers(config, formats, limit, sessions, network)?;
    for u in &mut upstreams {
        let declared = config.models.values().filter(|m| m.provider == u.name);
        u.models = declared.cloned().collect();
    }
    let serve = match &config.formats {
        Some(serve) => serve.clone(),
        None => default_serve(&upstreams, formats),
    };
    let served = served(&serve, &upstreams, formats)?;
    let routing = &config.plugins.routing;
    if let Some(default) = &routing.default
        && !upstreams.iter().any(|u| u.name == *default)
    {
        return Err(Fault::config(format!(
            "plugins: routing: default: unknown provider '{default}'; providers: [{}]",
            names(&upstreams)
        )));
    }
    let keys = clients(&config.client_keys)?;
    let rules = crate::rules::rules(config, &upstreams, &keys, formats)?;
    if upstreams.is_empty() && config.tools.is_empty() {
        return Err(Fault::config(
            "providers: none configured; add one, such as `[providers.openai]` with `vendor = 'openai'`",
        ));
    }
    let catalog = catalog(&upstreams, config)?;
    let routes = Arc::new(routes(&upstreams, rules, routing.default.clone()).with_catalog(catalog));
    let gateway = (served.into_iter()).try_fold(gateway, |gateway, f| f.serve(gateway, &routes))?;
    let plan = Plan {
        upstreams,
        serve,
        routes,
        keys,
        mcp: Vec::new(),
    };
    Ok((gateway, plan))
}

/// What the gateway knows of its providers' models, to begin with: the
/// models the config declares of each, until it lists its own.
fn catalog(upstreams: &[Upstream], config: &Config) -> Result<Arc<Catalog>, Fault> {
    let named = |m: &&Model| upstreams.iter().any(|u| u.name == m.provider);
    if let Some(m) = config.models.values().find(|m| !named(m)) {
        let names: Vec<&str> = upstreams.iter().map(|u| u.name.as_str()).collect();
        return Err(Fault::config(format!(
            "models: {}/{}: no provider '{}'; providers: [{}]",
            m.provider,
            m.id,
            m.provider,
            names.join(", ")
        )));
    }
    let catalog = Catalog::default();
    for u in upstreams {
        catalog.set(&u.name, u.models.iter().cloned());
    }
    Ok(Arc::new(catalog))
}

/// The routes to `upstreams`: by `rules`, then by default to `default`.
fn routes(upstreams: &[Upstream], rules: Vec<Rule>, default: Option<String>) -> Routes {
    let mut routes = Routes::new();
    for u in upstreams {
        let speaks = u.entries.iter().map(|e| Speaks {
            format: e.format.clone(),
            kind: e.kind,
            internal: e.internal.clone(),
        });
        routes = routes.provider(&u.name, speaks.collect());
        if let Some(format_for) = u.provider.vendor().format_for {
            routes = routes.format_for(&u.name, format_for);
        }
    }
    routes = rules.into_iter().fold(routes, Routes::rule);
    match default {
        Some(default) => routes.target(default),
        None => routes,
    }
}

/// The gateway with a core provider for each format of each configured
/// provider.
fn providers(
    config: &Config,
    formats: &[KnownFormat],
    limit: usize,
    sessions: Limits,
    network: &Network,
) -> Result<(Gateway, Vec<Upstream>), Fault> {
    let mut gateway = Gateway::new();
    let mut upstreams = Vec::new();
    for (name, p) in &config.providers {
        let owner = format!("provider '{name}'");
        let network = crate::network::of(&owner, network, &config.network, p.proxy.as_ref())?;
        let upstream = upstream(name, p, formats, network)?;
        let provider = upstream.provider.clone().limit(limit).sessions(sessions);
        gateway = provider
            .register(gateway)
            .map_err(|e| Fault::config(format!("provider '{name}': {}", e.message())))?;
        upstreams.push(upstream);
    }
    Ok((gateway, upstreams))
}

/// The formats of `serve`, in the order to register them: as listed, but
/// a format that claims some of its endpoints' requests before the rest,
/// which take the others. A session of Responses or a relay, routed by no
/// model, needs exactly one provider.
fn served<'a>(
    serve: &[String],
    upstreams: &[Upstream],
    formats: &'a [KnownFormat],
) -> Result<Vec<&'a KnownFormat>, Fault> {
    let after = serve.iter().skip_while(|n| *n != PASSTHROUGH).nth(1);
    if let Some(after) = after {
        return Err(Fault::config(format!(
            "formats: '{after}' comes after '{PASSTHROUGH}', which accepts every request first; \
             serve '{PASSTHROUGH}' last"
        )));
    }
    let mut served = Vec::new();
    for name in serve {
        let format = format_named(formats, name, "formats")?;
        served.push(format);
        // A session or a relay is not routed by its model: one provider
        // speaks it.
        if !matches!(format.kind, kind::RESPONSES | kind::PASSTHROUGH) {
            continue;
        }
        let speakers: Vec<&str> = upstreams
            .iter()
            .filter(|u| u.entries.iter().any(|e| e.format == *name))
            .map(|u| u.name.as_str())
            .collect();
        if speakers.len() != 1 {
            let mut message = format!("formats: {name} needs exactly one provider of that format");
            if speakers.len() > 1 {
                message.push_str(&format!("; {} all speak it", speakers.join(", ")));
            }
            return Err(Fault::config(message));
        }
    }
    served.sort_by_key(|f| !f.claims);
    Ok(served)
}

/// The providers' names, comma separated.
fn names(upstreams: &[Upstream]) -> String {
    let names: Vec<&str> = upstreams.iter().map(|u| u.name.as_str()).collect();
    names.join(", ")
}

/// The compiled-in format `name`; `owner` says who asked for it in an error.
fn format_named<'a>(
    formats: &'a [KnownFormat],
    name: &str,
    owner: &str,
) -> Result<&'a KnownFormat, Fault> {
    formats.iter().find(|f| f.name == name).ok_or_else(|| {
        let known: Vec<_> = formats.iter().map(|f| f.name.as_str()).collect();
        Fault::config(format!(
            "{owner}: unknown format '{name}'; compiled-in formats: [{}]",
            known.join(", ")
        ))
    })
}

/// Whether `name` may name a provider: routing reads `name/model`, video
/// ids `name~id`, and core providers `name:format`.
pub(crate) fn valid_provider_name(name: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    !name.is_empty() && name.chars().all(allowed)
}

/// The formats served when the config names none: OpenAI Chat, Anthropic
/// Messages, every other chat format some provider speaks, every client
/// format of a media or realtime kind some provider speaks, and Responses
/// over WebSocket if a provider speaks it; not passthrough, which takes
/// every request.
fn default_serve(upstreams: &[Upstream], formats: &[KnownFormat]) -> Vec<String> {
    let mut serve = vec!["openai-chat".to_owned(), "anthropic-messages".to_owned()];
    let entries = || upstreams.iter().flat_map(|u| &u.entries);
    let spoken = |kind: &'static str| entries().any(|e| e.kind == kind);
    let translated = formats
        .iter()
        .filter(|f| match f.kind {
            kind::CHAT => entries().any(|e| e.format == f.name),
            kind::MCP => true,
            k => (kind::ONE_SHOT.contains(&k) || k == kind::REALTIME) && spoken(k),
        })
        .filter(|f| f.servable());
    let sessions = entries().filter(|e| e.kind == kind::RESPONSES);
    let names = translated
        .map(|f| &f.name)
        .chain(sessions.map(|e| &e.format));
    for name in names {
        if !serve.contains(name) {
            serve.push(name.clone());
        }
    }
    serve
}

/// Whether a vendor puts `formats` at more than one base URL of its own.
fn at_different_urls(formats: &[(&dyn respan_gateway_vendors::Upstream, Option<&str>)]) -> bool {
    let mut urls = formats.iter().filter_map(|(_, url)| *url);
    let first = urls.next();
    urls.any(|url| Some(url) != first)
}

/// A provider's formats, each with its base URL and credentials.
fn upstream(
    name: &str,
    p: &ProviderConfig,
    formats: &[KnownFormat],
    network: Network,
) -> Result<Upstream, Fault> {
    let owner = format!("provider '{name}'");
    let bad = |e: String| Fault::config(format!("{owner}: {e}"));
    if !valid_provider_name(name) {
        return Err(bad(
            "a name is letters, digits, '_', '.' and '-' alone (no '/', '~' or ':')".into(),
        ));
    }
    let vendor = match &p.vendor {
        None => None,
        Some(vendor) => Some(respan_gateway_vendors::find(vendor).ok_or_else(|| {
            bad(format!(
                "unknown vendor '{vendor}'; vendors: [{}]",
                vendors::vendors().join(", ")
            ))
        })?),
    };
    let provider = match vendor {
        Some(vendor) => vendor.provider(name),
        None => VendorProvider::new(name),
    };
    let mut provider = provider.network(network);
    let names: Vec<&str> = match (&p.formats, vendor) {
        (Some(formats), _) => formats.names(),
        (None, Some(vendor)) => vendor.formats.iter().map(|(f, _)| f.name()).collect(),
        (None, None) => return Err(bad("needs a `vendor` or `formats`".into())),
    };
    if names.is_empty() {
        return Err(bad("has no formats".into()));
    }
    for format in &names {
        if format_named(formats, format, &owner)?.kind == kind::MCP {
            return Err(bad(
                "MCP servers are configured under `tools`, not `providers`".into(),
            ));
        }
    }
    let unique: BTreeSet<&&str> = names.iter().collect();
    if unique.len() != names.len() {
        return Err(bad("lists a format twice".into()));
    }
    // Each format as its vendor speaks it, at the vendor's URL for it, else
    // as its spec has it.
    let mut speaks = Vec::new();
    for format in &names {
        let found = respan_gateway_vendors::format(vendor, format);
        speaks.push(
            found.ok_or_else(|| bad(format!("speaks {format}, which no vendor or spec does")))?,
        );
    }
    let table = p.formats.as_ref().filter(|f| matches!(f, Formats::At(_)));
    match (&p.base_url, table) {
        (Some(_), Some(_)) => {
            return Err(bad(
                "give each format its base URL in `formats`, or one `base_url`, not both".into(),
            ));
        }
        // One URL for formats the vendor puts at different ones would send
        // some where they are not.
        (Some(_), None) if at_different_urls(&speaks) => {
            let each = speaks.iter().map(|(f, url)| {
                format!("  \"{}\" = \"{}\"", f.name(), url.unwrap_or("<base URL>"))
            });
            let each = each.collect::<Vec<_>>().join(",\n");
            return Err(bad(format!(
                "its formats are at different URLs, so one `base_url` cannot say where each is; \
                 give each its own:\nformats = {{\n{each},\n}}"
            )));
        }
        _ => {}
    }
    // Each format at the URL `formats` gives it, else at its vendor's, each
    // `{NAME}` in it from the environment; or, with one `base_url`, at that.
    let given = |format: &str| p.formats.as_ref().and_then(|f| f.url_of(format));
    let locate = vendor.map_or(respan_gateway_vendors::located as Locate, |v| v.locate);
    let mut at = Vec::new();
    for (format, default) in &speaks {
        let url = match (given(format.name()), default) {
            (Some(url), _) => Some(url.to_owned()),
            // One base URL, set below, is every format's.
            (None, _) if p.base_url.is_some() => None,
            (None, Some(url)) => Some(locate(url).map_err(|var| {
                bad(format!(
                    "environment variable {var} is not set; the base URL of {} needs it, or give \
                     it one",
                    format.name()
                ))
            })?),
            (None, None) => None,
        };
        at.push((*format, url));
    }
    provider = provider.formats(at);
    // One base URL is its API's, shared by every format.
    if let Some(url) = &p.base_url {
        provider = provider.base(url.clone());
    }
    // Where its own key comes from; none for the client's credentials.
    let key_from = match (&p.key, &p.auth, vendor) {
        (Some(_), Some(_), _) => return Err(bad("set `key` or `auth`, not both".into())),
        (Some(key), None, _) => {
            provider = provider.key(secret(&owner, key)?);
            Some(key_source(key))
        }
        (None, Some(auth), _) => {
            provider = provider.headers(auth_headers(name, auth)?);
            let (Auth::Bearer { key } | Auth::Header { key, .. }) = auth;
            Some(key_source(key))
        }
        (None, None, Some(vendor)) if !vendor.key_env.is_empty() => {
            let Some(key) = vendors::key_from_env(vendor.key_env) else {
                let var = vendor.key_env.join(" or ");
                return Err(bad(format!(
                    "environment variable {var} is not set; export it, put it in .env, or set `key`"
                )));
            };
            provider = provider.key(secret(&owner, &key)?);
            Some(key_source(&key))
        }
        _ => None,
    };
    let entries = provider.parts().map_err(|e| bad(e.message().into()))?;
    let entries = entries.into_iter().map(|part| {
        let format = part.format.name();
        Ok(Entry {
            format: format.to_owned(),
            kind: format_named(formats, format, &owner)?.kind,
            internal: part.name,
            base_url: part.base_url,
        })
    });
    Ok(Upstream {
        name: name.to_owned(),
        entries: entries.collect::<Result<_, Fault>>()?,
        own_key: key_from.is_some(),
        key_from: key_from.unwrap_or_else(|| "client credentials".to_owned()),
        models: Vec::new(),
        provider,
    })
}

/// Where a configured key comes from, for a person to read: never the key.
pub fn key_source(value: &str) -> String {
    match value.strip_prefix("env:") {
        Some(var) => format!("env {var}"),
        None => "config".to_owned(),
    }
}

/// `env:NAME` reads the environment variable; anything else is literal.
/// Either is trimmed, and must not be empty. `owner` says whose it is in
/// an error.
pub fn secret(owner: &str, value: &str) -> Result<String, Fault> {
    let (value, empty) = match value.strip_prefix("env:") {
        Some(var) => {
            let value = std::env::var(var).map_err(|_| {
                Fault::config(format!("{owner}: environment variable {var} is not set"))
            })?;
            (value, format!("environment variable {var} is empty"))
        }
        None => (value.to_owned(), "a key is empty".to_owned()),
    };
    match value.trim() {
        "" => Err(Fault::config(format!("{owner}: {empty}"))),
        trimmed => Ok(trimmed.to_owned()),
    }
}

/// The client keys, read, each with the name its requests are logged
/// under (its own, else `key-<n>` by its place in the list), whether it
/// reaches the admin API, and the tools it is limited to.
///
/// A key's `admin` is what its entry says. An entry that does not say is an
/// admin key unless some entry sets `admin: true`, when only those are.
fn clients(entries: &[ClientKey]) -> Result<Vec<Client>, Fault> {
    let named_admins = entries.iter().any(|e| e.admin() == Some(true));
    let mut clients: Vec<Client> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let key = secret("client_keys", entry.key())?;
        let name = match entry.name().map(str::trim) {
            Some("") => return Err(Fault::config("client_keys: a name is empty")),
            Some(name) => name.to_owned(),
            None => format!("key-{}", i + 1),
        };
        if clients.iter().any(|c| c.name == name) {
            return Err(Fault::config(format!(
                "client_keys: the name '{name}' is used twice"
            )));
        }
        // The first entry with a key would take every request with it.
        if let Some(first) = clients.iter().find(|c| c.key == key) {
            return Err(Fault::config(format!(
                "client_keys: '{name}' has the key of '{}'",
                first.name
            )));
        }
        let admin = entry.admin().unwrap_or(!named_admins);
        let tools = entry.tools().map(<[String]>::to_vec);
        if let Some(tools) = &tools {
            let owner = format!("client_keys: '{name}': tools");
            crate::mcp::check_patterns(&owner, tools).map_err(Fault::config)?;
        }
        clients.push(Client {
            name,
            key,
            admin,
            tools,
        });
    }
    Ok(clients)
}

/// The header of `auth`, its key read.
pub fn auth_headers(provider: &str, auth: &Auth) -> Result<HeaderMap, Fault> {
    let read = |key: &str| secret(&format!("provider '{provider}'"), key);
    match auth {
        Auth::Bearer { key } => {
            one_header(provider, AUTHORIZATION, &format!("Bearer {}", read(key)?))
        }
        Auth::Header { name, key } => {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| Fault::config(format!("provider '{provider}': {e}")))?;
            one_header(provider, name, &read(key)?)
        }
    }
}

fn one_header(provider: &str, name: HeaderName, value: &str) -> Result<HeaderMap, Fault> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|e| Fault::config(format!("provider '{provider}': {e}")))?;
    value.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(name, value);
    Ok(headers)
}

#[cfg(test)]
mod tests;
