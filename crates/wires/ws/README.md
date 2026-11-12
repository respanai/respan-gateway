# respan-gateway-ws

WebSocket sessions on the gateway, as neutral building blocks. A WebSocket is
an HTTP Upgrade exchange on the HTTP wire: the call's head is the upgrade
request, the answer's head is the `101`, and the two bodies are streams of
frames, **one body item per text frame**. This crate is a wire: it knows
no kind and reads no provider's events. The realtime kind's formats over
it, one per spec (OpenAI Realtime, Gemini Live, and Responses over
WebSocket), are
[`respan-gateway-realtime-ws`](../../formats/realtime-ws/README.md)'s, and
the vendors that speak them
[`respan-gateway-vendors`](../../vendors/README.md)'s.

## API

- `WsTransport::new(base_url)`: an outbound native HTTP sender to a `ws://` or
  `wss://` provider. It prepares a call as `HttpTransport` does
  (`respan_gateway_http::call::prepare_call`), so a client's key passes only where
  a plugin put it back. `.with_headers(map)` sets the transport's own keys,
  and `.with_limits(limits)` its `Limits`. The provider has 10 s to answer the upgrade,
  a refusal's body included.
- `.with_binary_text(true)` accepts a provider's binary messages containing
  UTF-8 text, such as Gemini Live's JSON. These become text messages toward
  the client; invalid UTF-8 is a fault. Disabled by default.
- `CloseCode(u16)`: an extension on a `Fault` that picks the code the client is
  closed with, in place of 1011. A code no Close may carry (1004 to 1006,
  1015 to 2999, outside 1000 to 4999) is sent as 1011.
- `ProviderClose(u16)`: the code a provider closed with, other than 1000, on
  the `upstream` fault the transport ends the answer with. The client is told
  it only as a `CloseCode`, which the transport adds on a call `Relayed`
  (`Relay`), or a plugin sets.
- `Limits { max_message, close_wait, close_send, ping_after }`: what an edge
  takes, with defaults and their reasons on the type: 16 MiB messages, 5 s
  for the other side's part of a close, 1 s for a Close to leave, a ping
  after 30 s of quiet.
- `serve(gateway, head, upgraded, limits, stop)`: a client's upgrade through
  the gateway, for an adapter that hosts sessions. `serve` documents how
  each end of a session reaches the client and the provider.

Hosting is `respan-gateway-serve`'s: its router serves each upgrade as a
session through `serve`, and `stop` closes every open session 1001, for a
graceful shutdown.

Both edges pump a session with the same code. Any other format on the HTTP
wire works with the transport and the adapter, such as `passthrough`.

## The transport

- The call's body ends clean: the provider is sent Close 1000, and its end,
  however it comes within `close_wait`, ends the answer clean.
- The call's body ends `Err`: the provider is sent Close (1001 for a client
  that left, else the fault's `CloseCode` or 1011) within `close_send`, and
  the answer ends with the fault. The answer's body dropped: Close 1001.
- The provider's Close 1000 (or no code) ends the answer clean; another code
  is an `upstream` fault with its `ProviderClose`, its code and reason in
  the fault's message, and a 400 for 1003, 1007, 1008 or 1009 (the provider
  refused what it was sent). The client's Close carries the fault's message
  as its reason, cut to 123 bytes: a provider's reason text among it.
- The provider dropping with no Close, or silent through a ping, is
  `truncated`; a rejected binary frame or a message over `max_message` is
  `upstream`, and the provider is sent Close 1003 or 1009.
- A provider that cannot be reached, or does not answer the upgrade within
  the handshake timeout (a refusal's body included), is `upstream`. A refusal
  of the upgrade is an answer with its body, which the format reads whole,
  up to `max_message`, as any error answer; a body that breaks off or is
  malformed is `truncated`.

## Limits

- **Text messages by default.** A provider's binary frame is a fault unless
  `with_binary_text(true)` enables UTF-8 normalization, as
  `GeminiLive.transport(..)` does for Gemini Live's JSON in
  binary frames. A client's binary frame is `bad_request`, closed 1003.
  Ping and pong are answered by each hop's library and never reach a
  plugin.
- **HTTP/1.1 upgrades only** (no WebSockets over HTTP/2), no
  `permessage-deflate` (the extension header is not passed on or negotiated).
- **Backpressure holds.** A frame moves from a socket to the next only as the
  next side takes it: one frame in flight per hop, plus the sockets' own
  buffers. A client that does not read stalls the provider after about 6 MiB,
  and a provider that does not read stalls the client after about 7 MiB.
- **Keep-alive, no idle limit**: a peer quiet for `ping_after` while read is
  pinged, and quiet that long again is gone, on both edges. A quiet session
  whose peers answer pings lasts; a limit on its length is a plugin's, and
  shutdown is the adapter's `stop`.
- **`wss` uses the system's root certificates** and the `aws-lc-rs` provider,
  named in the code because the workspace's dependencies enable both `ring`
  and `aws-lc-rs`, which makes rustls's own choice panic. Proxies are the
  environment's, as for `HttpTransport`.
- No reconnection or session resumption.

## Design notes

The kernel routes events; this integration binds owned `ClientIo` and
`ProviderIo` values to native `Request<Body>` and `Response<Body>`, so a frame
is a body item. `WsTransport::connect()` supplies the per-call provider
factory. Its `send` returns a separate completion future: response EOF alone
cannot establish outgoing delivery success. Client sessions explicitly
select live input; a streamed HTTP body alone does not turn a finite request
into a session. Where that shows:

- **Frame kinds.** A body item carries text bytes, so its original frame kind
  is lost. Opt-in binary UTF-8 normalization supports JSON protocols; raw
  binary audio would require a format that carried each event's frame kind.
- **Close mapping.** A close is only a clean end, or an `Err` with typed extensions
  (`ProviderClose`, `CloseCode`) on the fault. The client's close code and
  reason cannot cross; the provider's reason text does, in the fault's message.
- **The Upgrade dance.** A call keeps `Upgrade`, `prepare_call` gives it
  `Connection: upgrade`, and the transport leaves the handshake's own
  `Sec-WebSocket-Key`, `-Version` and `-Extensions` to the WebSocket library.
  The adapter answers with the `101`'s other headers, and its subprotocol if
  the client offered it and it is no key.

Used by: the [binary](../../../apps/gateway/src/main.rs), and the
[realtime examples](../../../examples/recipes/realtime.rs).
