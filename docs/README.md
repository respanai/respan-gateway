# Overview

## Calling it

You don't need a config file. Every vendor whose key is in the environment
becomes a provider, and the model name picks the provider, whatever API the
client speaks:

```bash
# OpenAI Chat client → Claude
curl localhost:8080/v1/chat/completions \
  -d '{"model":"claude-haiku-5-5","messages":[{"role":"user","content":"hi"}]}'

# Anthropic client → GPT
curl localhost:8080/v1/messages \
  -d '{"model":"gpt-6-luna","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}'

# `provider/model` names a provider explicitly
curl localhost:8080/v1/responses -d '{"model":"groq/openai/gpt-oss-20b","input":"hi"}'
```

An SDK only needs a base URL:

```python
from openai import OpenAI
OpenAI(base_url="http://localhost:8080/v1", api_key="unused").chat.completions.create(
    model="gemini-2.5-flash", messages=[{"role": "user", "content": "hi"}])
```

Media uses OpenAI's endpoints: `/v1/images/generations`,
`/v1/audio/transcriptions`, `/v1/audio/speech`, `/v1/embeddings` and
`/v1/videos`. `GET /v1/models` lists every provider's models, and the web UI
is at `localhost:8080/ui/`.

## Configuration

`respan-gateway init` writes a commented `gateway.toml` for client keys,
routes, custom providers, prices and plugins:

```toml
client_keys = [{ name = "app", key = "env:APP_KEY" }]

[providers.local]                       # Any OpenAI-compatible endpoint
formats = ["openai-chat"]
base_url = "http://localhost:8000/v1"

[routes.smart]                          # Clients ask for "model": "smart"
to = ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna"]
```

Without client keys, the gateway answers only loopback and refuses to start
on a public address. Every option is in [Configuration](configuration.md),
and [`gateway.example.toml`](../gateway.example.toml) is a short example to
start from.

## Supported APIs

| Kind | Formats |
|---|---|
| chat | `openai-chat`, `openai-responses`, `anthropic-messages`, `gemini-generate`; `passthrough` for a vendor's other endpoints |
| realtime | `openai-realtime` ⇄ `gemini-live`; transcription: `openai-realtime` ⇄ `gemini-live` ⇄ `mistral-realtime`; `responses-ws` relayed |
| image, speech, transcription, embedding | OpenAI's endpoints |
| video | `openai-videos`, `gemini-veo` |

[Vendors](vendors.md) lists each vendor's tier, key and formats. Translation
details are in each format's README, for
[chat](../crates/formats/conversation-http/README.md),
[media](../crates/formats/media-http/README.md) and
[realtime](../crates/formats/realtime-ws/README.md).

## Built-in plugins

| Plugin | What it does |
|---|---|
| [`routing`](../crates/plugins/routing/README.md) | Picks the provider for each request: routes, globs, fallback, retries, hedged races |
| [`tool-loop`](../crates/plugins/tool-loop/README.md) | Runs MCP tools inside chat requests, for any client and any model |
| [`mcp-router`](../crates/plugins/mcp-router/README.md) | Fronts MCP servers at `/mcp/{name}`, and all of them as one at `/mcp` |
| [`respan`](../crates/plugins/respan/README.md) | Sends each request to [Respan](https://respan.ai) as a trace |
| [`pricing`](../crates/plugins/pricing/README.md) | Prices each call from its usage |
| [`responses`](../crates/plugins/responses/README.md) | Keeps Responses API conversation state with any provider |
| [`credentials`](../crates/plugins/credentials/README.md) | Forwards a client's own key to providers that have none |

To write your own, start with the [plugin guide](plugins.md). The
[`examples/`](../examples/README.md) show guardrails, PII masking, billing,
an LLM judge that picks the model, and more, one file each.

## As a library

The binary is a library, `respan_gateway`: `App::new(config)?.router()`
returns an `axum::Router`. To build your own gateway from the crates, start
with the [plugin guide](plugins.md) and the [`examples/`](../examples/README.md).
