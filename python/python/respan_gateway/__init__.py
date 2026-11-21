"""respan-gateway: an LLM gateway you can run in-process or as a server.

    import respan_gateway as rg

    with rg.Gateway(config={...}) as gw:   # or rg.Gateway() to use OPENAI_API_KEY etc.
        client = gw.openai()               # an openai.OpenAI that talks to gw directly
        client.chat.completions.create(model="gpt-4o-mini", messages=[...])
        client.images.generate(model="gpt-image-1", prompt="a red fox")
        print(gw.models())

HTTP error statuses are responses (and the SDKs' own exceptions); a failure to
run a request or start a server is a `GatewayError`, a timeout httpx's
`httpx.ReadTimeout` (an SDK's own timeout error), an invalid configuration a
`ConfigError`.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import os
import sys
from types import ModuleType
from typing import TYPE_CHECKING, Any, AsyncIterator, Dict, Iterator, List, Mapping, Optional, Union, cast

import httpx

from . import _native
from ._native import ConfigError, GatewayError, GatewayTimeout, __version__
from ._server import Server
from ._transport import transports
from .config import GatewayConfig

if TYPE_CHECKING:
    import anthropic as _anthropic
    import openai as _openai

__all__ = [
    "ConfigError",
    "Gateway",
    "GatewayConfig",
    "GatewayError",
    "GatewayTimeout",
    "Server",
    "__version__",
]

# The base URL of in-process requests; the transport sends its host
# (`respan-gateway`) whatever URL a client was given.
_BASE = "http://respan-gateway"


class Gateway:
    """A configured gateway.

    `config` is a mapping (see `GatewayConfig`), or a TOML string, in the
    format of `gateway.example.toml`. Without it, the gateway is configured as
    the binary is with no arguments: from `./gateway.toml` if that file
    exists, else from the environment: every vendor whose key is set
    (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, ...), with
    `RESPAN_<VENDOR>_BASE_URL` to point one elsewhere.

    Use it as a context manager (`with` or `async with`) to stop the servers
    it started and close the clients it handed out.

    Raises `ConfigError` (a `ValueError`) if the configuration is invalid.
    """

    def __init__(self, config: Union[GatewayConfig, Mapping[str, Any], str, None] = None) -> None:
        if config is None:
            native, self._source = _native.Native.discover(), "discovered"
        elif isinstance(config, str):
            native, self._source = _native.Native.from_toml(config), "toml"
        else:
            # Pass structured mappings directly through the native JSON bridge.
            native, self._source = _native.Native.from_json(json.dumps(config)), "dict"
        self._setup(native)

    def _setup(self, native: _native.Native) -> None:
        self._native = native
        self._servers: List[Server] = []
        self._clients: List[Any] = []  # httpx clients handed to SDKs
        self._sync: Optional[httpx.Client] = None
        self._async: Optional[httpx.AsyncClient] = None
        self._admin_sync: Optional[httpx.Client] = None  # trusted, for the helpers
        self._admin_async: Optional[httpx.AsyncClient] = None

    @classmethod
    def from_file(cls, path: Union[str, "os.PathLike[str]"]) -> "Gateway":
        """A gateway from the TOML file at `path`."""
        gateway = cls.__new__(cls)
        gateway._source = os.fspath(path)
        gateway._setup(_native.Native.from_file(os.fspath(path)))
        return gateway

    def __repr__(self) -> str:
        try:
            providers = ", ".join(p["name"] for p in self.providers())
        except Exception:  # a repr must not raise (e.g. `ui = false`)
            providers = "?"
        return f"Gateway(source={self._source!r}, providers=[{providers}])"

    # -- lifecycle -----------------------------------------------------------

    def close(self) -> None:
        """Stops the servers started through `serve()` and closes the sync clients handed out."""
        servers, self._servers = self._servers, []
        for server in servers:
            server.stop()
        clients = [c for c in (*self._clients, self._sync) if c is not None and not hasattr(c, "aclose")]
        self._clients = [c for c in self._clients if c not in clients]
        self._sync = self._admin_sync = None
        for client in clients:
            client.close()

    async def aclose(self) -> None:
        """`close` for asyncio, which also closes the async clients."""
        servers, self._servers = self._servers, []
        for server in servers:
            await server.astop()
        clients = [c for c in (*self._clients, self._async) if c is not None and hasattr(c, "aclose")]
        self._clients = [c for c in self._clients if c not in clients]
        self._async = self._admin_async = None
        for client in clients:
            await client.aclose()
        self.close()

    def __enter__(self) -> "Gateway":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    async def __aenter__(self) -> "Gateway":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.aclose()

    # -- clients -------------------------------------------------------------

    def transport(self) -> httpx.BaseTransport:
        """An httpx transport that sends requests to this gateway in-process."""
        return cast(httpx.BaseTransport, transports(httpx)[0](self._native))

    def async_transport(self) -> httpx.AsyncBaseTransport:
        """An httpx async transport that sends requests to this gateway in-process."""
        return cast(httpx.AsyncBaseTransport, transports(httpx)[1](self._native))

    def openai(self, *, api_key: Optional[str] = None, **kwargs: Any) -> "_openai.OpenAI":
        """An `openai.OpenAI` client wired to this gateway: chat, images,
        audio, embeddings and videos as far as the config serves them.

        `api_key` is one of the config's `client_keys` if it sets any;
        otherwise a placeholder, which reaches a provider only if the gateway
        holds no key of its own for it. Other keyword arguments go to the
        client.
        """
        openai = _import("openai")
        http = httpx.Client(transport=self.transport())
        self._clients.append(http)
        made = openai.OpenAI(base_url=_BASE + "/v1", api_key=api_key or "unused", http_client=http, **kwargs)
        return cast("_openai.OpenAI", made)

    def openai_async(self, *, api_key: Optional[str] = None, **kwargs: Any) -> "_openai.AsyncOpenAI":
        """`openai()` for asyncio: an `openai.AsyncOpenAI`."""
        openai = _import("openai")
        http = httpx.AsyncClient(transport=self.async_transport())
        self._clients.append(http)
        made = openai.AsyncOpenAI(base_url=_BASE + "/v1", api_key=api_key or "unused", http_client=http, **kwargs)
        return cast("_openai.AsyncOpenAI", made)

    def anthropic(self, *, api_key: Optional[str] = None, **kwargs: Any) -> "_anthropic.Anthropic":
        """An `anthropic.Anthropic` client wired to this gateway; `api_key` is as in `openai()`."""
        return cast("_anthropic.Anthropic", self._anthropic(False, api_key, kwargs))

    def anthropic_async(self, *, api_key: Optional[str] = None, **kwargs: Any) -> "_anthropic.AsyncAnthropic":
        """`anthropic()` for asyncio: an `anthropic.AsyncAnthropic`."""
        return cast("_anthropic.AsyncAnthropic", self._anthropic(True, api_key, kwargs))

    def _anthropic(self, asynchronous: bool, api_key: Optional[str], kwargs: Dict[str, Any]) -> Any:
        anthropic = _import("anthropic")
        http = _httpx_of(anthropic)
        transport = transports(http)[1 if asynchronous else 0](self._native)
        client = (http.AsyncClient if asynchronous else http.Client)(transport=transport)
        # Kept before the SDK sees it, so it is closed even if `kwargs` are refused.
        self._clients.append(client)
        cls = anthropic.AsyncAnthropic if asynchronous else anthropic.Anthropic
        return cls(base_url=_BASE, api_key=api_key or "unused", http_client=client, **kwargs)

    # -- raw requests --------------------------------------------------------

    def _client(self) -> httpx.Client:
        if self._sync is None:
            self._sync = httpx.Client(base_url=_BASE, transport=self.transport(), timeout=None)
        return self._sync

    def _aclient(self) -> httpx.AsyncClient:
        if self._async is None:
            self._async = httpx.AsyncClient(base_url=_BASE, transport=self.async_transport(), timeout=None)
        return self._async

    def _admin(self, path: str, **kwargs: Any) -> httpx.Response:
        """A read of the admin API or the model list for the helpers below. This
        process owns the gateway, so these (and only these) need no client key."""
        if self._admin_sync is None:
            transport = transports(httpx)[0](self._native, trusted=True)
            self._admin_sync = httpx.Client(base_url=_BASE, transport=transport, timeout=None)
            self._clients.append(self._admin_sync)
        return self._admin_sync.get(path, **kwargs)

    async def _aadmin(self, path: str, **kwargs: Any) -> httpx.Response:
        if self._admin_async is None:
            transport = transports(httpx)[1](self._native, trusted=True)
            self._admin_async = httpx.AsyncClient(base_url=_BASE, transport=transport, timeout=None)
            self._clients.append(self._admin_async)
        return await self._admin_async.get(path, **kwargs)

    def request(self, method: str, path: str, **kwargs: Any) -> httpx.Response:
        """Sends one request and reads the whole response.

        `kwargs` are those of `httpx.Client.request` (`json`, `content`,
        `files`, `headers`, `params`, `timeout`...). An HTTP error status is a
        response, not an exception. There is no timeout unless you pass one:
        it bounds the wait for the response (the provider call), and raises
        `httpx.ReadTimeout`.
        """
        return self._client().request(method, path, **kwargs)

    async def arequest(self, method: str, path: str, **kwargs: Any) -> httpx.Response:
        """`request` for asyncio."""
        return await self._aclient().request(method, path, **kwargs)

    def stream(self, method: str, path: str, **kwargs: Any) -> Iterator[bytes]:
        """Sends one request and yields the response body as it arrives."""
        with self._client().stream(method, path, **kwargs) as response:
            yield from response.iter_raw()

    async def astream(self, method: str, path: str, **kwargs: Any) -> AsyncIterator[bytes]:
        """`stream` for asyncio."""
        async with self._aclient().stream(method, path, **kwargs) as response:
            async for chunk in response.aiter_raw():
                yield chunk

    # -- what the gateway knows ----------------------------------------------

    def models(self, kind: Optional[str] = None) -> List[Dict[str, Any]]:
        """The models every provider lists, merged, as `/v1/models` does.

        Each is a dict with `id` (as clients name it), `owned_by` (the
        provider) and `kind` (`chat`, `embedding`, `image`, `transcription`,
        `speech`, `rerank`, `video` or `realtime`); `kind` filters them. This asks
        every provider, so it takes as long as the slowest.
        """
        return _models(self._admin("/v1/models"), kind)

    async def amodels(self, kind: Optional[str] = None) -> List[Dict[str, Any]]:
        """`models` for asyncio."""
        return _models(await self._aadmin("/v1/models"), kind)

    def providers(self) -> List[Dict[str, Any]]:
        """The configured providers with their formats and request counts.

        Like `requests()`, this is the web UI's admin API, off with `ui = false`.
        """
        return _items(self._admin("/ui/api/providers"), "providers")

    async def aproviders(self) -> List[Dict[str, Any]]:
        """`providers` for asyncio."""
        return _items(await self._aadmin("/ui/api/providers"), "providers")

    def requests(self, limit: int = 100, **filters: str) -> List[Dict[str, Any]]:
        """The last `limit` requests the gateway served, newest first, as the web UI lists them.

        `filters` are those of `export_requests`, such as `client="app"` (the name of a client key; each record's `client`).

        The log is in memory, and `log = 'off'` (the default here) only silences stderr; this admin API is off with `ui = false`.
        """
        return _items(self._admin("/ui/api/requests", params={"limit": limit, **filters}), "requests")

    async def arequests(self, limit: int = 100, **filters: str) -> List[Dict[str, Any]]:
        """`requests` for asyncio."""
        return _items(await self._aadmin("/ui/api/requests", params={"limit": limit, **filters}), "requests")

    def export_requests(self, format: str = "jsonl", **filters: str) -> str:
        """The requests the gateway kept, oldest first, as `jsonl` (one record per line) or `csv`.

        `filters` narrow it as the web UI's do: `kind`, `status` ("ok", "error", "pending" or a class such as "2xx"),
        `provider`, `client` (a client key's name), `model` (a substring) and `since` (an RFC 3339 time).
        """
        return _export(self._admin("/ui/api/requests/export", params={"format": format, **filters}))

    async def aexport_requests(self, format: str = "jsonl", **filters: str) -> str:
        """`export_requests` for asyncio."""
        return _export(await self._aadmin("/ui/api/requests/export", params={"format": format, **filters}))

    # -- servers -------------------------------------------------------------

    def serve(self, host: str = "127.0.0.1", port: int = 0) -> Server:
        """Serves this gateway on a real socket in the background.

        Returns a `Server` (`.url`, `.stop()`), also a context manager that
        stops on exit. The server keeps running until stopped, the gateway is
        closed or the process exits, even if you drop the returned `Server`
        (so a bare `gw.serve(port=8080)` serves). `port=0` picks a free port.
        Without `client_keys` in the config, anyone who reaches it spends
        your keys: keep it local.

        Besides the HTTP APIs it serves WebSocket sessions (OpenAI Realtime,
        Gemini Live) when the config has a provider for them: connect any
        WebSocket client to `server.url` with `http` replaced by `ws`.
        """
        server = Server(self._native.serve(host, port))
        self._servers.append(server)
        return server

    @contextlib.asynccontextmanager
    async def aserve(self, host: str = "127.0.0.1", port: int = 0) -> AsyncIterator[Server]:
        """`async with gw.aserve() as server:` serves for the block, then stops."""
        server = await asyncio.to_thread(self.serve, host, port)
        try:
            yield server
        finally:
            await server.astop()

    def serve_forever(self, host: str = "127.0.0.1", port: int = 8080) -> None:
        """Serves until interrupted (Ctrl-C), printing the banner as the binary does.

        Unlike `serve()`, the port defaults to 8080: this is the foreground
        server.
        """
        with self.serve(host, port) as server:
            print(server.banner, flush=True)
            try:
                server.wait()
            except KeyboardInterrupt:
                pass


def _import(name: str) -> Any:
    try:
        return __import__(name)
    except ImportError:
        raise ImportError(f"this needs the {name} package: pip install {name}") from None


def _httpx_of(sdk: ModuleType) -> ModuleType:
    """The httpx-compatible module `sdk` builds its clients on: `httpx`, or
    `httpx2` in recent versions, read off the base of its `DefaultHttpxClient`."""
    default = getattr(sdk, "DefaultHttpxClient", None)
    for base in getattr(default, "__mro__", ()):
        top = base.__module__.split(".")[0]
        if top != sdk.__name__ and top in sys.modules and hasattr(sys.modules[top], "Client"):
            return sys.modules[top]
    return httpx


def _items(response: httpx.Response, key: str) -> List[Dict[str, Any]]:
    """`response`'s JSON list at `key`; an error status is a `GatewayError`."""
    _check(response)
    return cast(List[Dict[str, Any]], response.json()[key])


def _export(response: httpx.Response) -> str:
    _check(response)
    return response.text


def _check(response: httpx.Response) -> None:
    """An error status is a `GatewayError`."""
    if response.status_code != 200:
        try:
            message = response.json()["error"]["message"]
        except Exception:
            message = response.text[:200]
        hint = " (is `ui = false` set in the config?)" if response.url.path.startswith("/ui/") else ""
        what = f"{response.request.method} {response.url.path} answered {response.status_code}"
        raise GatewayError(f"{what}: {message}{hint}")


def _models(response: httpx.Response, kind: Optional[str]) -> List[Dict[str, Any]]:
    return [m for m in _items(response, "data") if kind in (None, m.get("kind"))]
