"""D: three TTFTs for the same 16K prompt: KV on GPU, KV reloaded from CPU, full recompute.

Server is started with a small GPU KV pool and --kv-offloading-size (CPU tier).
1) send P (cold, full prefill)  2) send P again (GPU hit)
3) push P out of GPU by sending enough other sessions  4) send P (should load from CPU tier)
5) a fresh random prompt of the same length (cold baseline). Repeated REPS times.
GPU and external (CPU-tier) hits are told apart with vllm:prefix_cache_hits vs vllm:external_prefix_cache_hits.
Usage: python exp_d_offload.py <port> <kv_capacity_tokens> <out.json>
"""
import sys
from common import complete, reset_cache, rand_ids, save, metrics, metric, med

port, cap, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
S, REPS = 16000, 3
n_push = int(cap * 1.5 // S) + 1

def hits():
    m = metrics(port)
    return metric(m, "vllm:prefix_cache_hits"), metric(m, "vllm:external_prefix_cache_hits")

complete(port, rand_ids(512, 7))
reps = []
for rep in range(REPS):
    reset_cache(port)
    P = rand_ids(S, seed=5000 + rep)
    cold = complete(port, P)
    g0, e0 = hits(); gpu = complete(port, P); g1, e1 = hits()
    for j in range(n_push):
        complete(port, rand_ids(S, seed=6000 + rep * 100 + j))
    g2, e2 = hits(); cpu = complete(port, P); g3, e3 = hits()
    fresh = complete(port, rand_ids(S, seed=7000 + rep))
    row = {"rep": rep, "cold_ttft": cold["ttft"], "gpu_hit_ttft": gpu["ttft"], "cpu_reload_ttft": cpu["ttft"],
           "fresh_cold_ttft": fresh["ttft"],
           "gpu_probe": {"cached": gpu["cached_tokens"], "gpu_hits": g1 - g0, "external_hits": e1 - e0},
           "cpu_probe": {"cached": cpu["cached_tokens"], "gpu_hits": g3 - g2, "external_hits": e3 - e2}}
    print(row)
    reps.append(row)
summary = {k: med([r[k] for r in reps]) for k in ["cold_ttft", "gpu_hit_ttft", "cpu_reload_ttft", "fresh_cold_ttft"]}
print("median:", summary)
save(out, {"experiment": "D_offload", "prompt_tokens": S, "kv_capacity_tokens": cap, "push_sessions": n_push,
           "summary": summary, "reps": reps})
