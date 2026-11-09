"""Section 8 diagram: one 16-turn session under three kinds of router. Illustrative, not measured.
Each square is a turn, labelled with the model it ran on; an orange outline marks a turn that rewrote the whole history (cold cache).
Kinds from research/L5-routing.md: per-request (Azure model router, Bedrock prompt routing, LiteLLM cost routing),
rule-based (GitHub Copilot Auto: first turn and after compaction), switch-priced (Cloudflare, DigitalOcean, Cursor, Not Diamond)."""
import os
from chartstyle import canvas, plt, BG, TEXT, SUB, HI, LIGHT
from matplotlib.patches import FancyBboxPatch

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "img")
MARK = "#f0883e"
COL = {"A": HI, "B": LIGHT, "C": "#8a8a86"}
EASY = 5   # from this turn (0-based) the task is easy enough for the cheaper model B
lanes = [("Decide every request, ignore the cache", "Azure model router, Bedrock prompt routing, LiteLLM cost routing",
          list("ABACBBACABCABBAC"), "switches 13 times, rewriting each time", MARK),
         ("Decide every request, price the switch", "Cloudflare, DigitalOcean, Cursor, Not Diamond",
          list("AAAAAABBBBBBBBBB"), "moves to B as soon as it pays", TEXT),
         ("Only switch when there's no cache to lose", "GitHub Copilot Auto: first turn and after compaction",
          list("AAAAAAAAAAA") + ["|"] + list("BBBBB"), "stays on A until compaction", TEXT)]
fig = canvas("Does your router count the cache?",
             "One 16-turn session that gets easier partway through. A is a strong, expensive model; B is cheaper.",
             "Illustration, not measured traffic. Orange outline: a switch, paid for with a cache write. Dimmed: turns spent on A after B would have done.\n"
             "| marks a compaction. Router examples from vendor docs and launch posts, November 2026.")
ax = fig.add_axes([0.035, 0.1, 0.93, 0.68]); ax.axis("off"); ax.set_xlim(0, 100); ax.set_ylim(0, 100)
STEP = 5.2
for li, (title, who, seq, verdict, vcol) in enumerate(lanes):
    y = 72 - li * 31
    ax.text(0, y + 17, title, fontsize=25, color=TEXT, va="bottom")
    ax.text(0, y + 13.5, who, fontsize=17, color=SUB, va="bottom")
    x = 0; prev = None; turn = 0
    for t in seq:
        if t == "|":
            ax.plot([x + 0.6, x + 0.6], [y - 1, y + 11], color=SUB, lw=3); x += 1.6; prev = "|"; continue
        cold = (prev is not None and prev != "|" and t != prev) or prev == "|"
        wasted = li == 2 and t == "A" and turn >= EASY
        ax.add_patch(FancyBboxPatch((x, y), 4.6, 10, boxstyle="round,pad=0,rounding_size=0.6",
                                    fc=COL[t], ec=MARK if cold else "none", lw=4 if cold else 0,
                                    hatch="//" if wasted else None, alpha=0.55 if wasted else 1))
        ax.text(x + 2.3, y + 5, t, fontsize=16, color=BG, ha="center", va="center", fontweight="bold")
        x += STEP; prev = t; turn += 1
    ax.text(100, y + 17, verdict, fontsize=22, color=vcol, va="bottom", ha="right")
xe = EASY * STEP - 0.3
for y0 in (41, 10):
    ax.plot([xe, xe], [y0 - 1.5, y0 + 11.5], color=TEXT, lw=2, ls="--")
ax.text(xe, 7, "↑ the task gets easier here", fontsize=17, color=TEXT, va="top", ha="center")
fig.savefig(os.path.join(OUT, "router-kinds.png"), facecolor=BG); plt.close(fig)
print("ok")
