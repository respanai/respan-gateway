import { describe, expect, it } from "vitest";
import { chatApi, chatApis, type ChatApiId, type ChatDelta } from "./chat-apis";
import type { ServedFormat } from "./api";
import { servedApis } from "./playground-apis";
import { EventStreamParser } from "./eventstream";
import { SseParser } from "./sse";

const messages = [
  { role: "system" as const, content: "Be brief" },
  { role: "user" as const, content: "Hi" },
  { role: "assistant" as const, content: "Hello" },
  { role: "user" as const, content: "Again" },
];

/** Reads a raw SSE stream through an adapter and merges what it saw. */
function run(id: ChatApiId, raw: string) {
  const out: ChatDelta & { text: string } = { text: "" };
  for (const event of new SseParser().push(raw)) {
    const d = chatApi(id).read(event);
    out.text += d.text ?? "";
    for (const k of ["inputTokens", "outputTokens", "finishReason"] as const) {
      if (d[k] !== undefined) Object.assign(out, { [k]: d[k] });
    }
  }
  return out;
}

describe("request adapters", () => {
  it("keep the system prompt in messages for OpenAI Chat", () => {
    expect(chatApi("openai-chat").body("m", messages)).toMatchObject({ model: "m", stream: true, messages });
  });

  it("lift the system prompt out for Anthropic Messages", () => {
    const body = chatApi("anthropic-messages").body("m", messages) as Record<string, unknown>;
    expect(body).toMatchObject({ system: "Be brief", stream: true, max_tokens: 4096 });
    expect(body.messages).toHaveLength(3);
  });

  it("send instructions and input for Responses", () => {
    const body = chatApi("openai-responses").body("m", messages) as Record<string, unknown>;
    expect(body).toMatchObject({ instructions: "Be brief", stream: true });
    expect(body.input).toHaveLength(3);
  });

  it("send contents with model turns for Gemini, and name the model in the path", () => {
    const gemini = chatApi("gemini-generate");
    expect(gemini.body("m", messages)).toEqual({
      systemInstruction: { parts: [{ text: "Be brief" }] },
      contents: [
        { role: "user", parts: [{ text: "Hi" }] },
        { role: "model", parts: [{ text: "Hello" }] },
        { role: "user", parts: [{ text: "Again" }] },
      ],
    });
    expect(gemini.path("gemini-2.5-flash")).toBe(
      "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
    );
  });
});

describe("stream adapters", () => {
  it("read OpenAI Chat chunks and usage", () => {
    const raw =
      `data: {"choices":[{"delta":{"content":"He"}}]}\n\n` +
      `data: {"choices":[{"delta":{"content":"y"},"finish_reason":"stop"}]}\n\n` +
      `data: {"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2}}\n\n` +
      `data: [DONE]\n\n`;
    expect(run("openai-chat", raw)).toEqual({
      text: "Hey",
      finishReason: "stop",
      inputTokens: 4,
      outputTokens: 2,
    });
  });

  it("read Anthropic events", () => {
    const raw =
      `event: message_start\ndata: {"message":{"usage":{"input_tokens":9}}}\n\n` +
      `event: content_block_delta\ndata: {"delta":{"type":"text_delta","text":"Hi "}}\n\n` +
      `event: content_block_delta\ndata: {"delta":{"type":"input_json_delta","partial_json":"{}"}}\n\n` +
      `event: content_block_delta\ndata: {"delta":{"type":"text_delta","text":"you"}}\n\n` +
      `event: message_delta\ndata: {"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}\n\n` +
      `event: ping\ndata: {}\n\n`;
    expect(run("anthropic-messages", raw)).toEqual({
      text: "Hi you",
      finishReason: "end_turn",
      inputTokens: 9,
      outputTokens: 3,
    });
  });

  it("read Responses events", () => {
    const raw =
      `event: response.created\ndata: {"type":"response.created","response":{}}\n\n` +
      `event: response.output_text.delta\ndata: {"type":"response.output_text.delta","delta":"Yo"}\n\n` +
      `event: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":1}}}\n\n`;
    expect(run("openai-responses", raw)).toEqual({
      text: "Yo",
      finishReason: "stop",
      inputTokens: 5,
      outputTokens: 1,
    });
  });

  it("read Gemini chunks", () => {
    const raw =
      `data: {"candidates":[{"content":{"parts":[{"text":"A"},{"text":"B"}]}}]}\n\n` +
      `data: {"candidates":[{"content":{"parts":[{"text":"C"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":6,"candidatesTokenCount":3}}\n\n`;
    expect(run("gemini-generate", raw)).toEqual({
      text: "ABC",
      finishReason: "stop",
      inputTokens: 6,
      outputTokens: 3,
    });
  });

  it("throw on an error each API reports in-stream", () => {
    expect(() => run("openai-chat", `data: {"error":{"message":"boom"}}\n\n`)).toThrow("boom");
    expect(() =>
      run("anthropic-messages", `event: error\ndata: {"error":{"message":"overloaded"}}\n\n`),
    ).toThrow("overloaded");
    expect(() =>
      run("openai-responses", `data: {"type":"response.failed","response":{"error":{"message":"no"}}}\n\n`),
    ).toThrow("no");
    expect(() => run("gemini-generate", `data: {"error":{"message":"quota"}}\n\n`)).toThrow("quota");
  });
});

/** A message of the AWS event stream, its CRCs left zero: the gateway checks them. */
function message(headers: Record<string, string>, payload: unknown): Uint8Array {
  const text = new TextEncoder();
  const block = Object.entries(headers).flatMap(([name, value]) => {
    const [n, v] = [text.encode(name), text.encode(value)];
    return [n.length, ...n, 7, v.length >> 8, v.length & 255, ...v];
  });
  const body = text.encode(JSON.stringify(payload));
  const total = 16 + block.length + body.length;
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  view.setUint32(0, total);
  view.setUint32(4, block.length);
  out.set(block, 12);
  out.set(body, 12 + block.length);
  return out;
}

const converse = (kind: string, payload: unknown) =>
  message({ ":event-type": kind, ":message-type": "event" }, payload);

describe("Bedrock Converse", () => {
  it("sends system blocks and content blocks, and names the model in the path", () => {
    const bedrock = chatApi("bedrock-converse");
    expect(bedrock.body("m", messages)).toEqual({
      system: [{ text: "Be brief" }],
      messages: [
        { role: "user", content: [{ text: "Hi" }] },
        { role: "assistant", content: [{ text: "Hello" }] },
        { role: "user", content: [{ text: "Again" }] },
      ],
    });
    expect(bedrock.path("us.amazon.nova-lite-v1:0")).toBe("/model/us.amazon.nova-lite-v1%3A0/converse-stream");
  });

  it("reads its event stream split anywhere, and throws on an exception", () => {
    const parts = [
      converse("messageStart", { role: "assistant" }),
      converse("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "He" } }),
      converse("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "y" } }),
      converse("messageStop", { stopReason: "end_turn" }),
      converse("metadata", { usage: { inputTokens: 4, outputTokens: 2 } }),
    ];
    const whole = new Uint8Array(parts.flatMap((p) => [...p]));
    for (const cut of [0, 5, 40, whole.length]) {
      const parser = new EventStreamParser();
      const events = [...parser.push(whole.subarray(0, cut)), ...parser.push(whole.subarray(cut))];
      const out: ChatDelta & { text: string } = { text: "" };
      for (const event of events) {
        const d = chatApi("bedrock-converse").read(event);
        out.text += d.text ?? "";
        for (const k of ["inputTokens", "outputTokens", "finishReason"] as const) {
          if (d[k] !== undefined) Object.assign(out, { [k]: d[k] });
        }
      }
      expect(out).toEqual({ text: "Hey", finishReason: "end_turn", inputTokens: 4, outputTokens: 2 });
    }
    const failed = message({ ":exception-type": "throttlingException", ":message-type": "exception" }, {
      message: "slow down",
    });
    const [exception] = new EventStreamParser().push(failed);
    expect(() => chatApi("bedrock-converse").read(exception)).toThrow("slow down");
  });
});

describe("servedApis for chat", () => {
  it("lists only the chat APIs the gateway serves, in display order", () => {
    const serve: Pick<ServedFormat, "format" | "kind">[] = [
      { format: "gemini-generate", kind: "chat" },
      { format: "openai-chat", kind: "chat" },
      { format: "openai-realtime", kind: "realtime" },
    ];
    expect(servedApis("chat", chatApis, serve).map((a) => a.id)).toEqual(["openai-chat", "gemini-generate"]);
  });

  it("falls back to OpenAI Chat before the gateway has answered", () => {
    expect(servedApis("chat", chatApis, undefined).map((a) => a.id)).toEqual(["openai-chat"]);
    expect(chatApis).toHaveLength(5);
  });
});
