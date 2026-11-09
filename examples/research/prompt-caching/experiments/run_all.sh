#!/usr/bin/env bash
# Run E1 experiments A–E on one H100. Usage: bash run_all.sh [a b c d e]   (default: all)
set -uo pipefail
cd "$(dirname "$0")"
# shellcheck disable=SC1091
source .venv/bin/activate
export VLLM_SERVER_DEV_MODE=1           # enables /reset_prefix_cache (experiment box only)
export VLLM_USE_FLASHINFER_SAMPLER=0    # FlashInfer JIT needs a newer nvcc than many pod images ship (CUDA 12.4)
MODEL=Qwen/Qwen3-8B
RUN="results/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$RUN/logs"
STEPS="${*:-a b c d e}"
# transformers 5.x renamed rope_scaling -> rope_parameters (Qwen3 rope_theta = 1e6)
YARN='{"max_position_embeddings":131072,"rope_parameters":{"rope_type":"yarn","factor":4.0,"original_max_position_embeddings":32768,"rope_theta":1000000}}'

{ nvidia-smi; python -c "import vllm, torch; print('vllm', vllm.__version__, 'torch', torch.__version__)"; } > "$RUN/env.txt" 2>&1

start() {  # start <name> <port> <gpu_mem_util> <max_len> [extra args...]
  local name=$1 port=$2 util=$3 maxlen=$4; shift 4
  vllm serve "$MODEL" --served-model-name qwen --port "$port" --enable-prompt-tokens-details \
    --gpu-memory-utilization "$util" --max-model-len "$maxlen" "$@" > "$RUN/logs/$name.log" 2>&1 &
  echo $! > "$RUN/logs/$name.pid"
  for _ in $(seq 1 180); do
    curl -sf "http://127.0.0.1:$port/health" >/dev/null && { echo "[$name] up on :$port"; return 0; }
    kill -0 "$(cat "$RUN/logs/$name.pid")" 2>/dev/null || { echo "[$name] exited, see $RUN/logs/$name.log"; return 1; }
    sleep 5
  done
  echo "[$name] timeout"; return 1
}
stop() {  # stop <name> <port>
  curl -s "http://127.0.0.1:$2/metrics" > "$RUN/logs/$1.metrics.txt" || true
  kill "$(cat "$RUN/logs/$1.pid")" 2>/dev/null; wait "$(cat "$RUN/logs/$1.pid")" 2>/dev/null; sleep 5
}
kv_tokens() { grep -oE "KV cache size: [0-9,]+ tokens" "$RUN/logs/$1.log" | tail -1 | grep -oE "[0-9,]+" | tr -d ,; }
has() { [[ " $STEPS " == *" $1 "* ]]; }

# A + B: one big server, try 128K with YaRN, fall back to 32K
if has a || has b; then
  if start long128k 8000 0.90 131072 --hf-overrides "$YARN"; then MAXLEN=131072; LONG=long128k
  else start long32k 8000 0.90 32768; MAXLEN=32768; LONG=long32k; fi
  has a && python exp_a_ttft_vs_length.py 8000 "$MAXLEN" "$RUN/a_ttft_vs_length.json"
  has b && python exp_b_edit_position.py 8000 "$RUN/b_edit_position.json"
  stop "$LONG" 8000
fi

# C: small KV pool so a few sessions fill it
if has c; then
  { start small 8000 0.35 32768 || start small 8000 0.45 32768; } && {
    python exp_c_eviction.py 8000 "$(kv_tokens small)" "$RUN/c_eviction.json"; stop small 8000; }
fi

# D: same small pool + 40 GiB CPU offload tier
if has d; then
  { start offload 8000 0.35 32768 --kv-offloading-size 40 --kv-offloading-backend native ||
    start offload 8000 0.45 32768 --kv-offloading-size 40 --kv-offloading-backend native; } && {
    python exp_d_offload.py 8000 "$(kv_tokens offload)" "$RUN/d_offload.json"; stop offload 8000; }
fi

# E: two replicas sharing the GPU
if has e; then
  if start rep_a 8101 0.42 32768; then
    start rep_b 8102 0.42 32768 && { python exp_e_routing.py 8101 8102 "$RUN/e_routing.json"; stop rep_b 8102; }
    stop rep_a 8101
  fi
fi

python plot.py "$RUN" || echo "plotting failed (raw JSON is still in $RUN)"
tar czf results.tar.gz results
echo "done: $RUN  ->  results.tar.gz   (remember to shut down the GPU instance)"
