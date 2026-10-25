// Typed client for the gateway's admin API (/ui/api) and the OpenAI-shaped
// endpoints the playground uses. Types mirror the contract field for field.

export type Kind =
  | "chat"
  | "realtime"
  | "responses"
  | "image"
  | "transcription"
  | "speech"
  | "embedding"
  | "rerank"
  | "decision"
  | "video"
  | "mcp"
  | "passthrough";

export interface Totals {
  requests: number;
  errors: number;
  in_flight: number;
  input_tokens: number;
  output_tokens: number;
  /** Total cost in US dollars; present when `prices:` is configured. */
  cost_usd?: number;
}

export interface Overview {
  version: string;
  /** null before the gateway has bound its address */
  listen: string | null;
  started_at: string;
  uptime_seconds: number;
  /** RFC 3339 time of the last config reload; null if never reloaded. */
  reloaded_at: string | null;
  auth_required: boolean;
  /** Where the configuration came from. */
  config_source: "file" | "environment" | "proxy" | "inline";
  /** The config file's path when `config_source` is `file`. */
  config_path: string | null;
  totals: Totals;
  /** How many requests the gateway keeps (`request_log.history`). */
  history: number;
  /** How sending traces to Respan goes; null without its plugin. */
  respan?: RespanHealth | null;
}

/** The Respan plugin's record of sending traces. */
export interface RespanHealth {
  /** Spans Respan accepted. */
  sent: number;
  /** Spans Respan answered it rejected. */
  rejected: number;
  /** Spans whose post failed. */
  failed: number;
  /** Traces dropped unsent while Respan was slow or down. */
  dropped: number;
  ok_at: string | null;
  error: { at: string; message: string } | null;
  /** Whether the last post failed. */
  failing: boolean;
}

/** What `POST /ui/api/reload` answers. */
export interface Reloaded {
  reloaded_at: string | null;
  /** What only a restart applies, such as a changed `listen`. */
  restart_required: string[];
}

export interface Provider {
  name: string;
  vendor: string | null;
  formats: string[];
  kinds: Kind[];
  base_url: string;
  credentials: "gateway" | "client";
  requests: number;
  errors: number;
  avg_latency_ms: number | null;
}

/** An MCP server the gateway fronts, served to clients at `endpoint`. */
export interface McpServer {
  name: string;
  transport: "http" | "stdio" | "vendor";
  /** Only the origin of an HTTP server's URL, the program a stdio server runs (never its path, query or arguments), or a tool vendor's id and origin. */
  target: string;
  /** Where clients reach it, such as `/mcp/github`. */
  endpoint: string;
  /** Whose credentials reach it: the gateway's, the client's, or none. */
  credentials: "gateway" | "client" | "none";
  /** How it authenticates, for a person to read, such as `bearer (env GITHUB_TOKEN)` or `oauth login (…)`; never a secret. */
  auth?: string;
  /** `idle` before a request, `failing` if every one failed, else `ok`. */
  status: "idle" | "ok" | "failing";
  /** Off: refused at `endpoint`, and none of its tools offered at `/mcp` or by the tool loop. In memory: a restart turns it on. */
  enabled: boolean;
  requests: number;
  errors: number;
  avg_latency_ms: number | null;
}

/** The MCP servers, and which the tool loop and the `/mcp` aggregate use. */
export interface McpOverview {
  servers: McpServer[];
  tool_loop: { servers: string[] } | null;
  aggregate: { servers: string[]; endpoint: string } | null;
}

export interface McpTool {
  name: string;
  title: string | null;
  description: string | null;
  /** The server's hints, where it gives them. */
  read_only: boolean | null;
  destructive: boolean | null;
  /** Whether the tool loop offers it to models, as `<server>__<tool>`. */
  tool_loop: boolean;
  /** Whether `/mcp` offers it. */
  aggregate: boolean;
  /** Off: neither `/mcp` nor the tool loop offers it. In memory: a restart turns it on. */
  enabled: boolean;
}

export interface McpTools {
  tools: McpTool[];
  took_ms: number;
  asked_at: string;
}

export interface ServedFormat {
  format: string;
  kind: Kind;
  endpoints: string[];
}

/** What the router saw of a candidate's calls, from its own record. */
export interface RouteSeen {
  /** Time to a response's head, a moving average. */
  latency_ms: number | null;
  /** Share of failed calls, a moving average from 0 to 1. */
  error_rate: number;
  calls: number;
  failures: number;
  /** Failed its last few calls just now, so it goes last for a while. */
  cooling: boolean;
}

/** A model's price, in US dollars per million tokens, and who says it: the config, which overrides, or the provider's list. */
export interface Price {
  input: number;
  output: number;
  cached_input?: number;
  /** Of each input token written to a cache. */
  cache_write?: number;
  /** Per second of audio, for a model billed by its length. */
  per_second?: number;
}

export type RoutePick = "first" | "cheapest" | "fastest" | "weighted" | "random";

export interface RouteCandidate {
  /** As configured: `provider`, `provider/model` or `provider/{model}`. */
  target: string;
  provider: string;
  /** The model it is sent; null when each request sends its own, as a glob's do. */
  model: string | null;
  weight: number;
  price: Price | null;
  seen: RouteSeen | null;
}

/** A `[routes.<name>]` of the config. */
export interface RouteRule {
  /** The model clients ask for, or a glob of them such as `llama-*`. */
  name: string;
  glob: boolean;
  pick: RoutePick;
  needs: string[];
  clients: string[];
  kinds: string[];
  fallback: boolean;
  hedge_ms: number | null;
  tries: number;
  to: RouteCandidate[];
}

export interface Routes {
  serve: ServedFormat[];
  defaults: { kind: Kind; provider: string }[];
  rules: RouteRule[];
}

/** Where a request would go now, as the router decides it. */
export interface Explained {
  by:
    | null
    | { type: "rule"; name: string }
    | { type: "named" }
    | { type: "listed" }
    | { type: "default" };
  pick?: RoutePick | null;
  mode?: "single" | "fallback" | "hedge";
  to?: { provider: string; model: string | null; price: Price | null; seen: RouteSeen | null }[];
  passed?: { candidate: string; why: string }[];
}

export interface Usage {
  input_tokens: number;
  output_tokens: number;
  /** Input tokens the provider served from its cache, when it says. */
  cached_input_tokens?: number;
  /** Seconds of audio billed, as a transcription's provider may report instead of tokens. */
  seconds?: number;
}

/** A tool the gateway ran for the model in a tool loop. */
export interface ToolLoopTool {
  /** As the model named it, `<server>__<tool>`. */
  name: string;
  duration_ms: number;
  error: boolean;
  /** The bytes of its answer the model saw. */
  bytes: number;
}

/** One call to the model in a tool loop, and the tools it asked for. */
export interface ToolLoopStep {
  index: number;
  duration_ms: number;
  input_tokens: number;
  output_tokens: number;
  /** Why the model stopped: `tool_use`, `stop`, … */
  finish?: string;
  tools: ToolLoopTool[];
}

export interface RequestRecord {
  id: string;
  started_at: string;
  method: string;
  path: string;
  format: string | null;
  kind: Kind | null;
  model: string | null;
  provider: string | null;
  /** The name of the client key the request came with; absent without named client keys. */
  client?: string;
  /** null while the request is in flight */
  status: number | null;
  duration_ms: number | null;
  ttfb_ms: number | null;
  stream: boolean;
  usage: Usage | null;
  /** What it cost in US dollars; absent when the model is unpriced. */
  cost_usd?: number;
  /** The tool loop's steps, in order; absent without the loop. */
  steps?: ToolLoopStep[];
  error: string | null;
}

/** What `GET /ui/api/config` answers: the config with every secret redacted. */
export interface ConfigView {
  source: "file" | "environment" | "proxy" | "inline";
  path: string | null;
  /** Redaction marker for secrets. */
  placeholder: string;
  text: string;
  format: "toml";
}

export interface Model {
  id: string;
  object: "model";
  owned_by: string;
  /** Gateway extension; may be absent. */
  kind?: Kind;
  /** Who made it, as a provider serving others' models says, such as `anthropic`. */
  maker?: string;
  /** What its provider's list says of it, where it says; may be absent. */
  display_name?: string;
  description?: string;
  context_window?: number;
  max_output_tokens?: number;
  /** What it reads beyond text. */
  input_modalities?: ("image" | "audio" | "video" | "document")[];
  capabilities?: ("tools" | "reasoning" | "structured_outputs")[];
  pricing?: Price;
  /** When it is retired, an RFC 3339 time. */
  retires_at?: string;
  /** A route of the config named for this model (`owned_by: "routes"`): how it picks, and its candidates. */
  route?: { pick: RoutePick; to: string[] };
}

export const KEY_STORAGE = "respan.gatewayKey";

export class ApiError extends Error {
  readonly status: number;
  readonly type: string | null;
  /** Where in a posted document the error is, when the gateway says. */
  readonly line: number | null;
  readonly column: number | null;

  constructor(
    status: number,
    message: string,
    type: string | null = null,
    at: { line?: number | null; column?: number | null } = {},
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.type = type;
    this.line = at.line ?? null;
    this.column = at.column ?? null;
  }
}

// --- gateway key ----------------------------------------------------------

type Listener = () => void;
const keyListeners = new Set<Listener>();
const unauthorizedListeners = new Set<Listener>();
const forbiddenListeners = new Set<Listener>();

export function getGatewayKey(): string | null {
  try {
    return localStorage.getItem(KEY_STORAGE) || null;
  } catch {
    return null;
  }
}

export function setGatewayKey(key: string | null): void {
  try {
    if (key) localStorage.setItem(KEY_STORAGE, key);
    else localStorage.removeItem(KEY_STORAGE);
  } catch {
    // Storage unavailable (private mode); the key just won't persist.
  }
  keyListeners.forEach((l) => l());
}

export function subscribeGatewayKey(listener: Listener): () => void {
  keyListeners.add(listener);
  return () => keyListeners.delete(listener);
}

/** Called whenever any request comes back 401, so the UI can ask for a key. */
export function onUnauthorized(listener: Listener): () => void {
  unauthorizedListeners.add(listener);
  return () => unauthorizedListeners.delete(listener);
}

/** Called whenever any request comes back 403: the key is valid but may not use the admin API. */
export function onForbidden(listener: Listener): () => void {
  forbiddenListeners.add(listener);
  return () => forbiddenListeners.delete(listener);
}

// --- requests -------------------------------------------------------------

export function authHeaders(init?: HeadersInit): Headers {
  const headers = new Headers(init);
  const key = getGatewayKey();
  if (key && !headers.has("authorization")) headers.set("authorization", `Bearer ${key}`);
  return headers;
}

async function errorFrom(res: Response): Promise<ApiError> {
  let message = res.statusText || `HTTP ${res.status}`;
  let type: string | null = null;
  let at: { line?: number | null; column?: number | null } = {};
  try {
    const text = await res.text();
    try {
      const body = JSON.parse(text) as {
        error?: { message?: string; type?: string; line?: number | null; column?: number | null } | string;
      };
      if (typeof body.error === "string") message = body.error;
      else if (body.error?.message) {
        message = body.error.message;
        type = body.error.type ?? null;
        at = body.error;
      }
    } catch {
      if (text.trim()) message = text.trim().slice(0, 300);
    }
  } catch {
    // Body unreadable; keep the status text.
  }
  return new ApiError(res.status, message, type, at);
}

/** Check a candidate before storing it; failures stay in the sign-in form. */
export async function verifyGatewayKey(key: string): Promise<void> {
  const response = await fetch("/ui/api/overview", {
    headers: { authorization: `Bearer ${key}`, accept: "application/json" },
    signal: AbortSignal.timeout(10_000),
  });
  if (!response.ok) throw await errorFrom(response);
}

/** fetch with the gateway key attached; throws ApiError on a non-2xx answer. */
export async function gatewayFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const res = await fetch(path, { ...init, headers: authHeaders(init.headers) });
  if (res.ok) return res;
  // Upstream authentication failures belong to the request, not gateway sign-in.
  if (path.startsWith("/ui/api/")) {
    if (res.status === 401) unauthorizedListeners.forEach((l) => l());
    if (res.status === 403) forbiddenListeners.forEach((l) => l());
  }
  throw await errorFrom(res);
}

export async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await gatewayFetch(path, { signal, headers: { accept: "application/json" } });
  return (await res.json()) as T;
}

export async function postJson<T>(path: string, body: unknown, signal?: AbortSignal): Promise<T> {
  const res = await gatewayFetch(path, {
    method: "POST",
    signal,
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  return (await res.json()) as T;
}

/** PUT `body` as JSON, for an answer with no body. */
export async function putJson(path: string, body: unknown): Promise<void> {
  await gatewayFetch(path, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

export const api = {
  config: (signal?: AbortSignal) => getJson<ConfigView>("/ui/api/config", signal),
  overview: (signal?: AbortSignal) => getJson<Overview>("/ui/api/overview", signal),
  providers: (signal?: AbortSignal) =>
    getJson<{ providers: Provider[] }>("/ui/api/providers", signal).then((r) => r.providers),
  mcpOverview: (signal?: AbortSignal) => getJson<McpOverview>("/ui/api/mcp", signal),
  mcpTools: (name: string, refresh: boolean, signal?: AbortSignal) =>
    getJson<McpTools>(`/ui/api/mcp/${encodeURIComponent(name)}/tools${refresh ? "?refresh=true" : ""}`, signal),
  /** Turn a tool server on or off, in the gateway's memory. */
  setServerEnabled: (name: string, enabled: boolean) =>
    putJson(`/ui/api/mcp/${encodeURIComponent(name)}`, { enabled }),
  /** Turn one of a server's tools on or off, in the gateway's memory. */
  setToolEnabled: (server: string, tool: string, enabled: boolean) =>
    putJson(`/ui/api/mcp/${encodeURIComponent(server)}/tools/${encodeURIComponent(tool)}`, { enabled }),
  routes: (signal?: AbortSignal) => getJson<Routes>("/ui/api/routes", signal),
  explain: (ask: { model: string; kind: string; client?: string }, signal?: AbortSignal) => {
    const query = new URLSearchParams({ model: ask.model, kind: ask.kind });
    if (ask.client) query.set("client", ask.client);
    return getJson<Explained>(`/ui/api/routes/explain?${query}`, signal);
  },
  requests: (limit: number, signal?: AbortSignal) =>
    getJson<{ requests: RequestRecord[] }>(`/ui/api/requests?limit=${limit}`, signal).then(
      (r) => r.requests,
    ),
  request: (id: string, signal?: AbortSignal) =>
    getJson<RequestRecord>(`/ui/api/requests/${encodeURIComponent(id)}`, signal),
  reload: (signal?: AbortSignal) => postJson<Reloaded>("/ui/api/reload", {}, signal),
  models: (signal?: AbortSignal) =>
    getJson<{ object: "list"; data: Model[] }>("/v1/models", signal).then((r) => r.data),
};
