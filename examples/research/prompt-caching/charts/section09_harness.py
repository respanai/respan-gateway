"""Section 7 diagram: the layers a harness stacks into every request, with real bugs that broke the cache at each layer.
Sources (research/L4-harnesses.md): Codex #2610 (MCP tool order), OpenCode #31266 (date at midnight), Cline sdk-mode-coordinator.ts
(Plan/Act rebuild, read in source), Claude Code #86244 (resume after auto-update), #96578 (auto-memory), Anthropic postmortem
Apr 23 2026 (thinking blocks dropped), Claude Code #91705 (pasted image re-sent as text), LiteLLM #26625 (cache markers dropped)."""
import os
from chartstyle import canvas, plt, BG, TEXT, SUB, HI
from matplotlib.patches import FancyBboxPatch

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "img")
MARK = "#f0883e"
layers = [("Tool definitions", "Codex listed MCP tools in a random order on each request"),
          ("System prompt", "OpenCode's date changed at midnight\nCline rebuilds it when you switch between Plan and Act"),
          ("Project memory", "Claude Code's auto-memory rewrote part of the prompt every turn"),
          ("Conversation history", "Claude Code dropped old thinking blocks (Anthropic's April postmortem)\nClaude Code re-sent a pasted image as text on the next turn"),
          ("New message", "always new, never cached")]
fig = canvas("Every layer of the prompt has broken someone's cache",
             "What a harness sends on every turn, top to bottom, and real bugs that changed each layer",
             "From public bug reports and Anthropic's April 2026 postmortem, plus Cline's source code. A change in any layer invalidates the cache for that layer and\n"
             "everything below it. Not shown: gateways that silently drop cache markers, like LiteLLM did for Claude on Bedrock.")
ax = fig.add_axes([0.035, 0.13, 0.93, 0.64]); ax.axis("off"); ax.set_xlim(0, 100); ax.set_ylim(0, 100)
n = len(layers); h = 15.5; gap = 4.2
for i, (name, bug) in enumerate(layers):
    y = 100 - (i + 1) * h - i * gap
    last = i == n - 1
    ax.add_patch(FancyBboxPatch((0, y), 30, h, boxstyle="round,pad=0,rounding_size=1.2",
                                fc="#2c2c2a" if last else HI, ec="none", alpha=1 if last else 0.9 - i * 0.12))
    ax.text(2, y + h / 2, name, fontsize=24, color=TEXT, va="center")
    if not last:
        ax.scatter([33.5], [y + h / 2], s=200, color=MARK, zorder=3)
        ax.text(36, y + h / 2, bug, fontsize=20, color=TEXT, va="center", linespacing=1.5)
    else:
        ax.text(36, y + h / 2, bug, fontsize=20, color=SUB, va="center")
ax.annotate("", xy=(31.5, 2), xytext=(31.5, 98), arrowprops=dict(arrowstyle="-|>", color=SUB, lw=1.5))
fig.savefig(os.path.join(OUT, "harness-layers.png"), facecolor=BG); plt.close(fig)
print("ok")
