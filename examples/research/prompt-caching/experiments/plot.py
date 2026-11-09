"""Draw one PNG per experiment from the JSON in a results folder. Usage: python plot.py results/<run>"""
import json, os, sys
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

run = sys.argv[1]
def load(name):
    p = os.path.join(run, name)
    return json.load(open(p)) if os.path.exists(p) else None
def done(fig, name):
    fig.tight_layout(); fig.savefig(os.path.join(run, name), dpi=160); plt.close(fig); print("wrote", name)

a = load("a_ttft_vs_length.json")
if a:
    xs = [r["tokens"] / 1000 for r in a["rows"]]
    fig, ax = plt.subplots(figsize=(7, 4))
    ax.plot(xs, [r["cold_ttft_s"] for r in a["rows"]], "o-", label="cold: full prefill")
    ax.plot(xs, [r["warm_ttft_s"] for r in a["rows"]], "o-", label="warm: prefix cache hit")
    ax.set_xlabel("prompt length (K tokens)"); ax.set_ylabel("time to first token (s)")
    ax.set_title("Qwen3-8B on 1x H100: same prompt, sent twice"); ax.legend(); ax.grid(alpha=.3)
    done(fig, "a_ttft_vs_length.png")

b = load("b_edit_position.json")
if b:
    rows = b["rows"]
    fig, ax = plt.subplots(figsize=(7, 4))
    ax.plot([r["edit_pos"] for r in rows], [r["cached_tokens"] for r in rows], "o", label="measured hit length")
    ax.plot([0, b["prompt_tokens"]], [0, b["prompt_tokens"]], "--", alpha=.4, label="edit position")
    ax.set_xlabel("position of the one changed token"); ax.set_ylabel("tokens still served from cache")
    ax.set_title(f"Change one token: everything after it is recomputed (block = {b['inferred_block_size']})")
    ax.legend(); ax.grid(alpha=.3)
    done(fig, "b_edit_position.png")

c = load("c_eviction.json")
if c:
    p = sorted(c["probes"], key=lambda r: r["session"])
    fig, ax = plt.subplots(figsize=(7, 4))
    ax.bar([r["session"] for r in p], [r["hit_frac"] * 100 for r in p])
    ax.set_xlabel("session (0 = oldest)"); ax.set_ylabel("% of prompt still cached")
    ax.set_title(f"KV pool ≈ {c['kv_capacity_tokens']:,} tokens, {c['n_sessions']} sessions × {c['session_tokens']:,}")
    ax.grid(alpha=.3, axis="y")
    done(fig, "c_eviction.png")

d = load("d_offload.json")
if d:
    s = d["summary"]
    labels = ["KV on GPU", "KV reloaded\nfrom CPU", "full recompute"]
    vals = [s["gpu_hit_ttft"], s["cpu_reload_ttft"], s["fresh_cold_ttft"]]
    fig, ax = plt.subplots(figsize=(6, 4))
    ax.bar(labels, vals)
    for i, v in enumerate(vals):
        ax.text(i, v, f"{v:.3f}s", ha="center", va="bottom")
    ax.set_ylabel("time to first token (s)"); ax.set_title(f"Same {d['prompt_tokens']:,}-token prompt, three places")
    done(fig, "d_offload.png")

e = load("e_routing.json")
if e:
    res = e["results"]
    fig, ax = plt.subplots(figsize=(6, 4))
    ax.bar([r["policy"] for r in res], [r["hit_rate"] * 100 for r in res])
    for i, r in enumerate(res):
        ax.text(i, r["hit_rate"] * 100, f"{r['hit_rate']:.0%}", ha="center", va="bottom")
    ax.set_ylabel("prefix cache hit rate (%)"); ax.set_title(f"2 replicas, {e['conversations']} conversations × {e['turns']} turns")
    done(fig, "e_routing.png")
