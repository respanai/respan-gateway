# respan-gateway-plugin-mcp-router

`McpRouter` takes the MCP format's requests: core providers of the `mcp`
format are its servers. It routes MCP servers, not models; the `routing`
plugin routes models.

## `/mcp/{name}`: one server

A request to `/mcp/{name}` is targeted at the server `name`, the last
segment of the request's cleaned path. A key limited to some tools (a
request carrying a `ToolScope`) is refused there with `403 tools_limited`: a
server's own endpoint offers all its tools, so such a key reaches them
through `/mcp`, which holds it to them. A server turned off in the shared
`ToolSwitches` is refused with `403 tool_server_off` (a tool turned off is
not: the endpoint relays the server as it is).

## `/mcp`: many servers as one

Given servers, the router answers `/mcp` itself, fronting them as one. A
client sees one server and one session; the gateway holds a session with
each server behind it, opened with the client's and ended with it.

- Tools and prompts are named `{server}__{name}`; a `ToolFilter` says which
  tools are offered, and a request's `ToolScope` limits it further. A server
  or tool turned off in the `ToolSwitches` is neither listed nor callable,
  at once.
- Resources keep their URIs under `mcp://{server}/{uri}`.
- Turning a server off also withholds its prompts, resources and resource
  templates, and refuses prompt and resource requests through `/mcp`.
- Lists are merged in the order the servers are given, with a cursor that
  pages the servers that have more. Each server's cursor is passed back
  unchanged, including an empty string; an absent cursor ends its list.
- The servers' notification streams are merged into the client's.

```rust,ignore
use respan_gateway_plugin_mcp_router::McpRouter;
use respan_gateway_plugin_tool_loop::ToolFilter;

let filter = ToolFilter::new(vec![], vec!["*__delete_*".into()]);
let router = McpRouter::new(vec!["github".into(), "files".into()], filter);
let closer = router.closer();
let gateway = gateway.plugin(closer.plugin()).plugin(router);
// On shutdown or reload, once the gateway's requests are done:
closer.close(&gateway).await;
```

`McpRouter::default()` fronts no servers and routes `/mcp/{name}` alone.
A session at `/mcp` is reached by its unguessable id alone. Give none of the
fronted servers a client's credentials: the sessions are the gateway's. The
binary fronts every tool server at `/mcp` but those with `auth = "forward"`.
