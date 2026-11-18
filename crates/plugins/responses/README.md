# Responses plugin

`respan-gateway-plugin-responses` (`Responses`) makes a Responses client's
`previous_response_id` work when it is translated to a provider that keeps
nothing (Anthropic, Gemini, Chat Completions): each response is given an id
the gateway mints (`resp_…`) and kept as its turn, beside the response it
followed, and a follow-up naming it is sent the whole conversation. A
request that says `store: false` is not kept; OpenAI's own Responses keep
theirs.

What is kept is reached by its id alone, which none can guess, in a `Store`: `Memory` by default, each turn for `keep`
after it was last read, up to about 256 MiB, the stalest dropped first. It
is one gateway's: not across a restart, nor shared between gateways, so a
client whose requests reach several gateways must reach the one that kept
its conversation, or use a `Store` they share (`Responses::with_store`).
A turn is read before the call it is sent on (`Store::get` is async), so a
shared store may answer over the network; `put` returns at once, such a
store writing the turn after. The `serde` feature makes a `Turn`
`Serialize` and `Deserialize`, for such a store to write.
A turn is kept without its natives, in memory as a shared store keeps it.

It serves OpenAI's Conversations API too (`POST /conversations`, a
conversation's `GET`, `POST` and `DELETE`, and its items' list, `POST`,
`GET` and `DELETE`), answering each request itself, as no model is called.
A Responses request naming a `conversation` (its id, or `{"id": ..}`) is
sent its items before its input, whatever the provider, OpenAI's own
included, which is sent the items rather than the id and asked for its
reasoning encrypted; once its response ended well, its input and output
items are added to the conversation, whatever its `store`. A request
naming a `previous_response_id` too is refused, as OpenAI refuses it.
Conversations are kept in `Conversations`, `Memory` by default, for `keep`
after each was last read; a deployment of several gateways gives them one
they share (`Responses::with_conversations`), each change a call of its own
for it to make atomic. A request's `Scope`, which its host gives before this
plugin, such as its project, is the only one that reaches the conversations
made under it.

```rust,ignore
use respan_gateway_plugin_responses::Responses;

let gateway = gateway.plugin(Responses::new(Duration::from_secs(60 * 60)));
```

Register it before the plugins that send a request on, such as routing's
dispatch, and before routing itself for the Conversations API, whose
requests name no model.
