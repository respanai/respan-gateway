# Prompt caching experiments

The GPU experiments behind [From the GPU to your bill: a deep dive into prompt caching](../README.md). They open up a KV cache on one H100 and measure what the article describes: how much a cache hit saves, why only the beginning of a prompt can be reused, how a full GPU evicts old caches, how fast a cache comes back from CPU memory, and how routing decides whether a turn finds its cache.

Everything runs on one NVIDIA H100 80GB with Qwen3-8B on vLLM. Our run (November 7, 2026, vLLM 0.33.0, torch 2.14) took about 30 minutes and cost about $2 on a rented GPU. Raw results are in [`results/`](results/).

| | Script | What it measures |
|---|---|---|
| A | `exp_a_ttft_vs_length.py` | Time to first token for the same prompt sent twice, cold then cached, from 1K to 100K tokens |
| B | `exp_b_edit_position.py` | Change one token in a cached 32K prompt at different positions; how much still hits |
| C | `exp_c_eviction.py` | Open more sessions than the cache pool holds; which ones are still cached afterwards |
| D | `exp_d_offload.py` | The same 16K prompt with its cache on the GPU, reloaded from CPU memory, or gone |
| E | `exp_e_routing.py` | Two replicas, 15 conversations of 6 turns; hit rate under random, round-robin and sticky routing |

## Results

**A.** 100K tokens took 7.55 s to read cold and 0.14 s from cache, 53× faster. At 8K the gap is 4.8×, at 32K 14×. Cold time grows faster than length: a least-squares fit of `a·n + b·n²` explains it, with the squared (attention) part at 65% of the time at 100K.

**B.** The cache is kept in 16-token blocks on this setup. Across 13 edit positions, the hit length was always exactly `floor(position / 16) × 16`. Changing the first token cost 1.34 s; changing token 31,990 cost 0.10 s.

**C.** With a pool of about 71K tokens and eight 16K sessions, the four newest stayed fully cached, the three oldest were evicted, and the one in between kept 44%: its beginning survived and its end was evicted first.

**D.** Median time to first token for a 16K prompt: 0.069 s with the cache on the GPU, 0.165 s reloaded from CPU memory (vLLM's native KV offloading, confirmed by `vllm:external_prefix_cache_hits`), 0.549 s with no cache.

**E.** Sticky routing (a conversation always goes to the same replica) hit 75% of input tokens from cache, random routing 56%, round robin 54%.

## Run it

1. Rent a machine with one H100 and SSH in.
2. Copy this folder over and run:
   ```bash
   bash setup.sh && bash run_all.sh
   ```
   `setup.sh` installs vLLM and matplotlib and downloads Qwen3-8B (about 16 GB). `run_all.sh` runs A to E, draws one PNG per experiment with `plot.py`, and packs everything into `results.tar.gz`. Run a subset with `bash run_all.sh a b`.
3. Shut the machine down when it's done.

Without a GPU, `mock_vllm.py` stands in for vLLM (64-token blocks, chained hashes, LRU eviction, simulated prefill time) so you can check the scripts end to end:

```bash
python3 mock_vllm.py 8000 1600 &
python3 exp_b_edit_position.py 8000 /tmp/b.json
```

## Notes

- Requests go to `/v1/completions` as token IDs, so chat templates and Qwen3's thinking don't shift positions.
- Cache hit length comes from `usage.prompt_tokens_details.cached_tokens` (`--enable-prompt-tokens-details`), cross-checked with `vllm:prefix_cache_hits` in `/metrics`.
- Each measurement starts from an empty cache via `/reset_prefix_cache`, which needs `VLLM_SERVER_DEV_MODE=1`. Only enable that on a throwaway machine.
- 100K context uses YaRN, set through `--hf-overrides` with both `rope_parameters` and `max_position_embeddings` (transformers 5.x). If the server won't start with it, `run_all.sh` falls back to 32K.
- On images with an older CUDA toolkit, FlashInfer's sampler fails to JIT-compile; `run_all.sh` sets `VLLM_USE_FLASHINFER_SAMPLER=0`.
- Experiment E runs two vLLM instances on one GPU on ports 8101 and 8102. Some hosting images already use 8001.
