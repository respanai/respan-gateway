# Wolf memory audit: RSS and physical footprint

Measurement implementation: Ark `fc4036e`, before integration of main in
`0313f56`. That merge changed native L1 rules and documentation, not the model
runtime measured here. HTTP and source-routing regressions were checked again
after the merge; memory was not remeasured for the new native rules.

[Raw aggregate JSON](memory-ab.json) contains the historical/current model-only
comparison, actual current default profiles, and separate diagnostic controls.
Separate current-default traces: [FP16](memory-default-fp16.json) and
[INT8/INT4](memory-default-int8_int4.json).
This is a memory audit on one Apple M1, not a new HTTP latency benchmark. Native
L1 and HTTP are absent. The older adapter measurement in `README.md` remains
explicitly historical.

## Actual current defaults

Fresh current-Core processes loaded the existing FP16 or pinned INT8/INT4
export with **no diagnostic flags and no optional manifest**. CPU, batch 1,
one requested intra/inter-op thread, spinning disabled and graph optimization
Level 3 were used. The current Wolf path owns its independent configured pool;
other Ark models retain their legacy runtime policy. Both precisions completed
real inference. Model and tokenizer identities are recorded in the JSON.

| Model-only profile | RSS after full input | Maximum RSS | Physical footprint after full input | Peak physical footprint | vmmap writable swapped-out | Physical footprint after drop |
|---|---:|---:|---:|---:|---:|---:|
| Current FP16 | 386.45 MiB | 649.03 MiB | 567.0 M | 567.0 M | 192.8 M | 48.8 M |
| Current INT8/INT4 | 233.11 MiB | 233.13 MiB | 125.0 M | 184.5 M | 0 | 45.0 M |

`M` values retain the units printed by macOS `vmmap`. RSS comes from `ps` and
`/usr/bin/time -l`. These metrics measure different things and must not be
substituted for each other. The swapped-out column is the exact `vmmap`
accounting category; it is not an independently measured disk-swap I/O total.

The FP16 process had physical footprint 288.4 M immediately after session
creation, before backend warmup, with a 499.1 M high-water mark. The quantized
process had 127.3 M at that checkpoint, with peak 184.5 M. The full input is
254 repetitions of ` a`, occupying 256 slots in both legacy and current paths.
Short and empty-input shapes were also exercised before the full input.

FP16 used the identical 281,826,832-byte file (268.77 MiB) in every FP16
comparison. INT8/INT4 used the 96,296,126-byte export from the existing pinned
revision `142fadc5474163d4c483cb761d5a6b02e3aa1741`. The latter was fetched only
for the requested precision comparison; no production pin was changed.
A smaller quantized footprint is a different export choice, not evidence of a
lower-memory FP16 implementation or equivalent classification calibration.

## What the legacy comparison establishes

The archived `c079462` legacy classifier used the **same FP16 file and compact
tokenizer**, and the exact same linked ONNX Runtime build. Its full-input
physical footprint was **593.6 M**, including about **194 M swapped-out** (193.7 M in writable regions).
Its constructor physical footprint was about 291.7 M, close to current Core's
288.4 M immediately after session creation. The legacy and current source
versions, runtime policies and all raw checkpoints are preserved in the JSON.

These are single fresh-process observations under different reclamation states.
The difference between 567.0 M and 593.6 M is **not** presented as a proven
memory-saving percentage. The user's earlier 400–500 MiB observation is not
dismissed; its metric, workload and backend were not specified. Our own legacy
RSS readings also reached that range. RSS alone cannot establish a 200 MiB
increase in live model memory.

The archived legacy path performs its historical global-pool warmup. Historical
Core retains its natural implicit initialization. Current Core uses its current
production initialization. Requested session settings are identical, while
historical global pools may override effective thread behavior. That runtime
policy difference is explicit rather than hidden by forced normalization.
Only temporary package names and dependency aliases were changed to compile
both historical and current sources together; production runtime sources were
not edited for the comparison.

## Why the earlier 725 MiB number was misleading alone

The historical adapter's **725.33 MiB** was total process RSS, including native
L1 and HTTP. Its approximate Wolf increment over the native mode was
**642.45 MiB**; neither is the FP16 file size or exclusive live-weight memory.
The default trace later read 386.45 MiB RSS for FP16, yet physical footprint was
567.0 M because macOS residency and swapped/compressed accounting had changed.

`vmmap` also exposes allocator-retained reusable regions. At current FP16
session creation, `MALLOC_LARGE_REUSABLE` had 139.0 M resident and zero dirty
bytes. After the full input it reported 91.2 M resident/dirty; these allocations
must not simply be labelled live weights or subtracted wholesale as a claimed
saving. After dropping the model, FP16 physical footprint fell to 48.8 M while
RSS still read 258.45 MiB. Quantized physical footprint fell to 45.0 M while RSS
still read 233.13 MiB. Retained RSS is therefore not itself evidence of a leak.

## Diagnostic controls are not production fixes

Separate fresh-process controls compared dynamic versus fixed-256 inputs,
shared versus independent pools, and disabled CPU arena/memory-pattern/
prepacking settings. They did **not** establish the claimed RSS reduction:
fixed-256 reached 691.80 MiB RSS; shared-plus-fixed reached 677.69 MiB; disabling
all three allocator/packing features reached 663.20 MiB versus its control's
670.97 MiB. No such diagnostic flag or forced padding was applied to production.
The earlier fixed/shared trace's 462.1 M physical footprint is deliberately not
reported as the current default.

The temporary reproducible harness, exact per-mode stdout/stderr and full
`vmmap -summary` reports remain under `/tmp/ark-wolf-memory-ab/` and
`/tmp/ark-wolf-memory-probe/`. Only aggregate metadata is in this repository;
model weights and full OS mapping dumps are not committed.
