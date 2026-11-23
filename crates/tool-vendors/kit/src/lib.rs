#![warn(missing_docs)]
//! A tool vendor is an HTTP API offered as tools, the way a vendor of
//! `respan-gateway-vendors` is a model API offered as formats: where it is,
//! how it takes a key, and its tools ([`ToolVendor`]). [`ToolServer`] serves
//! one as an MCP server in process, so a gateway registers it as a provider
//! of the MCP format like any other server, and everything that reaches MCP
//! servers (`/mcp/{name}`, the aggregate `/mcp`, the tool loop) reaches its
//! tools too.

mod api;
pub mod args;
mod output;
mod server;

pub use api::{Api, ToolError};
pub use output::{Content, ToolOutput, ToolUsage, decode_base64};
pub use reqwest;
pub use respan_gateway_http_transport::Network;
pub use server::{ToolServer, USAGE};

use async_trait::async_trait;
use serde_json::Value;

/// How a tool vendor takes its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIn {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// The key as the value of this header.
    Header(&'static str),
    /// `Authorization: <scheme> <key>`, such as Zep's `Api-Key`.
    Scheme(&'static str),
    /// `Authorization: Basic`, the key being `user:password`.
    Basic,
    /// Not sent by [`Api`]: the vendor's tools send it themselves, from
    /// [`Api::key`], as for a key that also says where the API is.
    Manual,
}

/// A tool vendor: an HTTP API's tools, its key and its base URL.
pub struct ToolVendor {
    /// Its name in configuration: `[tools.<name>] vendor = "<id>"`.
    pub id: &'static str,
    /// The environment variables its key may be in, the first set winning.
    pub key_env: &'static [&'static str],
    /// How it takes the key.
    pub key_in: KeyIn,
    /// Its base URL; `[tools.<name>] base_url` moves it.
    pub base_url: &'static str,
    /// Its tools.
    pub tools: &'static dyn Tools,
}

impl std::fmt::Debug for ToolVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolVendor")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ToolVendor {
    /// Its key from the first of [`key_env`](Self::key_env) that is set.
    pub fn key_from_env(&self) -> Option<String> {
        self.key_env
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    /// Its API with `key`, at `base_url` or its own.
    pub fn api(&self, key: impl Into<String>, base_url: Option<&str>) -> Api {
        Api::new(base_url.unwrap_or(self.base_url), key, self.key_in)
    }
}

/// One tool as MCP's `tools/list` describes it.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    /// Its name, unique within its vendor.
    pub name: &'static str,
    /// What it does, for the model.
    pub description: &'static str,
    /// Its arguments' JSON Schema, as JSON text (a root `object`).
    pub input_schema: &'static str,
}

/// A tool vendor's tools: their descriptions, and calling one.
#[async_trait]
pub trait Tools: Send + Sync + 'static {
    /// Every tool.
    fn list(&self) -> &'static [ToolSpec];

    /// Call the tool `name` with `arguments` through `api`. The output is
    /// the tool's result for the model, text or media (a `String` is
    /// `.into()` one); a [`ToolError`] is a result too, one the model is
    /// told failed.
    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError>;
}
