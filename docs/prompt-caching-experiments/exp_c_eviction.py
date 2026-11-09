"""C: fill KV memory with new sessions; when do old sessions lose their cache?

Server is started with a small KV pool (low --gpu-memory-utilization) so a handful of 16K sessions fill it.
After each new session, record KV usage. At the end, probe every session newest -> oldest
(probing newest first so the probes themselves mostly evict sessions that were already LRU-oldest).
Usage: python exp_c_eviction.py <port> <kv_capacity_tokens> <out.json>
"""
import sys
from common import complete, reset_cache, rand_ids, save, metrics

port, cap, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
S = 16000
n_sessions = max(4, int(cap * 1.8 // S))   # ~1.8x the pool, so the oldest ones must be evicted

reset_cache(port)
sessions, fill = [], []
for k in range(n_sessions):
    ids = rand_ids(S, seed=1000 + k)
    sessions.append(ids)
    complete(port, ids)
    m = metrics(port)
    fill.append({"session": k, "tokens_sent": (k + 1) * S, "kv_usage": m.get("vllm:kv_cache_usage_perc")})
    print(f"session {k:>2} sent, total {(k + 1) * S:>7} tok, kv usage {fill[-1]['kv_usage']}")

probes = []
for k in reversed(range(n_sessions)):
    r = complete(port, sessions[k])
    probes.append({"session": k, "age_rank": n_sessions - 1 - k, "cached_tokens": r["cached_tokens"],
                   "hit_frac": (r["cached_tokens"] or 0) / S, "ttft_s": r["ttft"]})
    print(f"probe session {k:>2}: cached {r['cached_tokens']}/{S}  ttft {r['ttft']:.3f}s")
save(out, {"experiment": "C_eviction", "kv_capacity_tokens": cap, "session_tokens": S,
           "n_sessions": n_sessions, "fill": fill, "probes": probes})
