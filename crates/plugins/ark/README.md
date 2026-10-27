![Patronus Ark — local PII, DLP and prompt-injection guardrails for Respan](assets/ark-respan-banner.svg)

# Patronus Ark for Respan

Protect chat requests before they reach a model: **redact or block PII and secrets**,
and **block prompt injection** with Ark's native checks and **Wolf Defender Small**.
Scanning runs locally in Rust with ONNX Runtime. Existing Ark model assets are reused.

[Quickstart](#quickstart) · [Configuration recipes](#configuration-recipes) ·
[Scan coverage](#scan-coverage) · [All settings](#configuration-reference) ·
[Errors and deadlines](#errors-and-deadlines) · [Development](#development)

[Wolf Defender](https://huggingface.co/patronus-studio/wolf-defender-prompt-injection-small)
is Patronus Protect's prompt-injection model family, with integrations in
[Apache Camel](https://camel.apache.org/components/next/others/semantic-wolf-defender.html)
and [TrustyAI](https://github.com/trustyai-explainability/trustyai-service-operator/blob/main/config/configmaps/nemo-guardrails/default-guardrails-injection.yaml).
See the model card for evaluations and methodology. This adapter runs the Small
model from your installed local bundle.

## Quickstart

### 1. Build the gateway

From the Respan repository root:

```sh
cargo build --release -p respan-gateway --features ark-wolf
```

`ark-wolf` includes the Ark plugin and Wolf Defender's runtime. For native PII,
DLP and PI checks, use `--features ark` and the [native-only recipe](#native-checks-without-a-model).
The default gateway build does not include Ark.

### 2. Configure the provider and guard

Save this as `gateway.toml`. Choose a writable local model directory. The
quickstart explicitly prepares missing pinned Wolf Defender assets during startup;
already installed assets are reused. No new model manifest or revision is needed.

```toml
listen = "127.0.0.1:8080"
client_keys = [{ name = "app", key = "env:GATEWAY_CLIENT_KEY" }]

[providers.openai]
vendor = "openai"
key = "env:OPENAI_API_KEY"

[plugins.ark]
pii = "redact"
dlp = "redact"

[plugins.ark.warmup]
download = "missing"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8 # Example only: choose and validate for your traffic.
backend = "cpu"
precision = "auto"
```

The `redact` actions replace sensitive values with `[REDACTED]` before forwarding.
PI findings still block. For example, a detected email in `Contact: alice@corp.org`
is forwarded as `Contact: [REDACTED]`. Use `block` to refuse the request instead.

The threshold is required when Wolf is configured; `0.8` is an example, not a
Core default. Model files are not embedded. Startup downloads require the explicit
`warmup.download = "missing"` opt-in above; the default is `never`. Request scans
never download assets. The ready scanner/session created during warmup is reused
by both guards.

### 3. Start and send a request

Set your provider key and a local client key, then start the gateway:

```sh
export OPENAI_API_KEY='your-provider-key'
export GATEWAY_CLIENT_KEY='your-local-client-key'
RESPAN_CONFIG=./gateway.toml ./target/release/respan-gateway serve
```

In another terminal, set the same client key and choose a model available from
your provider (`openai/your-model` below):

```sh
export GATEWAY_CLIENT_KEY='your-local-client-key'
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"openai/your-model","messages":[{"role":"user","content":"Summarize the meeting agenda."}]}'
```

A blocked request returns an error before the provider is called. A passing
request continues through Respan normally. Omitting `[plugins.ark]` disables the
guard even in an Ark-enabled build.

## Configuration recipes

These are alternative plugin fragments: add **one** to your existing gateway
configuration rather than defining `[plugins.ark]` several times. Each recipe
also lives in [examples/config](examples/config) and is parsed by the tests.

### Block sensitive content

Refuse PII, secrets and prompt injection. [Copy the file](examples/config/block.toml).

```toml
[plugins.ark]
pii = "block"
dlp = "block"

[plugins.ark.warmup]
download = "missing"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8 # Example: calibrate for your traffic.
backend = "cpu"
precision = "auto"
```

### Redact PII and secrets

Mask both categories and continue when PI checks pass. `mask` is an alias for
`redact`. [Copy the file](examples/config/redact.toml).

```toml
[plugins.ark]
pii = "redact"
dlp = "redact"

[plugins.ark.warmup]
download = "missing"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8
backend = "cpu"
precision = "auto"
```

Text redaction merges overlapping original UTF-8 byte ranges. Tool arguments
remain valid JSON: detected values become `[REDACTED]`; numeric values can become
strings. Sensitive JSON keys are refused rather than renamed. Redacted values
are not restored in responses.

### Choose the sources independently

Keep PII/DLP checks on all three sources, but run native PI and Wolf only on tool
results. This also demonstrates redacting PII while blocking secrets.
[Copy the file](examples/config/select-sources.toml).

```toml
[plugins.ark]
pii = "redact"
dlp = "block"
pii_sources = ["user_input", "tool_call", "tool_result"]
dlp_sources = ["user_input", "tool_call", "tool_result"]
pi_sources = ["tool_result"]

[plugins.ark.warmup]
download = "missing"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8
backend = "cpu"
precision = "int8_int4"
```

Use `pi_sources = ["user_input", "tool_call", "tool_result"]` to opt tool arguments
into PI checks. Use `[]` on any source list to disable that check everywhere.
Source names are exactly `user_input`, `tool_call` and `tool_result`.

### Prompt-injection checks only

Run native PI and Wolf Defender on user input and tool results. PII/DLP are
disabled. [Copy the file](examples/config/pi-only.toml).

```toml
[plugins.ark]
pii_sources = []
dlp_sources = []
pi_sources = ["user_input", "tool_result"]

[plugins.ark.scanner]
categories = ["injection"]

[plugins.ark.warmup]
download = "missing"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8
backend = "cpu"
precision = "fp16"
```

To use Wolf without native PI, also set
`"native:injection_l1" = false` in `[plugins.ark.scanner.models]`.

### Native checks without a model

Build with `--features ark`. This runs PII, DLP and native PI, with no Wolf session.
[Copy the file](examples/config/native-only.toml).

```toml
[plugins.ark]
pii = "redact"
dlp = "redact"

[plugins.ark.scanner]
pii_mode = "strict"
dlp_mode = "strict"
```

### Change modes and individual rules

`strict`, `balanced` and `recall` are the existing Ark native modes. Rule overrides
use the [Ark rule IDs](https://github.com/patronus-protect/patronus-security/blob/main/docs/reference/l1-rule-catalog.md).
Disabling an email rule below deliberately removes that rule's protection.

```toml
[plugins.ark]
pii = "redact"
dlp = "block"

[plugins.ark.scanner]
pii_mode = "balanced"
dlp_mode = "strict"

[plugins.ark.scanner.rules]
pii_email = false
```

The source lists select **where** checks run. `scanner.categories`, `rules` and
`models` select the **native detectors** that run there. The `scanner.wolf` block
configures Wolf separately, on the selected `pi_sources`.

### Choose FP16 or INT8/INT4

Set `precision = "fp16"` or `precision = "int8_int4"` in
`[plugins.ark.scanner.wolf]`. An explicit choice requires that export to exist.
`auto` prefers INT8/INT4, unless `PATRONUS_L3_PRECISION=fp16`, and falls back to
the other available export. Offline startup requires that export locally; explicit
`warmup.download = "missing"` prepares the selected pinned export before loading.

The CPU build is `ark-wolf`. Accelerator builds use the corresponding gateway
feature: `ark-coreml`, `ark-cuda`, `ark-directml` or `ark-tensorrt`, with a matching
`backend`. Local verification in this repository uses CPU.

### Bound waiting, scan time and input size

One inspection runs at a time. This profile allows eight waiting inspections,
a 500 ms wait and a shared 10-second work budget for all selected fields in a
started inspection. [Copy the file](examples/config/bounded-capacity.toml).

```toml
[plugins.ark]
pii = "redact"
dlp = "redact"
max_queued = 8
queue_timeout_ms = 500
scan_timeout_ms = 10000
max_request_bytes = 262144

[plugins.ark.scanner]
max_input_bytes = 262144
max_findings = 2000

[plugins.ark.warmup]
download = "missing"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8
backend = "cpu"
precision = "int8_int4"

[plugins.ark.scanner.wolf.runtime_options]
intra_threads = 1
inter_threads = 1
spinning = false
```

`max_request_bytes` bounds the sum of selected field bytes;
`scanner.max_input_bytes` bounds each Core input. Neither silently truncates text.
`max_findings` bounds actionable evidence. `max_queued = 0` disables waiting.

### Existing assets and offline startup

The default policy stays offline. To make it explicit, use this fragment with
your installed assets:

```toml
[plugins.ark]
pii = "redact"
dlp = "redact"

[plugins.ark.warmup]
download = "never"
download_timeout_ms = 600000

[plugins.ark.scanner.wolf]
bundle_dir = "/srv/models/wolf-defender"
threshold = 0.8
backend = "cpu"
precision = "auto"
```

`missing` prepares assets only during startup or configuration reload; it reuses
a ready cache and serializes preparation for the same bundle. `never` performs
no network acquisition and fails startup if the required local model cannot
load. Core owns this preparation; the adapter has no second downloader or model
probe. The 600,000 ms asset-acquisition budget is separate from request queue and
scan deadlines. Only a fully prepared, warmed guard becomes active. During a
reload the previous gateway keeps serving until the replacement is ready.

## Scan coverage

| Typed Respan source | PII | DLP | Native PI + configured Wolf |
|---|---|---|---|
| User message text (`user_input`) | On | On | On |
| Tool-call JSON arguments (`tool_call`) | On | On | Off |
| Tool-result text, including nested results (`tool_result`) | On | On | On |

These are the default source selections. The default PII/DLP **action** is
`block`; the quickstart explicitly changes both to `redact`.

**Reasoning is never scanned**, including `reasoning_content`. Other roles' plain
text, metadata, tool definitions and transport headers are outside these sources.
Respan has no distinct Skill content type; this adapter adds no custom markers.
A skill delivered as ordinary user text or a tool result follows that source's
configured policy. Unsupported media in a selected source is refused. Responses
and response-stream masking are outside this plugin's coverage.

The client guard checks ingress before answering plugins. The provider guard
checks new or changed content after input modifiers, including tool loops and
fallbacks. Both share one scanner/session. Request-local hashes reuse successful
checks for unchanged fields; they contain no raw text and are scoped to source,
checks and policy. Redacted output is recorded as the approved forwarded value,
not as a new model verdict about the transformed text.

Each PI text field is its own model document. Wolf uses batch 1, **254 content
tokens + BOS/EOS (256 total)**, and normalized logit Smooth Max at temperature
`0.1`, followed by sigmoid. Native scores and Wolf probabilities stay separate.

## Configuration reference

| Setting | Default | Meaning |
|---|---|---|
| `pii`, `dlp` | `block` | Category action: `block`, `redact` or synonym `mask` |
| `pii_sources`, `dlp_sources` | all three sources | Sources receiving that check; `[]` disables it |
| `pi_sources` | `user_input`, `tool_result` | Sources receiving native PI and enabled Wolf Defender checks |
| `max_queued` | `32` | Waiting inspections in addition to one active inspection; `0..=1024`, with `0` disabling waiting |
| `queue_timeout_ms` | `1000` | Positive maximum wait for the active scanner slot |
| `scan_timeout_ms` | `30000` | Positive work deadline for a whole started inspection pass |
| `max_request_bytes` | `1048576` | Positive aggregate limit of selected text/argument bytes, separate from HTTP body limits |
| `warmup.download` | `never` | Startup/reload policy: `never` for offline assets, `missing` to acquire missing pinned assets; requires `ark-wolf` for model preparation |
| `warmup.download_timeout_ms` | `600000` | Positive asset-acquisition budget; not a request/scan deadline |
| `scanner.categories` | all three | Nonempty subset of native categories `pii`, `dlp`, `injection` |
| `scanner.pii_mode`, `scanner.dlp_mode` | `strict` | `strict`, `balanced` or `recall` |
| `scanner.rules` | Ark defaults | Boolean overrides by [rule ID](https://github.com/patronus-protect/patronus-security/blob/main/docs/reference/l1-rule-catalog.md) |
| `scanner.models` | enabled | Boolean native producer overrides, e.g. `native:pii`, `native:dlp`, `native:injection_l1` |
| `scanner.explain` | `false` | Native detector explanations for internal reports |
| `scanner.max_input_bytes` | `1048576` | Positive per-Core-scan input limit, including tool-argument JSON |
| `scanner.max_findings` | `10000` | Positive actionable-evidence limit per Core scan |
| `scanner.wolf` | absent | Configure Wolf Defender; no model session without it |
| `scanner.wolf.bundle_dir` | required when enabled | Local model and tokenizer directory |
| `scanner.wolf.threshold` | required when enabled | Attack-probability threshold in `[0,1]`; independent of L1 scores |
| `scanner.wolf.precision` | `auto` | `auto`, `fp16`, `int8_int4`; auto prefers quantized unless `PATRONUS_L3_PRECISION=fp16`, falling back to the available export |
| `scanner.wolf.backend` | `auto` | ONNX execution provider; `cpu` is locally tested |
| `scanner.wolf.runtime_options` | threads `1`/`1`, spinning off | `intra_threads`, `inter_threads`, `spinning` |

Unknown fields, invalid source names and invalid settings are rejected. A
configured model that cannot load prevents startup. The model threshold is
independent of native modes and scores.

## Errors and deadlines

The plugin handles async admission and maps typed Core errors to Respan faults.
Core supplies the synchronous scanner and shared cancellation/deadline budget;
it has no request queue.

| Code | HTTP | Meaning |
|---|---|---|
| `ark_sensitive_content` | 400 | PII/DLP matched a category configured to block |
| `ark_injection_blocked` | 400 | Native PI or Wolf matched the configured policy |
| `ark_input_limit`, `ark_finding_limit` | 413 | A configured request/Core limit was exceeded |
| `ark_overloaded` | 429 | All `1 + max_queued` active/waiting slots are occupied |
| `ark_queue_timeout` | 504 | Waiting for the scanner exceeded `queue_timeout_ms` |
| `ark_scan_timeout` | 504 | Started work exceeded `scan_timeout_ms` |
| `ark_asset_failed`, `ark_inference_failed`, `ark_detector_failed` | 500 | Typed Core failure; forwarding is refused |
| `ark_config_failed` | 500 | Invalid scanner configuration |

Client abandonment maps to `ark_scan_cancelled` / Respan `Abandoned`. Invalid
JSON, sensitive keys that cannot be renamed, unsafe JSON redaction and unsupported
selected content are refused with distinct 400 errors.

The scan budget includes blocking-pool scheduling and all selected fields in that
inspection, not provider response time. A timeout or client cancellation signals
active ONNX Runtime termination. Runtime acknowledges this at operation boundaries;
native checks stop at checkpoints. Capacity stays reserved until the real work
ends. Errors do not downgrade to L1-only or forward an incomplete scan as clean.

## Embed in your Rust gateway

`Ark::new` is a blocking startup operation. Async embedders should call it in
`tokio::task::spawn_blocking` before exposing the gateway. The supplied binary
already does this for startup and reload preparation.

Use the same instance for both guards. Register the client guard before plugins
that can answer; register the provider guard after plugins that edit inputs:

```rust
use respan_gateway_core::{Fault, Gateway};
use respan_gateway_plugin_ark::{Ark, Config};

fn gateway(config: Config) -> Result<Gateway, Fault> {
    let ark = Ark::new(config)?;
    Ok(Gateway::new()
        .plugin(ark.clone())
        // Register your answering plugins and input modifiers here.
        .plugin(ark.provider_guard()))
}
```

The binary already registers these in `with_plugins`, following the
[Respan plugin guide](../../../docs/plugins.md#adding-a-plugin-to-the-binary).
The adapter depends on public Respan and Core APIs.

## Development

```text
src/
  lib.rs          public exports
  config.rs       actions, sources and configuration
  plugin.rs       Respan hooks
  runtime.rs      warm scanner, capacity, blocking execution and cancellation
  request.rs      request-local check reuse
  scan.rs         field traversal and Core calls
  text.rs         text decisions and UTF-8 redaction
  json.rs         tool-argument scanning and JSON redaction
  errors.rs       Core/adapter fault mapping
  tests/          unit and contract tests by responsibility
```

Run from the repository root:

```sh
cargo test -p respan-gateway-plugin-ark --features wolf
cargo clippy -p respan-gateway-plugin-ark --no-deps --all-targets --features wolf -- -D warnings
```

Tests use `mockllm` and verify blocked requests never reach a provider, redaction
positions/JSON, source selection, tool loops, request reuse, admission and
cancellation. Configuration tests parse the README snippets and recipe files.
Real-model tests use existing local assets:

```sh
ARK_WOLF_BUNDLE=/absolute/local/fp16-assets \
ARK_WOLF_QUANTIZED_BUNDLE=/absolute/local/int8-int4-assets \
  cargo test -p respan-gateway-plugin-ark --features wolf --test real_wolf \
  -- --ignored --test-threads=1
```

The [measurement reports](benchmarks/README.md) distinguish historical latency
runs from the later memory audit and document their inputs and limitations.

The plugin uses the published `patronus-ark-core` 0.1.13 crate from crates.io.
A sibling Ark checkout is not required.
