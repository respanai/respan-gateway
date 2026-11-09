# Examples

| Folder | What's in it |
| --- | --- |
| [rust](rust/README.md) | Runnable examples of building on the gateway's crates, one self-contained file each: guardrails, PII masking, billing, fallback, an LLM judge, MCP, realtime. |
| [research](research/) | Deep dives into how LLM inference works and what it costs, with the experiments and data behind them. |

## Rust

```bash
cargo run -p respan-gateway-examples --example billing
```

See [rust/README.md](rust/README.md) for the full list.

## Research

| Topic | Shows |
| --- | --- |
| [From the GPU to your bill: a deep dive into prompt caching](research/prompt-caching/README.md) | What a prompt cache is on the GPU, why it gets evicted, how providers price it, how harnesses break it, and what a router has to know to keep it. With [vLLM experiments](research/prompt-caching/experiments/) on an H100. |
