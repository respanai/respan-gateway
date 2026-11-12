# respan-gateway-plugin-tool-loop

`ToolLoop` runs tools inside chat requests, for every chat format and
provider, as a plugin over the conversation kind, and inside live sessions,
over the realtime kind. A `Tools` runner opens a
`Toolbox` per request (`list` the tools to offer, `call` one by name with
JSON arguments, answering an `Outcome` whose failure the model reads). The
tool loop adds the tools to the request, calls the target, and while the reply
calls only those tools runs them (at most `parallel` at once, 4 by default,
and their answers within `max_results` bytes in all) and calls again; the client gets
one reply with the usage summed. It stops at a reply with no such call, at a
call of a client's own tool (returned to the client; the gateway's calls in it
are left out), at `max_steps` (the last call forbids tools) and at `timeout`.
Register it after the plugins that route and edit requests, since it answers
the request itself. It calls the request's target alone, as routing set it
(a rule's first candidate): a request it runs gets none of its route's
fallback, retries or hedge, which routing's `Dispatch` would have given:

```rust,ignore
let tool_loop = ToolLoop::new(MyTools) // impl Tools
    .max_steps(8)
    .tool_timeout(Duration::from_secs(60))
    .when(|cx, model| model.is_some_and(|m| m.starts_with("gpt")))
    .observe(|cx| Some(sink_for(cx))); // each step and tool call, reported
let gateway = gateway.plugin(Routing::new(routes)).plugin(tool_loop);
```

Each model call carries `Step(n)` in its extensions; a runner that
calls tools through the gateway marks those calls with `ToolHop` so
other plugins can tell them apart. A request's `ToolScope` limits the tools
offered further; one that names none offers none. `ToolSwitches` turns
servers and tools off and on while the gateway runs, for a `Tools` to
consult. `ToolLoop::flag_errors_for` says which
providers can be sent a result flagged as a failure (others get `Error: `
text). What the model said goes back in the next step as the kind holds it,
reasoning and native fields included (a signed Anthropic `thinking` block, a
Gemini `thoughtSignature`); native events of later steps are renumbered with
their blocks. The timeout fails the request with `tool_loop_timeout`, a 504.
Used by the binary's `[plugins.tool_loop]`, with MCP servers as the tools.

A live session (the realtime kind) gets the same tools: they are added to
each configuration of the session that sets tools, and a call of one is run
while the session goes on, its answer sent back and the model asked to
respond. The client sees neither the call, nor its answer, nor a response
that said nothing else; what a format says of the call only in its own
events (OpenAI's items and argument deltas) is left out with it, as is any
object naming one of the gateway's tools or carrying one of its calls' ids.
Its `when` predicate sees no model (`None`): a host reads a session's from its
extensions.
