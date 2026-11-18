# respan-gateway-serve

Hosts a `Gateway` with Axum. Hosting is not a wire's job: the HTTP and
WebSocket wires bind protocols to the kernel, and this crate gives them a
server.

`router(gateway)` is an Axum router that sends every request to the gateway
through the HTTP wire's `handle`, its body read whole up to 32 MiB (413
`too_large` past it, 400 `bad_request` if it breaks off). A WebSocket upgrade
(`Upgrade: websocket`, HTTP/1.1) is a session instead, through
`respan_gateway_ws::serve`. Formats decide what each request is.

`Host::new(gateway)` configures that router before `.router()`:

- `.limit(|head| bytes)`: the body limit per request, from its head.
- `.limits(Limits)`: what either edge of a session takes.
- `.stop(future)`: when it completes, every open session is closed 1001, so a
  server's graceful shutdown (`axum::serve(..).with_graceful_shutdown`) drains
  them too.

`ResponsesWebSocket` is a layer for any HTTP service, such as this router
with a host's own layers: OpenAI's Responses WebSocket mode, the socket
accepted at a path ending in `/responses` and each `response.create` sent
within as an ordinary streamed `POST` to it, its SSE events written back as
frames. Install it outermost, so every layer runs per turn.

Await `gateway.idle()` after the server returns to let in-flight requests end.
