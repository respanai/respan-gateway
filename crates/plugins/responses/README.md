# Responses plugin

`respan-gateway-plugin-responses` keeps the conversation state a client or a
provider of the conversation kind cannot keep (`Responses`):

- **A Responses client, translated to a provider that keeps nothing**
  (Anthropic, Gemini, Chat Completions). Its `previous_response_id` works:
  each response is given an id the gateway mints (`resp_…`) and kept as its
  turn, beside the response it followed, and a follow-up naming it is sent
  the whole conversation. A request that says `store: false` is not kept;
  OpenAI's own Responses keep theirs.
- **A client of another format, such as Chat Completions, whose call goes
  to Responses** (OpenAI's chat format sends a call there when Chat
  Completions would refuse it). Such a call stores nothing at OpenAI
  (`store: false`) and asks for its reasoning encrypted, as the
  `openai-responses` format writes it. The reasoning a turn's tool calls
  came of is kept under each call's id, and put back before those calls
  when the client sends them in its next turn to a provider of Responses,
  as a Chat client has no place to carry it. Without
  it, a reasoning model loses its reasoning between tool calls.

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

```rust,ignore
use respan_gateway_plugin_responses::Responses;

let gateway = gateway.plugin(Responses::new(Duration::from_secs(60 * 60)));
```

Register it before the plugins that send a request on, such as routing's
dispatch.
