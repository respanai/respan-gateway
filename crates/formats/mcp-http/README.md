# MCP over HTTP

`respan-gateway-mcp-http` is the [MCP kind](../../kinds/mcp/README.md) over
HTTP: its format, streamable HTTP, and an MCP server run as a child process,
as an HTTP transport.

- `StreamableHttp::new(path)`: MCP over streamable HTTP as direct
  `Format<ClientIo, Input, Output>` and reversed `Format<ProviderIo, Output, Input>` bindings;
  `SESSION` is its session header. The request shapes the answer: a `POST`
  with no request gets 202 and no body, one with requests an event stream
  (one JSON body if its `Accept` takes no event stream), a `GET` the
  server's own event stream, a `DELETE` 204; a 202 or 204 waits for the
  server's, so its refusal (a 404 for an expired session) reaches the client.
  JSON-only answers contain responses; notifications are omitted, and
  server requests requiring bidirectional interaction are refused. Accept
  `text/event-stream` to receive those independent server messages.
  `.serving(names)` serves clients at `{path}/{name}` for each of `names`
  and nowhere else, so one gateway can front several servers; the caller
  routes by name (the binary's `[tools.<name>]` tables do).
- `StdioTransport::new(program, args)`: an MCP server run as a child
  process per session. `send(Request<Body>)` returns a native response and
  optional write completion; `.connect()` supplies the provider factory.
  Its limits, each with a documented default and reason: `.max_total(n)`
  sessions (64), `.idle(d)` (10 minutes; an
  open `GET` stream counts as activity), `.timeout(d)` for a `POST`'s
  answers (5 minutes; then the child is sent `notifications/cancelled`),
  `.backlog(n)` messages a stream may fall behind (64), `.max_line(bytes)`
  of one message from the child (32 MiB). A session is reached by its
  unguessable id alone. Its failures
  are faults the client's format writes, with their status, `error_code`
  and JSON-RPC code: no session named (400) or none there (404),
  `no_session`, -32003; a second `GET` (409), `busy`, -32004; past a limit
  (503), `too_many_sessions`, -32005; not answered in time (504),
  `timed_out`, -32001. The child gets only the gateway's `PATH` and what
  `.env(key, value)` adds, runs in `.current_dir(dir)` if given, and
  shares the gateway's stderr. It runs in its own process group, stopped
  as MCP asks: stdin closed, then SIGTERM, then SIGKILL to the group, 2
  seconds apart. A session whose `initialize` answer ends before its
  first message is forgotten at once; a request the client cancels is no
  longer waited for.

Used by: [mcp_proxy](../../../examples/rust/mcp_proxy.rs) and the
binary's `[tools.<name>]` servers.

Messages are validated as JSON-RPC envelopes before they can settle a
request. Invalid HTTP provider messages fail `upstream`; stray or malformed
stdio messages are ignored. A 202/204/205 cannot settle a posted request
that still needs a response. Request IDs must be strings or integers;
null IDs are reserved for error responses. An HTTP refusal may omit its
ID and still preserve its JSON-RPC error code and message.
Duplicate request IDs in a batch, and overlapping IDs in a stdio session,
are rejected before forwarding. A stdio backlog of zero is a configuration
fault rather than a channel panic.
Clients must still use unique request IDs across the entire session,
including completed requests; the gateway does not retain an unbounded
history of completed IDs.
Session IDs must contain visible ASCII, and scalar metadata headers must
appear once. Unicode resume cursors are preserved as UTF-8 in
`Last-Event-ID`; SSE ID resets and ignored NUL-containing ID fields follow
the SSE rules.
POST replies are matched to the IDs actually sent, including plugin
rewrites. A POST completes after its last reply without waiting for the
server to close SSE. Unexpected or duplicate replies before completion
fail upstream. GET resumption can receive replies from earlier POSTs.

JSON-only clients and JSON-RPC batches remain supported for compatibility.
MCP 2025-06-18 clients send one message per POST and advertise both JSON
and SSE; batch support is an extension for earlier clients.
