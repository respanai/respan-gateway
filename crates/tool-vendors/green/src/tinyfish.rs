//! TinyFish (<https://tinyfish.ai>): web search
//! (`GET https://api.search.tinyfish.ai/`), page fetches
//! (`POST https://api.fetch.tinyfish.ai/`) and its browser agent
//! (`POST https://agent.tinyfish.ai/v1/automation/run`, synchronous), the
//! key in `X-API-Key`. The base URL is the agent's; search and fetch are on
//! hosts of their own, called by absolute URL.

use std::time::Duration;

use async_trait::async_trait;
use respan_gateway_tool_vendor_kit::args::{
    object, optional_bool, optional_date, optional_enum, optional_int, optional_str,
    optional_str_list, pretty, required_str,
};
use respan_gateway_tool_vendor_kit::reqwest::Method;
use respan_gateway_tool_vendor_kit::{
    Api, KeyIn, ToolError, ToolOutput, ToolSpec, ToolVendor, Tools,
};
use serde_json::{Map, Value, json};

/// TinyFish.
pub const VENDOR: ToolVendor = ToolVendor {
    id: "tinyfish",
    key_env: &["TINYFISH_API_KEY"],
    key_in: KeyIn::Header("X-API-Key"),
    base_url: "https://agent.tinyfish.ai",
    tools: &TinyFishTools,
};

/// TinyFish's Search API.
pub const SEARCH_URL: &str = "https://api.search.tinyfish.ai/";
/// TinyFish's Fetch API.
pub const FETCH_URL: &str = "https://api.fetch.tinyfish.ai/";

/// TinyFish's tools: `search`, `fetch` and `run_agent`.
#[derive(Debug, Clone, Copy)]
pub struct TinyFishTools;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "search",
        description: "Search the web with TinyFish and get ranked results: title, URL, site and \
            snippet each (plus date and publisher for news; authors, venue, year and PDF URL for \
            research papers). Free: it uses no TinyFish credits.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "query": {"type": "string", "maxLength": 2000, "description": "The search query."},
    "purpose": {"type": "string", "maxLength": 2000, "description": "Why you are searching: the task the results are for. Used to rank them."},
    "domain_type": {"type": "string", "enum": ["web", "news", "research_paper"], "default": "web", "description": "What to search: the web, news articles, or academic papers."},
    "include_domains": {"type": "array", "items": {"type": "string"}, "description": "Only results from these domains."},
    "exclude_domains": {"type": "array", "items": {"type": "string"}, "description": "No results from these domains."},
    "after_date": {"type": "string", "description": "Only results after this date, YYYY-MM-DD."},
    "before_date": {"type": "string", "description": "Only results before this date, YYYY-MM-DD."},
    "location": {"type": "string", "description": "Country code for geo-targeted results, e.g. US. Defaults to US."},
    "language": {"type": "string", "description": "Language code of the results, e.g. en."},
    "page": {"type": "integer", "minimum": 0, "maximum": 10, "default": 0, "description": "Results page, from 0."}
  },
  "required": ["query"]
}"#,
    },
    ToolSpec {
        name: "fetch",
        description: "Fetch 1 to 10 web pages with TinyFish (in parallel, rendering JavaScript \
            when needed) and get each as clean markdown with its title. Pages that fail are \
            listed with the reason; the others still return. Free: it uses no TinyFish credits.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "urls": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 10, "description": "The pages' URLs."},
    "purpose": {"type": "string", "maxLength": 2000, "description": "Why you are fetching them: the task the content is for."},
    "links": {"type": "boolean", "default": false, "description": "Also list each page's outbound links, as absolute URLs."}
  },
  "required": ["urls"]
}"#,
    },
    ToolSpec {
        name: "run_agent",
        description: "Have TinyFish's browser agent open a website and carry out a goal in plain \
            language (navigate, click, fill forms, read), returning what it extracted as JSON. \
            Slow: it waits for the whole run, often tens of seconds to several minutes. Costs \
            TinyFish credits per run. Prefer `search` or `fetch` when reading a page is enough.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "url": {"type": "string", "description": "The website to start on."},
    "goal": {"type": "string", "description": "What to accomplish there, and what to return, e.g. \"Find the pricing page and extract every plan's name and monthly price\"."},
    "browser_profile": {"type": "string", "enum": ["lite", "stealth"], "default": "lite", "description": "stealth: an anti-detection browser, for sites that block bots."},
    "proxy_country": {"type": "string", "enum": ["US", "GB", "CA", "DE", "FR", "JP", "AU"], "description": "Browse through a proxy in this country."},
    "max_steps": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Stop after this many steps (TinyFish's default is 150)."},
    "max_duration_seconds": {"type": "integer", "minimum": 1, "maximum": 3600, "description": "Stop after this many seconds."}
  },
  "required": ["url", "goal"]
}"#,
    },
];

#[async_trait]
impl Tools for TinyFishTools {
    fn list(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = object(&arguments)?;
        match name {
            "search" => {
                let query = search_query(args)?;
                let query: Vec<(&str, String)> =
                    query.iter().map(|(k, v)| (*k, v.clone())).collect();
                Ok(render_search(&api.get_json(SEARCH_URL, &query).await?).into())
            }
            "fetch" => {
                let body = fetch_body(args)?;
                Ok(render_fetch(&api.post_json(FETCH_URL, &body).await?).into())
            }
            "run_agent" => {
                let body = run_body(args)?;
                let request = api
                    .request(Method::POST, "/v1/automation/run")
                    .json(&body)
                    .timeout(run_wait(&body));
                let response = api.json(request).await?;
                let output = ToolOutput::from(render_run(&response)?);
                Ok(match steps(&response) {
                    Some(n) => output.with_usage("steps", n),
                    None => output,
                })
            }
            _ => Err(ToolError::new(format!("no tool named {name:?}"))),
        }
    }
}

fn http_url(url: &str, key: &str) -> Result<(), ToolError> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(ToolError::new(format!(
            "`{key}` must hold http(s) URLs, not {url:?}"
        )))
    }
}

/// The Search API's query string from the tool's arguments.
fn search_query(args: &Map<String, Value>) -> Result<Vec<(&'static str, String)>, ToolError> {
    let mut q = vec![("query", required_str(args, "query")?.to_owned())];
    for key in ["purpose", "location", "language"] {
        if let Some(v) = optional_str(args, key)? {
            q.push((key, v.to_owned()));
        }
    }
    if let Some(v) = optional_enum(args, "domain_type", &["web", "news", "research_paper"])? {
        q.push(("domain_type", v.to_owned()));
    }
    for key in ["include_domains", "exclude_domains"] {
        if let Some(v) = optional_str_list(args, key)? {
            q.push((key, v.join(",")));
        }
    }
    for key in ["after_date", "before_date"] {
        if let Some(v) = optional_date(args, key)? {
            q.push((key, v.to_owned()));
        }
    }
    if let Some(v) = optional_int(args, "page", 0, 10)? {
        q.push(("page", v.to_string()));
    }
    Ok(q)
}

/// The Fetch API's body from the tool's arguments.
fn fetch_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let urls =
        optional_str_list(args, "urls")?.ok_or_else(|| ToolError::new("`urls` is required"))?;
    if urls.len() > 10 {
        return Err(ToolError::new("`urls` holds at most 10 URLs"));
    }
    for url in &urls {
        http_url(url, "urls")?;
    }
    let mut body = Map::new();
    body.insert("urls".into(), json!(urls));
    body.insert("format".into(), "markdown".into());
    if let Some(v) = optional_str(args, "purpose")? {
        body.insert("purpose".into(), v.into());
    }
    if let Some(v) = optional_bool(args, "links")? {
        body.insert("links".into(), v.into());
    }
    Ok(Value::Object(body))
}

/// How long a run is waited for when it sets no `max_duration_seconds`, as
/// TinyFish then sets no limit of its own.
const RUN_WAIT: Duration = Duration::from_secs(10 * 60);

/// How long the run of `body` is waited for: a minute beyond its
/// `max_duration_seconds`, else [`RUN_WAIT`], and never less than the kit
/// client's 120 seconds. A run of `/v1/automation/run` cannot be cancelled,
/// so one given up on still goes on, and is billed.
fn run_wait(body: &Value) -> Duration {
    let limit = body.pointer("/agent_config/max_duration_seconds");
    match limit.and_then(Value::as_u64) {
        Some(seconds) => Duration::from_secs(seconds.saturating_add(60).max(120)),
        None => RUN_WAIT,
    }
}

/// `POST /v1/automation/run`'s body from the tool's arguments.
fn run_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let url = required_str(args, "url")?;
    http_url(url, "url")?;
    let mut body = Map::new();
    body.insert("url".into(), url.into());
    body.insert("goal".into(), required_str(args, "goal")?.into());
    if let Some(v) = optional_enum(args, "browser_profile", &["lite", "stealth"])? {
        body.insert("browser_profile".into(), v.into());
    }
    let countries = ["US", "GB", "CA", "DE", "FR", "JP", "AU"];
    if let Some(v) = optional_enum(args, "proxy_country", &countries)? {
        body.insert(
            "proxy_config".into(),
            json!({"enabled": true, "country_code": v}),
        );
    }
    let mut config = Map::new();
    if let Some(v) = optional_int(args, "max_steps", 1, 500)? {
        config.insert("max_steps".into(), v.into());
    }
    if let Some(v) = optional_int(args, "max_duration_seconds", 1, 3600)? {
        config.insert("max_duration_seconds".into(), v.into());
    }
    if !config.is_empty() {
        body.insert("agent_config".into(), Value::Object(config));
    }
    Ok(Value::Object(body))
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn array_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Search results as JSON, with the fields worth reading.
fn render_search(response: &Value) -> String {
    const KEEP: &[&str] = &[
        "title",
        "url",
        "site_name",
        "snippet",
        "date",
        "publisher",
        "authors",
        "venue",
        "year",
        "cited_by_count",
        "pdf_url",
    ];
    let results: Vec<Value> = array_of(response, "results")
        .iter()
        .map(|r| {
            let kept: Map<String, Value> = KEEP
                .iter()
                .filter_map(|k| {
                    r.get(*k)
                        .filter(|v| !v.is_null())
                        .map(|v| ((*k).to_owned(), v.clone()))
                })
                .collect();
            Value::Object(kept)
        })
        .collect();
    if results.is_empty() {
        return "No results.".into();
    }
    pretty(&Value::Array(results))
}

/// Each page as markdown under its title and URL, then the failures.
fn render_fetch(response: &Value) -> String {
    let mut pages = Vec::new();
    for r in array_of(response, "results") {
        let url = str_of(r, "final_url")
            .or_else(|| str_of(r, "url"))
            .unwrap_or_default();
        let mut page = format!("# {}\nURL: {url}", str_of(r, "title").unwrap_or(url));
        if let Some(date) = str_of(r, "published_date") {
            page.push_str(&format!("\nPublished: {date}"));
        }
        let text = match r.get("text") {
            Some(Value::String(s)) => s.trim().to_owned(),
            Some(Value::Null) | None => String::new(),
            Some(other) => pretty(other),
        };
        page.push_str("\n\n");
        page.push_str(if text.is_empty() {
            "(no content)"
        } else {
            &text
        });
        let links = array_of(r, "links");
        if !links.is_empty() {
            page.push_str("\n\nLinks:");
            for link in links.iter().filter_map(Value::as_str) {
                page.push_str("\n- ");
                page.push_str(link);
            }
        }
        pages.push(page);
    }
    let errors = array_of(response, "errors");
    if !errors.is_empty() {
        let mut failed = String::from("Failed:");
        for e in errors {
            failed.push_str(&format!(
                "\n- {}: {}",
                str_of(e, "url").unwrap_or("?"),
                str_of(e, "error").unwrap_or("unknown error")
            ));
            if let Some(status) = e.get("status").filter(|s| !s.is_null()) {
                failed.push_str(&format!(" (HTTP {status})"));
            }
        }
        pages.push(failed);
    }
    if pages.is_empty() {
        return "Nothing was fetched.".into();
    }
    pages.join("\n\n---\n\n")
}

/// The steps a run took, its `num_of_steps`: what TinyFish bills an agent
/// run by, per step (<https://tinyfish.ai/pricing>); search and fetch are
/// free.
fn steps(response: &Value) -> Option<f64> {
    response.get("num_of_steps").and_then(Value::as_f64)
}

/// A completed run's result as JSON; a failed run is a [`ToolError`].
fn render_run(response: &Value) -> Result<String, ToolError> {
    if str_of(response, "status") == Some("COMPLETED") {
        let result = response.get("result").cloned().unwrap_or(Value::Null);
        let steps = steps(response)
            .map(|n| format!(" in {n} steps"))
            .unwrap_or_default();
        return Ok(format!(
            "Run completed{steps}. Result:\n{}",
            pretty(&result)
        ));
    }
    let error = response.get("error").unwrap_or(&Value::Null);
    let mut message = format!(
        "The run {}: {}",
        str_of(response, "status").map_or("failed".into(), str::to_lowercase),
        str_of(error, "message").unwrap_or("no reason given")
    );
    if let Some(category) = str_of(error, "category") {
        message.push_str(&format!(" ({category})"));
    }
    if let Some(help) = str_of(error, "help_message") {
        message.push_str(&format!(". {help}"));
    }
    Err(ToolError::new(message))
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

    #[test]
    fn search_query_maps_arguments() {
        let q = search_query(&args(json!({
            "query": "rust", "domain_type": "news", "include_domains": ["a.com", "b.com"],
            "after_date": "2025-01-01", "page": 2
        })))
        .expect("valid");
        assert_eq!(
            q,
            vec![
                ("query", "rust".to_owned()),
                ("domain_type", "news".to_owned()),
                ("include_domains", "a.com,b.com".to_owned()),
                ("after_date", "2025-01-01".to_owned()),
                ("page", "2".to_owned()),
            ]
        );
        assert!(search_query(&args(json!({}))).is_err());
        assert!(search_query(&args(json!({"query": "x", "domain_type": "blogs"}))).is_err());
        assert!(search_query(&args(json!({"query": "x", "page": 11}))).is_err());
    }

    #[test]
    fn fetch_body_maps_and_validates() {
        assert_eq!(
            fetch_body(&args(json!({"urls": ["https://a.b"], "links": true}))).expect("valid"),
            json!({"urls": ["https://a.b"], "format": "markdown", "links": true})
        );
        assert!(fetch_body(&args(json!({"urls": []}))).is_err());
        assert!(fetch_body(&args(json!({"urls": ["ftp://a"]}))).is_err());
        let many: Vec<String> = (0..11).map(|i| format!("https://a/{i}")).collect();
        assert!(fetch_body(&args(json!({ "urls": many }))).is_err());
    }

    #[test]
    fn run_body_maps_and_validates() {
        assert_eq!(
            run_body(&args(json!({
                "url": "https://a.b", "goal": "find prices", "proxy_country": "DE", "max_steps": 20
            })))
            .expect("valid"),
            json!({
                "url": "https://a.b", "goal": "find prices",
                "proxy_config": {"enabled": true, "country_code": "DE"},
                "agent_config": {"max_steps": 20}
            })
        );
        assert!(run_body(&args(json!({"url": "https://a.b"}))).is_err());
        assert!(run_body(&args(json!({"goal": "x"}))).is_err());
        assert!(
            run_body(&args(
                json!({"url": "https://a.b", "goal": "x", "browser_profile": "y"})
            ))
            .is_err()
        );
    }

    #[test]
    fn renders_search() {
        let text = render_search(
            &json!({"query": "q", "total_results": 1, "page": 0, "results": [
                {"position": 1, "site_name": "Rust", "title": "Rust", "url": "https://rust-lang.org", "snippet": "A language", "fetch": null}
            ]}),
        );
        let parsed: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            parsed,
            json!([{"title": "Rust", "url": "https://rust-lang.org", "site_name": "Rust", "snippet": "A language"}])
        );
        assert_eq!(render_search(&json!({"results": []})), "No results.");
    }

    #[test]
    fn renders_fetch() {
        let text = render_fetch(&json!({
            "results": [{"url": "https://a", "final_url": "https://a/", "title": "A", "description": null,
                "language": "en", "format": "markdown", "text": "Hello", "author": null,
                "published_date": null, "links": ["https://b"]}],
            "errors": [{"url": "https://c", "error": "page_not_found", "status": 404}]
        }));
        assert_eq!(
            text,
            "# A\nURL: https://a/\n\nHello\n\nLinks:\n- https://b\n\n---\n\nFailed:\n- https://c: page_not_found (HTTP 404)"
        );
    }

    #[test]
    fn renders_run() {
        let done = json!({
            "run_id": "r", "status": "COMPLETED", "num_of_steps": 5, "result": {"price": "$799"}, "error": null
        });
        assert_eq!(steps(&done), Some(5.0));
        assert_eq!(steps(&json!({"status": "COMPLETED"})), None);
        let ok = render_run(&done).expect("completed");
        assert_eq!(
            ok,
            "Run completed in 5 steps. Result:\n{\n  \"price\": \"$799\"\n}"
        );
        let err = render_run(&json!({
            "status": "FAILED", "result": null,
            "error": {"message": "Out of credits", "category": "BILLING_FAILURE"}
        }));
        assert_eq!(
            err,
            Err(ToolError::new(
                "The run failed: Out of credits (BILLING_FAILURE)"
            ))
        );
    }

    #[test]
    fn a_run_is_waited_for_as_long_as_it_may_take() {
        let wait = |more: Value| {
            let mut v = json!({"url": "https://a.b", "goal": "find prices"});
            v.as_object_mut().expect("object").extend(args(more));
            run_wait(&run_body(&args(v)).expect("valid"))
        };
        let minutes = |n: u64| Duration::from_secs(n * 60);
        assert_eq!(wait(json!({"max_duration_seconds": 300})), minutes(6));
        assert_eq!(wait(json!({"max_duration_seconds": 10})), minutes(2));
        assert_eq!(wait(json!({})), minutes(10));
    }
}
