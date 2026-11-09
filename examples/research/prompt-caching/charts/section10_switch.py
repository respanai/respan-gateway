"""Section 7: cumulative cost after switching models mid-session at 112K tokens (L5 coding shape: +3K tokens and 900 output per call).
Prices per million tokens (Anthropic pricing page, Nov 2026): Opus 5.5 write 5 / read 0.20 / out 20; Sonnet 5.5 2.5 / 0.10 / 10;
Haiku 5.5 above 100K: write 0.625 / read 0.05 / out 2.5. Cache assumed warm on the model in use; the first call after a switch writes everything."""
import os
from chartstyle import canvas, plt, BG, TEXT, SUB, HI, LIGHT, AXIS

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "img")
M = {"Opus 5.5": (5.0, 0.20, 20.0), "Sonnet 5.5": (2.5, 0.10, 10.0), "Haiku 5.5": (0.625, 0.05, 2.5)}
C0, D, O, N = 112_000, 3_000, 900, 20

def run(model, switched):
    w, r, o = M[model]; c = C0; tot = 0.0; out = [0.0]
    for k in range(N):
        if switched and k == 0:
            cost = (c + D) * w
        else:
            cost = c * r + D * w
        tot += (cost + O * o) / 1e6; out.append(tot); c += D + O
    return out

stay = run("Opus 5.5", False)
lines = [("Stay on Opus 5.5", stay, TEXT, "-"), ("Switch to Sonnet 5.5", run("Sonnet 5.5", True), HI, "-"),
         ("Switch to Haiku 5.5", run("Haiku 5.5", True), LIGHT, "--")]
fig = canvas("A cheaper model has to earn back the switch",
             "Total cost of the next 20 calls after switching models in a 112K-token coding session",
             "List prices. Each call adds about 3K tokens and writes 900 output tokens. Staying keeps a warm cache; switching makes the first call write the whole history\n"
             "on the new model. Output price differences included. Calls only: whether the cheaper model does the job as well isn't counted.")
ax = fig.add_axes([0.08, 0.18, 0.86, 0.57]); ax.set_facecolor(BG)
xs = list(range(N + 1))
for name, ys, col, ls in lines:
    ax.plot(xs, ys, color=col, lw=4, ls=ls)
    ax.text(N + 0.3, ys[-1], name, fontsize=20, color=col, va="center")
for name, ys, col, _ in lines[1:]:
    be = next(k for k in range(1, N + 1) if ys[k] <= stay[k])
    ax.scatter([be], [ys[be]], s=200, color=col, zorder=4, edgecolors=BG, linewidths=2)
    ax.annotate(f"pays back after {be} calls", (be, ys[be]), xytext=(90, -55) if "Sonnet" in name else (70, -45),
                textcoords="offset points", fontsize=19, color=col, ha="left",
                bbox=dict(boxstyle="square,pad=0.2", fc=BG, ec="none"))
ax.set_xlim(0, N + 6.5); ax.set_ylim(0, max(stay) * 1.08)
ax.set_xticks([0, 5, 10, 15, 20], ["switch", "5", "10", "15", "20 calls"])
ax.set_yticks([0, 0.25, 0.5, 0.75, 1.0, 1.25], ["$0", "$0.25", "$0.50", "$0.75", "$1.00", "$1.25"])
ax.tick_params(colors=SUB, labelsize=18, length=0, pad=10)
for sp in ax.spines.values(): sp.set_visible(False)
ax.grid(axis="y", color=AXIS, lw=1)
fig.savefig(os.path.join(OUT, "switch-payback.png"), facecolor=BG); plt.close(fig)
print("stay 20:", round(stay[-1], 3), "| sonnet first call", round(lines[1][1][1], 3), "| haiku", round(lines[2][1][1], 3))
