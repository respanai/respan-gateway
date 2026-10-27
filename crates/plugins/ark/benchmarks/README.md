# Local adapter measurements

For current model-only memory accounting, see the [RSS and physical-footprint
audit](memory-ab.md). The historical RSS number below is not live-weight memory.

The recorded run below belongs to local plugin commit `0019103`, before the
PI field-scope and metadata-coverage refinements. It is historical data, not a
measurement of the current branch head. Rerun the harness to measure that head.

Build the measurement harness once, wait for other builds and model tests to
finish, then run the compiled binary directly:

```sh
cargo build --release -p respan-gateway-plugin-ark --features wolf \
  --example adapter_benchmark
./target/release/examples/adapter_benchmark all /absolute/local/wolf-bundle \
  > crates/plugins/ark/benchmarks/local-cpu.json
```

The incoming HTTP request is created in process and passed directly to
`respan_gateway_http::handle`. This measures parsing, plugins, dispatch and
response consumption, without an inbound listener, client socket or inbound
TLS. The provider hop is real loopback TCP/HTTP to `mockllm`; no external
provider or network is involved.

The harness runs baseline HTTP gateway, L1-only guards and L1 plus real Wolf Defender in three fresh sequential processes. It uses `mockllm` over loopback HTTP;
provider requests consume their complete replies. Both client and provider
checks execute in guarded modes, so each original text runs through Wolf twice.

Each mode warms short and long inputs, measures 12 short requests and 3 long
requests, then sends 4 short clients concurrently. One scanner admits one active
scan. ONNX Runtime uses CPU with batch 1 and one intra/inter-op thread; Rayon has
one worker and Tokio two. The separate window-count helper uses Core's exact
compact tokenizer and does not inflate a measured process's memory.

JSON includes individual samples, nearest-rank p50/p95, single startup readings,
process RSS before/after initialization and warmup, final RSS, and OS-reported
peak RSS. macOS uses `/usr/bin/time -l`; Linux uses `/usr/bin/time -v`. Peak memory
covers startup and the entire workload. No model weights are written into the
repository.

Baseline and L1 modes use the same binary with optional ORT linked but no model
session. The native increment over baseline and Wolf increment over native are
process estimates, not precise detector or model-weight accounting. Allocator,
model buffers, tokenizer and ONNX sessions contribute to the Wolf increment.
The standalone Core native feature graph separately verifies that a production
L1-only build does not require ORT.

These local measurements are not service-level guarantees. Three long samples
make their p95 the maximum observed value. Four concurrent clients test bounded
serialization behavior, not a throughput capacity estimate. Startup is one
observation per fresh process; repeated startup distributions are not claimed.

## Recorded local run

[Raw JSON, including individual samples](local-cpu.json) records the run on
2026-10-25: Apple M1, macOS arm64, 16 GiB RAM, release build. Compilers and
verification jobs had finished before measurement. The local Small FP16 file
is 281,826,832 bytes (268.77 MiB). The prepared bundle has an optional manifest;
its startup reference check is included. No asset was downloaded or repinned.

| Mode | Gateway startup | Warm RSS | Peak RSS | Short p50 / p95 | Long p50 / p95 |
|---|---:|---:|---:|---:|---:|
| Baseline HTTP | 1.40 ms | 8.98 MiB | 9.73 MiB | 1.51 / 1.76 ms | 1.52 / 1.57 ms |
| Native L1 | 948 ms | 82.88 MiB | 83.19 MiB | 2.02 / 2.19 ms | 4.96 / 5.41 ms |
| Native L1 + Wolf Defender | 2,525 ms | 725.33 MiB | 725.50 MiB | 142.25 / 149.21 ms | 3,076.63 / 3,176.68 ms |

Short input: 54 UTF-8 bytes, 9 content tokens, one Wolf window per edge,
12 measured samples. Long input: 4,920 bytes, 661 content tokens, three windows
per edge, three measured samples. Both edges scan: the long guarded request
therefore performs six sequential batch-1 model calls. Dynamic sequence lengths
make a short window substantially cheaper than a full 256-token window.
The reported long p95 is simply the largest of three observations.

All three modes forwarded 21 successful requests to the fake provider: two
warmups, 12 short, three long, four concurrent short clients. The four-client
wave completed in 2.19 ms baseline, 3.39 ms with L1 and 513.54 ms with Wolf;
the configured scan capacity remained one. This wave checks the bounded path,
not sustained throughput. Per-request p50/p95 in this four-client wave was
1.85/2.13 ms baseline, 3.22/3.34 ms with L1 and 385.33/513.49 ms with Wolf.
With four samples that p95 is the largest observed latency.

The approximate warm native increment is 73.89 MiB over baseline. Wolf adds
642.45 MiB over native. These totals include model execution buffers and
runtime allocations: the 268.77 MiB FP16 file size is not the process RSS.
One model session serves both guards; a second model is not instantiated for
the provider edge. Response scanning and response-stream masking are outside
this measurement.

The measured Core working tree included the reviewed legacy progress correction,
subsequently saved as Ark commit `c079462`. Default `Scanner::scan` does not
request progress and does not perform its optional token-counting prepass.
