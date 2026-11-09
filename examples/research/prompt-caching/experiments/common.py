"""Shared helpers for the E1 vLLM experiments (stdlib only).

All requests go to /v1/completions with a list of token ids, max_tokens=1, streamed,
so TTFT ~= prefill time and usage.prompt_tokens_details.cached_tokens = prefix-cache hit length.
"""
import http.client, json, os, random, statistics, time

MODEL = os.environ.get("E1_MODEL_NAME", "qwen")
VOCAB_LO, VOCAB_HI = 1000, 150000          # stay clear of Qwen3 special tokens (>=151643)
KV_BYTES_PER_TOKEN = 2 * 36 * 8 * 128 * 2  # Qwen3-8B: K+V x layers x kv_heads x head_dim x bf16 = 147,456


def rand_ids(n, seed):
    r = random.Random(seed)
    return [r.randint(VOCAB_LO, VOCAB_HI) for _ in range(n)]


def complete(port, ids, max_tokens=1, timeout=600):
    """Send one streamed completion. Returns dict(ttft, total, prompt_tokens, cached_tokens)."""
    body = json.dumps({"model": MODEL, "prompt": ids, "max_tokens": max_tokens, "temperature": 0,
                       "stream": True, "stream_options": {"include_usage": True}})
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    t0 = time.perf_counter()
    conn.request("POST", "/v1/completions", body, {"Content-Type": "application/json"})
    resp = conn.getresponse()
    if resp.status != 200:
        raise RuntimeError(f"HTTP {resp.status}: {resp.read()[:500]!r}")
    ttft, usage = None, None
    for raw in resp:
        line = raw.decode().strip()
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            break
        chunk = json.loads(data)
        if ttft is None and chunk.get("choices"):
            ttft = time.perf_counter() - t0
        if chunk.get("usage"):
            usage = chunk["usage"]
    total = time.perf_counter() - t0
    conn.close()
    details = (usage or {}).get("prompt_tokens_details") or {}
    return {"ttft": ttft, "total": total, "prompt_tokens": (usage or {}).get("prompt_tokens"),
            "cached_tokens": details.get("cached_tokens")}


def _req(port, method, path, timeout=30):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    conn.request(method, path)
    r = conn.getresponse()
    out = r.status, r.read().decode()
    conn.close()
    return out


def reset_cache(port):
    status, text = _req(port, "POST", "/reset_prefix_cache")
    if status != 200:
        raise RuntimeError(f"reset_prefix_cache failed: {status} {text[:200]}")
    time.sleep(0.5)


def metrics(port):
    """Parse the vllm:* lines of /metrics into {name: summed value}."""
    _, text = _req(port, "GET", "/metrics")
    out = {}
    for line in text.splitlines():
        if not line.startswith("vllm:"):
            continue
        name_labels, _, value = line.rpartition(" ")
        name = name_labels.split("{")[0]
        try:
            out[name] = out.get(name, 0.0) + float(value)
        except ValueError:
            pass
    return out


def metric(m, base):
    """Counters may be exported with or without a _total suffix."""
    return m.get(base + "_total", m.get(base, 0.0))


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def save(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(obj, f, indent=1)
    print("saved", path)
