"""Mock vLLM server: block-64 chained-hash prefix cache with LRU, simulated prefill time. For dry-running E1 scripts."""
import json, sys, time, hashlib
from collections import OrderedDict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
CAP_BLOCKS = int(sys.argv[2]) if len(sys.argv) > 2 else 800; B = 64
cache = OrderedDict(); hits = [0]; queries = [0]
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, body, ctype="text/plain"):
        self.send_response(code); self.send_header("Content-Type", ctype); self.end_headers(); self.wfile.write(body.encode())
    def do_GET(self):
        if self.path == "/health": return self._send(200, "ok")
        if self.path == "/metrics":
            return self._send(200, f"vllm:prefix_cache_queries_total{{engine=\"0\"}} {queries[0]}\nvllm:prefix_cache_hits_total{{engine=\"0\"}} {hits[0]}\nvllm:external_prefix_cache_hits_total{{engine=\"0\"}} 0\nvllm:kv_cache_usage_perc{{engine=\"0\"}} {len(cache)/CAP_BLOCKS}\n")
    def do_POST(self):
        if self.path == "/reset_prefix_cache": cache.clear(); return self._send(200, "")
        ids = json.loads(self.rfile.read(int(self.headers["Content-Length"])))["prompt"]
        h, hit, keys = b"", 0, []
        for i in range(len(ids) // B):
            h = hashlib.sha256(h + json.dumps(ids[i*B:(i+1)*B]).encode()).digest(); keys.append(h)
        for k in keys:
            if k in cache: hit += B; cache.move_to_end(k)
            else: break
        hit = min(hit, (len(ids) - 1) // B * B)
        for k in keys:
            cache[k] = 1; cache.move_to_end(k)
            while len(cache) > CAP_BLOCKS: cache.popitem(last=False)
        queries[0] += len(ids); hits[0] += hit
        time.sleep((len(ids) - hit) * 2e-6 + 0.002)
        self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.end_headers()
        self.wfile.write(b'data: {"choices":[{"text":"x"}]}\n\n')
        u = {"usage": {"prompt_tokens": len(ids), "prompt_tokens_details": {"cached_tokens": hit}}, "choices": []}
        self.wfile.write(f"data: {json.dumps(u)}\n\ndata: [DONE]\n\n".encode())
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
