#!/usr/bin/env bash
# Install vLLM + plotting deps and pre-download Qwen3-8B on a fresh H100 machine.
set -euo pipefail
cd "$(dirname "$0")"

nvidia-smi
python3 -m pip install -q --user uv
export PATH="$HOME/.local/bin:$PATH"
[ -d .venv ] || uv venv -q --python 3.12 .venv
# shellcheck disable=SC1091
source .venv/bin/activate
uv pip install -q vllm matplotlib huggingface_hub
python -c "import vllm, torch; print('vllm', vllm.__version__, 'torch', torch.__version__, 'cuda', torch.cuda.is_available())"
# newer huggingface_hub ships `hf`; older ones `huggingface-cli`
python -c "from huggingface_hub import snapshot_download; print(snapshot_download('Qwen/Qwen3-8B'))"
echo "setup done"
