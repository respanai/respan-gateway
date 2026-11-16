#![warn(missing_docs)]
//! Routes an MCP client's request to the MCP server its path names, and
//! fronts many of them as one at `/mcp` ([`ROOT`]).
//!
//! [`McpRouter`] takes the MCP format's requests (core providers of the
//! format being its servers). It routes MCP servers, not models; the
//! `routing` plugin routes models.
//!
//! # `/mcp/{name}`: one server
//!
//! A request is targeted at the provider `name`: the last segment of the
//! request's cleaned path. A key limited to some tools (a request carrying a
//! [`ToolScope`]) is refused there with `403 tools_limited`: a server's own
//! endpoint offers all its tools, so such a key reaches them through `/mcp`,
//! which holds it to them. A server turned off in its [`ToolSwitches`] is
//! refused with `403 tool_server_off`; a tool turned off is not, since the
//! endpoint relays the server as it is.
//!
//! # `/mcp`: every server it fronts, as one
//!
//! Given servers, the router answers `/mcp` itself, fronting them as a
//! single server. A client sees one server and one session; the gateway
//! holds one session with each server behind it, opened as the client's
//! session is and ended with it, through those providers, with each
//! server's own credentials (so give none of them a client's). Given none,
//! `/mcp` is routed as any other path, to a server named `mcp`.
//!
//! What a client sees:
//!
//! - `initialize` answers with the capabilities of the servers merged (a
//!   capability any server has, `listChanged` and `subscribe` if any server
//!   does). A server that does not answer is left out of the session.
//! - **Tools and prompts** are named `{server}__{name}`; a call is sent to
//!   the server with the name it knows. A [`ToolFilter`] says which tools
//!   are offered, by that name, and a request's [`ToolScope`] limits it
//!   further. A server or tool turned off in its [`ToolSwitches`] is not
//!   offered either, from the next list or call on.
//! - **Resources** keep their URIs under the server's: `mcp://{server}/{uri}`
//!   (a template likewise). Nothing is looked up: the URI says the server.
//!   URIs inside tool and prompt results are given the prefix too, so what
//!   a tool links to can be read.
//! - **Lists** are the first page of every server's, merged, in the order
//!   `servers` names them. A server with more pages leaves a `nextCursor`,
//!   which holds the cursors of the servers that have more; it asks for
//!   those servers alone, until none has.
//! - **Notifications**: the `GET` stream is the servers' own, merged: their
//!   `list_changed`, `resources/updated` (URI prefixed) and log messages. A
//!   `notifications/cancelled` from the client goes to every server; progress
//!   notifications ride the answer to the call they belong to.
//! - Sampling, elicitation, roots and completion are not offered: the
//!   gateway tells its servers it is a client with no capabilities.
//!
//! The session is reached by its id alone, which none can guess, as a
//! server's is at `/mcp/{name}`. A client that ends its session (`DELETE`) ends the
//! servers' sessions with it; one that does not leaves them to the servers'
//! own idle timeouts (the gateway forgets its side after thirty idle minutes
//! or past a thousand sessions). A reload ends every session of the build it
//! retires, with the servers, once the requests on that build are done
//! ([`Closer`]); the clients open new ones.
//!
//! The request log has one entry per client request: the method and the
//! tool, prompt or resource it names, and the server a routed call went to
//! (the last one, for a list that asked several).

mod rewrite;
mod run;
mod session;
mod upstream;

#[cfg(test)]
mod switch_tests;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use respan_gateway_core::futures::stream::BoxStream;
use respan_gateway_core::{ClientContext, ErrorCode, Fault, Plugin, Response, async_trait};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http::path::clean_path;
use respan_gateway_http::raw::Original;
use respan_gateway_http_faults::Status;
use respan_gateway_mcp::{Input, Message, Output, PROTOCOL};
use respan_gateway_mcp_http::FORMAT;
use respan_gateway_plugin_tool_loop::{ToolFilter, ToolScope, ToolSwitches};
use run::Run;
use serde_json::Value;
use session::Inner;

pub use session::Closer;

/// Where clients reach the servers the router fronts as one.
pub const ROOT: &str = "/mcp";
/// The separator between a server's name and a tool's or prompt's.
const SEP: &str = "__";
/// The scheme of a resource's URI under a server's.
const SCHEME: &str = "mcp://";
/// The most client sessions held at once.
const SESSIONS: u64 = 1024;
/// How long a session no client uses is kept.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// The protocol versions `/mcp` answers with, newest first.
const VERSIONS: [&str; 3] = [PROTOCOL, "2025-03-26", "2024-11-05"];

/// Targets a client of `/mcp/{name}` at the server `name`, and answers
/// `/mcp` itself for the servers it fronts. See the [crate docs](crate).
pub struct McpRouter(Arc<Inner>);

impl std::fmt::Debug for McpRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRouter")
            .field("servers", &self.0.servers)
            .finish_non_exhaustive()
    }
}

impl Default for McpRouter {
    /// Routing `/mcp/{name}` alone, fronting no servers at `/mcp`.
    fn default() -> Self {
        Self::new(Vec::new(), ToolFilter::default())
    }
}

impl McpRouter {
    /// Front `servers`, core providers of the MCP format, at `/mcp`, in the
    /// order lists show them, offering the tools `filter` offers.
    pub fn new(servers: Vec<String>, filter: ToolFilter) -> Self {
        Self::with_switches(servers, filter, Arc::default())
    }

    /// Like [`new`](Self::new), refusing too the servers `switches` turns
    /// off, and withholding at `/mcp` the tools it turns off.
    pub fn with_switches(
        servers: Vec<String>,
        filter: ToolFilter,
        switches: Arc<ToolSwitches>,
    ) -> Self {
        Self(Arc::new(Inner {
            servers,
            filter,
            switches,
            sessions: moka::sync::Cache::builder()
                .time_to_idle(IDLE)
                .max_capacity(SESSIONS)
                .eviction_policy(moka::policy::EvictionPolicy::lru())
                .build(),
            next_id: AtomicU64::new(1),
        }))
    }

    /// What ends the sessions at `/mcp` once a reload retires it.
    pub fn closer(&self) -> Closer {
        Closer(self.0.clone())
    }

    /// Answers `/mcp` for the servers it fronts.
    async fn front(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let input = cx.input::<Input>()?.clone();
        let scope = cx.extensions().get::<ToolScope>().cloned();
        let calls = self.0.servers.iter().map(|s| (s.clone(), cx.call(s)));
        let run = Run {
            inner: self.0.clone(),
            calls: Arc::new(calls.collect()),
            scope,
        };
        let outputs = run.handle(input).await?;
        Ok(Some(Response::stream(outputs)))
    }

    /// Targets `/mcp/{name}` at the server `name`.
    fn route(&self, cx: &mut ClientContext, path: Option<&str>) -> Result<(), Fault> {
        let name = path.and_then(server_name);
        let name = name.ok_or_else(|| Fault::not_found("no MCP server named"))?;
        if cx.extensions().get::<ToolScope>().is_some() {
            let message = format!(
                "this key is limited to some tools, so it reaches MCP servers at /mcp, not /mcp/{name}"
            );
            return Err(Fault::new(message)
                .with(ErrorCode("tools_limited".into()))
                .with(Status(StatusCode::FORBIDDEN)));
        }
        if !self.0.switches.server_on(&name) {
            let message = format!("the tool server '{name}' is turned off");
            return Err(Fault::new(message)
                .with(ErrorCode("tool_server_off".into()))
                .with(Status(StatusCode::FORBIDDEN)));
        }
        cx.set_target(name);
        Ok(())
    }
}

/// The server a request path names: the last segment of the cleaned path.
pub fn server_name(path: &str) -> Option<String> {
    clean_path(path).rsplit('/').next().map(str::to_owned)
}

#[async_trait]
impl Plugin for McpRouter {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if cx.format() != FORMAT {
            return Ok(None);
        }
        let path = cx
            .extensions()
            .get::<Original>()
            .and_then(|wire| wire.get())
            .map(|request| request.uri().path().to_owned());
        if !self.0.servers.is_empty() && path.as_deref().map(clean_path).as_deref() == Some(ROOT) {
            return self.front(cx).await;
        }
        self.route(cx, path.as_deref())?;
        Ok(None)
    }
}

type Outputs = BoxStream<'static, Result<Output, Fault>>;

fn message(json: Value) -> Result<Message, Fault> {
    Message::try_from(json).map_err(Fault::internal)
}

#[cfg(test)]
mod tests {
    use super::server_name;

    #[test]
    fn names_the_last_segment() {
        assert_eq!(server_name("/mcp/github").as_deref(), Some("github"));
        assert_eq!(server_name("/v1/mcp/files").as_deref(), Some("files"));
    }

    #[test]
    fn cleans_the_path_first() {
        assert_eq!(server_name("/mcp/github/").as_deref(), Some("github"));
        assert_eq!(server_name("//mcp//github").as_deref(), Some("github"));
    }
}
