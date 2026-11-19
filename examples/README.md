# Examples

| Folder | What's in it |
| --- | --- |
| [recipes](recipes/README.md) | Runnable Rust examples of building on the gateway's crates, one self-contained file each: guardrails, PII masking, billing, fallback, an LLM judge, MCP, realtime. |
| [guides](guides/) | Step-by-step setups for using the gateway with the tools you already run, such as Claude Desktop. |
| [research](research/) | Deep dives into how LLM inference works and what it costs, backed by experiments and data. |

## Recipes

```bash
cargo run -p respan-gateway-examples --example billing
```

See [recipes/README.md](recipes/README.md) for the full list.

## Guides

| Guide | Shows |
| --- | --- |
| [Use DeepSeek and any other model in Claude Desktop](guides/claude-desktop/README.md) | Run Claude Desktop's chat and Code tab on DeepSeek, GPT, Gemini or a local model, by giving each one a Claude id in the gateway. |

## Research

| Topic | Shows |
| --- | --- |
| [From the GPU to your bill: a deep dive into prompt caching](research/prompt-caching/README.md) | What a prompt cache is on the GPU, why it gets evicted, how providers price it, how harnesses break it, and what a router has to know to keep it. With vLLM experiments on an H100. |
