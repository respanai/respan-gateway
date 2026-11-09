"""Charts for blog section 4 (a cache's life). Data: E1-C, E1-D, E1-E (Qwen3-8B, 1x H100, vLLM 0.33).

eviction.png  : eight session blocks, oldest evicted first (E1-C)
offload.png   : three big-number cards, where the cache comes back from (E1-D)
routing.png   : per-turn hit rate as filled circles, three routing policies (E1-E)
"""
import json, os
from chartstyle import canvas, plt, BG, TEXT, SUB, BAR, HI, LIGHT
from matplotlib.patches import Rectangle, FancyArrowPatch, FancyBboxPatch, Wedge, Circle

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "img")
EV = os.path.join(HERE, "..", "experiments", "results")
GONE = "#2c2c2a"

# ---- eviction ------------------------------------------------------------------
c = json.load(open(os.path.join(EV, "c_eviction.json")))
S, n, cap = c["session_tokens"], c["n_sessions"], c["kv_capacity_tokens"]
kept = {p["session"]: p["hit_frac"] for p in c["probes"]}
fig = canvas("When the GPU fills up, the oldest cache goes first",
             f"Cache pool of {cap:,} tokens. Eight 16K-token sessions opened one after another.",
             "Each block is one session, oldest on the left. Blue: still cached when probed afterwards. Dark: evicted. Measured on Qwen3-8B, 1x H100,\n"
             "vLLM 0.33 with --gpu-memory-utilization 0.35. A session is evicted from its end first, so its beginning survives longest.")
ax = fig.add_axes([0.035, 0.2, 0.93, 0.55]); ax.set_facecolor(BG); ax.axis("off")
W, gap = 11.2, 0.9
ax.set_xlim(0, n * (W + gap)); ax.set_ylim(0, 10)
for i in range(n):
    x = i * (W + gap); f = kept[i]
    ax.add_patch(Rectangle((x, 3), W, 3.2, color=GONE, lw=0))
    if f > 0:
        ax.add_patch(Rectangle((x, 3), W * f, 3.2, color=HI, lw=0))
    ax.text(x + W / 2, 6.6, f"session {i + 1}", fontsize=20, color=TEXT, ha="center", va="bottom")
    status = "evicted" if f == 0 else ("cached" if f > 0.99 else f"{f:.0%} cached")
    ax.text(x + W / 2, 2.4, status, fontsize=22, color=HI if f > 0 else SUB, ha="center", va="top",
            fontweight="bold" if 0 < f < 0.99 else "normal")
x_end = n * (W + gap) - gap
ax.add_patch(FancyArrowPatch((0, 9.0), (x_end, 9.0), arrowstyle="-|>", mutation_scale=28, color=SUB, lw=2))
ax.text(0, 9.35, "opened first", fontsize=18, color=SUB, va="bottom")
ax.text(x_end, 9.35, "opened last", fontsize=18, color=SUB, va="bottom", ha="right")
fig.savefig(os.path.join(OUT, "eviction.png"), facecolor=BG); plt.close(fig)

# ---- offload: three cards -------------------------------------------------------
d = json.load(open(os.path.join(EV, "d_offload.json")))["summary"]
cards = [("Still on the GPU", d["gpu_hit_ttft"], "GPU memory · 80 GB per H100", True),
         ("Pulled back from CPU memory", d["cpu_reload_ttft"], "CPU memory · terabytes per server", False),
         ("Gone, read again from scratch", d["fresh_cold_ttft"], "no cache left anywhere", False)]
fig = canvas("An evicted cache can move down instead of dying",
             "The same 16K-token prompt, with its cache in three different places",
             "Time to first token, median of 3 runs. Qwen3-8B, 1x H100, vLLM 0.33 native KV offloading with a 40 GiB CPU tier. The CPU-tier hit was confirmed\n"
             "by vllm:external_prefix_cache_hits rising by 16,000 tokens while GPU prefix-cache hits stayed at 0.")
for i, (title, t, note, hi) in enumerate(cards):
    x0 = 0.035 + i * 0.315
    ax = fig.add_axes([x0, 0.22, 0.29, 0.5]); ax.axis("off"); ax.set_xlim(0, 1); ax.set_ylim(0, 1)
    ax.add_patch(FancyBboxPatch((0, 0), 1, 1, boxstyle="round,pad=0,rounding_size=0.04",
                                fc="#262624", ec=HI if hi else "#3a3a37", lw=3 if hi else 1.5))
    ax.text(0.08, 0.84, title, fontsize=24, color=TEXT, va="top")
    ax.text(0.08, 0.45, f"{t:.2f} s", fontsize=72, color=HI if hi else TEXT, va="center")
    ax.text(0.08, 0.12, note, fontsize=19, color=SUB, va="bottom")
fig.savefig(os.path.join(OUT, "offload.png"), facecolor=BG); plt.close(fig)

# ---- routing: filled circles per turn --------------------------------------------
e = {r["policy"]: r for r in json.load(open(os.path.join(EV, "e_routing.json")))["results"]}
rows = [("Same conversation → same machine", "sticky", lambda t: "A"),
        ("Random machine each turn", "random", lambda t: "?"),
        ("Alternate machines (round robin)", "round_robin", lambda t: "AB"[t % 2])]
fig = canvas("Your next turn has to land where your cache is",
             "Two machines, 15 conversations of 6 turns. Each circle: share of that turn's prompt served from cache.",
             "Two vLLM instances on one H100, Qwen3-8B. Round robin with 15 conversations sends each conversation to the other machine every turn, so it\n"
             "can only reuse what it left there two turns earlier. Letters show the machine a single conversation hits under each policy.")
ax = fig.add_axes([0.035, 0.14, 0.93, 0.64]); ax.set_facecolor(BG); ax.axis("off")
ax.set_xlim(0, 100); ax.set_ylim(0, 30); ax.set_aspect("equal")
for j in range(6):
    ax.text(36 + j * 9, 28.3, f"turn {j + 1}", fontsize=18, color=SUB, ha="center")
for i, (label, key, mach) in enumerate(rows):
    y = 22 - i * 9; r = e[key]
    ax.text(0, y + 0.8, label, fontsize=21, color=TEXT, va="center")
    ax.text(0, y - 1.6, f"{r['hit_rate']:.0%} overall", fontsize=26, color=HI if key == "sticky" else SUB, va="center",
            fontweight="bold" if key == "sticky" else "normal")
    for j in range(6):
        cx = 36 + j * 9; h = r["hit_rate_by_turn"][str(j)]
        ax.add_patch(Circle((cx, y), 3.2, fc=GONE, ec="#3a3a37", lw=1))
        if h > 0:
            ax.add_patch(Wedge((cx, y), 3.2, 90 - 360 * h, 90, fc=HI if key == "sticky" else LIGHT, lw=0))
        ax.text(cx, y - 4.4, mach(j), fontsize=16, color=SUB, ha="center", va="center")
fig.savefig(os.path.join(OUT, "routing.png"), facecolor=BG); plt.close(fig)
print("ok")
