"""Cover image: the hook from section 1. Same session, next "continue": four cents with the cache, a dollar after one character changes at the start.
Blocks are illustrative (one block = one slice of the session's history); the $0.04 and $1.00 are the article's Opus 5.5 numbers."""
import os
from chartstyle import plt, BG, TEXT, SUB, BAR, HI, FOOT
from matplotlib.patches import FancyBboxPatch

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "img")
MARK = "#f0883e"
fig = plt.figure(figsize=(19.2, 10.08), dpi=100, facecolor=BG)
fig.text(0.05, 0.83, "A DEEP DIVE INTO PROMPT CACHING", fontsize=22, color=HI, fontweight="bold", va="baseline")
fig.text(0.05, 0.69, "From the GPU to your bill", fontsize=84, color=TEXT, fontweight="bold", va="baseline")

ax = fig.add_axes([0.05, 0.1, 0.9, 0.5]); ax.axis("off"); ax.set_xlim(0, 100); ax.set_ylim(0, 100)
N, W, G = 40, 1.55, 0.42          # blocks per row, block width, gap
rows = [("Cache warm: the history is read from the cache", HI, None, "$0.04", HI, 66),
        ("One character changed at the start: everything is read again", BAR, 0, "$1.00", MARK, 14)]
for label, col, changed, price, pcol, y in rows:
    ax.text(0, y + 24, label, fontsize=24, color=TEXT if col == HI else SUB, va="bottom")
    for i in range(N):
        x = i * (W + G)
        last = i == N - 1
        fc = "#a3a39f" if last else col
        ax.add_patch(FancyBboxPatch((x, y), W, 20, boxstyle="round,pad=0,rounding_size=0.35",
                                    fc=fc, ec=MARK if i == changed else "none", lw=5 if i == changed else 0))
    ax.text(N * (W + G) + 2, y + 10, price, fontsize=64, color=pcol, fontweight="bold", va="center")
ax.text(0, 2, "Same session, same next message. Each block is a slice of the history; the light grey one is the new message.", fontsize=18, color=FOOT, va="bottom")
fig.savefig(os.path.join(OUT, "cover.png"), facecolor=BG); plt.close(fig)
print("ok")
