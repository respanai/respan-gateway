# Ark guardrails

Local native PII, secret/DLP and prompt-injection detection, with optional
Wolf Small FP16. Construct `Ark::new(config)`, register its clone before
answering plugins, and register `ark.provider_guard()` after all provider
input modifiers. Both share one scanner and bounded admission capacity.

PII/DLP actions are `block` (default) or `mask`; PI blocks. Model and scanner
errors refuse forwarding. Client replies are not scanned. Roles and text
field boundaries are preserved. Masking operates on merged original UTF-8
byte ranges and does not restore sensitive values in responses.

The `wolf` feature requires a local, checksummed Core bundle and an explicit
probability threshold. Pooling uses raw binary logits with training's
normalized logsumexp at temperature 0.1, followed by sigmoid.

The development stack pins the Core commit from
[patronus-protect/patronus-security#172](https://github.com/patronus-protect/patronus-security/pull/172).
Replace the Git dependency with the published Core version before upstream merge. Core is dual-licensed MIT OR Apache-2.0, matching this gateway.

Optional `onnx-coreml`, `onnx-cuda`, `onnx-directml` and `onnx-tensorrt`
features forward to Core's ONNX Runtime execution providers. CPU is the
verified default; an explicitly selected provider must pass bundle warmup
and numerical checks. Binary feature names are `ark-coreml`, `ark-cuda`,
`ark-directml` and `ark-tensorrt`.

## Local CPU measurement

Measured in a release build on macOS arm64 with one scanner, ONNX Runtime
CPU, one inference thread and batch size 1. The pinned Small FP16 bundle
passed its startup numerical checks. Startup including warmup was 10.32 s;
warm process RSS was about 734 MiB. Short-input latency was p50 65 ms and
p95 about 71 ms (16 samples). A 17,220-byte input spanning 10 windows took
p50 5.58 s and p95 5.99 s (3 samples). These are local measurements, not
service-level guarantees; the long-input sample count is small.

Reproduce with Core's `wolf_benchmark` example against the prepared local
bundle. Preparation uses the Rust `prepare_wolf_bundle` example; serving
and scanning never fetch model assets.
