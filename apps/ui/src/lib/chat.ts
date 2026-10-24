import { gatewayFetch } from "@/lib/api";
import { chatApi, type ChatApiId, type ChatMessage } from "@/lib/chat-apis";
import { readEventStream } from "@/lib/eventstream";
import { readSse } from "@/lib/sse";

export type { ChatMessage } from "@/lib/chat-apis";

export interface ChatStats {
  inputTokens: number | null;
  outputTokens: number | null;
  ttfbMs: number | null;
  totalMs: number;
  finishReason: string | null;
}

/**
 * Streams a chat reply through the gateway over the chosen client API,
 * calling onDelta with each piece of text as it arrives. Abort with `signal`
 * to stop early.
 */
export async function streamChat({
  api = "openai-chat",
  model,
  messages,
  signal,
  onDelta,
}: {
  api?: ChatApiId;
  model: string;
  messages: ChatMessage[];
  signal: AbortSignal;
  onDelta: (text: string) => void;
}): Promise<ChatStats> {
  const adapter = chatApi(api);
  const started = performance.now();
  const stats: ChatStats = {
    inputTokens: null,
    outputTokens: null,
    ttfbMs: null,
    totalMs: 0,
    finishReason: null,
  };
  const res = await gatewayFetch(adapter.path(model), {
    method: "POST",
    signal,
    headers: { "content-type": "application/json", accept: "text/event-stream" },
    body: JSON.stringify(adapter.body(model, messages)),
  });
  if (!res.body) throw new Error("The gateway sent an empty response");

  const read = adapter.framing === "eventstream" ? readEventStream : readSse;
  await read(res.body, (event) => {
    const delta = adapter.read(event);
    if (delta.text) {
      stats.ttfbMs ??= performance.now() - started;
      onDelta(delta.text);
    }
    if (delta.finishReason) stats.finishReason = delta.finishReason;
    if (delta.inputTokens !== undefined) stats.inputTokens = delta.inputTokens;
    if (delta.outputTokens !== undefined) stats.outputTokens = delta.outputTokens;
  });
  stats.totalMs = performance.now() - started;
  return stats;
}
