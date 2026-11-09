"""Worked example for section 4: when does keeping a cache cost more than re-reading the prompt?

100K-token session on Llama 3.3 70B, KV cache in FP8 (2 x 80 layers x 8 KV heads x 128 x 1 byte = 163,840 B/token -> 16.4 GB).
Re-read: 2*N*n + L*H*(dqk+dv)*n^2 FLOPs at 50% of an H100's 989 TFLOPS, H100 rented at $3.50/hour.
Holding cost per GB-hour: GPU = $3.50 / 80 GB; CPU DRAM $0.0021 and SSD $0.000062 (L3 point estimates).
"""
import math, os
from chartstyle import canvas, plt, BG, TEXT, SUB, HI, LIGHT, AXIS

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "img")
N, L, H, D, n, RENT = 70.6e9, 80, 64, 128, 1e5, 3.5
flops = 2 * N * n + L * H * (D + D) * n * n
recompute = flops / (989e12 * 0.5) / 3600 * RENT
gb = 2 * 80 * 8 * 128 * 1 * n / 1e9
tiers = [("GPU memory", gb / 80 * RENT, HI), ("CPU memory", gb * 0.0021, LIGHT), ("SSD", gb * 0.000062, "#9a9a95")]

fig = canvas("Keeping a cache only pays for so long",
             "Cost of holding one 100K-token cache vs reading the prompt again, Llama 3.3 70B",
             f"Re-reading 100K tokens ≈ {flops / (989e12 * 0.5):.0f} s of an H100 at 50% utilisation ≈ ${recompute:.3f} at $3.50 per GPU-hour. The cache is {gb:.1f} GB in FP8. Holding cost per GB-hour:\n"
             "GPU = its share of a $3.50/hour 80 GB H100, CPU memory $0.0021, SSD $0.000062 (our estimates). An estimate of providers' economics, not their numbers.")
ax = fig.add_axes([0.075, 0.2, 0.86, 0.55]); ax.set_facecolor(BG)
hours = [10 ** (math.log10(1 / 60) + i * (math.log10(240) - math.log10(1 / 60)) / 400) for i in range(401)]
ax.axhline(recompute, color=TEXT, lw=2.5, ls="--")
ax.text(1 / 60 * 1.1, recompute * 1.15, f"read it again: ${recompute:.2f}", fontsize=22, color=TEXT, va="bottom")
for name, per_h, col in tiers:
    ax.plot(hours, [per_h * h for h in hours], color=col, lw=4)
    be = recompute / per_h
    ax.scatter([be], [recompute], s=160, color=col, zorder=4, edgecolors=BG, linewidths=2)
    label = f"{be * 60:.1f} min" if be < 1 else (f"{be:.1f} h" if be < 48 else f"{be / 24:.1f} days")
    ax.annotate(f"{name}\nbreaks even at {label}", (be, recompute), xytext=(0, -95), textcoords="offset points",
                fontsize=19, color=col, ha="center", va="top", linespacing=1.3,
                bbox=dict(boxstyle="square,pad=0.3", fc=BG, ec="none"),
                arrowprops=dict(arrowstyle="-", color=col, lw=1.2))
ax.set_xscale("log"); ax.set_yscale("log")
ax.set_xlim(1 / 60, 240); ax.set_ylim(1e-4, 30)
ticks = [1 / 60, 5 / 60, 1, 12, 72, 168]
ax.set_xticks(ticks, ["1 min", "5 min", "1 hour", "12 hours", "3 days", "7 days"])
ax.set_yticks([0.001, 0.01, 0.1, 1, 10], ["$0.001", "$0.01", "$0.10", "$1", "$10"])
ax.tick_params(colors=SUB, labelsize=18, length=0, pad=10)
for sp in ax.spines.values(): sp.set_visible(False)
ax.grid(color=AXIS, lw=1)
ax.set_xlabel("how long the cache sits idle", fontsize=19, color=SUB, labelpad=12)
ax.set_ylabel("cost of holding it", fontsize=19, color=SUB, labelpad=12)
fig.savefig(os.path.join(OUT, "hold-vs-recompute.png"), facecolor=BG); plt.close(fig)
print(f"recompute ${recompute:.3f}, {gb:.1f} GB, break-even: " + ", ".join(f"{t[0]} {recompute / t[1]:.2f} h" for t in tiers))
