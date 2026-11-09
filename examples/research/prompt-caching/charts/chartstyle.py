"""Shared dark benchmark style for the caching blog charts (1920x1080)."""
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import Rectangle

BG, TEXT, SUB, FOOT, BAR, HI, AXIS, LIGHT = "#1c1c1b", "#f2f2f0", "#a3a39f", "#8a8a86", "#5c5b57", "#3b88e6", "#3a3a37", "#d9d9d6"
plt.rcParams.update({"font.family": "Inter", "text.color": TEXT, "text.parse_math": False})


def canvas(title, subtitle, footnote):
    fig = plt.figure(figsize=(19.2, 10.8), dpi=100, facecolor=BG)
    fig.text(0.035, 0.89, title, fontsize=50, color=TEXT, va="baseline")
    fig.text(0.035, 0.835, subtitle, fontsize=27, color=SUB, va="baseline")
    fig.text(0.035, 0.04, footnote, fontsize=17, color=FOOT, va="baseline", linespacing=1.5)
    return fig


def hbars(fig, rows, rect=(0.035, 0.17, 0.93, 0.58), xmax=None, fmt=str, ref=None):
    """rows: list of (label, value, highlighted, value_text_or_None). Label above each bar, value on/after it.
    ref: optional (value, label) drawn as a vertical dashed reference line."""
    ax = fig.add_axes(rect); ax.set_facecolor(BG); ax.axis("off")
    n = len(rows); xmax = xmax or max(r[1] for r in rows) * 1.12
    ax.set_xlim(0, xmax); ax.set_ylim(0, n * 2)
    for i, (label, val, hi, vtxt) in enumerate(rows):
        y = (n - 1 - i) * 2 + 0.25
        ax.text(0, y + 1.0, label, fontsize=21, color=TEXT, va="bottom")
        ax.add_patch(Rectangle((0, y), val, 0.85, color=HI if hi else BAR, lw=0))
        t = vtxt if vtxt is not None else fmt(val)
        inside = val > xmax * 0.18
        ax.text(val - xmax * 0.008 if inside else val + xmax * 0.008, y + 0.42, t, fontsize=24,
                color=TEXT, va="center", ha="right" if inside else "left")
    if ref:
        ax.axvline(ref[0], color=SUB, ls="--", lw=2)
        ax.text(ref[0] + xmax * 0.006, n * 2 - 0.1, ref[1], fontsize=19, color=SUB, va="top")
    return ax
