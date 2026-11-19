# Use DeepSeek and any other model in Claude Desktop

*Claude Desktop, chat and the Code tab, can run on any model, through a
gateway on your own machine.*

Claude Desktop talks to one kind of server: one that speaks the Anthropic
Messages API and serves Claude model ids. In its third-party inference mode
you can point it at your own gateway, but it still only asks for Claude
models by their ids. So DeepSeek, GPT, Gemini or a model on your own GPU
can't be picked directly.

Respan Gateway fixes both halves. It speaks Anthropic Messages to Claude
Desktop and translates each request to whatever the model on the other end
speaks, and a route gives any model a Claude id that Claude Desktop will
accept.

```
Claude Desktop ──Anthropic Messages──▶ Respan Gateway ──▶ DeepSeek, OpenAI, Gemini, Ollama, …
   asks for "claude-sonnet-4-5"          route: claude-sonnet-4-5 → deepseek/deepseek-chat
```

## 1. Install the gateway

```bash
pip install respan-gateway
```

Or `cargo install respan-gateway`, or a
[GitHub release](https://github.com/respanai/respan-gateway/releases).

## 2. Give each model a Claude id

Write a `gateway.toml`:

```toml
listen = "127.0.0.1:8080"

[providers.deepseek]
vendor = "deepseek"
key = "env:DEEPSEEK_API_KEY"

[providers.openai]
vendor = "openai"
key = "env:OPENAI_API_KEY"

# Claude Desktop asks for Claude ids; each route sends one to another model.
[routes."claude-sonnet-4-5"]
to = "deepseek/deepseek-chat"

[routes."claude-opus-4-5"]
to = "deepseek/deepseek-reasoner"

[routes."claude-haiku-4-5"]
to = "openai/gpt-6-luna"
```

Any model the gateway can reach works on the right-hand side: `ollama/...`
for a local model, `gemini/...`, or a list of models to fall back through.
`respan-gateway models` lists what each provider serves.

Then start it:

```bash
export DEEPSEEK_API_KEY=sk-...
export OPENAI_API_KEY=sk-...
respan-gateway --watch
```

`respan-gateway check` shows where each Claude id goes before you start.
On `127.0.0.1` the gateway needs no client key: it only answers your own
machine.

## 3. Point Claude Desktop at the gateway

In Claude Desktop's third-party inference settings, choose a **gateway**
provider and fill in:

| Setting | Value |
|---|---|
| Base URL | `http://127.0.0.1:8080` |
| API key | anything, or one of your `client_keys` if you set them |
| Models | the Claude ids from your routes, each with a label |

Turn model discovery off and list the models yourself, so each one gets a
label that says what it really is. The saved configuration looks like this:

```json
{
  "inferenceProvider": "gateway",
  "inferenceGatewayBaseUrl": "http://127.0.0.1:8080",
  "inferenceGatewayApiKey": "unused",
  "inferenceCredentialKind": "static",
  "modelDiscoveryEnabled": false,
  "inferenceModels": [
    { "name": "claude-sonnet-4-5", "labelOverride": "DeepSeek Chat" },
    { "name": "claude-opus-4-5",   "labelOverride": "DeepSeek Reasoner" },
    { "name": "claude-haiku-4-5",  "labelOverride": "GPT-6 Luna" }
  ]
}
```

The model picker now shows DeepSeek and GPT, and both chat and the Code tab
use them.

## 4. Watch what it does

Open [localhost:8080/ui/](http://localhost:8080/ui/). Every request Claude
Desktop sends shows up with the model it really went to, its tokens, cost
and latency. To see where a Claude id goes and why:

```bash
curl "http://127.0.0.1:8080/ui/api/routes/explain?model=claude-sonnet-4-5"
```

## Going further

- **Fallback.** `to = ["deepseek/deepseek-chat", "openai/gpt-6-luna"]` moves
  to the next model when DeepSeek fails, and `hedge = "800ms"` races a slow
  call against the next one.
- **Tools for any model.** Add a web search or MCP server with
  [`[plugins.tool_loop]`](../../../docs/mcp.md), and models without built-in
  search get it too.
- **Tracing.** [`[plugins.respan]`](../../../docs/configuration.md) records
  each request as a trace in Respan.
- **Edit while it runs.** With `--watch`, change a route and the next
  message goes to the new model, with no restart.
