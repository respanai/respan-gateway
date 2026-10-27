# First-party plugins

Each plugin is a crate of its own, built on the public core APIs. A gateway
registers the ones it wants; they compose in registration order. To
write one, see the [plugin guide](../../docs/plugins.md).

| Crate | Package | What it does |
|---|---|---|
| [`routing`](routing/README.md) | `respan-gateway-plugin-routing` | `Routing` targets each request at a provider by its model and a route's rules; `Dispatch` sends it to the route's candidates |
| `relay` | `respan-gateway-plugin-relay` | `Relay` marks a call to a provider of the client's own format `Relayed`: the client's key, the provider's response head and error body pass as they came |
| [`fallback`](fallback/README.md) | `respan-gateway-plugin-fallback` | The retry, backoff and race helpers routing's `Dispatch` is built on |
| [`tool-loop`](tool-loop/README.md) | `respan-gateway-plugin-tool-loop` | `ToolLoop` runs tools inside chat requests, for every chat format; `ToolFilter` and `ToolScope` say which tools are offered |
| [`credentials`](credentials/README.md) | `respan-gateway-plugin-credentials` | `ClientCredentials` sends a client's own key to the named providers that have none, of the client's format |
| [`mcp-router`](mcp-router/README.md) | `respan-gateway-plugin-mcp-router` | `McpRouter` targets an MCP client of `/mcp/{name}` at the server `name`, and fronts many servers as one at `/mcp` |
| [`respan`](respan/README.md) | `respan-gateway-plugin-respan` | `Respan` sends each request to Respan as a trace: a span per call to a model or tool server |
| [`pricing`](pricing/README.md) | `respan-gateway-plugin-pricing` | `Pricing` charges each call to a provider at its price for the model sent, as its usage is reported |
| [`responses`](responses/README.md) | `respan-gateway-plugin-responses` | `Responses` keeps the conversation state a client or provider cannot: Responses' `previous_response_id` with any provider, and a Chat client's reasoning between tool calls with OpenAI's Responses |
| [`ark`](ark/README.md) | `respan-gateway-plugin-ark` | Local PII/DLP protection and prompt-injection checks with Wolf Defender |

Billing is application policy, not a supported plugin export: `Pricing`
measures what calls cost, and what to do with it is the host's. See
[the billing examples](../../examples/README.md#billing-policies) for pricing,
conservative per-attempt accounting, nested wallets and per-turn reservations.
Their support modules are private to the examples, not a replacement billing
crate. Provider usage is the conversation kind's typed `Usage`.
