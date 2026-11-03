//! deed.page (<https://deed.page>): static HTML hosting for agents, at
//! `https://deed.page` (`POST /v1/sites`, `GET /v1/sites`,
//! `DELETE /v1/sites/{slug}`), with a bearer token.

use async_trait::async_trait;
use respan_gateway_tool_vendor_kit::args::{
    object, optional_int, optional_str, pretty, required_str,
};
use respan_gateway_tool_vendor_kit::reqwest::Method;
use respan_gateway_tool_vendor_kit::{
    Api, KeyIn, ToolError, ToolOutput, ToolSpec, ToolVendor, Tools,
};
use serde_json::{Map, Value, json};

/// deed.page.
pub const VENDOR: ToolVendor = ToolVendor {
    id: "deedpage",
    key_env: &["DEEDPAGE_TOKEN"],
    key_in: KeyIn::Bearer,
    base_url: "https://deed.page",
    tools: &DeedPageTools,
};

/// deed.page's `publish_site`, `list_sites` and `delete_site` tools.
#[derive(Debug, Clone, Copy)]
pub struct DeedPageTools;

/// The most HTML one call publishes: deed.page takes 4 MB a request.
const MAX_HTML: usize = 4 * 1024 * 1024;

/// The longest `ttl_seconds`: a year.
const MAX_TTL: u64 = 365 * 24 * 60 * 60;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "publish_site",
        description: "Publish an HTML page to a public URL on deed.page and return the URL. \
            Publishing to a slug the token already owns replaces the page. Free; the Free \
            plan allows 25 sites.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "html": {"type": "string", "description": "The page's full HTML, served as index.html."},
    "slug": {"type": "string", "maxLength": 63, "description": "The site's name, lowercase letters, digits and hyphens: the page is at https://{slug}.deed.page/. One is chosen when omitted."},
    "ttl_seconds": {"type": "integer", "minimum": 60, "maximum": 31536000, "description": "Delete the site after this many seconds. Omit for a permanent site."}
  },
  "required": ["html"]
}"#,
    },
    ToolSpec {
        name: "list_sites",
        description: "List the token's deed.page sites with their URLs, update times and \
            expiry. Free.",
        input_schema: r#"{"type": "object", "properties": {}}"#,
    },
    ToolSpec {
        name: "delete_site",
        description: "Delete one of the token's deed.page sites; its URL answers 404 \
            afterwards. Free.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "slug": {"type": "string", "maxLength": 63, "description": "The site's slug, as publish_site or list_sites gave it."}
  },
  "required": ["slug"]
}"#,
    },
];

#[async_trait]
impl Tools for DeedPageTools {
    fn list(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = object(&arguments)?;
        match name {
            "publish_site" => {
                let (body, ttl) = publish_body(args)?;
                let mut request = api.request(Method::POST, "/v1/sites").json(&body);
                if let Some(ttl) = ttl {
                    request = request.header("X-Ttl", ttl.to_string());
                }
                let response = api.json(request).await?;
                Ok(pretty(&published(&response)?).into())
            }
            "list_sites" => {
                let response = api.get_json("/v1/sites", &[]).await?;
                Ok(pretty(&sites(&response)?).into())
            }
            "delete_site" => {
                let slug = slug_arg(required_str(args, "slug")?)?;
                let request = api.request(Method::DELETE, &format!("/v1/sites/{slug}"));
                api.json(request).await?;
                Ok(format!("Deleted site {slug}.").into())
            }
            _ => Err(ToolError::new(format!("no tool named {name:?}"))),
        }
    }
}

/// `slug` if deed.page would take it: 1 to 63 lowercase letters, digits
/// and hyphens, starting and ending with a letter or digit.
fn slug_arg(slug: &str) -> Result<&str, ToolError> {
    let bytes = slug.as_bytes();
    let edge = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let valid = !bytes.is_empty()
        && bytes.len() <= 63
        && bytes.iter().all(|b| edge(b) || *b == b'-')
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge);
    if valid {
        Ok(slug)
    } else {
        Err(ToolError::new(
            "`slug` must be 1 to 63 lowercase letters, digits and hyphens, \
             starting and ending with a letter or digit",
        ))
    }
}

/// The publish request's body, and its time to live, if any.
fn publish_body(args: &Map<String, Value>) -> Result<(Value, Option<u64>), ToolError> {
    let html = match args.get("html") {
        Some(Value::String(html)) if !html.trim().is_empty() => html,
        Some(Value::String(_)) | None | Some(Value::Null) => {
            return Err(ToolError::new("`html` is required"));
        }
        Some(_) => return Err(ToolError::new("`html` must be a string")),
    };
    if html.len() > MAX_HTML {
        return Err(ToolError::new("`html` must be at most 4 MB"));
    }
    let mut body = json!({"files": {"index.html": html}});
    if let Some(slug) = optional_str(args, "slug")? {
        body["slug"] = json!(slug_arg(slug)?);
    }
    let ttl = optional_int(args, "ttl_seconds", 60, MAX_TTL)?;
    Ok((body, ttl))
}

/// What the model needs of a publish: where the page is and for how long.
/// A token in the answer, which only a publish without one carries, is
/// never passed on.
fn published(response: &Value) -> Result<Value, ToolError> {
    if response.get("url").and_then(Value::as_str).is_none() {
        return Err(ToolError::new("the API answered without a URL"));
    }
    Ok(keep(
        response,
        &["url", "slug", "version", "permanent", "expires_at"],
    ))
}

/// The sites, each with what the model needs to link or delete it.
fn sites(response: &Value) -> Result<Value, ToolError> {
    let sites = response
        .get("sites")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new("the API answered without sites"))?;
    let sites: Vec<Value> = sites
        .iter()
        .map(|site| keep(site, &["slug", "url", "updated_at", "expires_at"]))
        .collect();
    Ok(json!({"sites": sites}))
}

/// `value`'s `fields`, those it has.
fn keep(value: &Value, fields: &[&str]) -> Value {
    let mut kept = Map::new();
    for field in fields {
        if let Some(v) = value.get(*field) {
            kept.insert((*field).into(), v.clone());
        }
    }
    Value::Object(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn maps_and_checks_publishes() {
        assert_eq!(
            publish_body(&args(json!({"html": "<h1>Hi</h1>"}))).expect("body"),
            (json!({"files": {"index.html": "<h1>Hi</h1>"}}), None)
        );
        assert_eq!(
            publish_body(&args(
                json!({"html": "<p>x</p>", "slug": "q3-report", "ttl_seconds": 3600})
            ))
            .expect("body"),
            (
                json!({"files": {"index.html": "<p>x</p>"}, "slug": "q3-report"}),
                Some(3600)
            )
        );
        assert!(publish_body(&args(json!({}))).is_err());
        assert!(publish_body(&args(json!({"html": "  "}))).is_err());
        assert!(publish_body(&args(json!({"html": 1}))).is_err());
        assert!(publish_body(&args(json!({"html": "x", "ttl_seconds": 0}))).is_err());
        assert!(publish_body(&args(json!({"html": "x", "slug": "Bad_Slug"}))).is_err());
        let big = "x".repeat(MAX_HTML + 1);
        assert!(publish_body(&args(json!({"html": big}))).is_err());
    }

    #[test]
    fn checks_slugs() {
        for ok in ["a", "q3-report", "a1-b2", &"a".repeat(63)] {
            assert_eq!(slug_arg(ok).expect(ok), ok);
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a/b", "../x", &"a".repeat(64)] {
            assert!(slug_arg(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn keeps_what_the_model_needs_and_never_the_token() {
        let out = published(&json!({
            "ok": true, "url": "https://q3-report.deed.page/", "slug": "q3-report",
            "version": "v1", "permanent": false, "expires_at": "2026-10-28T12:00:00Z",
            "token": "deed_tok_secret", "signature": {"alg": "Ed25519"}
        }))
        .expect("published");
        assert_eq!(
            out,
            json!({"url": "https://q3-report.deed.page/", "slug": "q3-report", "version": "v1", "permanent": false, "expires_at": "2026-10-28T12:00:00Z"})
        );
        assert!(!pretty(&out).contains("deed_tok_"));
        assert!(published(&json!({"ok": true})).is_err());

        let listed = sites(&json!({"ok": true, "sites": [
            {"slug": "a", "url": "https://a.deed.page/", "bytes": 14, "updated_at": "2026-10-28T12:00:00Z", "expires_at": null}
        ]}))
        .expect("sites");
        assert_eq!(
            listed,
            json!({"sites": [{"slug": "a", "url": "https://a.deed.page/", "updated_at": "2026-10-28T12:00:00Z", "expires_at": null}]})
        );
        assert!(sites(&json!({})).is_err());
    }
}
