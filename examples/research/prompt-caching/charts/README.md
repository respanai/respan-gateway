# Charts

The code behind every image in the article. Each script writes its PNGs to `../img`.

```bash
pip install matplotlib
python3 section04_gpu_memory.py
```

The charts use the [Inter](https://rsms.me/inter/) font; matplotlib falls back to its default font if Inter isn't installed.

| Script | Draws |
| --- | --- |
| `cover.py` | `cover.png` |
| `section02_input_cost.py` | `input-output-ratio.png`, `read-time-vs-length.png` |
| `section03_kv_cache.py` | `edit-one-token.png`, `kv-size.png` |
| `section04_gpu_memory.py` | `eviction.png`, `offload.png`, `routing.png` |
| `section05_hold_vs_recompute.py` | `hold-vs-recompute.png` |
| `section06_host_prices.py` | `host-cache-prices.png` |
| `section07_hit_rate.py` | `hit-rate-by-workload.png` |
| `section08_bill.py` | `bill-breakdown.png` |
| `section09_harness.py` | `harness-layers.png` |
| `section10_switch.py` | `switch-payback.png` |
| `section11_router.py` | `router-kinds.png` |
| `chartstyle.py` | The shared style: colors, font, layout |

Data: the GPU charts read the raw results in [`../experiments/results`](../experiments/results). `data/` holds the rest: OpenRouter's endpoint listings for four open models, fetched on November 6, 2026, and `session_scenarios.json`, the session cost model behind the bill breakdown. Prices in the other scripts are written inline, with their sources in each script's docstring.
