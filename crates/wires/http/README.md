# HTTP bindings

`respan-gateway-http` connects native HTTP messages to core's streams and sinks.
It is a wire: it knows no kind. It is for format authors; plugins need only
core and a kind. The conversation formats live in
`respan-gateway-conversation-http`, MCP's in `respan-gateway-mcp-http`;
WebSockets are `respan-gateway-ws`, on top of this crate.

A format implements core directly in two orientations:

- `Format<ClientIo, Input, Output>` accepts a request and writes its response.
- `Format<ProviderIo, Output, Input>` sends a request and reads its response.

`ClientIo` owns one accepted exchange. `ProviderIo` owns one provider attempt.
Their `bind`s mirror each other: a format gives each a reader and a writer,
in the order they happen on its edge, and the binding drives them over the
body.

| | `ClientIo::bind` | `ProviderIo::bind` |
| --- | --- | --- |
| first | the request's `Decoder`, given its head | the request's head, and its body's `Encoder` |
| then | the response's sink, from a `Reply` and an `Encoder` | the answer's `Decoder`, given its head, or `None` for no body |
| on failure | `with_error`: write the error body (plain text by default) | `with_refusal`: read the error answer (`provider_fault` by default) |

An `Encoder<T>` writes values as frames and a `Decoder<T>` reads frames as
values; a closure is either. `Sse` reads a body's server-sent events, one at
a time, and `Whole` all of a body at once; `All` writes all values at once.

`path::escape` writes a model id or any text as one path segment.

The bindings own everything else: response handoff, backpressure and
completion, whole or streamed uploads, what a proxy strips from a call, a
fault's status and `Retry-After`, refused answers read whole into a fault
before the decoder could see them, and no body where HTTP has none. An answer
succeeds by HTTP's rule, a 2xx or an asked-for 101, unless the format says
otherwise (`ProviderIo::with_success`). There is no additional HTTP format trait
or implementation macro.

The passthrough format, which relays traffic of no kind, is the
`respan-gateway-passthrough-http` crate's: this crate has no format.

## A small format

This format sends UTF-8 questions to `POST /echo` and receives byte answers:

```rust
use respan_gateway_http as h;
use h::bytes::Bytes;
use h::http::{Method, Uri};
use h::{Encoder, Whole};
use respan_gateway_core::{Connection, Fault, Format, Result};

#[derive(Clone, Copy)]
struct Echo;

impl Format<h::ClientIo, String, Vec<u8>> for Echo {
    fn name(&self) -> &str { "echo" }

    fn matches(&self, io: &h::ClientIo) -> bool {
        io.request().method() == Method::POST
            && io.request().uri().path() == "/echo"
    }

    fn bind(&self, io: h::ClientIo) -> Result<Connection<String, Vec<u8>>> {
        Ok(io.bind(
            |_| Box::new(|bytes: Bytes, out: &mut Vec<String>| {
                let question = String::from_utf8(bytes.to_vec())
                    .map_err(|_| Fault::bad_request("expected UTF-8"))?;
                out.push(question);
                Ok(())
            }),
            |reply| {
                let head = reply.head();
                Ok(reply.write(head, Box::new(|answer: &Vec<u8>, out: &mut Vec<Bytes>| {
                    out.push(Bytes::copy_from_slice(answer));
                    Ok(())
                })))
            },
        ))
    }
}

impl Format<h::ProviderIo, Vec<u8>, String> for Echo {
    fn name(&self) -> &str { "echo" }

    fn bind(&self, io: h::ProviderIo) -> Result<Connection<Vec<u8>, String>> {
        Ok(io.bind(
            |head, question: Option<&String>| {
                question.ok_or_else(|| Fault::config("echo takes one question"))?;
                head.method = Method::POST;
                head.uri = Uri::from_static("/echo");
                Ok(Box::new(|question: &String, out: &mut Vec<Bytes>| {
                    out.push(Bytes::from(question.clone()));
                    Ok(())
                }))
            },
            |_head, _sent| Some(Box::new(Whole::new(|bytes: Bytes, out: &mut Vec<Vec<u8>>| {
                out.push(bytes.to_vec());
                Ok(())
            }))),
        ))
    }
}

// An encoder works synchronously, without a connection or a runtime.
let mut encoder: Box<dyn Encoder<String>> =
    Box::new(|value: &String, out: &mut Vec<Bytes>| {
        out.push(Bytes::from(value.clone()));
        Ok(())
    });
let bytes = encoder.encode_all(&["hello".to_owned()])?;
assert_eq!(bytes, Bytes::from_static(b"hello"));
# Ok::<(), Fault>(())
```

For stateful output, implement `Encoder<T>`: `encode` writes one value as
zero or more frames, the chunks of a body or the messages of a live
session, and `end` writes final success or error framing: if ending well fails,
`end` is called once more with that failure, to write it as error framing.
The next value is taken only once its frames are read. For stateful input, implement
`Decoder<T>`: `decode` reads one frame (an event behind `Sse`, the whole body
behind `Whole`), `end` flushes what it held, `start` gives what comes before
any frame, and `done` says it read all it will, so the rest of the body goes
unread. Neither performs I/O, chooses headers or controls when a response
opens.

## Bodies and preparation

`Body` represents complete bytes or a live byte stream and implements both
`Stream<Item = Result<Bytes, Fault>>` and `http_body::Body`. Network chunks are
not semantic messages: accepted ordinary HTTP requests are collected, up to
the adapter's limit, before opening, while explicitly live/message ingress
remains streaming.

Use the native `http::request::Parts` and `http::response::Parts` for heads;
`http::Request::new(()).into_parts().0` and
`http::Response::new(()).into_parts().0` make empty heads. For an external
HTTP body, `http_body_util::BodyExt` supplies the standard data-frame stream:
`request.map(|body| Body::stream(body.map_err(Into::into).into_data_stream()))`
(and likewise for a response). This drops trailers, as gateway wire frames
carry data only. `BodyExt::collect(body).await?.to_bytes()` collects a streamed
body, retaining its frames until EOF. Client request accumulation instead
folds frames into `BytesMut`, releasing each before polling again so pooled
buffer producers can progress. A `Body` with `full_bytes()` can be cloned
without polling.

Keep `Body::full` for finite, inspectable bytes, and `Body::stream`
for pull-driven frames. A `Body` carries no limit of its own: the most
bytes a reader holds of a provider's answer at once is its `ProviderIo`'s,
the `limit` given to `connect`.

A client's `Decoder` reads a whole request, as one frame, into its one
input, lazily, or each live frame as it comes. A provider's answer decoder
is given the call as sent: the bytes of its one input, or an empty body for
live input, which streamed. The outgoing binding encodes
the request:

- One input is encoded, and finalized, synchronously. Failure occurs
  before the provider attempt is marked sent.
- Live input stays caller-owned. The sink encodes each submitted value with
  byte-reader backpressure, not an unbounded event queue.

The binding then snapshots the actual sent head and whole bytes for the
response decoder. Client negotiation and error writing instead receive the
client's request as it came, its `Original`, which a format may read too, such
as for the headers of its spec a call of its own carries. A `ClientIo`
publishes its `Original` and the client's `Credential` apart
(`ClientIo::extensions`); credentials must be copied deliberately.

A call marked `Relayed`, to a provider of its client's own format, gives
the client its answer's head, and its key to a transport with none of its
own; otherwise `Reply::head()` is a fresh success head. Provider failures
carry `ProviderError` for inspection; only an explicit `RawReply` opts into
writing their original body, which a `Relayed` call's faults get.

The incoming response handle constructs real sinks:

- `write` normally waits for bytes or completion before publishing the head.
  A live session's (`ClientIo::accept_live`) retains immediate response
  opening, so a duplex peer can send without waiting for its own first reply.
- `open` publishes a quiet stream immediately, optionally with a prefix.
- Either writes no body where HTTP has none (`bodiless`: to a `HEAD`, or a
  204, 205 or 304): the head goes at once, the encoder is never called, and a
  source not yet read is stopped. Stopping retains source ownership until
  delivery settles.

Finite offline encoding uses an encoder's `encode_all`, and a decoder's own
`decode` and `end` read finite bytes synchronously. Providers expose
finite response conversion helpers for applications such as durable jobs.

## Hosting

Hosting is not a wire's job. `handle(gateway, request, limit)` is the client
edge's entry point, which any server calls: it publishes the request as an
HTTP adapter does and answers with the response. `handle_live` is the same
for a session, each body frame one message, as `respan-gateway-ws` uses it. `respan-gateway-serve`
is the Axum server that calls it, for plain HTTP and WebSocket upgrades alike.

## Backends and completion

`connect(send, limit)` creates a provider factory from an async function
accepting `http::Request<Body>` and returning
`(http::Response<Body>, Option<Completion>)`; `limit` is the most bytes a
reader holds of one answer at once, past which it fails `upstream`. The
Reqwest backend, `HttpTransport`, is `respan-gateway-http-transport`'s, and
supplies a `.connect()` convenience.
Backends can share connection pools; each factory invocation owns a fresh
attempt. A transport prepares a call with `prepare_call` against a base URL
that `base_url` checked; the binding has already stripped what a proxy must not
pass on, so a custom transport cannot forward it by omission.

`Some(completion)` reports outgoing writing independently of the response:
response headers and response EOF do not shorten that wait. `None` means the
sender accepted the outgoing handoff; a retained byte reader may still pull live
input. The binding coordinates that reader with response termination. Refusal
cancels unfinished upload rather than waiting for completion.

Flush advances quiet transfers; close finalizes framing and waits until the
transport has taken every frame (its `Completion`, if it gave one, says when
they were written). Source faults may be encoded into terminal framing but cannot turn
an interrupted upload into successful EOF. Dropping a response cancels its work.

An answer succeeds by HTTP's rule, 2xx or a requested 101, unless the format
says otherwise (`ProviderIo::with_success`): strict WebSocket formats require
the handshake. An answer is refused if and only if it is no success, with the fault its
format's error reader makes of its whole body, read up to the limit. An
error body over the limit, or broken off, is the gateway's 502, keeping the
answer's `Retry-After`. A success opens at its head, and a fault its body
says later fails it. Parsing helpers preserve
semantic decoder failures separately from broken wire bodies and avoid
unbounded prefetch across wire frames.
