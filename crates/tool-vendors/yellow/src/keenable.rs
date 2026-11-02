//! Keenable (<https://keenable.ai>): web search and page fetches from
//! `https://api.keenable.ai` (`POST /v1/search`, `GET /v1/fetch`). It needs
//! no key: without one it calls the public endpoints (`/v1/search/public`,
//! `/v1/fetch/public`), which are rate-limited per IP; a key, if set, goes
//! in `X-API-Key` and lifts the limits ([`KeyIn::Manual`]).

use async_trait::async_trait;
use respan_gateway_tool_vendor_kit::args::{
    object, optional_date, optional_int, optional_str, pretty, required_str,
};
use respan_gateway_tool_vendor_kit::{
    Api, KeyIn, ToolError, ToolOutput, ToolSpec, ToolVendor, Tools, reqwest,
};
use serde_json::{Map, Value, json};

/// Keenable.
pub const VENDOR: ToolVendor = ToolVendor {
    id: "keenable",
    // No key is needed, so none is looked for: set `key` to use one.
    key_env: &[],
    key_in: KeyIn::Manual,
    base_url: "https://api.keenable.ai",
    tools: &KeenableTools,
};

/// The header naming the app that calls, which the public endpoints require.
const TITLE: (&str, &str) = ("X-Keenable-Title", "respan-gateway");

/// Keenable's tools: `search` and `fetch`.
#[derive(Debug, Clone, Copy)]
pub struct KeenableTools;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "search",
        description: "Search the web with Keenable and get ranked results: title, URL and a \
            snippet of the page's text each. Needs no key; without one, calls are rate-limited \
            per IP.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "query": {"type": "string", "description": "The search query."},
    "max_results": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "At most this many results."},
    "site": {"type": "string", "description": "Only results from this domain, e.g. docs.rs."},
    "published_after": {"type": "string", "description": "Only pages published on or after this date, YYYY-MM-DD."},
    "published_before": {"type": "string", "description": "Only pages published on or before this date, YYYY-MM-DD."}
  },
  "required": ["query"]
}"#,
    },
    ToolSpec {
        name: "fetch",
        description: "Fetch one web page with Keenable and get it as clean markdown, with its \
            title. Needs no key; without one, calls are rate-limited per IP.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "url": {"type": "string", "description": "The page's URL."}
  },
  "required": ["url"]
}"#,
    },
];

#[async_trait]
impl Tools for KeenableTools {
    fn list(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = object(&arguments)?;
        match name {
            "search" => {
                let body = search_body(args)?;
                let request = keyed(api, reqwest::Method::POST, "/v1/search").json(&body);
                Ok(render_search(&api.json(request).await?).into())
            }
            "fetch" => {
                let url = fetch_url(args)?;
                let request = keyed(api, reqwest::Method::GET, "/v1/fetch").query(&[("url", url)]);
                Ok(render_fetch(&api.json(request).await?).into())
            }
            _ => Err(ToolError::new(format!("no tool named {name:?}"))),
        }
    }
}

/// A request to `path` with the key in `X-API-Key`, or, with no key, to
/// the path's public endpoint.
fn keyed(api: &Api, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
    let key = api.key().trim();
    let request = if key.is_empty() {
        api.request(method, &format!("{path}/public"))
    } else {
        api.request(method, path).header("X-API-Key", key)
    };
    request.header(TITLE.0, TITLE.1)
}

/// `POST /v1/search`'s body from the tool's arguments.
fn search_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let mut body = Map::new();
    body.insert("query".into(), required_str(args, "query")?.into());
    if let Some(v) = optional_int(args, "max_results", 1, 50)? {
        body.insert("max_results".into(), v.into());
    }
    if let Some(v) = optional_str(args, "site")? {
        body.insert("site".into(), v.trim().into());
    }
    for key in ["published_after", "published_before"] {
        if let Some(v) = optional_date(args, key)? {
            body.insert(key.into(), v.into());
        }
    }
    Ok(Value::Object(body))
}

/// `GET /v1/fetch`'s `url` from the tool's arguments.
fn fetch_url(args: &Map<String, Value>) -> Result<&str, ToolError> {
    let url = required_str(args, "url")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ToolError::new("`url` must be an http(s) URL"));
    }
    Ok(url)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Search results as JSON: each a title, URL, snippet and, where known,
/// when it was published.
fn render_search(response: &Value) -> String {
    let results: Vec<Value> = (response.get("results").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut kept = Map::new();
            for key in ["title", "url", "snippet", "published_at"] {
                if let Some(v) = str_of(r, key) {
                    kept.insert(key.into(), v.into());
                }
            }
            Value::Object(kept)
        })
        .filter(|r| r.get("url").is_some())
        .collect();
    if results.is_empty() {
        return "No results.".into();
    }
    pretty(&json!(results))
}

/// The page's markdown under its title and URL.
fn render_fetch(response: &Value) -> String {
    let url = str_of(response, "url").unwrap_or_default();
    let mut out = format!("# {}\nURL: {url}", str_of(response, "title").unwrap_or(url));
    out.push_str("\n\n");
    out.push_str(str_of(response, "content").unwrap_or("The page had no extractable content."));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    fn sent(key: &str, method: reqwest::Method, path: &str) -> reqwest::Request {
        let api = VENDOR.api(key, None);
        keyed(&api, method, path).build().expect("a request")
    }

    #[test]
    fn calls_the_public_endpoints_without_a_key() {
        let search = sent("", reqwest::Method::POST, "/v1/search");
        assert_eq!(
            search.url().as_str(),
            "https://api.keenable.ai/v1/search/public"
        );
        assert!(search.headers().get("x-api-key").is_none());
        assert!(search.headers().get("authorization").is_none());
        assert_eq!(search.headers()["x-keenable-title"], "respan-gateway");

        let fetch = sent(" ", reqwest::Method::GET, "/v1/fetch");
        assert_eq!(
            fetch.url().as_str(),
            "https://api.keenable.ai/v1/fetch/public"
        );
        assert!(fetch.headers().get("x-api-key").is_none());
    }

    #[test]
    fn sends_a_key_to_the_keyed_endpoints() {
        let search = sent("kn-key", reqwest::Method::POST, "/v1/search");
        assert_eq!(search.url().as_str(), "https://api.keenable.ai/v1/search");
        assert_eq!(search.headers()["x-api-key"], "kn-key");
        assert_eq!(search.headers()["x-keenable-title"], "respan-gateway");
    }

    #[test]
    fn search_body_maps_and_validates() {
        assert_eq!(
            search_body(&args(json!({
                "query": "rust", "max_results": 3, "site": " docs.rs ",
                "published_after": "2025-01-01", "published_before": "2025-12-31"
            })))
            .expect("valid"),
            json!({
                "query": "rust", "max_results": 3, "site": "docs.rs",
                "published_after": "2025-01-01", "published_before": "2025-12-31"
            })
        );
        assert_eq!(
            search_body(&args(json!({"query": "rust"}))).expect("valid"),
            json!({"query": "rust"})
        );
        assert!(search_body(&args(json!({}))).is_err());
        assert!(search_body(&args(json!({"query": "x", "max_results": 51}))).is_err());
        assert!(
            search_body(&args(
                json!({"query": "x", "published_after": "2025/01/01"})
            ))
            .is_err()
        );
    }

    #[test]
    fn fetch_url_validates() {
        assert_eq!(
            fetch_url(&args(json!({"url": "https://example.com"}))),
            Ok("https://example.com")
        );
        assert!(fetch_url(&args(json!({}))).is_err());
        assert!(fetch_url(&args(json!({"url": "file:///etc/passwd"}))).is_err());
    }

    #[test]
    fn renders_search_without_empty_or_unknown_fields() {
        let text = render_search(&json!({"results": [
            {"title": "Rust", "url": "https://rust-lang.org", "description": "", "snippet": "A language", "published_at": "2015-05-15T00:00:00Z", "acquired_at": "2026-01-01T00:00:00Z"},
            {"title": "Book", "url": "https://doc.rust-lang.org/book", "description": "", "snippet": "", "published_at": 1431648000},
            {"title": "No URL"}
        ]}));
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("JSON"),
            json!([
                {"title": "Rust", "url": "https://rust-lang.org", "snippet": "A language", "published_at": "2015-05-15T00:00:00Z"},
                {"title": "Book", "url": "https://doc.rust-lang.org/book"}
            ])
        );
        assert_eq!(render_search(&json!({"results": []})), "No results.");
        assert_eq!(render_search(&json!({})), "No results.");
    }

    #[test]
    fn renders_fetch() {
        assert_eq!(
            render_fetch(
                &json!({"url": "https://example.com/", "title": "Example Domain", "content": "Hello", "description": ""})
            ),
            "# Example Domain\nURL: https://example.com/\n\nHello"
        );
        assert_eq!(
            render_fetch(&json!({"url": "https://example.com/", "content": ""})),
            "# https://example.com/\nURL: https://example.com/\n\nThe page had no extractable content."
        );
    }
}
