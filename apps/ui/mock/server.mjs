// A stand-in for the gateway, for developing and screenshotting the UI
// without the Rust binary. It serves the admin API from the UI contract with
// generated traffic, and fake /v1 endpoints for the playground (chat over
// the four client APIs, a realtime voice socket, images,
// transcription, speech, embeddings and video jobs; a video prompt containing
// "fail" makes its job fail). Zero dependencies.
//
//   pnpm mock                      # http://127.0.0.1:8080, open
//   MOCK_KEY=secret pnpm mock      # every call needs "Bearer secret"
//   PORT=8081 MOCK_QUIET=1 pnpm mock   # another port; no generated traffic
//   MOCK_NO_REALTIME=1 pnpm mock   # no realtime provider: the Realtime tab says so

import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { crc32, deflateSync } from "node:zlib";

const PORT = Number(process.env.PORT ?? 8080);
const KEY = process.env.MOCK_KEY ?? "";
const QUIET = !!process.env.MOCK_QUIET;
const NO_REALTIME = !!process.env.MOCK_NO_REALTIME;
const startedAt = new Date();

// Providers as the registry in docs/vendors.md has them: each
// vendor's formats. `gateway` credentials are the gateway's own key;
// `client` providers take the caller's.
const kindOf = {
  "openai-chat": "chat",
  "openai-responses": "chat",
  "anthropic-messages": "chat",
  "gemini-generate": "chat",
  "openai-realtime": "realtime",
  "gemini-live": "realtime",
  "responses-ws": "responses",
  "openai-images": "image",
  "openai-transcriptions": "transcription",
  "openai-speech": "speech",
  "openai-videos": "video",
  "gemini-veo": "video",
  "openai-embeddings": "embedding",
};

const vendor = (name, formats, base_url, credentials = "gateway") => ({
  name,
  vendor: name,
  formats,
  kinds: [...new Set(formats.map((f) => kindOf[f]))],
  base_url,
  credentials,
});

const providers = [
  vendor(
    "openai",
    [
      "openai-chat",
      "openai-responses",
      "openai-realtime",
      "responses-ws",
      "openai-images",
      "openai-transcriptions",
      "openai-speech",
      "openai-embeddings",
    ],
    "https://api.openai.com/v1",
  ),
  vendor("anthropic", ["anthropic-messages"], "https://api.anthropic.com"),
  vendor(
    "gemini",
    [
      "openai-chat",
      "gemini-generate",
      "gemini-live",
      "openai-images",
      "gemini-veo",
      "openai-embeddings",
    ],
    "https://generativelanguage.googleapis.com/v1beta/openai",
  ),
  vendor(
    "groq",
    ["openai-chat", "openai-transcriptions", "openai-speech"],
    "https://api.groq.com/openai/v1",
    "client",
  ),
  vendor("deepseek", ["openai-chat"], "https://api.deepseek.com"),
  vendor("openrouter", ["openai-chat"], "https://openrouter.ai/api/v1"),
  vendor("azure", ["openai-chat"], "https://acme.openai.azure.com/openai/v1"),
  vendor(
    "vllm",
    ["openai-chat"],
    "http://gpu-box:8000/v1",
    "client",
  ),
];

// What the web UI turned off: "server" or "server/tool". In memory, as the gateway keeps it.
const switchedOff = new Set();

/** PUT `{ enabled }`: turns `id` on or off. */
const toggle = (id) => async (req, res) => {
  const { enabled } = JSON.parse((await readBody(req)).toString() || "{}");
  if (enabled === false) switchedOff.add(id);
  else switchedOff.delete(id);
  res.writeHead(204).end();
};

// Tool servers, as `[tools]` configures them and /ui/api/mcp lists them.
const mcpServers = [
  {
    name: "github",
    transport: "http",
    target: "https://api.githubcopilot.com",
    endpoint: "/mcp/github",
    credentials: "gateway",
    auth: "bearer (env GITHUB_TOKEN)",
    labels: ["initialize", "tools/list", "tools/call search_repositories", "tools/call get_issue"],
    tools: [
      { name: "search_repositories", description: "Search GitHub repositories by name, topic or language.", read_only: true },
      { name: "get_issue", description: "Get an issue of a repository, with its comments.", read_only: true },
      { name: "create_issue", description: "Open an issue in a repository." },
      { name: "delete_branch", description: "Delete a branch of a repository.", destructive: true },
      { name: "list_pull_requests", description: "List the pull requests of a repository, open or closed.", read_only: true },
      { name: "get_pull_request", description: "Get a pull request with its diff stats, reviews and checks.", read_only: true },
      { name: "create_pull_request", description: "Open a pull request from one branch into another." },
      { name: "merge_pull_request", description: "Merge a pull request with a merge, squash or rebase.", destructive: true },
      { name: "add_comment", description: "Comment on an issue or a pull request." },
      { name: "list_commits", description: "List the commits of a branch, newest first.", read_only: true },
      { name: "get_file_contents", description: "Read a file or a directory listing at a ref.", read_only: true },
      { name: "create_or_update_file", description: "Commit a new or changed file to a branch." },
      { name: "delete_file", description: "Commit the deletion of a file.", destructive: true },
    ],
  },
  {
    name: "files",
    transport: "stdio",
    target: "npx",
    endpoint: "/mcp/files",
    credentials: "none",
    auth: "none (a child process)",
    labels: ["initialize", "tools/list", "tools/call read_file", "tools/call list_directory"],
    tools: [
      { name: "read_file", description: "Read a file under the allowed directories.", read_only: true },
      { name: "list_directory", description: "List a directory's entries.", read_only: true },
      { name: "write_file", description: "Create or overwrite a file.", destructive: true },
    ],
  },
  {
    name: "search",
    transport: "vendor",
    target: "linkup (https://api.linkup.so)",
    endpoint: "/mcp/search",
    credentials: "gateway",
    auth: "key (env LINKUP_API_KEY)",
    labels: ["initialize", "tools/list", "tools/call search", "tools/call answer"],
    tools: [
      { name: "search", description: "Search the web with Linkup and get ranked sources." },
      { name: "answer", description: "Answer a question from the web, with its sources." },
      { name: "fetch", description: "Fetch a web page as markdown." },
    ],
  },
];

const routes = {
  serve: [
    { format: "openai-chat", kind: "chat", endpoints: ["POST /v1/chat/completions"] },
    { format: "openai-responses", kind: "chat", endpoints: ["POST /v1/responses"] },
    { format: "anthropic-messages", kind: "chat", endpoints: ["POST /v1/messages"] },
    {
      format: "gemini-generate",
      kind: "chat",
      endpoints: [
        "POST /v1beta/models/{model}:generateContent",
        "POST /v1beta/models/{model}:streamGenerateContent",
      ],
    },
    { format: "openai-realtime", kind: "realtime", endpoints: ["GET /v1/realtime"] },
    { format: "responses-ws", kind: "responses", endpoints: ["GET /v1/responses"] },
    {
      format: "openai-images",
      kind: "image",
      endpoints: ["POST /v1/images/generations", "POST /v1/images/edits"],
    },
    {
      format: "openai-transcriptions",
      kind: "transcription",
      endpoints: ["POST /v1/audio/transcriptions", "POST /v1/audio/translations"],
    },
    { format: "openai-speech", kind: "speech", endpoints: ["POST /v1/audio/speech"] },
    {
      format: "openai-videos",
      kind: "video",
      endpoints: ["POST /v1/videos", "GET /v1/videos/{id}", "GET /v1/videos/{id}/content"],
    },
    { format: "openai-embeddings", kind: "embedding", endpoints: ["POST /v1/embeddings"] },
    {
      format: "mcp",
      kind: "mcp",
      endpoints: ["POST /mcp/{name}", "GET /mcp/{name}", "DELETE /mcp/{name}"],
    },
  ],
  // `[routes.*]` of the config, each candidate with its price and the
  // router's own record of it.
  rules: [
    {
      name: "smart", glob: false, pick: "cheapest", needs: ["tools"], clients: [], kinds: [],
      fallback: true, hedge_ms: null, tries: 1,
      to: [
        { target: "anthropic/claude-sonnet-4-5", provider: "anthropic", model: "claude-sonnet-4-5", weight: 1,
          price: { input: 3, output: 15 },
          seen: { latency_ms: 812, error_rate: 0.02, calls: 140, failures: 3, cooling: false } },
        { target: "openai/gpt-4.1", provider: "openai", model: "gpt-4.1", weight: 1,
          price: { input: 2, output: 8 },
          seen: { latency_ms: 655, error_rate: 0, calls: 310, failures: 0, cooling: false } },
        { target: "groq/llama-3.3-70b-versatile", provider: "groq", model: "llama-3.3-70b-versatile", weight: 1,
          price: { input: 0.59, output: 0.79 },
          seen: { latency_ms: 190, error_rate: 0.6, calls: 25, failures: 15, cooling: true } },
      ],
    },
    {
      name: "fast", glob: false, pick: "fastest", needs: [], clients: ["ops"], kinds: ["chat"],
      fallback: true, hedge_ms: 250, tries: 1,
      to: [
        { target: "groq/llama-3.1-8b-instant", provider: "groq", model: "llama-3.1-8b-instant", weight: 1,
          price: null, seen: { latency_ms: 120, error_rate: 0, calls: 88, failures: 0, cooling: false } },
        { target: "gemini/gemini-2.5-flash", provider: "gemini", model: "gemini-2.5-flash", weight: 1,
          price: { input: 0.3, output: 2.5 }, seen: null },
      ],
    },
    {
      name: "llama-*", glob: true, pick: "weighted", needs: [], clients: [], kinds: [],
      fallback: true, hedge_ms: null, tries: 2,
      to: [
        { target: "groq", provider: "groq", model: null, weight: 3, price: null, seen: null },
        { target: "deepseek/{model}", provider: "deepseek", model: null, weight: 1, price: null, seen: null },
      ],
    },
  ],
  defaults: [
    { kind: "chat", provider: "openai" },
    ...(NO_REALTIME ? [] : [{ kind: "realtime", provider: "openai" }]),
    { kind: "responses", provider: "openai" },
    { kind: "image", provider: "openai" },
    { kind: "transcription", provider: "openai" },
    { kind: "speech", provider: "openai" },
    { kind: "video", provider: "openai" },
    { kind: "embedding", provider: "openai" },
  ],
};

const allModels = [
  ["gpt-4o-mini", "openai", "chat"],
  ["gpt-4.1", "openai", "chat"],
  ["claude-sonnet-4-5", "anthropic", "chat"],
  ["claude-haiku-4-5", "anthropic", "chat"],
  ["gemini-2.5-flash", "gemini", "chat"],
  ["llama-3.3-70b-versatile", "groq", "chat"],
  ["deepseek-reasoner", "deepseek", "chat"],
  ["gpt-realtime", "openai", "realtime"],
  ["gpt-image-1", "openai", "image"],
  ["whisper-1", "openai", "transcription"],
  ["whisper-large-v3", "groq", "transcription"],
  ["gpt-4o-mini-tts", "openai", "speech"],
  ["gemini-omni-1.1-flash", "gemini", "video"],
  ["text-embedding-3-small", "openai", "embedding"],
].map(([id, owned_by, kind]) => ({ id, object: "model", owned_by, kind }));
// A route named for one model is a model clients may ask for, as `/v1/models` lists it.
const routeModels = routes.rules.filter((r) => !r.glob).map((r) => ({
  id: r.name, object: "model", owned_by: "routes", kind: "chat",
  route: { pick: r.pick, to: r.to.map((c) => c.target) },
}));
const models = [...routeModels, ...allModels].filter((m) => !(NO_REALTIME && m.kind === "realtime"));

// --- request records ------------------------------------------------------

const records = []; // newest first, at most 1000
const clients = new Set();
let seq = 0;

const pick = (items) => items[Math.floor(Math.random() * items.length)];
const between = (lo, hi) => Math.round(lo + Math.random() * (hi - lo));
const providerFor = (model) =>
  routes.rules.find((r) => r.name === model)?.to[0].provider ??
  allModels.find((m) => m.id === model)?.owned_by ??
  "openai";

/** Where a request would go, as `/ui/api/routes/explain` says. */
function explain(model, kind) {
  const glob = (name) => new RegExp(`^${name.split("*").map((p) => p.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*")}$`);
  const rule = routes.rules.find((r) => !r.glob && r.name === model) ??
    (model.includes("/") ? null : routes.rules.find((r) => r.glob && glob(r.name).test(model)));
  if (rule) {
    const to = rule.to.map((c) => ({
      provider: c.provider,
      model: c.model ?? model,
      price: c.price,
      seen: c.seen,
    }));
    if (rule.pick === "cheapest") {
      const cost = (c) => (c.price ? c.price.input + 3 * c.price.output : Infinity);
      to.sort((a, b) => cost(a) - cost(b));
    }
    const cooling = (c) => (c.seen?.cooling ? 1 : 0);
    to.sort((a, b) => cooling(a) - cooling(b));
    return { by: { type: "rule", name: rule.name }, pick: rule.pick,
      mode: rule.hedge_ms != null ? "hedge" : to.length > 1 ? "fallback" : "single", to, passed: [] };
  }
  const one = (by, provider, sent) =>
    ({ by, pick: null, mode: "single", to: [{ provider, model: sent, price: null, seen: null }], passed: [] });
  const slash = model.indexOf("/");
  if (slash > 0) return one({ type: "named" }, model.slice(0, slash), model.slice(slash + 1));
  const listed = allModels.find((m) => m.id === model && m.kind === kind);
  if (listed) return one({ type: "listed" }, listed.owned_by, model);
  const fallback = routes.defaults.find((d) => d.kind === kind);
  return fallback ? one({ type: "default" }, fallback.provider, model) : { by: null };
}

function broadcast(record) {
  const frame = `event: request\ndata: ${JSON.stringify(record)}\n\n`;
  for (const res of clients) res.write(frame);
}

function begin({ path, format, kind, model, provider, stream, at = new Date() }) {
  const record = {
    id: `r_${String(++seq).padStart(6, "0")}`,
    started_at: at.toISOString(),
    method: "POST",
    path,
    format,
    kind,
    model,
    ...(KEY ? { client: pick(["ci", "web", "ops"]) } : {}),
    provider: provider ?? (model ? providerFor(model) : null),
    status: null,
    duration_ms: null,
    ttfb_ms: null,
    stream,
    usage: null,
    error: null,
  };
  records.unshift(record);
  records.length = Math.min(records.length, 1000);
  broadcast(record);
  return record;
}

// What `prices:` would configure: US dollars per million tokens, in and out.
// Models without a price (llama, deepseek) carry no cost.
const prices = {
  "gpt-4o-mini": [0.15, 0.6],
  "gpt-4.1": [2, 8],
  "claude-sonnet-4-5": [3, 15],
  "claude-haiku-4-5": [1, 5],
  "gemini-2.5-flash": [0.3, 2.5],
};

/** Cost and cached tokens for a finished call, as the gateway adds them. */
function priced(record, usage) {
  const price = prices[record.model];
  if (!usage || !price) return { usage };
  const cached = record.provider === "gemini" ? 0 : Math.floor(usage.input_tokens * 0.4);
  const input = usage.input_tokens - cached;
  const cost = (input * price[0] + cached * price[0] * 0.1 + usage.output_tokens * price[1]) / 1e6;
  return { usage: cached ? { ...usage, cached_input_tokens: cached } : usage, cost_usd: cost };
}

function finish(record, { status, duration, usage = null, error = null, steps }) {
  Object.assign(record, {
    ...priced(record, usage),
    ...(steps ? { steps } : {}),
    status,
    duration_ms: duration,
    ttfb_ms: Math.round(duration * (0.15 + Math.random() * 0.3)),
    error,
  });
  broadcast(record);
}

const failures = [
  [429, "Rate limit reached for requests per minute; retry after 2s"],
  [500, "upstream connection reset by peer"],
  [400, "messages: at least one message is required"],
  [401, "Incorrect API key provided"],
];

// The calls clients make, weighted: mostly chat, some of everything else.
const traffic = [
  [10, { path: "/v1/chat/completions", format: "openai-chat", kind: "chat" }],
  [4, { path: "/v1/messages", format: "anthropic-messages", kind: "chat" }],
  [1, { path: "/v1/images/generations", format: "openai-images", kind: "image" }],
  [1, { path: "/v1/audio/transcriptions", format: "openai-transcriptions", kind: "transcription" }],
  [1, { path: "/v1/audio/speech", format: "openai-speech", kind: "speech" }],
  [1, { path: "/v1/embeddings", format: "openai-embeddings", kind: "embedding" }],
  ...mcpServers.map((s) => [
    1,
    { path: s.endpoint, format: "mcp", kind: "mcp", provider: s.name, labels: s.labels },
  ]),
].flatMap(([weight, call]) => Array(weight).fill(call));

function fakeTraffic(at) {
  const call = pick(traffic);
  const model = call.labels ? pick(call.labels) : pick(models.filter((m) => m.kind === call.kind)).id;
  const chat = call.kind === "chat";
  const record = begin({ ...call, model, stream: chat && Math.random() < 0.6, at });
  const failed = Math.random() < 0.06;
  const [status, error] = failed ? pick(failures) : [200, null];
  const usage = call.labels
    ? null
    : chat
    ? { input_tokens: between(12, 2400), output_tokens: between(8, 900) }
    : call.kind === "transcription"
      ? { input_tokens: 0, output_tokens: 0, seconds: between(3, 90) }
      : { input_tokens: between(4, 200), output_tokens: 0 };
  const outcome = {
    status,
    error,
    duration: between(180, chat ? (model.startsWith("llama") ? 900 : 3200) : 4500),
    usage: failed ? null : usage,
  };
  // Now and then a chat request is a tool loop: the gateway ran tools.
  if (chat && !failed && Math.random() < 0.2) {
    const tools = [{ name: "files__read", duration_ms: between(8, 120), error: false, bytes: between(200, 9000) }];
    if (Math.random() < 0.3) tools.push({ name: "github__search", duration_ms: between(80, 900), error: Math.random() < 0.3, bytes: between(100, 4000) });
    const step = (index, finish, tools) => ({
      index, duration_ms: between(300, 1400), input_tokens: between(100, 800), output_tokens: between(10, 200), finish, tools,
    });
    outcome.steps = [step(1, "tool_use", tools), step(2, "stop", [])];
  }
  return { record, outcome };
}

// History: a few dozen requests over the last half hour.
for (let i = 0; i < 90; i++) {
  const at = new Date(Date.now() - between(5, 29 * 60) * 1000);
  const { record, outcome } = fakeTraffic(at);
  finish(record, outcome);
}
records.sort((a, b) => (a.started_at < b.started_at ? 1 : -1));

function tick() {
  if (!QUIET) {
    const { record, outcome } = fakeTraffic();
    setTimeout(() => finish(record, outcome), outcome.duration);
  }
  setTimeout(tick, between(700, 4000));
}
tick();

function totals() {
  const done = records.filter((r) => r.status !== null);
  return {
    requests: records.length,
    errors: done.filter((r) => r.status >= 400).length,
    in_flight: records.length - done.length,
    input_tokens: done.reduce((n, r) => n + (r.usage?.input_tokens ?? 0), 0),
    output_tokens: done.reduce((n, r) => n + (r.usage?.output_tokens ?? 0), 0),
    cost_usd: done.reduce((n, r) => n + (r.cost_usd ?? 0), 0),
  };
}

function providerStats(name) {
  const mine = records.filter((r) => r.provider === name && r.status !== null);
  return {
    requests: mine.length,
    errors: mine.filter((r) => r.status >= 400).length,
    avg_latency_ms: mine.length
      ? Math.round(mine.reduce((n, r) => n + r.duration_ms, 0) / mine.length)
      : null,
  };
}

// --- fake media -------------------------------------------------------------

/** A soft gradient PNG, as base64: what gpt-image-1 answers with. */
function pngImage(prompt, i, size = 384) {
  const hue = (prompt.length * 37 + i * 70) % 360;
  const shade = (h, l) => {
    const a = 0.7 * Math.min(l, 1 - l);
    const f = (n) => l - a * Math.max(-1, Math.min((n + h / 30) % 12 - 3, 9 - ((n + h / 30) % 12), 1));
    return [f(0), f(8), f(4)].map((v) => Math.round(v * 255));
  };
  const rows = Buffer.alloc(size * (size * 3 + 1));
  for (let y = 0; y < size; y++) {
    rows[y * (size * 3 + 1)] = 0;
    for (let x = 0; x < size; x++) {
      const t = (x + y) / (2 * size);
      let [r, g, b] = shade((hue + t * 60) % 360, 0.62 - t * 0.24);
      const near = Math.hypot(x - (110 + i * 50), y - 150) < size * 0.2;
      if (near) [r, g, b] = [r, g, b].map((v) => Math.round(v + (255 - v) * 0.2));
      rows.set([r, g, b], y * (size * 3 + 1) + 1 + x * 3);
    }
  }
  const chunk = (type, data) => {
    const head = Buffer.alloc(4);
    head.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(body));
    return Buffer.concat([head, body, crc]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header.set([8, 2, 0, 0, 0], 8);
  const file = Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(rows)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
  return file.toString("base64");
}

/** A gentle two-note chime as 16-bit mono WAV. */
function chimeWav(seconds = 1.4, rate = 22050) {
  const n = Math.floor(seconds * rate);
  const buf = Buffer.alloc(44 + n * 2);
  buf.write("RIFF", 0);
  buf.writeUInt32LE(36 + n * 2, 4);
  buf.write("WAVEfmt ", 8);
  buf.writeUInt32LE(16, 16);
  buf.writeUInt16LE(1, 20);
  buf.writeUInt16LE(1, 22);
  buf.writeUInt32LE(rate, 24);
  buf.writeUInt32LE(rate * 2, 28);
  buf.writeUInt16LE(2, 32);
  buf.writeUInt16LE(16, 34);
  buf.write("data", 36);
  buf.writeUInt32LE(n * 2, 40);
  for (let i = 0; i < n; i++) {
    const t = i / rate;
    const f = t < seconds / 2 ? 660 : 880;
    const env = Math.exp(-3 * (t % (seconds / 2)));
    buf.writeInt16LE(Math.round(Math.sin(2 * Math.PI * f * t) * env * 9000), 44 + i * 2);
  }
  return buf;
}

const reply = `Here's how a request moves through **respan gateway**:

1. The client format that recognises the request decodes it.
2. Plugins run \`on_client\`, then the router picks a provider.
3. The provider format encodes the call, translating if the formats differ.

\`\`\`bash
curl localhost:8080/v1/chat/completions -d '{"model":"claude-sonnet-4-5", ...}'
\`\`\`

Responses stream back through the same plugins, *event by event*.`;

// --- HTTP -------------------------------------------------------------------

const json = (res, status, body) => {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
};
const notFound = (res, message = "Not found") =>
  json(res, 404, { error: { message, type: "not_found_error" } });

async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  return Buffer.concat(chunks);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function authorized(req) {
  if (!KEY) return true;
  const header = req.headers.authorization ?? "";
  return header === `Bearer ${KEY}` || req.headers["x-api-key"] === KEY;
}

// The four client APIs a chat can speak. Each says how to find the model and
// the user's last words in a request, and how its streamed and whole answers
// look; `chat` below does the rest the same way for all of them.
const sse = (data, event) => `${event ? `event: ${event}\n` : ""}data: ${JSON.stringify(data)}\n\n`;

const clientApis = {
  "openai-chat": {
    path: "/v1/chat/completions",
    model: (b) => b.model,
    last: (b) => b.messages?.at(-1)?.content,
    whole: ({ id, model, usage }) => ({
      id,
      object: "chat.completion",
      model,
      choices: [{ index: 0, message: { role: "assistant", content: reply }, finish_reason: "stop" }],
      usage: { prompt_tokens: usage.in, completion_tokens: usage.out },
    }),
    open: () => [],
    delta: ({ id, model }, text) =>
      sse({ id, object: "chat.completion.chunk", model, choices: [{ index: 0, delta: { content: text } }] }),
    close: ({ id, model, usage }) => [
      sse({ id, object: "chat.completion.chunk", model, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }),
      sse({
        id,
        object: "chat.completion.chunk",
        model,
        choices: [],
        usage: { prompt_tokens: usage.in, completion_tokens: usage.out },
      }),
      "data: [DONE]\n\n",
    ],
  },
  "anthropic-messages": {
    path: "/v1/messages",
    model: (b) => b.model,
    last: (b) => {
      const c = b.messages?.at(-1)?.content;
      return typeof c === "string" ? c : c?.[0]?.text;
    },
    whole: ({ id, model, usage }) => ({
      id,
      type: "message",
      role: "assistant",
      model,
      content: [{ type: "text", text: reply }],
      stop_reason: "end_turn",
      usage: { input_tokens: usage.in, output_tokens: usage.out },
    }),
    open: ({ id, model, usage }) => [
      sse(
        {
          type: "message_start",
          message: { id, type: "message", role: "assistant", model, content: [], usage: { input_tokens: usage.in, output_tokens: 0 } },
        },
        "message_start",
      ),
      sse({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }, "content_block_start"),
    ],
    delta: (_ctx, text) =>
      sse({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } }, "content_block_delta"),
    close: ({ usage }) => [
      sse({ type: "content_block_stop", index: 0 }, "content_block_stop"),
      sse(
        { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: usage.out } },
        "message_delta",
      ),
      sse({ type: "message_stop" }, "message_stop"),
    ],
  },
  "openai-responses": {
    path: "/v1/responses",
    model: (b) => b.model,
    last: (b) => {
      const c = typeof b.input === "string" ? b.input : b.input?.at(-1)?.content;
      return typeof c === "string" ? c : c?.[0]?.text;
    },
    whole: ({ id, model, usage }) => ({
      id,
      object: "response",
      model,
      status: "completed",
      output: [{ type: "message", role: "assistant", content: [{ type: "output_text", text: reply }] }],
      usage: { input_tokens: usage.in, output_tokens: usage.out },
    }),
    open: ({ id, model }) => [
      sse({ type: "response.created", response: { id, object: "response", model, status: "in_progress" } }, "response.created"),
    ],
    delta: (_ctx, text) =>
      sse(
        { type: "response.output_text.delta", item_id: "msg_1", output_index: 0, content_index: 0, delta: text },
        "response.output_text.delta",
      ),
    close: ({ id, model, usage }) => [
      sse(
        {
          type: "response.completed",
          response: { id, object: "response", model, status: "completed", usage: { input_tokens: usage.in, output_tokens: usage.out } },
        },
        "response.completed",
      ),
    ],
  },
  "gemini-generate": {
    path: (url) => url.pathname,
    model: (_b, url) => decodeURIComponent(url.pathname.split("/").pop().split(":")[0]),
    last: (b) => b.contents?.at(-1)?.parts?.[0]?.text,
    whole: ({ usage }) => ({
      candidates: [{ content: { role: "model", parts: [{ text: reply }] }, finishReason: "STOP", index: 0 }],
      usageMetadata: { promptTokenCount: usage.in, candidatesTokenCount: usage.out },
    }),
    open: () => [],
    delta: (_ctx, text) => sse({ candidates: [{ content: { role: "model", parts: [{ text }] }, index: 0 }] }),
    close: ({ usage }) => [
      sse({
        candidates: [{ content: { role: "model", parts: [{ text: "" }] }, finishReason: "STOP", index: 0 }],
        usageMetadata: { promptTokenCount: usage.in, candidatesTokenCount: usage.out },
      }),
    ],
  },
};

async function chat(req, res, format = "openai-chat") {
  const api = clientApis[format];
  const url = new URL(req.url, "http://x");
  const body = JSON.parse((await readBody(req)).toString() || "{}");
  const model = api.model(body, url) ?? "gpt-4o-mini";
  const streaming = format === "gemini-generate" ? url.pathname.includes(":stream") : !!body.stream;
  const record = begin({
    path: typeof api.path === "function" ? api.path(url) : api.path,
    format,
    kind: "chat",
    model,
    stream: streaming,
  });
  const started = Date.now();
  const usage = { in: between(20, 80), out: reply.split(/\s+/).length };
  const ctx = { id: `chatcmpl-${record.id}`, model, usage };
  const done = () => {
    // Asking for tools or files makes this a tool loop the gateway ran.
    const toolLoop = /tool|file/i.test(String(api.last(body, url) ?? ""));
    const step = (index, finish, tools) => ({
      index, duration_ms: between(300, 900), input_tokens: usage.in, output_tokens: usage.out, finish, tools,
    });
    finish(record, {
      status: 200,
      duration: Date.now() - started,
      usage: { input_tokens: usage.in, output_tokens: usage.out },
      ...(toolLoop && {
        steps: [
          step(1, "tool_use", [
            { name: "files__read_file", duration_ms: between(8, 90), error: false, bytes: between(300, 6000) },
            { name: "files__list_directory", duration_ms: between(5, 40), error: false, bytes: between(100, 900) },
          ]),
          step(2, "stop", []),
        ],
      }),
    });
  };

  if (!streaming) {
    await sleep(600);
    done();
    return json(res, 200, api.whole(ctx));
  }

  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  let closed = false;
  req.on("close", () => (closed = true));
  await sleep(350);
  for (const frame of api.open(ctx)) res.write(frame);
  for (const piece of reply.match(/\S+\s*/g)) {
    if (closed) break;
    res.write(api.delta(ctx, piece));
    await sleep(between(15, 45));
  }
  res.end(api.close(ctx).join(""));
  done();
}

// --- realtime: a WebSocket by hand, so the mock needs no dependency ----------
//
// Speak into the mic and the mock hears a turn (any audible chunk starts one,
// half a second of quiet ends it), transcribes it as a scripted question and
// answers aloud with a short rising tone and a transcript. Speaking while it
// answers cuts the answer off, as server VAD does.

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const RATE = 24000;

function wsFrame(text) {
  const payload = Buffer.from(text);
  const n = payload.length;
  const head =
    n < 126
      ? Buffer.from([0x81, n])
      : n < 65536
        ? Buffer.from([0x81, 126, n >> 8, n & 255])
        : Buffer.concat([Buffer.from([0x81, 127, 0, 0, 0, 0]), Buffer.from([n >>> 24, (n >> 16) & 255, (n >> 8) & 255, n & 255])]);
  return Buffer.concat([head, payload]);
}

/** Calls onText for each text frame in what the client sends; onClose on a close frame. */
function wsReader(socket, onText, onClose) {
  let buf = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const opcode = buf[0] & 15;
      let len = buf[1] & 127;
      let at = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        at = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        at = 10;
      }
      const masked = (buf[1] & 128) !== 0;
      if (buf.length < at + (masked ? 4 : 0) + len) return;
      const mask = masked ? buf.subarray(at, at + 4) : null;
      const start = at + (masked ? 4 : 0);
      const data = Buffer.from(buf.subarray(start, start + len));
      if (mask) for (let i = 0; i < data.length; i++) data[i] ^= mask[i % 4];
      buf = buf.subarray(start + len);
      if (opcode === 1) onText(data.toString());
      else if (opcode === 8) return onClose();
      else if (opcode === 9) socket.write(Buffer.concat([Buffer.from([0x8a, data.length]), data]));
    }
  });
  socket.on("close", onClose);
  socket.on("error", onClose);
}

/** 100 ms of a sine at `hz`, as base64 PCM16 at 24 kHz. */
function toneChunk(hz) {
  const samples = new Int16Array(RATE / 10);
  for (let i = 0; i < samples.length; i++) {
    const fade = Math.min(1, i / 240, (samples.length - i) / 240);
    samples[i] = Math.round(Math.sin((2 * Math.PI * hz * i) / RATE) * 9000 * fade);
  }
  return Buffer.from(samples.buffer).toString("base64");
}

const spoken = "It's a clear afternoon, with a light breeze and temperatures in the low twenties.";

function realtimeSession(socket, model) {
  const send = (event) => socket.writable && socket.write(wsFrame(JSON.stringify(event)));
  const record = begin({ path: "/v1/realtime", format: "openai-realtime", kind: "realtime", model, stream: true });
  const started = Date.now();
  let turn = 0;
  let voiced = 0;
  let quiet = 0;
  let spoke = 0;
  let latched = false;
  let speaking = false; // the user
  let answer = null; // the running answer: { cancelled }
  let closed = false;

  send({ type: "session.created", session: { type: "realtime", model } });

  async function reply(n) {
    const mine = (answer = { cancelled: false });
    const item = `item_a${n}`;
    send({ type: "response.created", response: { id: `resp_${n}`, status: "in_progress" } });
    send({ type: "conversation.item.created", item: { id: item, role: "assistant" } });
    const words = spoken.match(/\S+\s*/g);
    for (let i = 0; i < 18 && !mine.cancelled && !closed; i++) {
      send({
        type: "response.output_audio.delta",
        response_id: `resp_${n}`,
        item_id: item,
        content_index: 0,
        delta: toneChunk(330 + 40 * (i % 6)),
      });
      const piece = words.splice(0, 1)[0];
      if (piece) {
        send({ type: "response.output_audio_transcript.delta", response_id: `resp_${n}`, item_id: item, delta: piece });
      }
      await sleep(100);
    }
    if (mine.cancelled || closed) return;
    send({ type: "response.output_audio_transcript.done", item_id: item, transcript: spoken });
    send({ type: "response.output_audio.done", item_id: item });
    send({ type: "response.done", response: { id: `resp_${n}`, status: "completed" } });
    answer = null;
  }

  function endOfTurn() {
    speaking = false;
    spoke = 0;
    const n = ++turn;
    send({ type: "input_audio_buffer.speech_stopped" });
    send({ type: "conversation.item.created", item: { id: `item_u${n}`, role: "user" } });
    send({
      type: "conversation.item.input_audio_transcription.completed",
      item_id: `item_u${n}`,
      transcript: "What's the weather like this afternoon?",
    });
    void reply(n);
  }

  function hear(base64) {
    const pcm = new Int16Array(Buffer.from(base64, "base64").buffer.slice(0));
    let sum = 0;
    for (const v of pcm) sum += (v / 32768) ** 2;
    const loud = Math.sqrt(sum / Math.max(1, pcm.length)) > 0.01;
    // A speaker who never pauses is cut off after two seconds, as a VAD timeout would.
    if (speaking && ++spoke > 50) {
      spoke = 0;
      voiced = 0;
      latched = true;
      return endOfTurn();
    }
    // Having been cut off, the speaker is not heard again until the answer is done.
    if (latched && answer) return;
    latched = false;
    if (loud) {
      quiet = 0;
      if (++voiced === 2 && !speaking) {
        speaking = true;
        if (answer) answer.cancelled = true;
        send({ type: "input_audio_buffer.speech_started" });
      }
    } else if (speaking && ++quiet >= 12) {
      voiced = 0;
      quiet = 0;
      endOfTurn();
    }
  }

  wsReader(
    socket,
    (text) => {
      const event = JSON.parse(text);
      if (event.type === "input_audio_buffer.append") hear(event.audio);
      else if (event.type === "response.cancel" && answer) answer.cancelled = true;
      else if (event.type === "response.create" && !answer) void reply(++turn);
    },
    () => {
      if (closed) return;
      closed = true;
      if (answer) answer.cancelled = true;
      finish(record, { status: 101, duration: Date.now() - started });
      socket.destroy();
    },
  );
}

function upgrade(req, socket) {
  const url = new URL(req.url, "http://x");
  const protocols = (req.headers["sec-websocket-protocol"] ?? "").split(",").map((p) => p.trim());
  const sent =
    protocols.find((p) => p.startsWith("openai-insecure-api-key."))?.slice("openai-insecure-api-key.".length) ??
    url.searchParams.get("key");
  const reject = (status, text) => {
    socket.end(`HTTP/1.1 ${status} ${text}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n`);
  };
  if (url.pathname !== "/v1/realtime") return reject(404, "Not Found");
  if (KEY && sent !== KEY && !authorized(req)) return reject(401, "Unauthorized");
  const model = url.searchParams.get("model") ?? "gpt-realtime";
  if (NO_REALTIME) return reject(404, "No realtime provider");
  const accept = createHash("sha1").update(req.headers["sec-websocket-key"] + WS_GUID).digest("base64");
  socket.write(
    [
      "HTTP/1.1 101 Switching Protocols",
      "upgrade: websocket",
      "connection: Upgrade",
      `sec-websocket-accept: ${accept}`,
      ...(protocols.includes("realtime") ? ["sec-websocket-protocol: realtime"] : []),
      "",
      "",
    ].join("\r\n"),
  );
  realtimeSession(socket, model);
}

async function media(req, res, { path, format, kind, model, answer }) {
  const raw = await readBody(req);
  const record = begin({ path, format, kind, model, stream: false });
  const started = Date.now();
  await sleep(between(600, 1400));
  finish(record, { status: 200, duration: Date.now() - started });
  answer(raw);
}

// A 64-number vector of a text's letter pairs, so texts that share words are alike.
function hashedVector(text) {
  const vector = new Array(64).fill(0);
  const words = text.toLowerCase().replace(/[^a-z0-9 ]/g, "");
  for (let i = 0; i + 1 < words.length; i++) {
    vector[(words.charCodeAt(i) * 31 + words.charCodeAt(i + 1)) % 64] += 1;
  }
  const norm = Math.hypot(...vector) || 1;
  return vector.map((v) => Number((v / norm).toFixed(6)));
}

const sentences = [
  "This is a mock transcript of the audio you sent.",
  "In the real gateway it comes from the transcription provider.",
  "Each line carries a timestamp when you ask for subtitles.",
];

/** Answers a transcription in the multipart form's `response_format`. */
function transcript(res, raw) {
  const format = /name="response_format"\r\n\r\n(\w+)/.exec(raw.toString("latin1"))?.[1] ?? "json";
  const at = (i, sep) =>
    new Date(i * 3000).toISOString().slice(11, 23).replace(".", sep);
  const end = (i, sep) =>
    new Date(i * 3000 + 2800).toISOString().slice(11, 23).replace(".", sep);
  if (format === "srt" || format === "vtt") {
    const sep = format === "srt" ? "," : ".";
    const cues = sentences.map((line, i) => {
      const times = `${at(i, sep)} --> ${end(i, sep)}\n${line}\n`;
      return format === "srt" ? `${i + 1}\n${times}` : times;
    });
    res.writeHead(200, { "content-type": format === "srt" ? "text/plain" : "text/vtt" });
    return res.end((format === "vtt" ? "WEBVTT\n\n" : "") + cues.join("\n"));
  }
  const text = sentences.join(" ");
  if (format === "text") {
    res.writeHead(200, { "content-type": "text/plain" });
    return res.end(text);
  }
  json(res, 200, { text });
}

// --- video jobs: queued, then rendering for a few seconds, then done ----------

const JOB_MS = 7000;
const jobs = new Map();
const clip = readFileSync(new URL("./clip.webm", import.meta.url));

function jobView(job) {
  const age = Date.now() - job.created;
  const progress = Math.min(100, Math.round((age / JOB_MS) * 100));
  const status =
    progress >= 100 ? (job.fail ? "failed" : "completed") : age < 1200 ? "queued" : "in_progress";
  return {
    id: job.id,
    object: "video",
    model: job.model,
    status,
    progress,
    seconds: job.seconds,
    error:
      status === "failed"
        ? { code: "moderation_blocked", message: "The prompt was blocked by the provider's safety system." }
        : null,
  };
}

async function videos(req, res, path) {
  const [, , , id, rest] = path.split("/");
  if (req.method === "POST" && !id) {
    const body = JSON.parse((await readBody(req)).toString() || "{}");
    const job = {
      id: `veo_${Math.random().toString(36).slice(2, 10)}`,
      created: Date.now(),
      model: body.model ?? "gemini-omni-1.1-flash",
      seconds: body.seconds ?? "4",
      fail: /fail/i.test(body.prompt ?? ""),
    };
    jobs.set(job.id, job);
    const record = begin({ path, format: "openai-videos", kind: "video", model: job.model, stream: false });
    finish(record, { status: 200, duration: between(200, 500) });
    return json(res, 200, jobView(job));
  }
  const job = jobs.get(id);
  if (!job) return notFound(res, `No video ${id}`);
  if (req.method === "GET" && !rest) return json(res, 200, jobView(job));
  if (req.method === "GET" && rest === "content") {
    if (jobView(job).status !== "completed") {
      return json(res, 409, { error: { message: "The video isn't ready" } });
    }
    res.writeHead(200, { "content-type": "video/webm" });
    return res.end(clip);
  }
  notFound(res, `No format serves ${req.method} ${path}`);
}

let reloadedAt = null; // set by POST /ui/api/reload

// The read-only admin config, with secrets redacted.
const configToml = `listen = "127.0.0.1:8080"
client_keys = ["…", "env:GATEWAY_KEY"]

[providers.openai]
vendor = "openai"
key = "env:OPENAI_API_KEY"

[providers.anthropic]
vendor = "anthropic"
key = "…"

[providers.local]
formats = ["openai-chat"]
base_url = "http://127.0.0.1:11434/v1"

[mcp.github]
url = "https://api.githubcopilot.com/mcp/"
auth = { bearer = "…" }

[mcp.files]
command = "npx"
args = ["…", "…", "…"]
`;



const routesTable = {
  "GET /healthz": (_req, res) => res.end("ok"),
  "GET /ui/api/overview": (_req, res) =>
    json(res, 200, {
      version: "0.1.0",
      listen: `127.0.0.1:${PORT}`,
      started_at: startedAt.toISOString(),
      uptime_seconds: Math.floor((Date.now() - startedAt) / 1000) + 7_380,
      reloaded_at: reloadedAt?.toISOString() ?? null,
      auth_required: !!KEY,
      config_source: "file",
      config_path: "./gateway.toml",
      totals: totals(),
    }),
  "POST /ui/api/reload": (_req, res) => {
    reloadedAt = new Date();
    json(res, 200, { reloaded_at: reloadedAt.toISOString(), restart_required: [] });
  },
  "GET /ui/api/config": (_req, res) =>
    json(res, 200, {
      source: "file",
      path: "./gateway.toml",
      placeholder: "…",
      text: configToml,
      format: "toml",
    }),
  "GET /ui/api/providers": (_req, res) =>
    json(res, 200, { providers: providers.map((p) => ({ ...p, ...providerStats(p.name) })) }),
  "GET /ui/api/mcp": (_req, res) =>
    json(res, 200, {
      servers: mcpServers.map((server) => {
        const { name, transport, target, endpoint, credentials, auth } = server;
        const stats = providerStats(name);
        const status = !stats.requests ? "idle" : stats.errors === stats.requests ? "failing" : "ok";
        return { name, transport, target, endpoint, credentials, auth, ...stats, status, enabled: !switchedOff.has(name) };
      }),
      // As `plugins.tool_loop` configures it, and /mcp fronts every server.
      tool_loop: { servers: ["github"] },
      aggregate: { servers: mcpServers.map((s) => s.name), endpoint: "/mcp" },
    }),
  // Each server's tools, as asking it through the gateway lists them.
  ...Object.fromEntries(mcpServers.map((server) => [
    `GET /ui/api/mcp/${server.name}/tools`,
    (_req, res) =>
      setTimeout(() => json(res, 200, {
        tools: server.tools.map((t) => ({
          title: null, read_only: null, destructive: null, ...t,
          tool_loop: server.name === "github" && !t.destructive,
          aggregate: true,
          enabled: !switchedOff.has(`${server.name}/${t.name}`),
        })),
        took_ms: between(200, 900),
        asked_at: new Date().toISOString(),
      }), 300),
  ])),
  ...Object.fromEntries(mcpServers.flatMap((server) => [
    [`PUT /ui/api/mcp/${server.name}`, toggle(server.name)],
    ...server.tools.map((t) => [`PUT /ui/api/mcp/${server.name}/tools/${t.name}`, toggle(`${server.name}/${t.name}`)]),
  ])),
  "GET /ui/api/routes": (_req, res) => json(res, 200, routes),
  "GET /ui/api/routes/explain": (req, res) => {
    const q = new URL(req.url, "http://mock").searchParams;
    json(res, 200, explain(q.get("model") ?? "", q.get("kind") ?? "chat"));
  },
  "GET /ui/api/requests": (req, res) => {
    const limit = Number(new URL(req.url, "http://x").searchParams.get("limit") ?? 100);
    json(res, 200, { requests: records.slice(0, limit) });
  },
  "GET /ui/api/events": (req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
    res.write(": connected\n\n");
    clients.add(res);
    const ping = setInterval(() => res.write(": ping\n\n"), 15_000);
    req.on("close", () => {
      clearInterval(ping);
      clients.delete(res);
    });
  },
  "GET /ui/api/requests/export": (req, res) => {
    const q = new URL(req.url, "http://x").searchParams;
    const outcome = (r) => (r.status === null ? "pending" : r.status >= 400 || r.error ? "error" : "ok");
    const rows = records
      .filter(
        (r) =>
          (!q.get("kind") || r.kind === q.get("kind")) &&
          (!q.get("status") || outcome(r) === q.get("status")) &&
          (!q.get("client") || r.client === q.get("client")) &&
          (!q.get("provider") || r.provider === q.get("provider")) &&
          (!q.get("model") || (r.model ?? "").toLowerCase().includes(q.get("model").toLowerCase())) &&
          (!q.get("since") || r.started_at >= new Date(q.get("since")).toISOString()),
      )
      .reverse();
    if (q.get("format") === "csv") {
      const cols = ["id", "started_at", "method", "path", "format", "kind", "model", "provider", "client", "status", "duration_ms", "input_tokens", "output_tokens", "cost_usd", "error"];
      const cell = (v) => (/[",\n]/.test(String(v ?? "")) ? `"${String(v).replace(/"/g, '""')}"` : (v ?? ""));
      const line = (r) => cols.map((c) => cell(c.endsWith("_tokens") ? r.usage?.[c] : r[c])).join(",");
      res.writeHead(200, { "content-type": "text/csv; charset=utf-8", "content-disposition": 'attachment; filename="requests.csv"' });
      return res.end([cols.join(","), ...rows.map(line)].join("\n") + "\n");
    }
    res.writeHead(200, { "content-type": "application/x-ndjson", "content-disposition": 'attachment; filename="requests.jsonl"' });
    res.end(rows.map((r) => JSON.stringify(r) + "\n").join(""));
  },
  "GET /v1/models": (_req, res) => json(res, 200, { object: "list", data: models }),
  "POST /v1/chat/completions": chat,
  "POST /v1/messages": (req, res) => chat(req, res, "anthropic-messages"),
  "POST /v1/responses": (req, res) => chat(req, res, "openai-responses"),
  "POST /v1/images/generations": (req, res) =>
    media(req, res, {
      path: "/v1/images/generations",
      format: "openai-images",
      kind: "image",
      model: "gpt-image-1",
      answer: (raw) => {
        const body = JSON.parse(raw.toString() || "{}");
        const n = Math.min(4, Number(body.n ?? 1));
        json(res, 200, {
          created: Math.floor(Date.now() / 1000),
          data: Array.from({ length: n }, (_, i) => ({ b64_json: pngImage(body.prompt ?? "", i) })),
        });
      },
    }),
  "POST /v1/audio/transcriptions": (req, res) =>
    media(req, res, {
      path: "/v1/audio/transcriptions",
      format: "openai-transcriptions",
      kind: "transcription",
      model: "whisper-1",
      answer: (raw) => transcript(res, raw),
    }),
  "POST /v1/embeddings": (req, res) =>
    media(req, res, {
      path: "/v1/embeddings",
      format: "openai-embeddings",
      kind: "embedding",
      model: "text-embedding-3-small",
      answer: (raw) => {
        const body = JSON.parse(raw.toString() || "{}");
        const inputs = [body.input].flat();
        json(res, 200, {
          object: "list",
          model: body.model,
          data: inputs.map((text, index) => ({
            object: "embedding",
            index,
            embedding: hashedVector(String(text)),
          })),
          usage: { prompt_tokens: inputs.length * 4, total_tokens: inputs.length * 4 },
        });
      },
    }),
  "POST /v1/audio/speech": (req, res) =>
    media(req, res, {
      path: "/v1/audio/speech",
      format: "openai-speech",
      kind: "speech",
      model: "gpt-4o-mini-tts",
      answer: () => {
        res.writeHead(200, { "content-type": "audio/wav" });
        res.end(chimeWav());
      },
    }),
};

const server = createServer((req, res) => {
  const path = new URL(req.url, "http://x").pathname;
  if (path !== "/healthz" && !authorized(req)) {
    return json(res, 401, {
      error: { message: "A gateway key is required", type: "authentication_error" },
    });
  }
  if (req.method === "POST" && /^\/v1beta\/models\/[^/]+:(stream)?[gG]enerateContent$/.test(path)) {
    return chat(req, res, "gemini-generate").catch((err) =>
      json(res, 500, { error: { message: String(err), type: "mock_error" } }),
    );
  }
  const route = path.startsWith("/ui/api/requests/") && path !== "/ui/api/requests/export" ? null : routesTable[`${req.method} ${path}`];
  if (route) {
    return Promise.resolve(route(req, res)).catch((err) => {
      console.error(err);
      if (!res.headersSent) json(res, 500, { error: { message: String(err), type: "mock_error" } });
    });
  }
  if (path === "/v1/videos" || path.startsWith("/v1/videos/")) {
    return videos(req, res, path).catch((err) =>
      json(res, 500, { error: { message: String(err), type: "mock_error" } }),
    );
  }
  if (req.method === "GET" && path.startsWith("/ui/api/requests/") && path !== "/ui/api/requests/export") {
    const id = decodeURIComponent(path.slice("/ui/api/requests/".length));
    const record = records.find((r) => r.id === id);
    return record ? json(res, 200, record) : notFound(res, "Request evicted or unknown");
  }
  notFound(res, `No format serves ${req.method} ${path}`);
});
server.on("upgrade", upgrade);
server.listen(PORT, "127.0.0.1", () => {
  console.log(`mock gateway on http://127.0.0.1:${PORT}${KEY ? " (key required)" : ""}`);
});
