//! Calls each tool vendor's tools against its live API, the check that
//! moves a vendor from yellow to green. Each vendor with its key in the
//! environment runs; the others are skipped.
//!
//! ```sh
//! cargo run -p respan-gateway-tool-vendors --example live            # all
//! cargo run -p respan-gateway-tool-vendors --example live -- tavily  # some
//! ```
//!
//! Calls are billed as the tools say; each is the cheapest that exercises
//! the tool, and what a run creates (sandboxes, assets) it deletes.

use respan_gateway_tool_vendors::{ALL, Content, ToolVendor};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Instant;

/// One call: the tool, its arguments (`${var}` filled from earlier
/// captures), and what to capture from its output: the word after `label`.
struct Step {
    tool: &'static str,
    args: Value,
    capture: Option<(&'static str, &'static str)>,
}

fn step(tool: &'static str, args: Value) -> Step {
    Step {
        tool,
        args,
        capture: None,
    }
}

fn capturing(tool: &'static str, args: Value, label: &'static str, var: &'static str) -> Step {
    Step {
        tool,
        args,
        capture: Some((label, var)),
    }
}

const PDF: &str = "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf";
const IMAGE: &str =
    "https://upload.wikimedia.org/wikipedia/commons/4/47/PNG_transparency_demonstration_1.png";
const AUDIO: &str = "https://upload.wikimedia.org/wikipedia/commons/c/c8/Example.ogg";

fn steps(vendor: &str) -> Vec<Step> {
    match vendor {
        "apify" => vec![
            step(
                "search_actors",
                json!({"query": "google search", "limit": 3}),
            ),
            step(
                "browse_web",
                json!({"query": "https://example.com", "max_results": 1}),
            ),
            step("get_run_items", json!({"actor": "apify~rag-web-browser"})),
            step(
                "run_actor",
                json!({"actor": "apify~rag-web-browser", "input": {"query": "https://example.com", "maxResults": 1}}),
            ),
        ],
        "browserbase" => vec![
            step(
                "search",
                json!({"query": "rust programming language", "num_results": 2}),
            ),
            step("fetch", json!({"url": "https://example.com"})),
            capturing(
                "run_agent",
                json!({"task": "Go to https://example.com and return the page's heading."}),
                "Run ",
                "run",
            ),
            step("get_agent_run", json!({"run_id": "${run}"})),
            step("stop_agent_run", json!({"run_id": "${run}"})),
        ],
        "cloudinary" => vec![
            step(
                "upload",
                json!({"file": IMAGE, "public_id": "respan-gateway-live-test"}),
            ),
            step(
                "get_asset",
                json!({"public_id": "respan-gateway-live-test"}),
            ),
            step(
                "search",
                json!({"expression": "public_id:respan-gateway-live-test"}),
            ),
            step(
                "delivery_url",
                json!({"public_id": "respan-gateway-live-test", "transformation": "c_fill,w_100,h_100"}),
            ),
            step(
                "delete_asset",
                json!({"public_id": "respan-gateway-live-test"}),
            ),
        ],
        "context7" => vec![
            step(
                "resolve_library_id",
                json!({"library_name": "React", "query": "useEffect cleanup"}),
            ),
            step(
                "query_docs",
                json!({"library_id": "/reactjs/react.dev", "query": "useEffect cleanup"}),
            ),
            step(
                "search_docs",
                json!({"query": "useEffect cleanup", "libraries": ["/reactjs/react.dev"]}),
            ),
        ],
        "daytona" => vec![
            capturing(
                "create_sandbox",
                json!({"auto_stop_minutes": 5}),
                "Sandbox ",
                "sb",
            ),
            step("list_sandboxes", json!({})),
            step(
                "write_file",
                json!({"sandbox": "${sb}", "path": "/tmp/hello.txt", "content": "hello from respan"}),
            ),
            step(
                "run_command",
                json!({"sandbox": "${sb}", "command": "cat /tmp/hello.txt"}),
            ),
            step(
                "run_code",
                json!({"sandbox": "${sb}", "code": "import sys; print(sys.version); print(6*7)"}),
            ),
            step("delete_sandbox", json!({"sandbox": "${sb}"})),
        ],
        "deedpage" => vec![
            capturing(
                "publish_site",
                json!({"html": "<h1>respan-gateway live check</h1>", "ttl_seconds": 300}),
                "\"slug\": \"",
                "slug",
            ),
            step("list_sites", json!({})),
            step("delete_site", json!({"slug": "${slug}"})),
        ],
        "e2b" => vec![
            capturing(
                "run_code",
                json!({"code": "x = 6 * 7\nprint(x)\nx", "timeout_seconds": 60}),
                "Sandbox: ",
                "sb",
            ),
            step(
                "run_code",
                json!({"code": "print(x + 1)", "sandbox_id": "${sb}"}),
            ),
            step("list_sandboxes", json!({})),
            step("kill_sandbox", json!({"sandbox_id": "${sb}"})),
        ],
        "elevenlabs" => vec![
            step("search_voices", json!({"search": "George"})),
            step(
                "text_to_speech",
                json!({"text": "Hi.", "voice_id": "JBFqnCBsd6RMkjVDRZzb", "model_id": "eleven_flash_v2_5"}),
            ),
            step(
                "sound_effect",
                json!({"text": "a short door knock", "duration_seconds": 1}),
            ),
            step("speech_to_text", json!({"url": AUDIO})),
        ],
        "exa" => vec![
            step(
                "search",
                json!({"query": "Rust 1.0 release date", "num_results": 1}),
            ),
            step("get_contents", json!({"urls": ["https://example.com"]})),
        ],
        "firecrawl" => vec![
            step("scrape", json!({"url": "https://example.com"})),
            step(
                "map_site",
                json!({"url": "https://example.com", "limit": 1}),
            ),
            step(
                "search",
                json!({"query": "Rust 1.0 release date", "limit": 1}),
            ),
            capturing(
                "crawl",
                json!({"url": "https://example.com", "limit": 1}),
                "\"id\":\"",
                "crawl",
            ),
            step("get_crawl", json!({"crawl_id": "${crawl}", "limit": 1})),
        ],
        "linkup" => vec![
            step("search", json!({"q": "Rust 1.0 release date"})),
            step("answer", json!({"q": "When was Rust 1.0 released?"})),
            step("fetch", json!({"url": "https://example.com"})),
        ],
        "reducto" => vec![
            step("parse_document", json!({"document_url": PDF})),
            step(
                "extract_fields",
                json!({"document_url": PDF, "schema": {"type": "object", "properties": {"title": {"type": "string"}}}}),
            ),
            step(
                "split_document",
                json!({"document_url": PDF, "sections": [{"name": "body", "description": "The document's text"}]}),
            ),
        ],
        "tavily" => vec![
            step(
                "search",
                json!({"query": "Rust 1.0 release date", "max_results": 2}),
            ),
            step("extract", json!({"urls": ["https://example.com"]})),
            step(
                "map_site",
                json!({"url": "https://example.com", "limit": 5}),
            ),
            step("crawl", json!({"url": "https://example.com", "limit": 2})),
        ],
        "tinyfish" => vec![
            step("search", json!({"query": "Rust 1.0 release date"})),
            step("fetch", json!({"urls": ["https://example.com"]})),
            step(
                "run_agent",
                json!({"url": "https://example.com", "goal": "Return the page's heading."}),
            ),
        ],
        // Zep's tools take UUIDs Zep made; the run takes them from
        // ZEP_GRAPH_UUID (a user's UUID will do) and ZEP_THREAD_UUID.
        "zep" => vec![
            step(
                "add_data",
                json!({"graph_uuid": "${ZEP_GRAPH_UUID}", "data": "Alice's favourite colour is green."}),
            ),
            step(
                "search",
                json!({"graph_uuid": "${ZEP_GRAPH_UUID}", "query": "Alice favourite colour"}),
            ),
            step(
                "get_graph_context",
                json!({"graph_uuid": "${ZEP_GRAPH_UUID}", "query": "Alice"}),
            ),
            step(
                "add_messages",
                json!({"thread_uuid": "${ZEP_THREAD_UUID}", "messages": [{"role": "user", "name": "alice", "content": "I like green."}]}),
            ),
            step(
                "get_thread_context",
                json!({"thread_uuid": "${ZEP_THREAD_UUID}"}),
            ),
        ],
        _ => vec![],
    }
}

/// `value` with each `${var}` in its strings filled from `vars`, or the
/// environment.
fn fill(value: &Value, vars: &HashMap<&str, String>) -> Value {
    match value {
        Value::String(s) => {
            let mut s = s.clone();
            while let Some(start) = s.find("${") {
                let Some(len) = s[start..].find('}') else {
                    break;
                };
                let name = &s[start + 2..start + len];
                let with = vars
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default();
                s.replace_range(start..start + len + 1, &with);
            }
            Value::String(s)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| fill(v, vars)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), fill(v, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The word after the first `label` in `text`.
fn word_after(text: &str, label: &str) -> Option<String> {
    let rest = &text[text.find(label)? + label.len()..];
    let word: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!word.is_empty()).then_some(word)
}

fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(240) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// Runs `vendor`'s steps; the number that failed.
async fn run(vendor: &ToolVendor, key: String) -> usize {
    let api = vendor.api(key, None);
    let mut vars = HashMap::new();
    let mut failed = 0;
    for s in steps(vendor.id) {
        let args = fill(&s.args, &vars);
        let started = Instant::now();
        let result = vendor.tools.call(&api, s.tool, args).await;
        let took = started.elapsed().as_secs_f32();
        match result {
            Ok(output) => {
                let mut text = String::new();
                for content in &output.content {
                    match content {
                        Content::Text(t) => text.push_str(t),
                        Content::Image { data, mime_type } => {
                            text.push_str(&format!("[image {mime_type}, {} B] ", data.len()))
                        }
                        Content::Audio { data, mime_type } => {
                            text.push_str(&format!("[audio {mime_type}, {} B] ", data.len()))
                        }
                    }
                }
                if let Some((label, var)) = s.capture {
                    match word_after(&text, label) {
                        Some(word) => {
                            vars.insert(var, word);
                        }
                        None => eprintln!("    (no {label:?} in the output to capture)"),
                    }
                }
                println!("  ok   {:<20} {took:5.1}s  {}", s.tool, preview(&text));
            }
            Err(e) => {
                failed += 1;
                println!("  FAIL {:<20} {took:5.1}s  {e}", s.tool);
            }
        }
    }
    failed
}

#[tokio::main]
async fn main() {
    let only: Vec<String> = std::env::args().skip(1).collect();
    let mut failed = 0;
    for vendor in ALL {
        if !only.is_empty() && !only.iter().any(|id| id == vendor.id) {
            continue;
        }
        let Some(key) = vendor.key_from_env() else {
            println!("{}: skipped, no {}", vendor.id, vendor.key_env.join(" or "));
            continue;
        };
        println!("{}:", vendor.id);
        failed += run(vendor, key).await;
    }
    if failed > 0 {
        println!("{failed} failed");
        std::process::exit(1);
    }
}
