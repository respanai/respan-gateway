//! MCP servers behind the gateway: `tools` in the config names them, and
//! clients reach each one over streamable HTTP at `/mcp/{name}`, behind the
//! gateway's client keys. An upstream is a streamable-HTTP server (`url`) or
//! a child process speaking MCP on stdio (`command`); either way its
//! credentials stay on the gateway, and a client's are not sent on unless the
//! server says `auth = "forward"`.
//!
//! A stdio server is not a daemon: the transport starts one child per
//! client session, when the session `initialize`s, and ends it with the
//! session (a client's `DELETE`, ten idle minutes, or the child exiting).
//! A child that dies fails its session's requests; the client starts a new
//! session and so a fresh child, as MCP asks of a client whose session ended.
//! At shutdown the children are killed with the runtime.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use respan_gateway_core::futures::future::join_all;
use respan_gateway_core::{Fault, Gateway, Provider};
use respan_gateway_http::http::header::{self, HeaderName, HeaderValue};
use respan_gateway_http::http::{self, HeaderMap, StatusCode};
use respan_gateway_http::{Body, Completion};
use respan_gateway_http_transport::reqwest;
use respan_gateway_http_transport::{HttpTransport, Network, unreachable};
use respan_gateway_mcp::{Input, Output};
use respan_gateway_mcp_http::{StdioTransport, StreamableHttp};
use respan_gateway_plugin_routing::Model;
use respan_gateway_tool_vendors::{ToolServer, ToolVendor};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use crate::compose::{key_source, secret, valid_provider_name};
use crate::config::Config;
use crate::formats::{KnownFormat, kind};
use crate::mcp_oauth::{OAuth, OAuthConfig};
use crate::models::{Probe, Reach};
use crate::network::Proxy;
use crate::vendors::origin;

/// The format's name, as the request log shows it.
pub use respan_gateway_mcp_http::FORMAT;
/// Where clients reach a server: `/mcp/{name}`.
const MOUNT: &str = "/mcp";
/// The most client sessions, so child processes, one server holds at once.
const SESSIONS: usize = 32;

/// One MCP server, as `gateway.toml` writes it under `tools`, in the fields
/// MCP clients' `mcp.json` use, so an entry copies across: a `url`
/// (streamable HTTP) with its `headers` and `auth`, or a `command` (stdio)
/// with its `args`, `env` and `cwd`.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// A streamable-HTTP server's endpoint, such as
    /// `https://api.githubcopilot.com/mcp/`.
    pub url: Option<String>,
    /// Headers sent to it, values literal or `env:NAME`, such as a key in a
    /// header of its own: `{ "x-api-key" = "env:KEY" }`.
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// How the gateway authenticates to it, if not by `headers`.
    pub auth: Option<McpAuth>,
    /// A stdio server's program, run once per client session.
    pub command: Option<Program>,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Its environment, values literal or `env:NAME`. It gets nothing else
    /// of the gateway's but `PATH` and its proxy (`child_env`).
    #[serde(default)]
    pub env: IndexMap<String, String>,
    /// The directory it runs in.
    pub cwd: Option<PathBuf>,
    /// A tool vendor's id, such as `linkup`: an HTTP API whose tools the
    /// gateway serves as an MCP server in process.
    pub vendor: Option<String>,
    /// A tool vendor's key, literal or `env:NAME`; its own variable if
    /// unset. On a `url` or `command`, a retired field, refused with how.
    pub key: Option<String>,
    /// A tool vendor's base URL, if not its own.
    pub base_url: Option<String>,
    /// Its proxy, in place of `network`'s: a URL, literal or `env:NAME`, or
    /// `false` for none. A `command` is given it as `HTTPS_PROXY` and the
    /// rest.
    pub proxy: Option<Proxy>,
    // The fields this block once had, refused with how to write them now.
    #[serde(default)]
    oauth: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    forward_auth: Option<serde::de::IgnoredAny>,
}

// Its headers, auth, arguments and environment are secrets: not printed.
impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            // Its origin alone: a path can hold a token (`/mcp/<token>`).
            .field("url", &self.url.as_deref().map(origin))
            .field("vendor", &self.vendor)
            .finish_non_exhaustive()
    }
}

impl McpConfig {
    /// The tool vendor `id`, with its key from the environment.
    pub fn tool_vendor(id: &str) -> Self {
        Self {
            vendor: Some(id.to_owned()),
            ..Self::default()
        }
    }
}

/// How the gateway authenticates to an HTTP MCP server.
#[derive(Clone)]
pub enum McpAuth {
    /// `{ bearer = "env:KEY" }`: a key, as `Authorization: Bearer`.
    Bearer(String),
    /// `{ oauth = { token_url, client_id, client_secret, .. } }`: the
    /// gateway's own OAuth client, which gets its tokens from the endpoint.
    OAuth(OAuthConfig),
    /// `"login"`: the login `respan-gateway mcp login <name>` saved.
    Login,
    /// `"forward"`: each client's own credentials, passed on.
    Forward,
}

// Its key and OAuth client are secrets: only which it is is printed.
impl std::fmt::Debug for McpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            McpAuth::Bearer(_) => "Bearer",
            McpAuth::OAuth(_) => "OAuth",
            McpAuth::Login => "Login",
            McpAuth::Forward => "Forward",
        })
    }
}

impl<'de> Deserialize<'de> for McpAuth {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Auth;
        impl<'de> serde::de::Visitor<'de> for Auth {
            type Value = McpAuth;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("\"login\", \"forward\", { bearer = \"...\" } or { oauth = { ... } }")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<McpAuth, E> {
                match v {
                    "login" => Ok(McpAuth::Login),
                    "forward" => Ok(McpAuth::Forward),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(v), &self)),
                }
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut m: M,
            ) -> Result<McpAuth, M::Error> {
                use serde::de::Error;
                let Some(kind) = m.next_key::<String>()? else {
                    return Err(M::Error::invalid_length(0, &self));
                };
                let auth = match kind.as_str() {
                    "bearer" => McpAuth::Bearer(m.next_value()?),
                    "oauth" => McpAuth::OAuth(m.next_value()?),
                    "type" | "name" | "key" => {
                        return Err(M::Error::custom(
                            "`auth = { type = .. }` is now `auth = { bearer = \"KEY\" }`, \
                             or a key in a header of its own in `headers`",
                        ));
                    }
                    other => return Err(M::Error::unknown_field(other, &["bearer", "oauth"])),
                };
                match m.next_key::<String>()? {
                    Some(more) => Err(M::Error::custom(format!(
                        "`auth` is one of bearer or oauth, not also `{more}`"
                    ))),
                    None => Ok(auth),
                }
            }
        }
        d.deserialize_any(Auth)
    }
}

/// A stdio server's program alone, as `mcp.json` writes it.
#[derive(Clone, Debug)]
pub struct Program(String);

impl Program {
    /// The program, as configured.
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Program {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Name;
        impl<'de> serde::de::Visitor<'de> for Name {
            type Value = Program;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a program, such as \"npx\"")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Program, E> {
                Ok(Program(v.to_owned()))
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(self, _: S) -> Result<Program, S::Error> {
                Err(serde::de::Error::custom(
                    "`command` is the program alone; its arguments go in `args`, as in mcp.json",
                ))
            }
        }
        d.deserialize_any(Name)
    }
}

/// A checked server.
#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub transport: Transport,
    /// Whether it sends a key of its own.
    pub own_key: bool,
    /// Where its key comes from, never the key.
    pub key_from: String,
    pub forward_auth: bool,
    /// Its token source, if it has OAuth.
    pub oauth: Option<Arc<OAuth>>,
    /// What it is reached through: its proxy, the certificates trusted.
    pub network: Network,
}

#[derive(Clone)]
pub enum Transport {
    Http {
        /// As configured.
        url: String,
        /// `scheme://host[:port]`, and the path a call is sent to.
        origin: String,
        path: String,
        auth: HeaderMap,
    },
    Stdio {
        command: Vec<String>,
        /// The program as found: on Windows `npx` is `npx.cmd`.
        program: PathBuf,
        env: Vec<(String, String)>,
        cwd: Option<PathBuf>,
    },
    /// A tool vendor, served in process.
    Vendor {
        vendor: &'static ToolVendor,
        key: String,
        /// Its base URL, if not its own.
        base: Option<String>,
    },
}

// What it reaches and runs are not printed beyond `target`: a URL, an
// argument, the environment, the headers and a key can hold tokens.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http { origin, .. } => write!(f, "Http({origin})"),
            Self::Stdio { program, .. } => write!(f, "Stdio({})", program.display()),
            Self::Vendor { vendor, .. } => write!(f, "Vendor({})", vendor.id),
        }
    }
}

impl McpServer {
    /// `http`, `stdio` or `vendor`.
    pub fn transport_label(&self) -> &'static str {
        match self.transport {
            Transport::Http { .. } => "http",
            Transport::Stdio { .. } => "stdio",
            Transport::Vendor { .. } => "vendor",
        }
    }

    /// What it runs or reaches, for a person to read and for the admin API:
    /// an HTTP server's origin (`scheme://host[:port]`) and a stdio
    /// server's program name. Never the path or the arguments, which hold
    /// tokens (`/mcp/<token>`, `--token X`), nor the environment.
    pub fn target(&self) -> String {
        match &self.transport {
            Transport::Http { origin, .. } => origin.clone(),
            Transport::Stdio { command, .. } => {
                let program = command.first().map_or("", String::as_str);
                let name = Path::new(program).file_name();
                name.map_or(program, |n| n.to_str().unwrap_or(program))
                    .to_owned()
            }
            Transport::Vendor { vendor, base, .. } => {
                let base = base.as_deref().unwrap_or(vendor.base_url);
                format!("{} ({})", vendor.id, origin(base))
            }
        }
    }

    /// Where clients reach it.
    pub fn endpoint(&self) -> String {
        format!("{MOUNT}/{}", self.name)
    }
}

/// The servers `config` names, checked: names, transports, keys read, and
/// the programs found; each reached through `network`, the config's, or
/// a `proxy` of its own.
pub fn servers(
    config: &Config,
    network: &Network,
    providers: &[&str],
    logins: &std::path::Path,
) -> Result<Vec<McpServer>, Fault> {
    (config.tools)
        .iter()
        .map(|(name, c)| {
            let bad = |e: &str| Fault::config(format!("tool server '{name}': {e}"));
            if !valid_provider_name(name) || name.chars().all(|c| c == '.') {
                return Err(bad("a name is letters, digits, '_', '.' and '-' alone"));
            }
            if providers.contains(&name.as_str()) {
                return Err(bad("is also the name of a provider; names are shared"));
            }
            let owner = format!("tool server '{name}'");
            let reach = crate::network::of(&owner, network, &config.network, c.proxy.as_ref())?;
            let mut server = server(name, c, logins, &reach)?;
            if let Transport::Stdio { env, .. } = &mut server.transport {
                let proxy = crate::network::child_env(&owner, &config.network, c.proxy.as_ref());
                // Its own `env` says last.
                let own = |k: &String| env.iter().any(|(set, _)| set == k);
                let proxy: Vec<_> = proxy?.into_iter().filter(|(k, _)| !own(k)).collect();
                env.splice(0..0, proxy);
            }
            Ok(server)
        })
        .collect()
}

/// The servers `/mcp` fronts, in their config order: all but those that
/// pass on a client's credentials (`auth = "forward"`), as its sessions are
/// the gateway's own. None turns `/mcp` off.
pub fn fronted(servers: &[McpServer]) -> Vec<String> {
    (servers.iter().filter(|s| !s.forward_auth))
        .map(|s| s.name.clone())
        .collect()
}

/// Patterns of tool names as `/mcp` has them, `{server}__{name}`, with `*`
/// for any characters: none empty or with a space.
pub fn check_patterns(key: &str, patterns: &[String]) -> Result<(), String> {
    match patterns
        .iter()
        .any(|p| p.is_empty() || p.contains(char::is_whitespace))
    {
        true => Err(format!(
            "{key}: a pattern is a tool name, with `*` for any characters"
        )),
        false => Ok(()),
    }
}

fn server(
    name: &str,
    c: &McpConfig,
    logins: &std::path::Path,
    network: &Network,
) -> Result<McpServer, Fault> {
    if let Some(id) = &c.vendor {
        return vendor_server(name, c, id, network);
    }
    let owner = format!("tool server '{name}'");
    let bad = |e: &str| Fault::config(format!("{owner}: {e}"));
    if c.base_url.is_some() {
        return Err(bad(
            "`base_url` is for a `vendor`; a `url` is the whole endpoint",
        ));
    }
    let moved = [
        (
            c.key.is_some(),
            "`key` is now `auth = { bearer = \"KEY\" }`",
        ),
        (
            c.oauth.is_some(),
            "`oauth = { .. }` is now `auth = { oauth = { .. } }`",
        ),
        (
            c.forward_auth.is_some(),
            "`forward_auth = true` is now `auth = \"forward\"`",
        ),
    ];
    if let Some((_, how)) = moved.iter().find(|(set, _)| *set) {
        return Err(bad(how));
    }
    let (transport, own_key, key_from, oauth) = match (&c.url, &c.command) {
        (Some(_), Some(_)) => return Err(bad("set `url` or `command`, not both")),
        (None, None) => return Err(bad("needs a `url` (HTTP) or a `command` (stdio)")),
        (Some(url), None) => {
            if !c.args.is_empty() || !c.env.is_empty() || c.cwd.is_some() {
                return Err(bad("`args`, `env` and `cwd` are for a `command`"));
            }
            let mut auth = headers(&owner, &c.headers)?;
            let given = |kind: &str| match c.headers.is_empty() {
                true => kind.to_owned(),
                false => format!("{kind} and headers"),
            };
            let (oauth, key_from) = match &c.auth {
                None if c.headers.is_empty() => (None, "none".to_owned()),
                None => (None, "headers".to_owned()),
                Some(McpAuth::Bearer(key)) => {
                    let value = secret(&format!("{owner}: auth bearer"), key)?;
                    let bearer = HeaderValue::from_str(&format!("Bearer {value}"));
                    let mut bearer = bearer.map_err(|e| bad(&format!("auth bearer: {e}")))?;
                    bearer.set_sensitive(true);
                    auth.insert(header::AUTHORIZATION, bearer);
                    (None, given(&format!("bearer ({})", key_source(key))))
                }
                Some(McpAuth::OAuth(o)) => {
                    let o = OAuth::new(&owner, o, network)?;
                    let from = given(&format!("oauth {} ({})", o.grant(), o.source));
                    (Some(Arc::new(o)), from)
                }
                Some(McpAuth::Login) => {
                    let path = logins.join(format!("{name}.json"));
                    let o = OAuth::login(&owner, name, &path, network)?;
                    let from = given(&format!("oauth login ({})", o.source));
                    (Some(Arc::new(o)), from)
                }
                Some(McpAuth::Forward) => (None, given("the client's own")),
            };
            if c.auth.is_some()
                && !matches!(c.auth, Some(McpAuth::Forward))
                && c.headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("authorization"))
            {
                return Err(bad(
                    "an `Authorization` header and `auth` both authenticate; set one",
                ));
            }
            // Parsed as the transport will: a bad URL fails here, not on a call.
            let url = url.trim();
            HttpTransport::new(origin(url)).map_err(|e| bad(e.message()))?;
            let path = &url[origin(url).len()..];
            if path.contains(['?', '#']) {
                return Err(bad("a `url` has no query or fragment"));
            }
            let path = if path.is_empty() { "/" } else { path };
            let (origin, path) = (origin(url).to_owned(), path.to_owned());
            let own = !auth.is_empty() || oauth.is_some();
            let t = Transport::Http {
                url: url.to_owned(),
                origin,
                path,
                auth,
            };
            (t, own, key_from, oauth)
        }
        (None, Some(Program(program))) => {
            if !c.headers.is_empty() || c.auth.is_some() {
                return Err(bad("`headers` and `auth` are for a `url`"));
            }
            if program.trim().is_empty() {
                return Err(bad("`command` is empty"));
            }
            let Some(found) = find_program(program) else {
                return Err(bad(&format!("command '{program}' was not found on PATH")));
            };
            if let Some(dir) = &c.cwd
                && !dir.is_dir()
            {
                let why = match dir.exists() {
                    true => "is not a directory",
                    false => "does not exist",
                };
                return Err(bad(&format!("cwd '{}' {why}", dir.display())));
            }
            let env = c
                .env
                .iter()
                .map(|(k, v)| Ok((k.clone(), secret(&format!("{owner}: env {k}"), v)?)))
                .collect::<Result<_, Fault>>()?;
            let mut command = vec![program.clone()];
            command.extend(c.args.iter().cloned());
            let t = Transport::Stdio {
                command,
                program: found,
                env,
                cwd: c.cwd.clone(),
            };
            (t, false, "none (a child process)".to_owned(), None)
        }
    };
    Ok(McpServer {
        name: name.to_owned(),
        transport,
        own_key,
        key_from,
        forward_auth: matches!(c.auth, Some(McpAuth::Forward)),
        oauth,
        network: network.clone(),
    })
}

/// A `vendor` server: the tool vendor, its key (its own variable's if
/// `key` is unset) and any `base_url`, and nothing of a `url` or `command`.
fn vendor_server(
    name: &str,
    c: &McpConfig,
    id: &str,
    network: &Network,
) -> Result<McpServer, Fault> {
    let owner = format!("tool server '{name}'");
    let bad = |e: &str| Fault::config(format!("{owner}: {e}"));
    if c.url.is_some() || c.command.is_some() {
        return Err(bad("set one of `vendor`, `url` and `command`"));
    }
    let other = !c.headers.is_empty()
        || c.auth.is_some()
        || !c.args.is_empty()
        || !c.env.is_empty()
        || c.cwd.is_some()
        || c.oauth.is_some()
        || c.forward_auth.is_some();
    if other {
        return Err(bad(
            "a `vendor` takes `key` and `base_url` alone, not `headers`, `auth`, \
             `args`, `env` or `cwd`",
        ));
    }
    let Some(vendor) = find_tool_vendor(id) else {
        let known: Vec<&str> = respan_gateway_tool_vendors::ALL
            .iter()
            .map(|v| v.id)
            .collect();
        return Err(bad(&format!(
            "unknown tool vendor '{id}'; the tool vendors are [{}]",
            known.join(", ")
        )));
    };
    let key = match &c.key {
        Some(key) => Some(key.clone()),
        None => crate::vendors::key_from_env(vendor.key_env),
    };
    let (key, key_from) = match key {
        Some(key) => (secret(&format!("{owner}: key"), &key)?, key_source(&key)),
        None if vendor.key_env.is_empty() => (String::new(), "none".to_owned()),
        None => {
            return Err(bad(&format!(
                "tool vendor '{id}' needs a key: set {} or `key`",
                vendor.key_env.join(" or ")
            )));
        }
    };
    let base = match &c.base_url {
        Some(url) => {
            let url = url.trim().trim_end_matches('/');
            HttpTransport::new(origin(url))
                .map_err(|e| bad(&format!("base_url: {}", e.message())))?;
            Some(url.to_owned())
        }
        None => None,
    };
    Ok(McpServer {
        name: name.to_owned(),
        own_key: !key.is_empty(),
        transport: Transport::Vendor { vendor, key, base },
        key_from,
        forward_auth: false,
        oauth: None,
        network: network.clone(),
    })
}

/// The tool vendor `id`; in tests, also one of their own.
fn find_tool_vendor(id: &str) -> Option<&'static ToolVendor> {
    #[cfg(test)]
    if id == tests::FAKE.id {
        return Some(&tests::FAKE);
    }
    respan_gateway_tool_vendors::find(id)
}

/// `headers`, each value literal or `env:NAME`, marked sensitive: a header
/// of a server is as likely a key as not.
fn headers(owner: &str, headers: &IndexMap<String, String>) -> Result<HeaderMap, Fault> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let bad = |e: String| Fault::config(format!("{owner}: header '{name}': {e}"));
        let name =
            HeaderName::from_bytes(name.trim().as_bytes()).map_err(|e| bad(e.to_string()))?;
        let value = secret(&format!("{owner}: header {name}"), value)?;
        let mut value = HeaderValue::from_str(&value).map_err(|e| bad(e.to_string()))?;
        value.set_sensitive(true);
        map.insert(name, value);
    }
    Ok(map)
}

/// The file `program` names, or finds on the gateway's `PATH`, which a
/// child is given. On Windows a program is also found with an extension of
/// `PATHEXT` (`npx` is `npx.cmd`).
fn find_program(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let pathext = match cfg!(windows) {
        true => std::env::var("PATHEXT").ok(),
        false => None,
    };
    let exts = pathext_list(pathext.as_deref());
    resolve(program, &dirs, &exts)
}

/// The extensions `PATHEXT` lists, or the usual ones if it is unset; none
/// where it is not used (`None` is an unset one only on Windows).
fn pathext_list(pathext: Option<&str>) -> Vec<String> {
    if !cfg!(windows) && pathext.is_none() {
        return Vec::new();
    }
    let list = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
    let exts = list.split(';').map(str::trim).filter(|e| !e.is_empty());
    exts.map(str::to_owned).collect()
}

/// `program` as a file in one of `dirs` (itself, if it has a directory), as
/// it is or with one of `exts` added, in that order.
fn resolve(program: &str, dirs: &[PathBuf], exts: &[String]) -> Option<PathBuf> {
    let candidates = |base: PathBuf| -> Option<PathBuf> {
        let name = base.as_os_str().to_owned();
        let with = exts.iter().map(|ext| {
            let mut name = name.clone();
            name.push(ext);
            PathBuf::from(name)
        });
        std::iter::once(base).chain(with).find(|p| p.is_file())
    };
    if program.contains(std::path::is_separator) {
        return candidates(PathBuf::from(program));
    }
    dirs.iter().find_map(|dir| candidates(dir.join(program)))
}

/// How long a server has to answer a probe's `initialize`.
const PROBE: Duration = Duration::from_secs(5);

/// The `initialize` a probe sends: a client that only looks.
fn hello() -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "respan-gateway-check", "version": env!("CARGO_PKG_VERSION")}
        }
    })
    .to_string()
}

/// `check --online` for MCP servers: each is asked to `initialize`, as a
/// client would, and the session it opens is ended; a stdio server is
/// started for it and stopped. No key and no URL is in the result.
pub async fn probe(servers: &[McpServer]) -> Vec<Probe> {
    join_all(servers.iter().map(|s| async move {
        let started = Instant::now();
        let reach = match timeout(PROBE, probe_one(s)).await {
            Ok(reach) => reach,
            Err(_) => Reach::Unreachable("it did not answer `initialize` in time".to_owned()),
        };
        Probe {
            provider: format!("mcp/{}", s.name),
            reach,
            took: started.elapsed(),
        }
    }))
    .await
}

async fn probe_one(server: &McpServer) -> Reach {
    match &server.transport {
        Transport::Http { url, auth, .. } => {
            probe_http(url, auth, server.oauth.as_deref(), &server.network).await
        }
        Transport::Stdio {
            command,
            program,
            env,
            cwd,
        } => probe_stdio(command, program, env, cwd.as_deref()).await,
        // Served in process: there is nothing to reach until a tool is
        // called, which would spend the vendor's credits.
        Transport::Vendor { .. } => Reach::Reachable,
    }
}

async fn probe_http(
    url: &str,
    auth: &HeaderMap,
    oauth: Option<&OAuth>,
    network: &Network,
) -> Reach {
    let mut auth = auth.clone();
    if let Some(oauth) = oauth {
        let sent = async { OAuth::header(&oauth.token(None).await?) }.await;
        match sent {
            Ok(value) => {
                auth.insert(http::header::AUTHORIZATION, value);
            }
            Err(fault) => return Reach::Unreachable(fault.message().to_owned()),
        }
    }
    let probe = |b: reqwest::ClientBuilder| Ok(b.redirect(reqwest::redirect::Policy::none()));
    let client = match network.client("MCP probe", probe) {
        Ok(client) => client,
        Err(fault) => return Reach::Unreachable(fault.message().to_owned()),
    };
    let asked = client
        .post(url)
        .headers(auth.clone())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(hello())
        .send()
        .await;
    let resp = match asked {
        Ok(resp) => resp,
        Err(e) => return Reach::Unreachable(unreachable(&e)),
    };
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Reach::Unauthorized(status.as_u16());
    }
    if !status.is_success() {
        return Reach::Unreachable(format!("it answered {status} to `initialize`"));
    }
    if let Some(session) = resp.headers().get("mcp-session-id") {
        // Courtesy: the session is not left to idle out. Its answer is not
        // the probe's.
        let _ = client
            .delete(url)
            .headers(auth.clone())
            .header("mcp-session-id", session)
            .send()
            .await;
    }
    Reach::Reachable
}

async fn probe_stdio(
    command: &[String],
    program: &Path,
    env: &[(String, String)],
    cwd: Option<&Path>,
) -> Reach {
    let args = command.get(1..).unwrap_or_default();
    let mut child = tokio::process::Command::new(program);
    child
        .args(args)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        child.env("PATH", path);
    }
    #[cfg(windows)]
    {
        // As a session starts it: its own process group, and what a
        // program cannot start without.
        child.creation_flags(0x0000_0200);
        for name in ["SystemRoot", "ComSpec", "PATHEXT", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                child.env(name, value);
            }
        }
    }
    if let Some(dir) = cwd {
        child.current_dir(dir);
    }
    let Ok(mut child) = child.spawn() else {
        return Reach::Unreachable("the program could not be started".to_owned());
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Reach::Unreachable("the program has no stdio".to_owned());
    };
    let line = format!("{}\n", hello());
    let wrote = stdin.write_all(line.as_bytes()).await;
    let wrote = match wrote {
        Ok(()) => stdin.flush().await,
        Err(e) => Err(e),
    };
    let mut answer = String::new();
    let read = match wrote {
        Ok(()) => BufReader::new(stdout).read_line(&mut answer).await,
        Err(e) => Err(e),
    };
    // Stopped as a session's end stops it: stdin closed, then killed.
    drop(stdin);
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        // The program and what it started, as a session's end stops them.
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
    let answered = serde_json::from_str::<serde_json::Value>(&answer)
        .ok()
        .is_some_and(|m| m.get("result").is_some());
    match (read, answered) {
        (Ok(n), true) if n > 0 => Reach::Reachable,
        (Ok(0) | Err(_), _) => Reach::Unreachable("the program exited before answering".to_owned()),
        _ => Reach::Unreachable("it did not answer `initialize` with a result".to_owned()),
    }
}

/// The MCP format: clients of every server in `servers`, and the servers
/// themselves as providers, registered when it is served. `limit` caps what
/// the gateway holds of one HTTP answer.
pub fn known(servers: Vec<McpServer>, aggregate: bool, limit: usize) -> KnownFormat {
    let client = StreamableHttp::new(MOUNT).serving(servers.iter().map(|s| s.name.clone()));
    let client = if aggregate {
        client.with_root()
    } else {
        client
    };
    let mut endpoints = vec![
        format!("POST {MOUNT}/{{name}}"),
        format!("GET {MOUNT}/{{name}}"),
        format!("DELETE {MOUNT}/{{name}}"),
    ];
    if aggregate {
        endpoints.extend(["POST", "GET", "DELETE"].map(|m| format!("{m} {MOUNT} (all servers)")));
    }
    let endpoints: Vec<&str> = endpoints.iter().map(String::as_str).collect();
    let serve_client = client.clone();
    KnownFormat::of(kind::MCP, &endpoints, client)
        .model(model())
        .serving(move |gateway| register(gateway, &servers, &serve_client, limit))
}

/// `client`'s calls, with the server's `oauth` token if it has one, a server that cannot be reached
/// named in the fault with its host and the cause: the transport's own
/// message names neither.
fn reached(
    client: HttpTransport,
    server: &str,
    url: &str,
    limit: usize,
    oauth: Option<Arc<OAuth>>,
) -> impl Fn(respan_gateway_core::Extensions) -> respan_gateway_http::ProviderIo + Send + Sync + use<>
{
    let client = Arc::new(client);
    let host = origin(url).split_once("://").map_or("", |(_, host)| host);
    let who: Arc<str> = format!("provider '{server}' at {host}").into();
    respan_gateway_http::connect(
        move |call| {
            let (client, who, oauth) = (client.clone(), who.clone(), oauth.clone());
            async move {
                let sent = authorized(oauth.as_deref(), call, |call| client.send(call)).await;
                sent.map_err(|fault| match fault.cause::<reqwest::Error>() {
                    Some(e) => {
                        Fault::upstream(format!("{who} {}", unreachable(e))).with_source(fault)
                    }
                    None => fault,
                })
            }
        },
        limit,
    )
}

/// `gateway` with each server as a provider, and the client format.
fn register(
    mut gateway: Gateway,
    servers: &[McpServer],
    client: &StreamableHttp,
    limit: usize,
) -> Result<Gateway, Fault> {
    for s in servers {
        let name = s.name.clone();
        gateway = match &s.transport {
            Transport::Http {
                origin, path, auth, ..
            } => {
                let transport = HttpTransport::new(origin)?
                    .with_network(&s.network)?
                    .with_headers(auth.clone())?;
                let io = reached(transport, &s.name, origin, limit, s.oauth.clone());
                gateway.provider(Provider::new(name, StreamableHttp::new(path), io))?
            }
            Transport::Stdio {
                command,
                program,
                env,
                cwd,
            } => {
                let args = command.get(1..).unwrap_or_default();
                let mut child = StdioTransport::new(program, args).max_total(SESSIONS);
                for (key, value) in env {
                    child = child.env(key, value);
                }
                if let Some(dir) = cwd {
                    child = child.current_dir(dir);
                }
                gateway.provider(Provider::new(
                    name,
                    StreamableHttp::new("/"),
                    child.connect(),
                ))?
            }
            Transport::Vendor { vendor, key, base } => {
                let api = vendor.api(key.clone(), base.as_deref());
                let api = api.with_network(&s.network)?;
                let server = ToolServer::new(vendor, api);
                gateway.provider(Provider::new(
                    name,
                    StreamableHttp::new("/"),
                    server.connect(limit),
                ))?
            }
        };
    }
    gateway.client::<Input, Output, _>(client.clone())
}

/// Sends `call` with `send`, with the server's `oauth` token if it has
/// one. A `401` drops the token, and the call is sent once more with a
/// new one, if its body is whole to send again.
async fn authorized<F>(
    oauth: Option<&OAuth>,
    call: http::Request<Body>,
    send: impl Fn(http::Request<Body>) -> F,
) -> Result<(http::Response<Body>, Option<Completion>), Fault>
where
    F: Future<Output = Result<(http::Response<Body>, Option<Completion>), Fault>>,
{
    let Some(oauth) = oauth else {
        return send(call).await;
    };
    let (parts, body) = call.into_parts();
    let whole = body.full_bytes().cloned();
    let with = |token: &str, body: Body| -> Result<http::Request<Body>, Fault> {
        let mut call = http::Request::new(body);
        *call.method_mut() = parts.method.clone();
        *call.uri_mut() = parts.uri.clone();
        *call.version_mut() = parts.version;
        *call.headers_mut() = parts.headers.clone();
        *call.extensions_mut() = parts.extensions.clone();
        call.headers_mut()
            .insert(http::header::AUTHORIZATION, OAuth::header(token)?);
        Ok(call)
    };
    let token = oauth.token(None).await?;
    let (resp, completion) = send(with(&token, body)?).await?;
    let Some(bytes) = whole.filter(|_| resp.status() == StatusCode::UNAUTHORIZED) else {
        return Ok((resp, completion));
    };
    drop((resp, completion));
    let fresh = oauth.token(Some(&token)).await?;
    send(with(&fresh, Body::full(bytes))?).await
}

/// What a request does, for the request log in place of a model: its
/// method, and the tool, prompt or resource it names.
fn model() -> Model {
    Model::new(|cx| cx.input::<Input>().ok().map(label), |_, _| Ok(()))
}

fn label(input: &Input) -> String {
    let Some(message) = input.messages.first() else {
        // A `GET` opens the server's stream; a `DELETE` ends the session.
        return input.method.to_string();
    };
    let Some(method) = message.method() else {
        return "response".to_owned();
    };
    let params = message.params();
    let named = |field: &str| params.and_then(|p| p.get(field)?.as_str());
    let subject = match method {
        "tools/call" | "prompts/get" => named("name"),
        "resources/read" | "resources/subscribe" | "resources/unsubscribe" => named("uri"),
        _ => None,
    };
    match subject {
        Some(subject) => format!("{method} {subject}"),
        None => method.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<McpServer>, String> {
        let tools: IndexMap<String, McpConfig> = toml::from_str(text).map_err(|e| e.to_string())?;
        let config = Config {
            tools,
            ..Config::default()
        };
        let logins = std::path::Path::new(".respan/mcp");
        servers(&config, Network::shared(), &["openai"], logins).map_err(|e| e.message().to_owned())
    }

    /// A tool vendor of the tests' own, with no tools.
    pub(super) static FAKE: ToolVendor = ToolVendor {
        id: "fake-tools",
        key_env: &["RESPAN_TEST_NO_SUCH_TOOL_KEY"],
        key_in: respan_gateway_tool_vendors::KeyIn::Bearer,
        base_url: "https://tools.test/v1",
        tools: &NoTools,
    };

    struct NoTools;

    #[respan_gateway_core::async_trait]
    impl respan_gateway_tool_vendors::Tools for NoTools {
        fn list(&self) -> &'static [respan_gateway_tool_vendors::ToolSpec] {
            &[]
        }
        async fn call(
            &self,
            _: &respan_gateway_tool_vendors::Api,
            name: &str,
            _: serde_json::Value,
        ) -> Result<respan_gateway_tool_vendors::ToolOutput, respan_gateway_tool_vendors::ToolError>
        {
            Err(respan_gateway_tool_vendors::ToolError::new(format!(
                "no tool {name}"
            )))
        }
    }

    #[test]
    fn a_server_is_a_tool_vendor() {
        let ok = parse("s = { vendor = \"fake-tools\", key = \"sk-t0ken\" }")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ok[0].transport_label(), "vendor");
        assert_eq!(ok[0].target(), "fake-tools (https://tools.test)");
        assert!(ok[0].own_key && !ok[0].forward_auth);
        assert_eq!(ok[0].key_from, "config");
        assert!(!format!("{:?}", ok[0]).contains("sk-t0ken"));
        let moved = parse(
            "s = { vendor = \"fake-tools\", key = \"k\", base_url = \"http://local.test:9/v1/\" }",
        );
        let moved = moved.unwrap_or_else(|e| panic!("{e}"));
        let Transport::Vendor { base, key, .. } = &moved[0].transport else {
            panic!("vendor")
        };
        assert_eq!(
            (base.as_deref(), key.as_str()),
            (Some("http://local.test:9/v1"), "k")
        );
    }

    #[test]
    fn tool_vendor_mistakes_are_named() {
        let said = |text: &str| parse(text).err().unwrap_or_default();
        assert!(
            said("s = { vendor = \"nope\", key = \"k\" }").contains("unknown tool vendor 'nope'")
        );
        assert!(
            said("s = { vendor = \"fake-tools\", key = \"k\", url = \"http://h.test\" }")
                .contains("one of `vendor`, `url` and `command`")
        );
        assert!(
            said("s = { vendor = \"fake-tools\", key = \"k\", headers = { x = \"y\" } }")
                .contains("`key` and `base_url` alone")
        );
        assert!(said("s = { vendor = \"fake-tools\" }").contains("RESPAN_TEST_NO_SUCH_TOOL_KEY"));
        assert!(
            said("s = { url = \"http://h.test\", base_url = \"http://x.test\" }")
                .contains("for a `vendor`")
        );
    }

    #[test]
    fn a_server_is_a_url_or_a_command() {
        let ok =
            parse("a = { url = \"https://h.test/mcp/\", auth = { bearer = \"sk-1\" } }\nb = { command = \"sh\", args = [\"-c\", \"true\"] }")
                .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ok[0].transport_label(), "http");
        assert!(ok[0].own_key && ok[0].key_from == "bearer (config)");
        assert_eq!(ok[0].endpoint(), "/mcp/a");
        assert_eq!(ok[1].transport_label(), "stdio");
        let Transport::Http { origin, path, .. } = &ok[0].transport else {
            panic!("http")
        };
        assert_eq!(
            (origin.as_str(), path.as_str()),
            ("https://h.test", "/mcp/")
        );
    }

    #[test]
    fn mistakes_are_named() {
        let said = |text: &str| parse(text).err().unwrap_or_default();
        assert!(said("a = {  }").contains("needs a `url`"));
        assert!(said("a = { url = \"http://h.test\", command = \"x\" }").contains("not both"));
        assert!(said("a = { url = \"http://h.test?x=1\" }").contains("query"));
        assert!(
            said("a = { command = \"definitely-not-a-program\" }").contains("not found on PATH")
        );
        assert!(
            said("a = { command = \"sh\", auth = { bearer = \"k\" } }").contains("for a `url`")
        );
        assert!(
            said("a = { url = \"http://h.test\", env = { A = \"b\" } }")
                .contains("for a `command`")
        );
        assert!(said("openai = { url = \"http://h.test\" }").contains("provider"));
        assert!(said("\"a/b\" = { url = \"http://h.test\" }").contains("name"));
        assert!(
            said("a = { url = \"http://h.test\", auth = { bearer = \"env:NO_SUCH_VAR_X\" } }")
                .contains("NO_SUCH_VAR_X")
        );
    }

    #[test]
    fn credentials_are_said_where_they_come_from() {
        let from = |text: &str| {
            parse(text)
                .map(|s| s[0].key_from.clone())
                .unwrap_or_else(|e| e)
        };
        assert_eq!(from("a = { url = \"http://h.test\" }"), "none");
        assert_eq!(
            from("a = { url = \"http://h.test\", headers = { x-k = \"k\" } }"),
            "headers"
        );
        assert_eq!(
            from("a = { url = \"http://h.test\", auth = \"forward\" }"),
            "the client's own"
        );
        let forward = parse("a = { url = \"http://h.test\", auth = \"forward\" }");
        assert!(forward.is_ok_and(|s| s[0].forward_auth && !s[0].own_key));
        assert!(
            from("a = { url = \"http://h.test\", headers = { Authorization = \"x\" }, auth = { bearer = \"k\" } }")
                .contains("set one")
        );
        assert!(from("a = { url = \"http://h.test\", auth = \"nope\" }").contains("login"));
    }

    #[test]
    fn the_old_fields_say_how_to_write_them_now() {
        let said = |text: &str| parse(text).err().unwrap_or_default();
        let url = "url = \"http://h.test\"";
        assert!(
            said(&format!("a = {{ {url}, key = \"k\" }}"))
                .contains("`key` is now `auth = { bearer = \"KEY\" }`")
        );
        assert!(
            said(&format!("a = {{ {url}, oauth = {{ client_id = \"i\" }} }}"))
                .contains("`oauth = { .. }` is now `auth = { oauth = { .. } }`")
        );
        assert!(
            said(&format!("a = {{ {url}, forward_auth = true }}"))
                .contains("`forward_auth = true` is now `auth = \"forward\"`")
        );
        assert!(
            said(&format!(
                "a = {{ {url}, auth = {{ type = \"bearer\", key = \"k\" }} }}"
            ))
            .contains("`auth = { bearer = \"KEY\" }`")
        );
        assert!(
            said("a = { command = [\"sh\", \"-c\"] }")
                .contains("`command` is the program alone; its arguments go in `args`")
        );
        assert!(
            said("a = { command = \"sh\", headers = { x = \"y\" } }")
                .contains("`headers` and `auth` are for a `url`")
        );
        assert!(
            said(&format!("a = {{ {url}, args = [\"x\"] }}"))
                .contains("`args`, `env` and `cwd` are for a `command`")
        );
    }

    #[test]
    fn a_program_is_found_with_an_extension_of_pathext() {
        let dir =
            std::env::temp_dir().join(format!("respan-pathext-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.join("npx.cmd"), "").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.join("plain"), "").unwrap_or_else(|e| panic!("{e}"));
        let dirs = [dir.clone()];
        let exts = pathext_list(Some(".com;.exe;.cmd"));
        assert_eq!(exts, [".com", ".exe", ".cmd"]);
        assert_eq!(resolve("npx", &dirs, &exts), Some(dir.join("npx.cmd")));
        assert_eq!(resolve("npx", &dirs, &[]), None);
        assert_eq!(resolve("plain", &dirs, &exts), Some(dir.join("plain")));
        assert_eq!(resolve("absent", &dirs, &exts), None);
        let whole = dir.join("npx").to_string_lossy().into_owned();
        assert_eq!(resolve(&whole, &[], &exts), Some(dir.join("npx.cmd")));
        assert_eq!(pathext_list(Some(" ;")), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_cwd_must_be_a_directory() {
        let said = |cwd: &str| parse(&format!("a = {{ command = 'sh', cwd = '{cwd}' }}")).err();
        assert!(said("/definitely/not/here").is_some_and(|e| e.contains("does not exist")));
        assert!(said("/bin/sh").is_some_and(|e| e.contains("not a directory")));
        assert!(said("/").is_none());
    }

    #[test]
    fn a_target_shows_no_path_argument_or_environment() {
        let http =
            parse("a = { url = \"https://h.test:8443/mcp/tok3n/\", auth = { bearer = \"sk-1\" } }")
                .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(http[0].target(), "https://h.test:8443");
        let plain =
            parse("a = { url = \"http://h.test/secret/path\" }").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plain[0].target(), "http://h.test");
        let stdio = parse(
            "a = { command = \"sh\", args = [\"--token\", \"tok3n\"], env = { K = \"sk-2\" } }",
        );
        let stdio = stdio.unwrap_or_else(|e| panic!("{e}"));
        let target = stdio[0].target();
        assert_eq!(target, "sh");
    }

    #[test]
    fn a_request_is_labelled_by_what_it_does() {
        use respan_gateway_http::http::Method;
        use respan_gateway_mcp::Message;
        let message =
            |json: serde_json::Value| Message::try_from(json).unwrap_or_else(|e| panic!("{e:?}"));
        let post = |m: Message| Input::new(Method::POST, vec![m]);
        let call = message(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "echo", "arguments": {} }
        }));
        assert_eq!(label(&post(call)), "tools/call echo");
        let list =
            message(serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        assert_eq!(label(&post(list)), "tools/list");
        assert_eq!(label(&Input::new(Method::GET, Vec::new())), "GET");
    }
}
