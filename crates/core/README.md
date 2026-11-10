# respan-gateway-core

The neutral kernel. It runs each request through the registered plugins and
streams the response back, and knows nothing of HTTP, providers,
credentials or message kinds. It is also the plugin-author API: plugins,
protocols and adapters all depend on it directly.

- `Gateway`: built by value (`Gateway::new().provider(..)?.client(..)?
  .plugin(..).target(..)?`) and handed one transport at a time
  (`handle(io, extensions)`); `idle()` waits for every request and end hook.
- `Provider`: a provider to add, as a value: `Provider::new(name, format,
  transport)`, with its own plugins (`.plugin(..)`).
- `Format<T, R, W>`: a protocol bound to an owned transport `T`; `bind`
  returns a `Connection`, the receive future of `R` and the `EventSink` of
  `W`. Registration decides the edge: a client format receives a kind's
  `Input` and sends its `Output`, a provider format the reverse.
- `Plugin`: `on_client` and `on_client_end` once per request,
  `on_provider` and `on_provider_end` once per call, in registration order.
  `Provider::plugin` gives one to a single provider instead: its
  provider-edge hooks run on that provider's calls alone, after the
  gateway's plugins, as part of the provider.
- `Filter<T>`: edits a kind's inputs or outputs as they stream; its type
  says which. `ClientContext::wrap` is for work that awaits.
- `Call` / `Response`: a plugin's call to a provider (`cx.call`), and what
  a call or `Response::stream` gives back: streamed, read or held.
- `Fault`: the one error: a message, a `FaultKind` saying who it is on
  (`Client`, `Provider`, `Gateway`, or `Abandoned` when no one waits), a
  code saying what went wrong (`fault.code()`: its `ErrorCode`, such as
  `not_found`, `truncated`, `config` or `internal`, else its kind's), and
  typed extensions (`fault.with(..)`, `fault.get::<T>()`) any protocol can
  render: `RetryAfter`, `ErrorCode` and `Details`. `Fault::upstream(..)`
  and the like make one.
- `NativeFields { format, fields }` preserves unsupported fields on a typed
  value; `NativeValue { format, value }` an entire unsupported value. The
  format ID names the wire schema, not a provider deployment, so formats can
  reject unsafe cross-format forwarding. Neither carries transport state.

Core defines no request/event kinds or `Format` implementations. Each kind
names its pair `Input` / `Output`, in the crate that owns it:
[`respan-gateway-conversation`](../kinds/conversation/README.md),
[`respan-gateway-realtime`](../kinds/realtime/README.md),
[`respan-gateway-decision`](../kinds/decision/README.md),
[`respan-gateway-media`](../kinds/media/README.md) (one kind per operation), and
[`respan-gateway-mcp`](../kinds/mcp/README.md) (envelopes around its JSON-RPC
`Message`). Direction is
gateway-wide: `Input` travels from client to provider, `Output` from
provider to client. Use a crate alias when working with several kinds.

See the [plugin guide](../../docs/plugins.md) for how to write a plugin on it.
