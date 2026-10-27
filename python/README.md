# respan-gateway for Python

The [respan-gateway](https://github.com/respanai/respan-gateway) LLM gateway as
a Python package: use it in-process with the official OpenAI and Anthropic
SDKs (no socket, no server), run it as a local server, or use the same
command line as the Rust binary. The gateway is the Rust one, compiled into a
native extension (`abi3`, Python 3.10+).

## Install

```bash
pip install respan-gateway            # add `openai` and/or `anthropic` for gw.openai() / gw.anthropic()
```

## Use the SDKs in-process

```python
import respan_gateway as rg

with rg.Gateway() as gw:             # from OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, ...
    client = gw.openai()             # an openai.OpenAI that talks to gw directly

    # A Claude model through the OpenAI SDK is translated to Anthropic.
    reply = client.chat.completions.create(
        model="claude-haiku-5-5", messages=[{"role": "user", "content": "hi"}]
    )
```

`gw.anthropic()` is the same for `anthropic.Anthropic`; `gw.openai_async()`
and `gw.anthropic_async()` give the async clients. Leaving the `with` (or
`async with`) block stops the servers the gateway started and closes the
clients it handed out. For any other httpx-based client, `gw.transport()` and
`gw.async_transport()` are `httpx` transports:
`httpx.Client(transport=gw.transport())`. Raw requests:
`gw.request("POST", "/v1/chat/completions", json=...)` returns an
`httpx.Response`; `gw.stream(...)` yields the body chunk by chunk as it
arrives. `arequest` and `astream` are the asyncio versions.

### Every kind

Each kind is an OpenAI endpoint the gateway serves when a provider speaks its
format (`openai-images`, `openai-transcriptions`, `openai-speech`,
`openai-embeddings`, `openai-videos`; see `docs/configuration.md`), and works
through the same client:

```python
client.images.generate(model="gpt-image-1", prompt="a red fox").data[0].b64_json

with open("talk.wav", "rb") as f:                      # a multipart upload
    client.audio.transcriptions.create(model="whisper-1", file=f).text

client.audio.speech.create(model="tts-1", voice="alloy", input="hi").read()   # audio bytes

client.embeddings.create(model="text-embedding-3-small", input="hi").data[0].embedding
# (the SDK asks for base64 and decodes it; the gateway passes it through)

job = client.videos.create(model="gemini-omni-1.1-flash", prompt="waves")  # then poll:
job = client.videos.retrieve(job.id)                             # queued -> completed
client.videos.download_content(job.id).read()                    # mp4 bytes
```

A model's kind is read from its name and the providers' formats: `gw.models()`
lists the merged models of every provider as `/v1/models` does, each with its
`kind` (`gw.models(kind="image")` filters).

### Errors

HTTP error statuses are responses (and the SDKs raise their own exceptions
for them), never exceptions of this package. A failure to carry out a request
or start a server is a `GatewayError`. A timeout is httpx's
`httpx.ReadTimeout`, which an SDK raises as its own timeout error. An invalid
configuration is a `ConfigError` (a `ValueError`).

### What the gateway knows

```python
gw.models()                # [{"id": "gpt-image-1", "owned_by": "openai", "kind": "image"}, ...]
gw.providers()             # each provider with its formats and request counts
gw.requests(limit=20)      # the request log, newest first; filters such as client="ci" (a client key's name)
```

`amodels()`, `aproviders()` and `arequests()` are the asyncio versions.
`providers()` and `requests()` read the web UI's admin API in-process, so
`ui = false` turns them off (the log is in memory, always on with the UI).
A gateway in your process prints nothing per request: `log` defaults to
`off` here (the binary's is `text`). Set `log = 'text'` or `log = 'json'` in the
config for stderr lines, `serve_forever()` included.

## Serve

```python
with gw.serve(port=0) as server:     # a real socket, in the background
    print(server.url)                # http://127.0.0.1:54321

async with gw.aserve() as server:    # the same for asyncio
    ...

gw.serve_forever(port=8080)          # blocks; Ctrl-C stops it (serve() picks a free port by default)
```

The server is the binary's (`server.url + "/ui/"` is the web UI; wheels from
PyPI embed it, a build from the sdist or a plain checkout shows a placeholder
page unless `apps/ui` is built first). Without
`client_keys` in the config, anyone who reaches it spends your provider keys:
keep it on a local address. `Gateway.openai()`'s `api_key` is one of those
client keys when the config sets them; otherwise a placeholder, forwarded
only to a provider with no key of its own.

### Realtime

WebSocket sessions (OpenAI Realtime, Gemini Live, translated between each
other) need a socket: use `serve()` and connect any WebSocket client, with
`http` replaced by `ws`:

```python
import websockets

with gw.serve() as server:
    async with websockets.connect(server.url.replace("http", "ws") + "/v1/realtime?model=gpt-realtime") as ws:
        ...
```

## Command line

```bash
respan-gateway                                # also: init, check, proxy --anthropic|--openai, --help
python -m respan_gateway proxy --openai
```

## Configuration

`Gateway(config=...)` takes a dict (typed as `rg.GatewayConfig` for editor completion) or a TOML string in the format of
[`gateway.example.toml`](../gateway.example.toml); `Gateway.from_file(path)`
reads a file. With no config, `Gateway()` starts as the binary does with no
arguments: from `./gateway.toml` if it exists, else from the environment:
every vendor whose key is set (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`, ...) becomes a
provider, models route by name (`claude-*` to Anthropic, `gemini-*` to
Gemini, or explicitly `groq/openai/gpt-oss-20b`), and
`RESPAN_<VENDOR>_BASE_URL` points one elsewhere.

## Development

```bash
cd python
uv venv && uv pip install maturin pytest httpx openai anthropic mypy
.venv/bin/maturin develop        # builds the extension into .venv
.venv/bin/pytest                 # tests run against a fake provider, offline
.venv/bin/mypy --strict python/respan_gateway
```

The extension is the `respan-gateway-python` crate (`src/`), a workspace
member that depends on the `respan-gateway` library in `apps/gateway`. The
`extension-module` feature is enabled by maturin only, so `cargo test` and
`cargo clippy` of the workspace link normally. One tokio runtime, started on
first use, serves every gateway, server and the command line; every call that
waits releases the GIL. Async requests use a native callback API (`send_async`, `next_chunk_async`)
rather than executor threads: a waiting request holds no thread, so many
open streams cannot starve anything, and cancelling the awaiting task drops
the in-flight router future, which cancels the provider call. Sync calls
block with the GIL released and wake every 100 ms to deliver Ctrl-C. httpx's
read timeout bounds the wait for the response head and each body chunk
(connect, write and pool timeouts have nothing to bound in-process). A server
from `serve()` keeps running when its handle is dropped, until `stop()` or
the process exits.
