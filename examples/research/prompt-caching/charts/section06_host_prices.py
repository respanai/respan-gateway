"""Strip chart for section 6: cached-input price as % of input, one dot per host endpoint on OpenRouter.
Data: data/endpoints_*_20261106T030600Z.json (fetched 2026-11-06 03:06 UTC). Missing cache price = 100%."""
import glob, json, os, random
from chartstyle import canvas, plt, BG, TEXT, SUB, HI, LIGHT, AXIS

HERE = os.path.dirname(os.path.abspath(__file__))
EV = os.path.join(HERE, "data")
OUT = os.path.join(HERE, "..", "img")
models = [("DeepSeek V4.1 Flash", "deepseek__deepseek-v4.1-flash-1017", 2.0, "DeepSeek's own price"),
          ("Kimi K3", "moonshotai__kimi-k3_2", 10.0, "Moonshot's own price"),
          ("GLM-5.3", "z-ai__glm-5.3_2", 18.6, "Z.ai's own price"),
          ("gpt-oss-120b", "openai__gpt-oss-120b", None, "no official API")]
fig = canvas("Hosts copy the model maker's cache price",
             "Cached input price as a share of normal input price. Each dot is one host serving the model.",
             "OpenRouter endpoints API, November 6, 2026: 154 endpoints across four open-weight models. A host with no cache price is plotted at 100%.\n"
             "gpt-oss-120b is released by OpenAI but not sold on OpenAI's own API, so there is no official price for hosts to follow.")
ax = fig.add_axes([0.2, 0.16, 0.76, 0.6]); ax.set_facecolor(BG)
rnd = random.Random(3)
n = len(models)
for i, (name, slug, anchor, alabel) in enumerate(models):
    y = n - 1 - i
    eps = json.load(open(glob.glob(os.path.join(EV, f"endpoints_{slug}*.json"))[0]))["data"]["endpoints"]
    xs = []
    for e in eps:
        p = e["pricing"]; inp = float(p["prompt"]); c = p.get("input_cache_read")
        xs.append(100.0 if c in (None, "") or inp == 0 else min(float(c) / inp * 100, 100.0))
    ax.scatter(xs, [y + rnd.uniform(-0.22, 0.22) for _ in xs], s=110, color=HI if anchor else LIGHT, alpha=0.75,
               edgecolors=BG, linewidths=1, zorder=3)
    if anchor:
        ax.plot([anchor, anchor], [y - 0.38, y + 0.38], color=TEXT, lw=3, zorder=4)
        ax.text(anchor, y + 0.42, alabel, fontsize=16, color=TEXT, ha="center", va="bottom")
    else:
        ax.text(100, y + 0.42, f"{sum(x >= 95 for x in xs)} of {len(xs)} hosts: no real discount", fontsize=16, color=TEXT,
                ha="right", va="bottom")
ax.set_xscale("log"); ax.set_xlim(1.5, 130); ax.set_ylim(-0.6, n - 0.2)
ax.set_xticks([2, 5, 10, 20, 50, 100], ["2%", "5%", "10%", "20%", "50%", "100%"]); ax.minorticks_off()
ax.set_yticks(range(n), [m[0] for m in reversed(models)])
ax.tick_params(axis="x", colors=SUB, labelsize=17, length=0)
ax.tick_params(axis="y", colors=TEXT, labelsize=21, length=0, pad=230)
for t in ax.get_yticklabels(): t.set_ha("left")
for sp in ax.spines.values(): sp.set_visible(False)
ax.grid(axis="x", color=AXIS, lw=1)
fig.savefig(os.path.join(OUT, "host-cache-prices.png"), facecolor=BG); plt.close(fig)
print("ok")
