<div align="center">
  <a href="https://www.respan.ai">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/assets/wordmark-dark.png">
      <img src="docs/assets/wordmark.png" alt="Respan Gateway" width="420">
    </picture>
  </a>
  <p><b>The universal adapter for AI.</b></p>
  <div>
    <a href="#license"><img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-brightgreen.svg?style=flat" alt="License: MIT OR Apache-2.0"></a>
    <a href="https://github.com/respanai/respan-gateway/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/respanai/respan-gateway/ci.yml?branch=main&style=flat&label=CI" alt="CI"></a>
    <a href="https://github.com/respanai/respan-gateway/releases"><img src="https://img.shields.io/github/v/release/respanai/respan-gateway?style=flat&label=Latest%20Release&color=6D28D9" alt="Latest release"></a>
    <a href="https://crates.io/crates/respan-gateway"><img src="https://img.shields.io/crates/v/respan-gateway?style=flat&logo=rust&label=crates.io&color=6D28D9" alt="crates.io"></a>
    <a href="https://pypi.org/project/respan-gateway/"><img src="https://img.shields.io/pypi/v/respan-gateway?style=flat&logo=python&logoColor=white&label=PyPI&color=6D28D9" alt="PyPI"></a>
    <a href="https://discord.com/invite/KEanfAafQQ"><img src="https://img.shields.io/badge/Discord-Join-5865F2?style=flat&logo=discord&logoColor=white" alt="Discord"></a>
  </div>
</div>
<br>

**Respan Gateway** is a single Rust binary that sits between your code and
every AI API. Point the SDK you already use at it, and it connects you to
everything: chat models, images, video and voice from every major provider;
tools for any model, from web search and memory to code sandboxes and any MCP
server; and traffic control (format translation, fallback, retries and hedged
requests), with a live log of every call. **[▶ Watch the demo](https://www.youtube.com/watch?v=pVv2ibXiTXM)**

<br>
<div align="center">
  <img alt="Your app plugs into Respan Gateway, which connects it to models, voice, image and video, tools, embeddings and self-hosted models, with fallback, hedging, translation, tool loop, PII redaction and tracing built in" src="docs/assets/gateway-hero.png" width="900">
</div>
<br>

## Key features

- **Any client, any model**<br>
  OpenAI Chat Completions, OpenAI Responses, Anthropic Messages and Gemini API
  clients can all call any model, with streaming, tools,
  images and reasoning translated.

- **Multimodal**<br>
  Realtime voice across vendors (an OpenAI Realtime client talks to Gemini
  Live, and back), plus images, speech, transcription, embeddings and video
  through OpenAI's or Gemini's APIs, to any vendor that has them.

- **MCP tools for every model**<br>
  Put MCP servers or built-in tool vendors behind the gateway, from web search
  to code sandboxes. It runs their tools inside chat requests, so a plain chat
  call can use them, whatever the client or model.

- **Traffic control**<br>
  Named routes, globs and `provider/model`; fallback, retries, cheapest or
  fastest; and hedged requests that race a slow call against a second
  provider.

- **Observability**<br>
  A live request log with cost, latency and tokens for every call, and traces
  in [Respan](https://respan.ai), where span-02 analyzes how your agents
  behave.

- **Best-in-class performance**<br>
  0.07 ms of added latency, 56,000 requests per second and 31 MB of memory in
  our benchmarks: about 3× the throughput of the next
  fastest gateway we tested.

- **Write your own plugin in minutes**<br>
  Every feature above is a plugin, built on the same public Rust API you can
  use: one trait with four hooks. Start from the [plugin guide](docs/plugins.md)
  or an [example](examples/README.md) such as guardrails, PII masking or
  billing.

## Quickstart

**1. Install**

```bash
pip install respan-gateway
```

Or `cargo install respan-gateway`, or a
[GitHub release](https://github.com/respanai/respan-gateway/releases).

**2. Run it with your keys**

```bash
export OPENAI_API_KEY=sk-...
respan-gateway
```

No config file needed: every vendor whose key is set becomes a provider.

**3. Call any model from any SDK**

```python
from anthropic import Anthropic

client = Anthropic(base_url="http://localhost:8080", api_key="unused")
reply = client.messages.create(
    model="gpt-6-luna", max_tokens=256,
    messages=[{"role": "user", "content": "Hello from the Anthropic SDK"}])
print(reply.content[0].text)
```

The web UI is at [localhost:8080/ui/](http://localhost:8080/ui/). Next:
[configuration](docs/configuration.md) and [MCP tools](docs/mcp.md).

### Other ways to run it

- [Python, in-process](python/README.md) — Run the gateway inside your Python app, with no server.
- [Embed in Rust](docs/plugins.md#in-a-gateway-of-your-own) — Mount the gateway in your own Rust service.
- [Docker](Dockerfile) — Run the image `ghcr.io/respanai/respan-gateway` on any container host.

## Performance

| | Respan Gateway | Bifrost | Portkey | LiteLLM (1 worker) |
|---|---:|---:|---:|---:|
| Added latency (p50) | **0.07 ms** | 0.16 ms | 0.17 ms | 4.34 ms |
| Throughput | **56,734/s** | 18,182/s | 4,187/s | 268/s |
| Memory | **31 MB** | 488 MB | 350 MB | 418 MB |
| 1k streams, success | **100%** | 92–100% | — | 79% |
| 1k streams, memory | **128 MB** | 688 MB | — | 493 MB |
| p99, 5% slow calls | **105 ms** | 2,002 ms | 2,003 ms | 2,024 ms |

## Configuration

You don't need a config file to start. When you want more, run
`respan-gateway init` for a commented `gateway.toml`. One file sets up keys,
providers, traffic control, tools and tracing:

```toml
client_keys = [{ name = "app", key = "env:APP_KEY" }]   # who may call the gateway

[providers.openai]                      # a built-in vendor: just add a key
vendor = "openai"
key = "env:OPENAI_API_KEY"

[providers.anthropic]
vendor = "anthropic"
key = "env:ANTHROPIC_API_KEY"

[providers.local]                       # any OpenAI-compatible endpoint
formats = ["openai-chat"]
base_url = "http://localhost:8000/v1"

[routes.smart]                          # clients ask for "model": "smart"
to = ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna"]
hedge = "800ms"                         # race the second if the first is slow

[tools.search]                          # web search as MCP tools
vendor = "linkup"
key = "env:LINKUP_API_KEY"

[plugins.tool_loop]                     # every model can use them
servers = ["search"]

[plugins.respan]                        # trace every call in Respan
key = "env:RESPAN_API_KEY"
```

`respan-gateway check` validates it, and `--watch` or `SIGHUP` reloads it
without a restart. Without client keys, the gateway answers only on loopback
and refuses to start on a public address.

**Reference:** [configuration](docs/configuration.md) ·
[vendors](docs/vendors.md) · [tool vendors](docs/tool-vendors.md) ·
[MCP and tools](docs/mcp.md) · [`gateway.example.toml`](gateway.example.toml)

## Supported API formats

Call the gateway in any format on the left, and it reaches every provider on
the right, translating between them.

| | Your code calls | It can reach |
|---|---|---|
| **Chat** | OpenAI Chat Completions<br>OpenAI Responses<br>Anthropic Messages<br>Gemini API | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openai-dark.svg"><img src="docs/assets/vendors/openai.svg" alt="OpenAI" title="OpenAI" width="20" height="20"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/anthropic-dark.svg"><img src="docs/assets/vendors/anthropic.svg" alt="Anthropic" title="Anthropic" width="20" height="20"></picture> <img src="docs/assets/vendors/gemini.svg" alt="Gemini" title="Gemini" width="20" height="20"> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/xai-dark.svg"><img src="docs/assets/vendors/xai.svg" alt="xAI" title="xAI" width="20" height="20"></picture> <img src="docs/assets/vendors/deepseek.svg" alt="DeepSeek" title="DeepSeek" width="20" height="20"> <img src="docs/assets/vendors/mistral.svg" alt="Mistral" title="Mistral" width="20" height="20"> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/groq-dark.svg"><img src="docs/assets/vendors/groq.svg" alt="Groq" title="Groq" width="20" height="20"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openrouter-dark.svg"><img src="docs/assets/vendors/openrouter.svg" alt="OpenRouter" title="OpenRouter" width="20" height="20"></picture> <img src="docs/assets/vendors/together.svg" alt="Together" title="Together" width="20" height="20"> <img src="docs/assets/vendors/deepinfra.svg" alt="DeepInfra" title="DeepInfra" width="20" height="20"> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/ollama-dark.svg"><img src="docs/assets/vendors/ollama.svg" alt="Ollama" title="Ollama" width="20" height="20"></picture><br>any chat model, whichever of the four formats it speaks |
| **Realtime voice** | OpenAI Realtime<br>Gemini Live | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openai-dark.svg"><img src="docs/assets/vendors/openai.svg" alt="OpenAI" title="OpenAI" width="20" height="20"></picture> <img src="docs/assets/vendors/gemini.svg" alt="Gemini" title="Gemini" width="20" height="20"> <img src="docs/assets/vendors/mistral.svg" alt="Mistral" title="Mistral" width="20" height="20"><br>OpenAI ⇄ Gemini voice; Mistral for transcription |
| **Images** | OpenAI Images<br>Gemini Images | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openai-dark.svg"><img src="docs/assets/vendors/openai.svg" alt="OpenAI" title="OpenAI" width="20" height="20"></picture> <img src="docs/assets/vendors/gemini.svg" alt="Gemini" title="Gemini" width="20" height="20"> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/xai-dark.svg"><img src="docs/assets/vendors/xai.svg" alt="xAI" title="xAI" width="20" height="20"></picture> <img src="docs/assets/vendors/together.svg" alt="Together" title="Together" width="20" height="20"> <img src="docs/assets/vendors/deepinfra.svg" alt="DeepInfra" title="DeepInfra" width="20" height="20"> |
| **Speech, transcription** | OpenAI Speech<br>OpenAI Transcriptions<br>Gemini Speech and Transcriptions (providers only) | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openai-dark.svg"><img src="docs/assets/vendors/openai.svg" alt="OpenAI" title="OpenAI" width="20" height="20"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/groq-dark.svg"><img src="docs/assets/vendors/groq.svg" alt="Groq" title="Groq" width="20" height="20"></picture> <img src="docs/assets/vendors/deepinfra.svg" alt="DeepInfra" title="DeepInfra" width="20" height="20"> <img src="docs/assets/vendors/gemini.svg" alt="Gemini" title="Gemini" width="20" height="20"> |
| **Embeddings** | OpenAI Embeddings | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openai-dark.svg"><img src="docs/assets/vendors/openai.svg" alt="OpenAI" title="OpenAI" width="20" height="20"></picture> <img src="docs/assets/vendors/gemini.svg" alt="Gemini" title="Gemini" width="20" height="20"> <img src="docs/assets/vendors/mistral.svg" alt="Mistral" title="Mistral" width="20" height="20"> <img src="docs/assets/vendors/together.svg" alt="Together" title="Together" width="20" height="20"> <img src="docs/assets/vendors/deepinfra.svg" alt="DeepInfra" title="DeepInfra" width="20" height="20"> <img src="docs/assets/vendors/fireworks.svg" alt="Fireworks" title="Fireworks" width="20" height="20"> <img src="docs/assets/vendors/cloudflare.svg" alt="Cloudflare" title="Cloudflare" width="20" height="20"> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/ollama-dark.svg"><img src="docs/assets/vendors/ollama.svg" alt="Ollama" title="Ollama" width="20" height="20"></picture> |
| **Video** | OpenAI Videos<br>Gemini Veo<br>Vertex AI Veo (providers only) | <img src="docs/assets/vendors/veo.svg" alt="Google Veo" title="Google Veo" width="20" height="20"> Google Veo |
| **Decisions** | Respan Decisions<br>TypeSafe System One | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/respan-dark.svg"><img src="docs/assets/vendors/respan.svg" alt="Respan" title="Respan" width="20" height="20"></picture> <img src="docs/assets/vendors/typesafe.png" alt="TypeSafe" title="TypeSafe" width="20" height="20"> |
| **Tools** | MCP | <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/mcp-dark.svg"><img src="docs/assets/vendors/mcp.svg" alt="MCP" title="MCP" width="20" height="20"></picture> your MCP servers and tool vendors |
| **Anything else** | A vendor's own API | Relayed to that vendor untouched |

Each vendor's formats are in [vendors](docs/vendors.md), and the translation
details in each format's README: [chat](crates/formats/conversation-http/README.md),
[media](crates/formats/media-http/README.md),
[realtime](crates/formats/realtime-ws/README.md) and
[decisions](crates/formats/decision-http/README.md).

## Plugins

Everything the gateway does is a plugin on one public Rust API.

**Models** · 16 providers and any compatible endpoint, each reachable from any client format. [All vendors →](docs/vendors.md)

<p>
  <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openai-dark.svg"><img src="docs/assets/vendors/openai.svg" alt="OpenAI" title="OpenAI" width="28" height="28"></picture>&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/anthropic-dark.svg"><img src="docs/assets/vendors/anthropic.svg" alt="Anthropic" title="Anthropic" width="28" height="28"></picture>&nbsp;&nbsp;<img src="docs/assets/vendors/gemini.svg" alt="Gemini" title="Gemini" width="28" height="28">&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/xai-dark.svg"><img src="docs/assets/vendors/xai.svg" alt="xAI" title="xAI" width="28" height="28"></picture>&nbsp;&nbsp;<img src="docs/assets/vendors/deepseek.svg" alt="DeepSeek" title="DeepSeek" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/vendors/mistral.svg" alt="Mistral" title="Mistral" width="28" height="28">&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/groq-dark.svg"><img src="docs/assets/vendors/groq.svg" alt="Groq" title="Groq" width="28" height="28"></picture>&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/openrouter-dark.svg"><img src="docs/assets/vendors/openrouter.svg" alt="OpenRouter" title="OpenRouter" width="28" height="28"></picture>&nbsp;&nbsp;<img src="docs/assets/vendors/together.svg" alt="Together" title="Together" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/vendors/fireworks.svg" alt="Fireworks" title="Fireworks" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/vendors/cerebras.svg" alt="Cerebras" title="Cerebras" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/vendors/perplexity.svg" alt="Perplexity" title="Perplexity" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/vendors/azure.svg" alt="Azure" title="Azure" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/vendors/cloudflare.svg" alt="Cloudflare" title="Cloudflare" width="28" height="28">&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/vendors/ollama-dark.svg"><img src="docs/assets/vendors/ollama.svg" alt="Ollama" title="Ollama" width="28" height="28"></picture>&nbsp;&nbsp;<img src="docs/assets/vendors/vllm.svg" alt="vLLM" title="vLLM" width="28" height="28">
</p>

**Tools** · any MCP server, plus built-in tools for search, browsing, code, documents and memory, usable from any model. [All tool vendors →](docs/tool-vendors.md)

<p>
  <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/tool-vendors/linkup-dark.svg"><img src="docs/assets/tool-vendors/linkup.svg" alt="Linkup" title="Linkup" width="28" height="28"></picture>&nbsp;&nbsp;<img src="docs/assets/tool-vendors/tavily.svg" alt="Tavily" title="Tavily" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/browserbase.svg" alt="Browserbase" title="Browserbase" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/apify.svg" alt="Apify" title="Apify" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/tinyfish.png" alt="TinyFish" title="TinyFish" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/e2b.png" alt="E2B" title="E2B" width="28" height="28">&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/tool-vendors/daytona-dark.png"><img src="docs/assets/tool-vendors/daytona.png" alt="Daytona" title="Daytona" width="28" height="28"></picture>&nbsp;&nbsp;<img src="docs/assets/tool-vendors/context7.svg" alt="Context7" title="Context7" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/reducto.svg" alt="Reducto" title="Reducto" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/zep.png" alt="Zep" title="Zep" width="28" height="28">&nbsp;&nbsp;<img src="docs/assets/tool-vendors/cloudinary.svg" alt="Cloudinary" title="Cloudinary" width="28" height="28">&nbsp;&nbsp;<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/tool-vendors/elevenlabs-dark.svg"><img src="docs/assets/tool-vendors/elevenlabs.svg" alt="ElevenLabs" title="ElevenLabs" width="28" height="28"></picture>
</p>

**Traffic control** · named routes, fallback, retries, hedged requests, and the cheapest or fastest provider.

**Observability** · a live request log, the cost of every call, and traces in [Respan](https://respan.ai).

**Your framework** · anything with a base URL. Guides for the [OpenAI SDK](https://www.respan.ai/docs/integrations/gateway/open-ai-sdk), [Anthropic SDK](https://www.respan.ai/docs/integrations/gateway/anthropic-sdk), [Google GenAI](https://www.respan.ai/docs/integrations/gateway/google-gen-ai), [Vercel AI SDK](https://www.respan.ai/docs/integrations/gateway/vercel-ai-sdk), [LangChain](https://www.respan.ai/docs/integrations/gateway/lang-chain), [Pydantic AI](https://www.respan.ai/docs/integrations/gateway/pydantic-ai), [Pi](https://www.respan.ai/docs/integrations/gateway/pi), [OpenCode](https://www.respan.ai/docs/integrations/gateway/open-code) and [more](https://www.respan.ai/docs/integrations/gateway/overview).

**Your own plugin** · start from the [plugin guide](docs/plugins.md) or an [example](examples/README.md): guardrails, PII masking, billing, an LLM judge.

<sub>Built in: [`routing`](crates/plugins/routing/README.md) · [`tool-loop`](crates/plugins/tool-loop/README.md) · [`mcp-router`](crates/plugins/mcp-router/README.md) · [`pricing`](crates/plugins/pricing/README.md) · [`respan`](crates/plugins/respan/README.md) · [`responses`](crates/plugins/responses/README.md) · [`credentials`](crates/plugins/credentials/README.md)</sub>

## Community

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the
architecture, building and tests, and a new vendor is usually one file.
Start with a
[good first issue](https://github.com/respanai/respan-gateway/labels/good%20first%20issue),
ask in [Discussions](https://github.com/respanai/respan-gateway/discussions)
or [Discord](https://discord.com/invite/KEanfAafQQ), and report security
issues as [SECURITY.md](SECURITY.md) describes.

**Roadmap:** what we're building next is in
[issues](https://github.com/respanai/respan-gateway/issues). Missing a vendor,
format or feature? [Ask for it](https://github.com/respanai/respan-gateway/issues/new/choose).

**Want it managed?** [Respan](https://respan.ai) runs the gateway for you, with
dashboards, evals and span-02.

<a href="https://github.com/respanai/respan-gateway/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=respanai/respan-gateway" alt="Contributors">
</a>

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
