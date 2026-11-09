"""Charts for blog section 2, in the dark respan-gateway benchmark style (1920x1080).

Data: OpenRouter Inferred #3 (2026-10-08, Sept traffic) and E1-A (Qwen3-8B, 1x H100, vLLM 0.33).
Run: python3 scripts/charts_section2.py   -> img/input-output-ratio.png, img/read-time-vs-length.png
"""
import json, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import Rectangle

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "img")
BG, TEXT, SUB, FOOT, BAR, HI, AXIS = "#1c1c1b", "#f2f2f0", "#a3a39f", "#8a8a86", "#5c5b57", "#3b88e6", "#3a3a37"
plt.rcParams.update({"font.family": "Inter", "text.color": TEXT})


def canvas(title, subtitle, footnote):
    fig = plt.figure(figsize=(19.2, 10.8), dpi=100, facecolor=BG)
    fig.text(0.035, 0.89, title, fontsize=50, color=TEXT, va="baseline")
    fig.text(0.035, 0.835, subtitle, fontsize=27, color=SUB, va="baseline")
    fig.text(0.035, 0.04, footnote, fontsize=17, color=FOOT, va="baseline", linespacing=1.5)
    return fig


# ---- 1. input vs output share -------------------------------------------------
fig = canvas("Agents send ~100 tokens in for every token out",
             "Human chat 7 : 1  ·  Agents ~100 : 1",
             "Share of all tokens on OpenRouter, September 2026. Agent traffic: output and reasoning about 1%, cached input just under 90%.\n"
             "Human traffic: output and reasoning about 12%. Source: OpenRouter, Inferred #3 (October 8, 2026).")
ax = fig.add_axes([0.035, 0.2, 0.93, 0.5]); ax.set_facecolor(BG); ax.axis("off")
ax.set_xlim(0, 100); ax.set_ylim(0, 10)
rows = [("Human chat", 88, 12, "7 : 1", BAR, 7.0), ("Agent", 99, 1, "~100 : 1", HI, 2.0)]
for name, inp, out, ratio, col, y in rows:
    w = 82  # bar width in axis units
    ax.text(0, y + 1.75, name, fontsize=22, color=TEXT, va="bottom")
    ax.add_patch(Rectangle((0, y), w * inp / 100, 1.5, color=col, lw=0))
    ax.add_patch(Rectangle((w * inp / 100, y), w * out / 100, 1.5, color="#d9d9d6", lw=0))
    ax.text(1, y + 0.75, f"{inp}% input", fontsize=24, color=TEXT, va="center")
    ax.text(w + 1.5, y + 0.75, ratio, fontsize=34, color=TEXT, va="center", fontweight="bold")
    if out >= 5:
        ax.text(w * inp / 100 + 0.8, y + 0.75, f"{out}% out", fontsize=20, color=BG, va="center")
    else:
        ax.annotate(f"{out}% out", (w, y + 1.5), xytext=(w - 6, y + 2.0), fontsize=20, color=SUB,
                    va="bottom", arrowprops=dict(arrowstyle="-", color=SUB, lw=1.2))
fig.savefig(os.path.join(OUT, "input-output-ratio.png"), facecolor=BG); plt.close(fig)

# ---- 2. read time vs prompt length -------------------------------------------
a = json.load(open(os.path.join(HERE, "..", "experiments", "results", "a_ttft_vs_length.json")))["rows"]
xs = [r["tokens"] / 1000 for r in a]; ys = [r["cold_ttft_s"] for r in a]
A, B = 2.637e-05 * 1000, 4.909e-10 * 1e6          # least-squares fit t = A*k + B*k^2, k = thousand tokens
grid = list(range(0, 101))
fig = canvas("Reading time grows faster than prompt length",
             "100K tokens take 7.55 s to read, not the 2.6 s a straight line predicts",
             "Time to first token with no cache, median of 3 runs. Qwen3-8B, 1x H100 80GB, vLLM 0.33, random token prompts.\n"
             "Solid line: least-squares fit t = a·n + b·n² through the measured points. Dashed line: the a·n part alone, i.e. time growing in step with length.")
ax = fig.add_axes([0.075, 0.2, 0.86, 0.53]); ax.set_facecolor(BG)
ax.plot(grid, [A * k for k in grid], "--", color=SUB, lw=2.5)
ax.plot(grid, [A * k + B * k * k for k in grid], color=HI, lw=5)
ax.scatter(xs, ys, color=HI, s=110, zorder=3, edgecolors=BG, linewidths=2)
ax.text(99, 7.55 + 0.25, "7.55 s measured", fontsize=26, color=TEXT, ha="right", va="bottom", fontweight="bold")
ax.text(99, A * 100 + 0.3, "2.6 s if it grew in step with length", fontsize=22, color=SUB, ha="right", va="bottom")
ax.set_xlim(0, 102); ax.set_ylim(0, 8.6)
ax.set_xticks([0, 20, 40, 60, 80, 100], ["0", "20K", "40K", "60K", "80K", "100K"])
ax.set_yticks([0, 2, 4, 6, 8], ["0", "2 s", "4 s", "6 s", "8 s"])
ax.tick_params(colors=SUB, labelsize=20, length=0, pad=10)
for sp in ax.spines.values(): sp.set_visible(False)
ax.grid(axis="y", color=AXIS, lw=1)
ax.set_xlabel("prompt length (tokens)", fontsize=20, color=SUB, labelpad=12)
fig.savefig(os.path.join(OUT, "read-time-vs-length.png"), facecolor=BG); plt.close(fig)
print("ok")
