"""Section 7: what cache hit rate to expect by kind of workload, drawn as a vertical 0-100% scale.
Agents ~90%: OpenRouter Inferred #3 (Oct 8, 2026), agent traffic just under 90% cached in September.
Single-turn ~70%: OpenAI prompt caching guide, example of a single-turn judge (~70%, "not guarantees").
Chat ~55%: DeepSeek, 56.3% of input hit its disk cache (Feb 2025); Kimi's Mooncake trace, reuse tops out at 55%.
Short prompts 0%: below the minimum cacheable length (512 / 1,024 / 4,096 tokens) nothing is cached."""
import os
from chartstyle import canvas, plt, BG, TEXT, SUB, HI, LIGHT, AXIS

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "img")
rows = [(90, "Agents", "Each turn resends a long, growing history. Only the newest part is new.", True),
        (70, "Single-turn tasks", "Classifying, grading, answering over documents. Only the shared instructions repeat.", False),
        (55, "Chat", "Shorter conversations, and each new message is a bigger share of the prompt.", False),
        (0, "Short prompts", "Under 512 to 4,096 tokens, depending on the model, nothing is cached at all.", False)]
fig = canvas("The hit rate you can expect depends on the workload",
             "Share of input tokens that can come from the cache, by kind of workload",
             "Agents: OpenRouter agent traffic, Sept 2026. Single-turn: OpenAI's caching guide, single-turn grader example. Chat: DeepSeek's API (56%, Feb 2025)\n"
             "and Kimi's Mooncake trace (55%). Short prompts: providers' minimum lengths. Coding agents on the left: Vibe Usage, April 2026 (vendor data).")
ax = fig.add_axes([0.06, 0.13, 0.9, 0.64]); ax.axis("off"); ax.set_xlim(0, 100); ax.set_ylim(-6, 106)
X = 24
ax.plot([X, X], [0, 100], color=AXIS, lw=10, solid_capstyle="round")
ax.text(X, 102.5, "100%", fontsize=16, color=SUB, ha="center", va="bottom")
for v, name, why, hi in rows:
    col = HI if hi else LIGHT
    ax.scatter([X], [v], s=420, color=col, zorder=3, edgecolors=BG, linewidths=3)
    ax.plot([X + 1.5, 34], [v, v], color=col, lw=1.5)
    ax.text(35, v + 1.2, f"~{v}%" if v else "0%", fontsize=40, color=col if hi else TEXT, va="bottom")
    ax.text(45, v + 2.4, name, fontsize=26, color=TEXT, va="bottom")
    ax.text(45, v + 0.6, why, fontsize=18, color=SUB, va="top")
# coding agents measured by Vibe Usage (Apr 22, 2026), small, left of the scale
agents = [(92, "Codex CLI"), (91, "Claude Code"), (67, "OpenCode"), (59, "Gemini CLI")]
for v, name in agents:
    ax.plot([X - 4.5, X - 1.5], [v, v], color=SUB, lw=1.2)
    ax.scatter([X - 4.5], [v], s=60, color=SUB, zorder=3)
ax.text(X - 5.5, 92.6, "Codex CLI 92%", fontsize=16, color=SUB, ha="right", va="bottom")
ax.text(X - 5.5, 90.4, "Claude Code 91%", fontsize=16, color=SUB, ha="right", va="top")
ax.text(X - 5.5, 67, "OpenCode 67%", fontsize=16, color=SUB, ha="right", va="center")
ax.text(X - 5.5, 59, "Gemini CLI 59%", fontsize=16, color=SUB, ha="right", va="center")
ax.text(X - 5.5, 97.5, "coding agents, measured:", fontsize=15, color=SUB, ha="right", va="bottom", style="italic")
fig.savefig(os.path.join(OUT, "hit-rate-by-workload.png"), facecolor=BG); plt.close(fig)
print("ok")
