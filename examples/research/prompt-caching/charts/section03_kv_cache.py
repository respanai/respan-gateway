"""Charts for blog section 3, as tile grids (no bar charts).

1. edit-one-token.png: the 32K prompt as a grid of 200 tiles (160 tokens each), for three edit positions.
2. kv-size.png: one H100 as 80 tiles of 1 GB, filled by one 150K-token session's KV cache, per model.
Data: E1-B (Qwen3-8B, 1x H100, vLLM 0.33) and L0 KV sizes (config.json, BF16).
"""
import json, os
from chartstyle import canvas, plt, BG, TEXT, SUB, BAR, HI, LIGHT
from matplotlib.patches import Rectangle

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "img")
MARK, EMPTY = "#f0883e", "#2c2c2a"


def grid(fig, rect, cols, rows, color_of, gap=0.18):
    ax = fig.add_axes(rect); ax.set_facecolor(BG); ax.axis("off")
    ax.set_xlim(0, cols); ax.set_ylim(0, rows); ax.set_aspect("equal"); ax.set_anchor("W")
    for k in range(cols * rows):
        r, c = divmod(k, cols)
        ax.add_patch(Rectangle((c + gap / 2, rows - 1 - r + gap / 2), 1 - gap, 1 - gap, color=color_of(k), lw=0))
    return ax


# ---- 1. change one token ------------------------------------------------------
b = json.load(open(os.path.join(HERE, "..", "experiments", "results", "b_edit_position.json")))
N, COLS, ROWS = b["prompt_tokens"], 20, 10
per = N / (COLS * ROWS)
rows = {r["edit_pos"]: r for r in b["rows"]}
panels = [(0, "Change the first token"), (16000, "Change token 16,000"), (31990, "Change token 31,990")]
fig = canvas("Change one token, and everything after it is re-read",
             "The same 32K-token prompt, one token changed. Each square is 160 tokens.",
             "Blue: served from cache. Grey: read again. Orange: where the change is. Time to first token on Qwen3-8B, 1x H100 80GB, vLLM 0.33.\n"
             "The cache is kept in 16-token blocks, so the hit stops at the last full block before the change.")
for i, (pos, title) in enumerate(panels):
    r = rows[pos]; cached_tiles = r["cached_tokens"] / per; edit_tile = int(pos // per)
    def col(k, ct=cached_tiles, et=edit_tile):
        if k == et: return MARK
        return HI if k < ct else BAR
    x0 = 0.035 + i * 0.315
    grid(fig, [x0, 0.25, 0.28, 0.42], COLS, ROWS, col)
    fig.text(x0, 0.70, title, fontsize=24, color=TEXT)
    fig.text(x0, 0.175, f"{r['ttft_s']:.2f} s", fontsize=44, color=TEXT)
    pct = r["cached_tokens"] / N
    fig.text(x0 + 0.1, 0.18, (f"{int(pct * 1000) / 10:.1f}%" if 0.99 < pct < 1 else f"{pct:.0%}") + " from cache",
             fontsize=22, color=SUB)
fig.savefig(os.path.join(OUT, "edit-one-token.png"), facecolor=BG); plt.close(fig)

# ---- 2. one number line from 0 to 80 GB (one H100), each model a dot ---------------
kv = [("Llama 3.1 405B", 77.4), ("Llama 3.3 70B", 49.2), ("Qwen3-235B", 28.9),
      ("Qwen3-8B", 22.1), ("DeepSeek V3 / Kimi K2", 10.5), ("DeepSeek V4 Pro", 1.5)]
fig = canvas("One long session's cache can fill a whole GPU",
             "KV cache of a single 150K-token session, on a scale that ends at one H100's 80 GB",
             "BF16, computed from each model's published config.json (2 × layers × KV heads × head dim × 2 bytes per token; MLA and DeepSeek V4's\n"
             "compressed formats from their reference code). The model's own weights need memory on top of this.")
ax = fig.add_axes([0.05, 0.14, 0.9, 0.62]); ax.set_facecolor(BG); ax.axis("off")
ax.set_xlim(-3, 92); ax.set_ylim(-8, 8)
ax.add_patch(Rectangle((0, -0.35), 80, 0.7, color="#3a3a37", lw=0))          # the GPU, 0-80 GB
ax.plot([80, 80], [-2.2, 2.2], color=TEXT, lw=3)
ax.text(80.8, 0, "80 GB\none H100", fontsize=22, color=TEXT, va="center", linespacing=1.3)
for gb in [0, 20, 40, 60]:
    ax.text(gb, -1.4, f"{gb} GB", fontsize=17, color=SUB, ha="center", va="top")
# alternate labels above / below so neighbours don't collide
place = {"Llama 3.1 405B": 1, "Llama 3.3 70B": 1, "Qwen3-235B": -1, "Qwen3-8B": 1, "DeepSeek V3 / Kimi K2": -1, "DeepSeek V4 Pro": 1}
level = {"Llama 3.1 405B": 3.5, "Llama 3.3 70B": 3.5, "Qwen3-235B": -3.5, "Qwen3-8B": 3.5, "DeepSeek V3 / Kimi K2": -3.5, "DeepSeek V4 Pro": 3.5}
for name, gb in kv:
    hi = gb / 80 > 0.9; side = place[name]; ly = level[name]
    ax.plot([gb, gb], [0, ly - side * 0.6], color=HI if hi else SUB, lw=1.5)
    ax.scatter([gb], [0], s=520 if hi else 300, color=HI if hi else LIGHT, zorder=3, edgecolors=BG, linewidths=3)
    va = "bottom" if side > 0 else "top"
    ax.text(gb, ly, f"{gb:g} GB", fontsize=30 if hi else 26, color=HI if hi else TEXT, ha="center", va=va,
            fontweight="bold" if hi else "normal")
    ax.text(gb, ly + (1.45 if side > 0 else -1.45), name, fontsize=19, color=SUB, ha="center", va=va)
fig.savefig(os.path.join(OUT, "kv-size.png"), facecolor=BG); plt.close(fig)
print("ok")
