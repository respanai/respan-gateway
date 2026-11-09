"""E: two vLLM replicas; how routing decides the hit rate.

15 multi-turn conversations, interleaved (odd count, so round robin really alternates replicas
across turns). Each starts with a 2K unique prefix; every turn appends
800 user tokens + 200 "assistant" tokens (random ids, so the run is deterministic).
Policies: random, round_robin (per request), sticky (conversation id -> fixed replica, c % 2).
Sizes keep each replica's history (~8 x 8K tokens) inside its KV pool, so misses come from routing, not eviction.
Hit rate = sum(cached_tokens) / sum(prompt_tokens). Both replicas' caches are reset between policies.
Usage: python exp_e_routing.py <port_a> <port_b> <out.json>
"""
import sys, random
from common import complete, reset_cache, rand_ids, save, med

ports = [int(sys.argv[1]), int(sys.argv[2])]
out = sys.argv[3]
CONVS, TURNS, PREFIX, USER, ASSIST = 15, 6, 2000, 800, 200

def run(policy):
    for p in ports:
        reset_cache(p)
    rng = random.Random(1)
    hist = {c: rand_ids(PREFIX, seed=c) for c in range(CONVS)}
    rr, cached, prompt, ttfts, per_turn = 0, 0, 0, [], {}
    for t in range(TURNS):
        for c in range(CONVS):
            hist[c] = hist[c] + rand_ids(USER, seed=c * 1000 + t * 2)
            if policy == "random":
                port = rng.choice(ports)
            elif policy == "round_robin":
                port = ports[rr % 2]; rr += 1
            else:
                port = ports[c % 2]
            r = complete(port, hist[c])
            cached += r["cached_tokens"] or 0; prompt += r["prompt_tokens"] or len(hist[c])
            ttfts.append(r["ttft"])
            pt = per_turn.setdefault(t, [0, 0]); pt[0] += r["cached_tokens"] or 0; pt[1] += len(hist[c])
            hist[c] = hist[c] + rand_ids(ASSIST, seed=c * 1000 + t * 2 + 1)
    res = {"policy": policy, "hit_rate": cached / prompt, "cached_tokens": cached, "prompt_tokens": prompt,
           "median_ttft_s": med(ttfts), "hit_rate_by_turn": {t: v[0] / v[1] for t, v in per_turn.items()}}
    print(res)
    return res

results = [run(p) for p in ["random", "round_robin", "sticky"]]
save(out, {"experiment": "E_routing", "replicas": 2, "conversations": CONVS, "turns": TURNS, "results": results})
