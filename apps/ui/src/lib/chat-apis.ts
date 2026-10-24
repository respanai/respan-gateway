// The client APIs the chat playground can speak. Each adapter builds one
// API's streaming request and reads its stream events; nothing else here
// knows which API is in use.

import type { Json } from "@/lib/playground-apis";
import type { SseEvent } from "@/lib/sse";

export type ChatApiId =
  | "openai-chat"
  | "anthropic-messages"
  | "openai-responses"
  | "gemini-generate"
  | "bedrock-converse";

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

/** What one stream event carries; any part may be absent. */
export interface ChatDelta {
  text?: string;
  inputTokens?: number;
  outputTokens?: number;
  finishReason?: string;
}

export interface ChatApi {
  id: ChatApiId;
  label: string;
  /** The path a client calls, as the gateway lists it in `serve`. */
  endpoint: string;
  path: (model: string) => string;
  body: (model: string, messages: ChatMessage[]) => unknown;
  /** Reads one stream event; throws on an error the stream reports. */
  read: (event: SseEvent) => ChatDelta;
  /** How its stream is framed: server-sent events unless it says. */
  framing?: "eventstream";
}

const parse = (event: SseEvent): Json => JSON.parse(event.data) as Json;

function fail(error: unknown): never {
  const message =
    typeof error === "string" ? error : ((error as Json | undefined)?.message as string | undefined);
  throw new Error(message ?? "The provider reported an error");
}

const split = (messages: ChatMessage[]) => ({
  system: messages
    .filter((m) => m.role === "system")
    .map((m) => m.content)
    .join("\n\n"),
  turns: messages.filter((m): m is ChatMessage & { role: "user" | "assistant" } => m.role !== "system"),
});

const openaiChat: ChatApi = {
  id: "openai-chat",
  label: "OpenAI Chat",
  endpoint: "/v1/chat/completions",
  path: () => "/v1/chat/completions",
  body: (model, messages) => ({
    model,
    messages,
    stream: true,
    stream_options: { include_usage: true },
  }),
  read(event) {
    if (event.data === "[DONE]") return {};
    const chunk = parse(event);
    if (chunk.error) fail(chunk.error);
    const choice = chunk.choices?.[0];
    return {
      text: choice?.delta?.content || undefined,
      finishReason: choice?.finish_reason || undefined,
      inputTokens: chunk.usage?.prompt_tokens,
      outputTokens: chunk.usage?.completion_tokens,
    };
  },
};

const anthropicMessages: ChatApi = {
  id: "anthropic-messages",
  label: "Anthropic Messages",
  endpoint: "/v1/messages",
  path: () => "/v1/messages",
  body(model, messages) {
    const { system, turns } = split(messages);
    return { model, max_tokens: 4096, stream: true, ...(system && { system }), messages: turns };
  },
  read(event) {
    const data = parse(event);
    switch (event.event) {
      case "error":
        return fail(data.error);
      case "message_start":
        return { inputTokens: data.message?.usage?.input_tokens };
      case "content_block_delta":
        return { text: data.delta?.type === "text_delta" ? data.delta.text : undefined };
      case "message_delta":
        return { finishReason: data.delta?.stop_reason, outputTokens: data.usage?.output_tokens };
      default:
        return {};
    }
  },
};

const openaiResponses: ChatApi = {
  id: "openai-responses",
  label: "OpenAI Responses",
  endpoint: "/v1/responses",
  path: () => "/v1/responses",
  body(model, messages) {
    const { system, turns } = split(messages);
    return { model, stream: true, ...(system && { instructions: system }), input: turns };
  },
  read(event) {
    const data = parse(event);
    switch (data.type ?? event.event) {
      case "error":
        return fail(data);
      case "response.failed":
        return fail(data.response?.error);
      case "response.output_text.delta":
        return { text: data.delta };
      case "response.completed":
      case "response.incomplete":
        return {
          finishReason: data.response?.incomplete_details?.reason ?? "stop",
          inputTokens: data.response?.usage?.input_tokens,
          outputTokens: data.response?.usage?.output_tokens,
        };
      default:
        return {};
    }
  },
};

const geminiGenerate: ChatApi = {
  id: "gemini-generate",
  label: "Gemini",
  endpoint: "/v1beta/models/{model}:streamGenerateContent",
  path: (model) => `/v1beta/models/${encodeURIComponent(model)}:streamGenerateContent?alt=sse`,
  body(_model, messages) {
    const { system, turns } = split(messages);
    return {
      ...(system && { systemInstruction: { parts: [{ text: system }] } }),
      contents: turns.map((m) => ({
        role: m.role === "assistant" ? "model" : "user",
        parts: [{ text: m.content }],
      })),
    };
  },
  read(event) {
    const chunk = parse(event);
    if (chunk.error) fail(chunk.error);
    const candidate = chunk.candidates?.[0];
    const text = (candidate?.content?.parts ?? [])
      .map((p: Json) => p.text ?? "")
      .join("");
    return {
      text: text || undefined,
      finishReason: candidate?.finishReason?.toLowerCase(),
      inputTokens: chunk.usageMetadata?.promptTokenCount,
      outputTokens: chunk.usageMetadata?.candidatesTokenCount,
    };
  },
};

const bedrockConverse: ChatApi = {
  id: "bedrock-converse",
  label: "Bedrock Converse",
  endpoint: "/model/{model}/converse-stream",
  path: (model) => `/model/${encodeURIComponent(model)}/converse-stream`,
  framing: "eventstream",
  body(_model, messages) {
    const { system, turns } = split(messages);
    return {
      ...(system && { system: [{ text: system }] }),
      messages: turns.map((m) => ({ role: m.role, content: [{ text: m.content }] })),
    };
  },
  read(event) {
    const data = parse(event);
    switch (event.event) {
      case "exception":
        return fail(data);
      case "contentBlockDelta":
        return { text: data.delta?.text };
      case "messageStop":
        return { finishReason: data.stopReason };
      case "metadata":
        return { inputTokens: data.usage?.inputTokens, outputTokens: data.usage?.outputTokens };
      default:
        return {};
    }
  },
};

export const chatApis: ChatApi[] = [
  openaiChat,
  anthropicMessages,
  openaiResponses,
  geminiGenerate,
  bedrockConverse,
];

export const chatApi = (id: string): ChatApi => chatApis.find((a) => a.id === id) ?? openaiChat;
