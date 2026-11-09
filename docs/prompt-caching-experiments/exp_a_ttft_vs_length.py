"""A: cold vs warm TTFT as prompt length grows.

For each length: reset cache, send a fresh random prompt (cold = full prefill), send it again (warm = prefix hit).
Repeated REPS times with different prompts; medians reported.
Usage: python exp_a_ttft_vs_length.py <port> <max_len> <out.json>
"""
import sys
from common import complete, reset_cache, rand_ids, med, save, KV_BYTES_PER_TOKEN

port, max_len, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
LENGTHS = [n for n in [1024, 2048, 4096, 8192, 16384, 32000, 64000, 100000] if n <= max_len - 16]
REPS = 3

complete(port, rand_ids(512, 999))  # warm up kernels / CUDA graphs
rows = []
for n in LENGTHS:
    cold, warm, cached = [], [], []
    for rep in range(REPS):
        reset_cache(port)
        ids = rand_ids(n, seed=n * 10 + rep)
        c = complete(port, ids)
        w = complete(port, ids)
        cold.append(c["ttft"]); warm.append(w["ttft"]); cached.append(w["cached_tokens"])
    row = {"tokens": n, "cold_ttft_s": med(cold), "warm_ttft_s": med(warm), "warm_cached_tokens": med(cached),
           "cold_all": cold, "warm_all": warm, "kv_gb": n * KV_BYTES_PER_TOKEN / 1e9}
    row["speedup"] = row["cold_ttft_s"] / row["warm_ttft_s"] if row["warm_ttft_s"] else None
    print(f"{n:>7} tok  cold {row['cold_ttft_s']:.3f}s  warm {row['warm_ttft_s']:.3f}s  "
          f"x{row['speedup']:.1f}  cached {row['warm_cached_tokens']}  KV {row['kv_gb']:.2f} GB")
    rows.append(row)
save(out, {"experiment": "A_ttft_vs_length", "reps": REPS, "rows": rows})
