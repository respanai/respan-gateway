"""Chart for blog section 7: where the money goes in a 60-turn Opus 5.5 coding session (L3 cost model, coding_agent scenario)."""
import json, os
from chartstyle import canvas, plt, BG, TEXT, SUB, HI, LIGHT, BAR
from matplotlib.patches import Wedge, Circle

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "img")
r = json.load(open(os.path.join(HERE, "data", "session_scenarios.json")))["results"]["coding_agent"]["Opus 5.5"]
parts = [("Writing to the cache", r["share"]["write"], HI), ("Output", r["share"]["output"], BAR), ("Reading from the cache", r["share"]["read"], LIGHT)]
fig = canvas("With reads this cheap, the bill is in the writes",
             f"One 60-turn coding session on Claude Opus 5.5 that grows to 200K tokens: ${r['total_usd']:.2f} in total",
             "Modelled with list prices (input $4, 5-minute cache write $5, cache read $0.20, output $20 per million tokens). Each turn adds about 3K tokens of tool results\n"
             "and messages and 900 output tokens; 8% of gaps between turns are long enough for the 5-minute cache to expire. Without caching the same session costs $27.74.")
ax = fig.add_axes([0.06, 0.12, 0.42, 0.66]); ax.set_aspect("equal"); ax.axis("off"); ax.set_xlim(-1.1, 1.1); ax.set_ylim(-1.1, 1.1)
start = 90
for name, share, col in parts:
    ax.add_patch(Wedge((0, 0), 1, start - 360 * share, start, width=0.36, fc=col, ec=BG, lw=4))
    start -= 360 * share
ax.text(0, 0.08, f"${r['total_usd']:.2f}", fontsize=56, color=TEXT, ha="center", va="center")
ax.text(0, -0.22, "per session", fontsize=20, color=SUB, ha="center", va="center")
for i, (name, share, col) in enumerate(parts):
    y = 0.62 - i * 0.17
    fig.text(0.53, y, f"{share:.0%}", fontsize=64, color=col if col != BAR else TEXT, va="center")
    fig.text(0.665, y + 0.012, name, fontsize=26, color=TEXT, va="center")
    fig.text(0.665, y - 0.035, f"${share * r['total_usd']:.2f}", fontsize=20, color=SUB, va="center")
fig.savefig(os.path.join(OUT, "bill-breakdown.png"), facecolor=BG); plt.close(fig)
print("ok", {k: round(v, 3) for k, v in r["share"].items()})
