# Examples

Runnable examples of building on the gateway's crates. Each is one
self-contained file under 1,000 lines: read it top to bottom. Its first
comment says what it shows and what to look for in its output.

```bash
cargo run -p respan-gateway-examples --example billing
```

Every example runs offline against a scripted provider in the same process,
except the three marked **live**, which need real credentials and cost
provider usage.

| Topic | Example | Shows |
| --- | --- | --- |
| Policies | [guardrail](guardrail.rs) | A term list checks every call's input and every response, streamed or not. |
| | [pii](pii.rs) | E-mail addresses hidden from the model and restored in the reply, across stream deltas and tool calls. |
| | [logging](logging.rs) | Request logs through a sink you choose; no prompts or replies for zero-retention tenants. |
| | [admission](admission.rs) | Requests wait in a weighted fair queue before any provider call; past their deadline, 429. |
| | [tenants](tenants.rs) | Tenant config edited while the gateway runs, each tenant's own provider key on its calls. |
| | [media_caps](media_caps.rs) | Caps and per-key metering over images, transcriptions and embeddings. |
| Billing | [billing](billing.rs) | Each call priced from your own table, held on a prepaid wallet before it and settled after, output capped. |
| Routing | [fallback](fallback.rs) | Retries, fallback, hedging and stall watchdogs, as routes. |
| | [judge](judge.rs) | A plugin that asks a small model whether a request is hard, then picks the model. |
| | [batch_split](batch_split.rs) | One embeddings batch split into bounded concurrent calls, each retried, merged into one answer. |
| | [models_merge](models_merge.rs) | `GET /v1/models` merged from every provider's list, each called with its own key. |
| Integrations | [attachments](attachments.rs) | A PDF becomes text for a text-only model; its fallback that reads PDFs gets the document. |
| | [custom_format](custom_format.rs) | A new protocol, OpenAI embeddings, added by writing its kind and `Format`. |
| | [custom_transport](custom_transport.rs) | A model in this process reached as a provider, with no HTTP. |
| | [mcp_proxy](mcp_proxy.rs) | MCP clients over streamable HTTP through the gateway, with a tool allow-list. |
| Hosting | [embedded](embedded.rs) | The gateway as a library inside another Rust service. |
| | [local_proxy](local_proxy.rs) | A local proxy for Claude Code, passing on its credentials, headers and unknown fields. |
| Jobs | [jobs](jobs.rs) | Durable idempotency in SQLite: a retry, a duplicate or a restart reuses the first run's result. |
| | [background_jobs](background_jobs.rs) | Durable background Responses jobs: submit, poll, cancel, and recovery after a crash. |
| Realtime | [realtime](realtime.rs) | An OpenAI Realtime session, metered and capped, text and audio. |
| | [gemini_live](gemini_live.rs) | Gemini Live with each turn paid for up front, tools and shutdown. **Live.** |
| | [responses_ws](responses_ws.rs) | Responses over WebSocket: named lanes, continuation and per-turn billing. **Live.** |
| | [tool_guard](tool_guard.rs) | A tool allow-list for any realtime session: OpenAI Realtime clients on Gemini Live. **Live.** |

## Live and real-world runs

```bash
GEMINI_API_KEY=... MODEL=gemini-live-2.5-flash MODEL_INPUT_LIMIT=1048576 \
  cargo run -p respan-gateway-examples --example gemini_live      # :8082
OPENAI_API_KEY=... MODEL=gpt-4.1-mini MODEL_INPUT_LIMIT=1047576 \
  cargo run -p respan-gateway-examples --example responses_ws     # :8081
GEMINI_API_KEY=... ALLOWED_TOOLS=get_weather,get_time \
  cargo run -p respan-gateway-examples --example tool_guard       # :8083
```

Some offline examples take an option to run against the real thing:
`mcp_proxy -- --stdio PROGRAM [ARGS...]` fronts a real MCP server,
`local_proxy -- --serve 127.0.0.1:8080` listens for Claude Code, and
`PDFTOTEXT=1` makes `attachments` use Poppler's `pdftotext`.

Policies such as prices, wallets, queues and job stores are application
code: these examples show how to build them on the public APIs, not
supported APIs of their own. Each keeps its state in memory or a temporary
SQLite file.
