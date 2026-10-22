import { describe, expect, it } from "vitest";
import { initialRealtime, realtimeReducer, type RealtimeState } from "./realtime";
import type { ServedFormat } from "./api";
import { servedApis } from "./playground-apis";
import { geminiLive, mistralRealtime, realtimeApis } from "./realtime-apis";

/** The state a session reaches from Gemini Live's `messages`, as the page reads them. */
function geminiSession(...messages: unknown[]): RealtimeState {
  const read = geminiLive.reader();
  const live: RealtimeState = { ...initialRealtime, phase: "live" };
  return messages
    .flatMap((m) => read(m))
    .reduce<RealtimeState>((state, event) => realtimeReducer(state, { type: "event", event }), live);
}

const lines = (state: RealtimeState) =>
  state.lines.map(({ role, text, done, interrupted }) => ({ role, text, done, interrupted }));

describe("Gemini Live", () => {
  it("opens with a setup that asks for audio and both transcripts", () => {
    const gemini = geminiLive;
    expect(gemini.opening("gemini-live-2.5-flash", "conversation")).toEqual([
      {
        setup: {
          model: "models/gemini-live-2.5-flash",
          generationConfig: { responseModalities: ["AUDIO"] },
          inputAudioTranscription: {},
          outputAudioTranscription: {},
        },
      },
    ]);
    expect(gemini.audio("AAAA")).toEqual({
      realtimeInput: { audio: { mimeType: "audio/pcm;rate=24000", data: "AAAA" } },
    });
    expect(gemini.cancel).toBeUndefined();
  });

  it("reads a turn into the user's line, then the model's, in order", () => {
    const state = geminiSession(
      { setupComplete: {} },
      { serverContent: { inputTranscription: { text: "What time" } } },
      { serverContent: { modelTurn: { parts: [{ inlineData: { mimeType: "audio/pcm;rate=24000", data: "AAAA" } }] } } },
      { serverContent: { inputTranscription: { text: " is it?" } } },
      { serverContent: { outputTranscription: { text: "It is noon." } } },
      { serverContent: { turnComplete: true } },
    );
    expect(lines(state)).toEqual([
      { role: "user", text: "What time is it?", done: false, interrupted: false },
      { role: "assistant", text: "It is noon.", done: true, interrupted: false },
    ]);
    expect(state.speaker).toBeNull();
  });

  it("plays the model's audio as OpenAI's deltas, under its turn's id", () => {
    const read = geminiLive.reader();
    const audio = { inlineData: { mimeType: "audio/pcm;rate=24000", data: "AAAA" } };
    const events = read({ serverContent: { modelTurn: { parts: [audio] } } });
    expect(events.find((e) => e.type === "response.output_audio.delta")).toEqual({
      type: "response.output_audio.delta",
      item_id: "model-0",
      content_index: 0,
      delta: "AAAA",
    });
  });

  it("marks the model's line interrupted when the user speaks over it, and starts a new turn", () => {
    const state = geminiSession(
      { serverContent: { inputTranscription: { text: "Tell me a story" } } },
      { serverContent: { outputTranscription: { text: "Once upon" } } },
      { serverContent: { interrupted: true } },
      // As Gemini sends it: the interrupted turn's end, after it.
      { serverContent: { turnComplete: true } },
      { serverContent: { inputTranscription: { text: "Stop" } } },
    );
    expect(lines(state)).toEqual([
      { role: "user", text: "Tell me a story", done: false, interrupted: false },
      { role: "assistant", text: "Once upon", done: true, interrupted: true },
      { role: "user", text: "Stop", done: false, interrupted: false },
    ]);
    expect(state.speaker).toBe("user");
  });

  it("gives a reply's late transcript to that reply, not a new turn", () => {
    const state = geminiSession(
      { serverContent: { inputTranscription: { text: "Hi" } } },
      { serverContent: { outputTranscription: { text: "Hello" } } },
      { serverContent: { turnComplete: true } },
      { serverContent: { outputTranscription: { text: " there" } } },
    );
    expect(lines(state)).toEqual([
      { role: "user", text: "Hi", done: false, interrupted: false },
      { role: "assistant", text: "Hello there", done: true, interrupted: false },
    ]);
  });

  it("ignores what is no server content, and a model's thoughts", () => {
    const read = geminiLive.reader();
    expect(read({ setupComplete: {} })).toEqual([]);
    expect(read({ usageMetadata: { totalTokenCount: 3 } })).toEqual([]);
    const thought = read({ serverContent: { modelTurn: { parts: [{ text: "planning", thought: true }] } } });
    expect(thought.some((e) => e.type === "response.output_text.delta")).toBe(false);
  });
});

describe("servedApis for realtime", () => {
  it("offers the APIs the gateway serves, else OpenAI Realtime", () => {
    const ids = (serve: Pick<ServedFormat, "format" | "kind">[] | undefined) =>
      servedApis("realtime", realtimeApis, serve).map((a) => a.id);
    expect(ids([{ format: "gemini-live", kind: "realtime" }, { format: "openai-realtime", kind: "realtime" }])).toEqual(
      ["openai-realtime", "gemini-live"],
    );
    expect(ids([{ format: "gemini-live", kind: "realtime" }])).toEqual(["gemini-live"]);
    expect(ids(undefined)).toEqual(["openai-realtime"]);
  });
});

describe("Mistral Realtime", () => {
  it("transcribes alone, at its path, its model and key in its query", () => {
    expect(mistralRealtime.modes).toEqual(["transcription"]);
    const url = new URL(mistralRealtime.url("voxtral", "sk-1", { protocol: "https:", host: "gw.test" }, "transcription"));
    expect(url.pathname).toBe("/v1/audio/transcriptions/realtime");
    expect(url.searchParams.get("model")).toBe("voxtral");
    expect(url.searchParams.get("key")).toBe("sk-1");
    expect(mistralRealtime.opening("voxtral", "transcription")).toEqual([
      { type: "session.update", session: { audio_format: { encoding: "pcm_s16le", sample_rate: 24000 } } },
    ]);
    expect(mistralRealtime.cancel).toBeUndefined();
  });

  it("reads each whole transcript as a line of its own", () => {
    const read = mistralRealtime.reader();
    const live: RealtimeState = { ...initialRealtime, phase: "live" };
    const state = [
      { type: "session.created", session: {} },
      { type: "transcription.text.delta", text: "Hel" },
      { type: "transcription.text.delta", text: "lo" },
      { type: "transcription.done", text: "Hello" },
      { type: "transcription.text.delta", text: "Again" },
    ]
      .flatMap((m) => read(m))
      .reduce<RealtimeState>((st, event) => realtimeReducer(st, { type: "event", event }), live);
    expect(lines(state)).toEqual([
      { role: "user", text: "Hello", done: true, interrupted: false },
      { role: "user", text: "Again", done: false, interrupted: false },
    ]);
    expect(read({ type: "error", error: { message: "bad audio", code: 422 } })).toEqual([
      { type: "error", error: { message: "bad audio" } },
    ]);
  });
});
