# respan-gateway-plugin-pricing

`Pricing` says what each call to a provider cost, as it goes: each usage
report the provider sends is charged, before the client hears it, at what
that provider charges for the model the call sent. A route's pick, each step
of a tool loop, a retry and a race's loser are each a call, so a request
costs the sum of its calls.

The host gives its catalog (`respan_gateway_sdk::Catalog`), where a call's
model's `pricing` is, and takes the charges:

```rust,ignore
let pricing = Pricing::new(catalog)
    .provider_names(names) // a core provider's name for its catalog provider's
    .on_charge(|extensions, charge| record(extensions, charge.usd))
    .model_of(|px| session_model(px)); // a live session's, which its input does not name
let gateway = gateway.plugin(Routing::new(routes)).plugin(pricing);
```

Register it after the plugins that choose a call's model, such as the router,
so it reads the model sent. A `Price` is US dollars per million input and
output tokens, and cached input at its own rate if given. Every kind's usage
is read: chat, images, transcriptions, speech, embeddings, rerank,
realtime and decisions; seconds of audio and search units are not priced.

`on_usage(edge, f)` gives `f` each usage report of a request's or a call's
outputs, of every kind (tokens, or seconds of audio), as they pass: the
gateway's request log counts with it too.

It measures. What a host does with a charge, a budget or a wallet, is its
own; see [the billing examples](../../../examples/README.md#billing-policies).
The binary's catalog holds each provider's list with the config's declared
`models` laid over it, and it adds each charge to the request's `cost_usd`.
