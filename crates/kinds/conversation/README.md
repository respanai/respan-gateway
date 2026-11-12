# Conversation kind

`respan-gateway-conversation` is the conversation kind: the shared LLM
request/event pair (`Input` in, `Output` events out). Plugins read any
conversation through the pair and never see which vendor it came from. Its
formats over HTTP are
[`respan-gateway-conversation-http`](../../formats/conversation-http/README.md)'s.

```rust
use respan_gateway_conversation::{Input, Message, Output, Role};
use respan_gateway_conversation::Usage;
use respan_gateway_core::{ClientContext, Fault, Plugin, Response};
```

The crate exports conversation types and helpers directly from its root. It
is the kind's types alone, depending only on `respan-gateway-core`,
`serde_json` and `bytes`, for plugins that need no I/O. With the `serde`
feature a conversation's messages (`Message`, `Role`, `Content`, `Media`)
are `Serialize` and `Deserialize`, for a host that keeps them, such as a
shared store of the responses plugin.

`Usage` is the token counts a provider reports: input, output, and the
cached, cache-write and reasoning counts that are parts of them. A count
of 0 is one the provider did not report. A format refuses usage it cannot
write, such as cache writes in Gemini.

A reply is a sequence of events, the same for a stream and a whole body, and
an `Output` is one such event, not a whole reply: `Output::Start`, then for each content block a `BlockStart`, its `Delta`s and a
`BlockEnd`, with `Usage` increments, a `Finish` and `Native` (an event the kind
has no form for, in its format's own JSON) among them; `BlockFields` and
`BlockNative` are a block's native fields and events, typed by the block they
are of, so a plugin that renumbers blocks renumbers them too. A `Delta` is a
`Delta::Text`, `Delta::Arguments` or `Delta::Reasoning`, of its block's own
kind, so a plugin need not track what a block holds. An encoder collects the
events into one body or writes them as a stream, so a plugin handles one shape;
`Assembled` folds events for both plugins and whole-body encoders. Its `items()`
keeps text, reasoning, tool blocks and native events together; `blocks()` reads
just the blocks, and `text()` joins only text blocks. Blocks occupy their opening
position, with deltas concatenated; native events retain their arrival order.
This is not the original event stream or an automatic conversion to conversation
history. Assembly checks block consistency; the reader checks stream completion.
`Output::is_content` selects starts, block events and finishes; usage and opaque
events are excluded. Policies may instead select specific variants.

Tools are typed too: a request's `Tool`s and `ToolChoice`, an assistant's
`Content::ToolCall` and its `Content::ToolResult`, and a response's
`Block::ToolCall`, whose arguments stream as `Delta::Arguments` of JSON text.
`Message::text` exposes typed text, explicitly skipping tool calls and other
content; `Message::is_plain` checks for a known role, text parts and no message
native fields. Text-part native fields are allowed.

What the kind has no form for stays as the format that decoded it has it, in a
`Native` on an `Input`, `Message`, `Tool` or `Start`, or a `Content::Native`,
`Output::Native`, `Role::Native` or `Finish::Native`; it is opaque to every
other format, so only that format reads or writes it, and another drops it:
what has meaning across formats belongs here.

Input values are plain data: they keep nothing of the bytes they were
decoded from, and compare by what they say. A request is always written
again from its value, and a response from its events: a format's promise is
that a request of its own round-trips.

`Input.options: GenerationOptions` groups the output token limit, sampling
(`temperature`, `top_p`), stop sequences, tool selection, parallel
calls, structured-output requirements, the service tier and `reasoning`.
What few formats have stays in their own native: `top_k` (Anthropic's and
Gemini's), `verbosity` and a JSON schema's `strict` (OpenAI's two formats),
and Anthropic's effort `max`.

`Reasoning` says how hard the model thinks: a named `Effort` (`Off`,
`Minimal`, `Low`, `Medium`, `High`, `XHigh`), a `budget_tokens` limit, and `summary`
(whether the thinking is returned). Vendors state either an effort or a
budget, and the two do not convert exactly: an effort buys different
thinking from different models. So an effort travels between formats that
have efforts, a budget between formats that have budgets, only `Off` is both,
and a format that has only one writes the nearest of the other, by the
budgets the gateway gives a Claude's efforts (a plugin that wants another policy sets
the one the provider takes, in a filter). A vendor's value the kind has no
name for (such as an effort `max`) stays native to its format.
`Content` includes text, reasoning, tool calls/results, images, audio, documents,
and native values. A `Media` has a MIME type, URL or bytes, optional name, and
optional native fields. Formats do not fetch URLs.

Transport preferences are the formats', not `Input`'s, such as whether a
provider is asked to stream.

`Finish` records why the model stopped. Delivery completes when the event stream
ends; usage and native events may arrive after `Finish`. A `Finish::Native`
reason, one the kind has no form for, may carry a `fallback`
(`Finish::native_or`), the kind's nearest reason, which another format writes.
