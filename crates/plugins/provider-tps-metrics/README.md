# Provider TPS Metrics

`respan-gateway-plugin-provider-tps-metrics` observes each conversation call at the
provider edge and aggregates effective gateway-observed metrics by provider and
model. It is a library plugin, not a sidecar, and does not change the client
protocol.

## Metrics

- `input_tps`: reported input tokens per second. The numerator is
  `input_tokens`; the denominator is the time from this provider call's start
  hook to its first output delta.
- `output_tps`: output tokens per second over the window from the first output
  delta to the last output delta.
- `cache_hit_rate`: token-weighted `cached_input_tokens / input_tokens` across calls whose cache count was reported.
- `*_timed_calls`: the number of successful calls included in the corresponding
  rate.

Usage events are increments and are accumulated until the provider call ends,
including Usage events received after Finish. Each rate keeps only calls that
have both the required token data and a valid, non-zero timing window, so its
numerator and denominator describe the same sample set.

Cache counts keep whether the provider reported the field. Explicitly reported
zero counts are included as cache misses; calls without a cache field are
excluded from the hit-rate numerator and denominator. The snapshot includes
`cache_known_calls` and `cache_total_calls` so the UI can show coverage.
`cached_input_tokens` and `processed_input_tokens` are `None` only when no calls
have a known cache count. Input TPS still uses the reported input tokens and
timing window. Average context size is total reported input tokens divided by
successful calls that reported input usage.

Only responses explicitly identified by the HTTP transport as SSE or Amazon
EventStream contribute timing samples. Non-streaming and unrecognized response
formats contribute usage and call counts but no TPS. A single Delta cannot form
an output window. Missing Usage produces no TPS sample. Failed calls are counted in `failed_calls`; cancelled or abandoned calls
are additionally counted in `abandoned_calls`. Neither contributes to successful-
call usage or rates. Retries, fallbacks,
races, and tool-loop steps are independent provider calls.

These values are gateway observations. They include request encoding, network,
provider queueing, cache work, and gateway decoding. They do not claim to be
the provider's internal prefill or decode speed. In particular, cache-read time
cannot be accurately removed from the first-token window. Output tokens include
reasoning tokens when the provider reports them as output usage.

## Usage

This vendored copy must match `respan-ursi-plugins/plugins/provider-tps-metrics`,
which is the source of truth. The workspace uses path dependencies for
`respan-gateway-core` and `respan-gateway-conversation`, so it builds against
the same Rust types as its other plugins.

```rust
use std::collections::HashMap;
use respan_gateway_plugin_provider_tps_metrics::ProviderMetrics;

let mut names = HashMap::new();
names.insert("openai-openai-chat".into(), "openai".into());
let plugin = ProviderMetrics::new().provider_names(names);
let metrics = plugin.metrics();
let gateway = gateway.plugin(plugin);
let snapshots = metrics.snapshot();
```

Metrics live in the plugin instance. A configuration reload creates a new
instance, so new calls start a new accumulation; in-flight calls on the old
instance finish there. A process restart clears all metrics. The admin API is
responsible for exposing the snapshot to authorized users.

## Validation

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```
