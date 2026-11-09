"""B: change one token at position i; how much of the prefix still hits?

Warm a 32K base prompt, then send copies with a single token changed at position i.
Expected hit length = floor(i / block_size) * block_size (only full blocks before the edit survive).
Each probe is followed by re-warming the base so probes don't contaminate each other.
Usage: python exp_b_edit_position.py <port> <out.json>
"""
import sys
from common import complete, reset_cache, rand_ids, save, metrics

port, out = int(sys.argv[1]), sys.argv[2]
N = 32000
POSITIONS = [0, 10, 63, 64, 65, 500, 1000, 4000, 8000, 16000, 24000, 31000, 31990]

reset_cache(port)
base = rand_ids(N, seed=42)
complete(port, base)
again = complete(port, base)
print(f"base re-send cached {again['cached_tokens']} / {N}")
block_info = {k: v for k, v in metrics(port).items() if "cache_config" in k}

rows = []
for pos in POSITIONS:
    ids = list(base)
    ids[pos] = ids[pos] + 1 if ids[pos] < 150000 else ids[pos] - 1
    r = complete(port, ids)
    rows.append({"edit_pos": pos, "cached_tokens": r["cached_tokens"], "ttft_s": r["ttft"]})
    print(f"edit @{pos:>6}: cached {r['cached_tokens']:>6}  ttft {r['ttft']:.3f}s")
    reset_cache(port)          # isolate probes: start each from only the base being cached
    complete(port, base)

hits = [r["cached_tokens"] for r in rows if r["cached_tokens"]]
from math import gcd
from functools import reduce
block = reduce(gcd, hits) if hits else None
for r in rows:
    r["expected_if_block"] = (r["edit_pos"] // block) * block if block else None
save(out, {"experiment": "B_edit_position", "prompt_tokens": N, "base_resend_cached": again["cached_tokens"],
           "inferred_block_size": block, "cache_config_metric": block_info, "rows": rows})
