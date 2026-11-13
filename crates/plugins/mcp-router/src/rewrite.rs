//! What `/mcp` puts on what it passes: the names and URIs a client
//! sees, the routing by them, and the answers it builds.

use respan_gateway_mcp::{Message, Outcome, Output, RpcError};
use serde_json::{Map, Value, json};

use super::run::Run;
use super::{SCHEME, SEP};

/// that server knows it by, put in `params`.
pub(super) fn by_name(tool: bool) -> impl Fn(&Run, &mut Value) -> Result<String, String> {
    move |run, params| {
        let unknown =
            |name: &str| format!("unknown {} '{name}'", if tool { "tool" } else { "prompt" });
        let name = params["name"].as_str().unwrap_or_default().to_owned();
        let (server, own) = split(&run.inner.servers, &name).ok_or_else(|| unknown(&name))?;
        if !run.inner.switches.server_on(server) || (tool && !run.offers(server, own, &name)) {
            return Err(unknown(&name));
        }
        let (server, own) = (server.to_owned(), own.to_owned());
        params["name"] = json!(own);
        Ok(server)
    }
}

/// The server a resource's URI names, and its own URI, put in `params`.
pub(super) fn by_uri(run: &Run, params: &mut Value) -> Result<String, String> {
    let uri = params["uri"].as_str().unwrap_or_default().to_owned();
    let (server, own) = unprefix(&run.inner.servers, &uri)
        .filter(|(server, _)| run.inner.switches.server_on(server))
        .ok_or_else(|| format!("unknown resource '{uri}': it is `{SCHEME}<server>/<uri>`"))?;
    let (server, own) = (server.to_owned(), own.to_owned());
    params["uri"] = json!(own);
    Ok(server)
}

/// The server a name `{server}__{name}` starts with (the longest, if
/// several do) and the rest.
pub(super) fn split<'a>(servers: &'a [String], name: &'a str) -> Option<(&'a str, &'a str)> {
    let found = servers.iter().filter_map(|server| {
        let rest = name.strip_prefix(server.as_str())?.strip_prefix(SEP)?;
        (!rest.is_empty()).then_some((server.as_str(), rest))
    });
    found.max_by_key(|(server, _)| server.len())
}

/// `uri` of `server` as the client has it.
pub(super) fn prefixed(server: &str, uri: &str) -> String {
    format!("{SCHEME}{server}/{uri}")
}

/// The server a prefixed URI names, and its own URI.
pub(super) fn unprefix<'a>(servers: &'a [String], uri: &'a str) -> Option<(&'a str, &'a str)> {
    let (server, own) = uri.strip_prefix(SCHEME)?.split_once('/')?;
    let server = servers.iter().find(|s| *s == server)?;
    (!own.is_empty()).then_some((server.as_str(), own))
}

pub(super) fn prefix_field(server: &str, value: &mut Value, key: &str) {
    if let Some(Value::String(uri)) = value.get_mut(key) {
        *uri = prefixed(server, uri);
    }
}

/// A content item of a tool's or prompt's result: the URI it links to or
/// holds, prefixed.
pub(super) fn prefix_content(server: &str, item: &mut Value) {
    match item["type"].as_str() {
        Some("resource_link") => prefix_field(server, item, "uri"),
        Some("resource") => prefix_field(server, &mut item["resource"], "uri"),
        _ => {}
    }
}

/// An output of `server`'s answer to a routed request, as the client has
/// it: URIs prefixed in a result, requests of the server's own left out
/// (the client has none to answer them), and no event id of the server's.
pub(super) fn passed(server: &str, mut output: Output) -> Output {
    output.session = None;
    output.event_id = None;
    output.event = None;
    let messages = std::mem::take(&mut output.messages);
    output.messages = messages
        .into_iter()
        .filter_map(|m| match m {
            Message::Request { .. } => None,
            Message::Notification { .. } => notified(server, m),
            m => Some(rewrite_result(server, m)),
        })
        .collect();
    output
}

/// A response with the URIs in its result prefixed: those of
/// `resources/read`'s `contents`, and of the content of a tool's result or
/// a prompt's messages.
pub(super) fn rewrite_result(server: &str, mut m: Message) -> Message {
    if let Some(result) = m.result_mut() {
        for content in each(result, "contents") {
            prefix_field(server, content, "uri");
        }
        for item in each(result, "content") {
            prefix_content(server, item);
        }
        for message in each(result, "messages") {
            if let Some(content) = message.get_mut("content") {
                prefix_content(server, content);
            }
        }
    }
    m
}

/// The items of the list `value` holds at `key`, if it holds one: looked
/// up, not indexed, which would add a `null` where there is none.
fn each<'a>(value: &'a mut Value, key: &str) -> impl Iterator<Item = &'a mut Value> {
    value
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

/// A notification of `server`'s as the client has it: `resources/updated`
/// with the URI prefixed.
pub(super) fn notified(server: &str, mut m: Message) -> Option<Message> {
    if let Message::Notification { method, params, .. } = &mut m
        && method == "notifications/resources/updated"
        && let Some(params) = params
    {
        prefix_field(server, params, "uri");
    }
    Some(m)
}

/// What a server tells on its own stream that the client is told: that its
/// lists changed, a resource changed, or a log message. The rest is meant for
/// a request of its own.
pub(super) fn told(server: &str, output: Output) -> Option<Output> {
    let messages: Vec<Message> = output
        .messages
        .into_iter()
        .filter(|m| {
            matches!(
                m.method(),
                Some(
                    "notifications/tools/list_changed"
                        | "notifications/prompts/list_changed"
                        | "notifications/resources/list_changed"
                        | "notifications/resources/updated"
                        | "notifications/message"
                )
            ) && m.request_id().is_none()
        })
        .filter_map(|m| notified(server, m))
        .collect();
    (!messages.is_empty()).then(|| Output::new(messages))
}

/// The capabilities a client sees: those any server has.
pub(super) fn merge_capabilities(all: &[Value]) -> Value {
    let any = |capability: &str, flag: &str| {
        all.iter()
            .any(|c| c[capability][flag].as_bool().unwrap_or(false))
    };
    let has = |capability: &str| all.iter().any(|c| c.get(capability).is_some());
    let mut merged = Map::new();
    for capability in ["tools", "prompts", "resources"] {
        if !has(capability) {
            continue;
        }
        let mut flags = Map::new();
        for flag in ["listChanged", "subscribe"] {
            let applies = flag == "listChanged" || capability == "resources";
            if applies && any(capability, flag) {
                flags.insert(flag.to_owned(), json!(true));
            }
        }
        merged.insert(capability.to_owned(), Value::Object(flags));
    }
    Value::Object(merged)
}

/// The answer to request `id`: a result, or an error code and message.
pub(super) fn reply(id: &Value, outcome: Result<Value, (i64, String)>) -> Output {
    let json = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, why)) => respan_gateway_mcp::error(id.clone(), code, &why),
    };
    let message = Message::try_from(json).unwrap_or_else(|_| Message::Response {
        id: None,
        outcome: Outcome::Error(RpcError {
            code: -32603,
            message: "the answer could not be built".into(),
            data: None,
            extensions: Map::new(),
        }),
        extensions: Map::new(),
    });
    Output::new(vec![message])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_tool_switched_off_is_neither_listed_nor_called() {
        use respan_gateway_plugin_tool_loop::{ToolFilter, ToolSwitches};
        use std::sync::Arc;
        let switches = Arc::new(ToolSwitches::default());
        let router = crate::McpRouter::with_switches(
            names(&["gh"]),
            ToolFilter::default(),
            switches.clone(),
        );
        let run = Run {
            inner: router.0.clone(),
            calls: Arc::default(),
            scope: None,
        };
        let listed = |tool: &str| run.namespaced("gh", "tools", json!({ "name": tool }));
        let called =
            |tool: &str| by_name(true)(&run, &mut json!({ "name": format!("gh__{tool}") }));
        assert!(listed("search").is_some() && called("search").is_ok());
        switches.set_tool("gh", "search", false);
        assert!(listed("search").is_none() && called("search").is_err());
        assert!(listed("read").is_some() && called("read").is_ok());
        switches.set_server("gh", false);
        assert!(listed("read").is_none() && called("read").is_err());
    }

    #[test]
    fn a_name_belongs_to_the_longest_server_it_starts_with() {
        let servers = names(&["a", "a__b", "files"]);
        assert_eq!(split(&servers, "files__read"), Some(("files", "read")));
        assert_eq!(split(&servers, "a__b__x"), Some(("a__b", "x")));
        assert_eq!(split(&servers, "a__c"), Some(("a", "c")));
        assert_eq!(split(&servers, "a__"), None);
        assert_eq!(split(&servers, "nothing"), None);
    }

    #[test]
    fn a_result_is_rewritten_with_nothing_added() {
        let link = json!({ "type": "resource_link", "uri": "file:///a" });
        let said = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [link] } });
        let m: Message = serde_json::from_value(said).expect("a response");
        let result = rewrite_result("fs", m)
            .result_mut()
            .cloned()
            .expect("a result");
        let link = json!({ "type": "resource_link", "uri": "mcp://fs/file:///a" });
        assert_eq!(result, json!({ "content": [link] }));
    }

    #[test]
    fn a_resource_uri_says_its_server() {
        let servers = names(&["fs", "gh"]);
        let uri = prefixed("fs", "file:///a/b.txt");
        assert_eq!(uri, "mcp://fs/file:///a/b.txt");
        assert_eq!(unprefix(&servers, &uri), Some(("fs", "file:///a/b.txt")));
        assert_eq!(unprefix(&servers, "mcp://other/x"), None);
        assert_eq!(unprefix(&servers, "file:///x"), None);
        assert_eq!(unprefix(&servers, "mcp://fs/"), None);
    }

    #[test]
    fn capabilities_are_what_any_server_has() {
        let merged = merge_capabilities(&[
            json!({ "tools": {} }),
            json!({ "tools": { "listChanged": true }, "resources": { "subscribe": true } }),
        ]);
        assert_eq!(
            merged,
            json!({ "tools": { "listChanged": true }, "resources": { "subscribe": true } })
        );
        assert_eq!(merge_capabilities(&[json!({})]), json!({}));
    }

    #[test]
    fn a_servers_own_stream_tells_only_what_changed() {
        let note = |method: &str, params: Value| {
            let json = json!({ "jsonrpc": "2.0", "method": method, "params": params });
            Message::try_from(json).unwrap_or_else(|e| panic!("{e}"))
        };
        let output = Output::new(vec![
            note("notifications/tools/list_changed", json!({})),
            note("notifications/progress", json!({ "progress": 1 })),
            note(
                "notifications/resources/updated",
                json!({ "uri": "file:///x" }),
            ),
        ]);
        let told = told("fs", output).unwrap_or_default();
        assert_eq!(told.messages.len(), 2);
        assert_eq!(
            told.messages[1].params().map(|p| p["uri"].clone()),
            Some(json!("mcp://fs/file:///x"))
        );
    }

    #[test]
    fn uris_in_a_result_are_prefixed() {
        let json = json!({ "jsonrpc": "2.0", "id": 1, "result": {
            "content": [
                { "type": "resource_link", "uri": "file:///a" },
                { "type": "resource", "resource": { "uri": "file:///b", "text": "t" } },
                { "type": "text", "text": "file:///c" },
            ],
            "contents": [{ "uri": "file:///d", "text": "t" }],
        }});
        let m = Message::try_from(json).unwrap_or_else(|e| panic!("{e}"));
        let Message::Response {
            outcome: Outcome::Success(result),
            ..
        } = rewrite_result("s", m)
        else {
            panic!("a result")
        };
        assert_eq!(result["content"][0]["uri"], "mcp://s/file:///a");
        assert_eq!(result["content"][1]["resource"]["uri"], "mcp://s/file:///b");
        assert_eq!(result["content"][2]["text"], "file:///c");
        assert_eq!(result["contents"][0]["uri"], "mcp://s/file:///d");
    }
}
