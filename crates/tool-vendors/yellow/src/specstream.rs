//! SpecStream (<https://specstream.ai>): web search and page text from
//! `https://api.specstream.ai/v1` (`POST /web-search`, `POST /extract`), with
//! a bearer key.

use async_trait::async_trait;
use respan_gateway_tool_vendor_kit::args::{
    object, optional_enum, optional_int, optional_str, pretty, required_str,
};
use respan_gateway_tool_vendor_kit::reqwest::Url;
use respan_gateway_tool_vendor_kit::{
    Api, KeyIn, ToolError, ToolOutput, ToolSpec, ToolVendor, Tools,
};
use serde_json::{Map, Value, json};

/// SpecStream.
pub const VENDOR: ToolVendor = ToolVendor {
    id: "specstream",
    key_env: &["SPECSTREAM_API_KEY"],
    key_in: KeyIn::Bearer,
    base_url: "https://api.specstream.ai/v1",
    tools: &SpecStreamTools,
};

/// SpecStream's `search` and `extract` tools.
#[derive(Debug, Clone, Copy)]
pub struct SpecStreamTools;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "search",
        description: "Search the web with SpecStream and get ranked results with snippets and, \
            where a page states one, its published date. Each call is billed to the SpecStream \
            account; `speed: \"fast\"` costs less.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "q": {"type": "string", "description": "The search, in everyday words; `site:`, quoted phrases and `-word` hold on every result."},
    "num": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10, "description": "At most this many results."},
    "speed": {"type": "string", "enum": ["default", "fast"], "description": "`fast` costs less."},
    "country": {"type": "string", "description": "Two-letter country to search from, such as `DE`."},
    "language": {"type": "string", "description": "Two-letter language of the results, such as `de`."}
  },
  "required": ["q"]
}"#,
    },
    ToolSpec {
        name: "extract",
        description: "Read up to 20 web pages or documents (PDF, Word, Excel) with SpecStream, \
            as their main text. Each page read is billed to the SpecStream account; the result \
            has a status for each URL, and a page with no text is not billed.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "urls": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 20, "description": "HTTP or HTTPS page URLs to read."},
    "format": {"type": "string", "enum": ["markdown", "text"], "default": "markdown", "description": "Markdown, or plain text."}
  },
  "required": ["urls"]
}"#,
    },
];

#[async_trait]
impl Tools for SpecStreamTools {
    fn list(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = object(&arguments)?;
        let (path, body) = match name {
            "search" => ("web-search", search_body(args)?),
            "extract" => ("extract", extract_body(args)?),
            _ => return Err(ToolError::new(format!("no tool named {name:?}"))),
        };
        let response = api.post_json(path, &body).await?;
        render(&response, name == "extract")
    }
}

fn search_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let q = required_str(args, "q")?;
    if q.chars().count() > 2000 {
        return Err(ToolError::new("`q` must be at most 2000 characters"));
    }
    let mut body = json!({"q": q});
    if let Some(n) = optional_int(args, "num", 1, 100)? {
        body["num"] = json!(n);
    }
    if let Some(speed) = optional_enum(args, "speed", &["default", "fast"])? {
        body["speed"] = json!(speed);
    }
    for key in ["country", "language"] {
        if let Some(code) = optional_str(args, key)? {
            if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(ToolError::new(format!("`{key}` must be two letters")));
            }
            body[key] = json!(code);
        }
    }
    Ok(body)
}

fn extract_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let urls = args
        .get("urls")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new("`urls` must be an array of 1 to 20 URLs"))?;
    if urls.is_empty() || urls.len() > 20 {
        return Err(ToolError::new("`urls` must be an array of 1 to 20 URLs"));
    }
    for item in urls {
        let url = item
            .as_str()
            .ok_or_else(|| ToolError::new("`urls` must contain HTTP(S) URLs"))?;
        let valid = Url::parse(url)
            .ok()
            .is_some_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some());
        if !valid {
            return Err(ToolError::new("`urls` must contain HTTP(S) URLs"));
        }
    }
    let format = optional_enum(args, "format", &["markdown", "text"])?.unwrap_or("markdown");
    Ok(json!({"urls": urls, "format": format}))
}

/// The fields of each result kept for the model: a search's sources, and
/// each page read with its status.
const SOURCE_FIELDS: &[&str] = &["title", "url", "site", "snippet", "published"];
const PAGE_FIELDS: &[&str] = &["url", "status", "title", "published", "text"];

fn render(response: &Value, extract: bool) -> Result<ToolOutput, ToolError> {
    let data = response
        .get("data")
        .ok_or_else(|| ToolError::new("the API answered without data"))?;
    let (list, fields) = if extract {
        ("results", PAGE_FIELDS)
    } else {
        ("sources", SOURCE_FIELDS)
    };
    let items = data
        .get(list)
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new(format!("the API answered without {list}")))?;
    let items: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut kept = Map::new();
            for field in fields {
                if let Some(value) = item.get(*field).filter(|v| !v.is_null()) {
                    kept.insert((*field).into(), value.clone());
                }
            }
            Value::Object(kept)
        })
        .collect();
    let mut output = json!({ list: items });
    if !extract && let Some(value) = data.pointer("/answer/value").filter(|v| !v.is_null()) {
        output["answer"] = value.clone();
    }
    let mut output = ToolOutput::from(pretty(&output));
    if let Some(price) = data.get("price_usd").and_then(Value::as_f64) {
        output = output.with_usage("usd", price);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_tool_vendor_kit::Content;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn maps_and_checks_requests() {
        assert_eq!(
            search_body(&args(
                json!({"q": "Rust", "num": 2, "speed": "fast", "country": "DE"})
            ))
            .expect("search body"),
            json!({"q": "Rust", "num": 2, "speed": "fast", "country": "DE"})
        );
        assert!(search_body(&args(json!({}))).is_err());
        assert!(search_body(&args(json!({"q": "Rust", "num": 101}))).is_err());
        assert!(search_body(&args(json!({"q": "Rust", "speed": "slow"}))).is_err());
        assert!(search_body(&args(json!({"q": "Rust", "language": "german"}))).is_err());
        assert!(search_body(&args(json!({"q": "x".repeat(2001)}))).is_err());
        assert_eq!(
            extract_body(&args(json!({"urls": ["https://example.com"]}))).expect("extract body"),
            json!({"urls": ["https://example.com"], "format": "markdown"})
        );
        assert!(extract_body(&args(json!({"urls": []}))).is_err());
        assert!(extract_body(&args(json!({"urls": ["file:///etc/passwd"]}))).is_err());
        let many: Vec<_> = (0..21)
            .map(|i| format!("https://example.com/{i}"))
            .collect();
        assert!(extract_body(&args(json!({ "urls": many }))).is_err());
        assert!(
            extract_body(&args(
                json!({"urls": ["https://example.com"], "format": "html"})
            ))
            .is_err()
        );
    }

    #[test]
    fn keeps_useful_results_failures_and_usage() {
        let search = render(
            &json!({"data": {"kind": "fact", "answer": {"value": "May 15, 2015", "page": "https://blog.rust-lang.org"}, "sources": [{"title": "Rust 1.0", "url": "https://blog.rust-lang.org", "site": "blog.rust-lang.org", "snippet": "Announcing Rust 1.0", "published": "2015-05-15", "cited": true}], "rows": [], "price_usd": 0.005}}),
            false,
        )
        .expect("search output");
        assert_eq!(search.usage.get("usd"), Some(&0.005));
        let Content::Text(text) = &search.content[0] else {
            panic!("text")
        };
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("search JSON"),
            json!({"answer": "May 15, 2015", "sources": [{"title": "Rust 1.0", "url": "https://blog.rust-lang.org", "site": "blog.rust-lang.org", "snippet": "Announcing Rust 1.0", "published": "2015-05-15"}]})
        );

        let extract = render(
            &json!({"data": {"results": [{"url": "https://example.com", "status": "ok", "title": "Example", "text": "Hello", "emails": []}, {"url": "https://bad.example", "status": "blocked"}], "price_usd": 0.0008}}),
            true,
        )
        .expect("extract output");
        assert_eq!(extract.usage.get("usd"), Some(&0.0008));
        let Content::Text(text) = &extract.content[0] else {
            panic!("text")
        };
        let value: Value = serde_json::from_str(text).expect("extract JSON");
        assert_eq!(value["results"][0]["text"], "Hello");
        assert!(value["results"][0].get("emails").is_none());
        assert_eq!(value["results"][1]["status"], "blocked");
        assert!(render(&json!({}), false).is_err());
        assert!(render(&json!({"data": {}}), true).is_err());
    }
}
