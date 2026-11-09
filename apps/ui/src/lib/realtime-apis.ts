// The client APIs the realtime playground can speak. Each adapter says where
// its session is, what a client sends (its opening, the mic's audio, a cut),
// and reads what the server sends into OpenAI Realtime's events, which the
// page and its reducer read whatever the API.

import type { Model } from "@/lib/api";
import type { Json } from "@/lib/playground-apis";
import type { ServerEvent } from "@/lib/realtime";
import { REALTIME_RATE } from "@/lib/realtime-audio";

export type RealtimeApiId = "openai-realtime" | "openai-live" | "gemini-live" | "mistral-realtime" | "phonic-sts";

/** A message a client sends, as the API has it. */
export type ClientMessage = Record<string, unknown>;

/** What a session is for: the model answering, or the user's speech transcribed alone. */
export type RealtimeMode = "conversation" | "transcription";

/** Whether a model transcribes, as its name says: a guess, as a model list says no more. */
const transcribes = (model: Model) => /transcribe|whisper/i.test(model.id);

/**
 * `models` with those that transcribe first: a transcription's model is the
 * one that transcribes, and a conversational one may refuse to.
 */
export const transcribersFirst = (models: Model[]) => [
  ...models.filter(transcribes),
  ...models.filter((m) => !transcribes(m)),
];

export interface RealtimeApi {
  id: RealtimeApiId;
  label: string;
  /** The path its sessions open at, as the gateway serves it. */
  path: string;
  /** That path, as shown. */
  endpoint: string;
  /** Its URL on the page's host, for `model` and `mode`, carrying `key` if the API takes it there. */
  url: (
    model: string,
    key: string | null,
    location: Pick<Location, "protocol" | "host">,
    mode: RealtimeMode,
  ) => string;
  /** The subprotocols it offers, carrying `key` if the API takes it there. */
  protocols: (key: string | null) => string[];
  /** What its sessions can be for, the first its default. */
  modes: RealtimeMode[];
  /** What a client sends first, once the session is open. */
  opening: (model: string, mode: RealtimeMode) => ClientMessage[];
  /** One chunk of the mic: base64 PCM16 mono at 24 kHz. */
  audio: (base64: string) => ClientMessage;
  /**
   * What tells the server the client stopped playing a reply, `heardMs` of it
   * heard; none for an API that keeps no record of what was heard.
   */
  truncate?: (reply: { id: string; index: number; heardMs: number }) => ClientMessage;
  /** What cancels the reply being made; none for an API that cannot. */
  cancel?: ClientMessage;
  /** A reader of one session's server messages, into OpenAI Realtime's events. */
  reader: () => (message: unknown) => ServerEvent[];
}

const socketUrl = (location: Pick<Location, "protocol" | "host">, path: string) =>
  `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${path}`;

/**
 * OpenAI Realtime: `/v1/realtime?model=…`. A browser's WebSocket can set no
 * header, so the gateway key rides in a subprotocol, as OpenAI's own
 * browser clients send theirs.
 */
export const openaiRealtime: RealtimeApi = {
  id: "openai-realtime",
  label: "OpenAI Realtime",
  path: "/v1/realtime",
  endpoint: "/v1/realtime",
  // A transcription is one from its connect, as OpenAI takes it; the model
  // routes it, as the gateway takes it.
  url: (model, _key, location, mode) =>
    socketUrl(
      location,
      `/v1/realtime?model=${encodeURIComponent(model)}${mode === "transcription" ? "&intent=transcription" : ""}`,
    ),
  protocols: (key) => (key ? ["realtime", `openai-insecure-api-key.${key}`] : ["realtime"]),
  modes: ["conversation", "transcription"],
  opening: (_model, mode) => [
    {
      type: "session.update",
      session: {
        type: mode === "transcription" ? "transcription" : "realtime",
        audio: { input: { transcription: { model: "gpt-4o-mini-transcribe" } } },
      },
    },
  ],
  audio: (audio) => ({ type: "input_audio_buffer.append", audio }),
  truncate: ({ id, index, heardMs }) => ({
    type: "conversation.item.truncate",
    item_id: id,
    content_index: index,
    audio_end_ms: heardMs,
  }),
  cancel: { type: "response.cancel" },
  reader: () => (message) => [message as ServerEvent],
};

const LIVE_PATH = "/v1/live/sessions";

/**
 * OpenAI's Live API, its model on `?model=`, which routes it, and the key on
 * `?key=`. It is full duplex: it streams audio all the time, silence (zero
 * bytes, all `A`s in base64) included, and has no turns or ids. So the
 * reader drops the silence and numbers the lines by who speaks: the user's
 * transcript is `user-n`, the model's voice or transcript `model-n`, and
 * the user speaking again ends the model's. A client cannot cancel it.
 */
export const openaiLive: RealtimeApi = {
  id: "openai-live",
  label: "OpenAI Live",
  path: LIVE_PATH,
  endpoint: LIVE_PATH,
  url: (model, key, location) => {
    const query = new URLSearchParams({ model });
    if (key) query.set("key", key);
    return socketUrl(location, `${LIVE_PATH}?${query}`);
  },
  protocols: () => [],
  modes: ["conversation"],
  opening: (model) => [
    { type: "session.start", session: { model, audio: { format: { type: "audio/pcm", rate: REALTIME_RATE } } } },
  ],
  audio: (audio) => ({ type: "session.input_audio.append", audio }),
  reader: () => {
    let turn = 0;
    let speaking: "user" | "model" | null = null;
    return (message) => {
      const m = message as Json;
      const delta = String(m.delta ?? "");
      const out: ServerEvent[] = [];
      const speaks = (who: "user" | "model") => {
        if (speaking === who) return;
        if (who === "user" && speaking === "model") {
          out.push({ type: "response.done", response: { status: "completed" } });
          turn += 1;
        }
        if (who === "model") out.push({ type: "input_audio_buffer.speech_stopped" });
        out.push({ type: "conversation.item.created", item: { id: `${who}-${turn}`, role: who === "user" ? "user" : "assistant" } });
        speaking = who;
      };
      switch (m.type) {
        case "session.input_transcript.delta":
          speaks("user");
          out.push({ type: "conversation.item.input_audio_transcription.delta", item_id: `user-${turn}`, delta });
          break;
        case "session.output_audio.delta":
          if (/^A*=*$/.test(delta)) break;
          speaks("model");
          out.push({ type: "response.output_audio.delta", item_id: `model-${turn}`, content_index: 0, delta });
          break;
        case "session.output_transcript.delta":
          speaks("model");
          out.push({ type: "response.output_audio_transcript.delta", item_id: `model-${turn}`, delta });
          break;
        case "error":
          out.push({ type: "error", error: { message: String(m.error?.message ?? "") } });
      }
      return out;
    };
  },
};

const GEMINI_PATH = "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

/**
 * Gemini Live, at its own path, its model also on `?model=`, which routes
 * it, and the gateway key on `?key=`, as Google's own clients send theirs.
 * Its turns have no ids, so the reader numbers them: the user's line and
 * the model's of turn `n` are `user-n` and `model-n`. Its server notices a
 * barge-in itself and says `interrupted`; a client can neither cancel a
 * reply nor say how much of it was heard.
 */
export const geminiLive: RealtimeApi = {
  id: "gemini-live",
  label: "Gemini Live",
  path: GEMINI_PATH,
  endpoint: GEMINI_PATH,
  url: (model, key, location) => {
    const query = new URLSearchParams({ model });
    if (key) query.set("key", key);
    return socketUrl(location, `${GEMINI_PATH}?${query}`);
  },
  protocols: () => [],
  modes: ["conversation"],
  opening: (model) => [
    {
      setup: {
        model: `models/${model}`,
        generationConfig: { responseModalities: ["AUDIO"] },
        inputAudioTranscription: {},
        outputAudioTranscription: {},
      },
    },
  ],
  audio: (data) => ({
    realtimeInput: { audio: { mimeType: `audio/pcm;rate=${REALTIME_RATE}`, data } },
  }),
  reader: () => {
    let turn = 0;
    let begun = false;
    let answering = false;
    return (message) => {
      const m = message as Json;
      const content = m.serverContent as Json | undefined;
      if (!content) return [];
      const user = `user-${turn}`;
      const model = `model-${turn}`;
      const out: ServerEvent[] = [];
      const done = (status: string) => {
        out.push({ type: "response.done", response: { status } });
        turn += 1;
        begun = false;
        answering = false;
      };
      const parts: Json[] = content.modelTurn?.parts ?? [];
      const carries =
        Boolean(content.inputTranscription?.text) ||
        Boolean(content.outputTranscription?.text) ||
        content.interrupted === true ||
        parts.some((p) => p.inlineData || p.text);
      if (!carries) {
        // A turn's end alone, such as the one after an interruption, which
        // already ended it, opens no turn.
        if (content.turnComplete === true && begun) done("completed");
        return out;
      }
      const lateSaid = content.outputTranscription?.text;
      if (!begun && turn > 0 && lateSaid && !content.inputTranscription?.text && parts.length === 0) {
        // The rest of the last reply's transcript, after its turn ended: its
        // own, which the reducer ignores if it was cut off.
        return [{ type: "response.output_audio_transcript.delta", item_id: `model-${turn - 1}`, delta: lateSaid }];
      }
      if (!begun) {
        // The user's line first, even if its transcript comes after the reply.
        out.push({ type: "conversation.item.created", item: { id: user, role: "user" } });
        begun = true;
      }
      const heard = content.inputTranscription?.text;
      if (typeof heard === "string" && heard) {
        out.push({ type: "conversation.item.input_audio_transcription.delta", item_id: user, delta: heard });
      }
      if (content.interrupted === true) {
        out.push({ type: "input_audio_buffer.speech_started" });
      }
      for (const part of parts) {
        if (!answering && (part.inlineData || part.text)) {
          out.push({ type: "input_audio_buffer.speech_stopped" });
          out.push({ type: "conversation.item.created", item: { id: model, role: "assistant" } });
          answering = true;
        }
        const audio = part.inlineData?.data;
        if (typeof audio === "string" && String(part.inlineData?.mimeType ?? "").startsWith("audio/")) {
          out.push({ type: "response.output_audio.delta", item_id: model, content_index: 0, delta: audio });
        }
        if (typeof part.text === "string" && part.text && part.thought !== true) {
          out.push({ type: "response.output_text.delta", item_id: model, delta: part.text });
        }
      }
      const said = content.outputTranscription?.text;
      if (typeof said === "string" && said) {
        out.push({ type: "response.output_audio_transcript.delta", item_id: model, delta: said });
      }
      if (content.interrupted === true) done("cancelled");
      else if (content.turnComplete === true) done("completed");
      return out;
    };
  },
};

const MISTRAL_PATH = "/v1/audio/transcriptions/realtime";

/**
 * Mistral's realtime transcription, at its own path, its model on `?model=`
 * and the gateway key on `?key=`. It transcribes alone, with no voice
 * detection: the user's line grows until its audio ends.
 */
export const mistralRealtime: RealtimeApi = {
  id: "mistral-realtime",
  label: "Mistral Realtime",
  path: MISTRAL_PATH,
  endpoint: MISTRAL_PATH,
  url: (model, key, location) => {
    const query = new URLSearchParams({ model });
    if (key) query.set("key", key);
    return socketUrl(location, `${MISTRAL_PATH}?${query}`);
  },
  protocols: () => [],
  modes: ["transcription"],
  opening: () => [
    {
      type: "session.update",
      session: { audio_format: { encoding: "pcm_s16le", sample_rate: REALTIME_RATE } },
    },
  ],
  audio: (audio) => ({ type: "input_audio.append", audio }),
  reader: () => {
    let turn = 0;
    return (message) => {
      const m = message as Json;
      const user = `user-${turn}`;
      switch (m.type) {
        case "transcription.text.delta":
          return [{ type: "conversation.item.input_audio_transcription.delta", item_id: user, delta: m.text }];
        case "transcription.done":
          turn += 1;
          return [{ type: "conversation.item.input_audio_transcription.completed", item_id: user, transcript: m.text }];
        case "error":
          return [{ type: "error", error: { message: String(m.error?.message ?? "") } }];
        default:
          return [];
      }
    };
  },
};

const PHONIC_PATH = "/v1/sts/ws";

/**
 * Phonic's speech-to-speech, its model on `?model=`, which routes it, and
 * the key on `?key=`. Its turns have no ids, so the reader numbers them:
 * the user's line and the model's of turn `n` are `user-n` and `model-n`.
 * The user's transcript comes a word at a time, unspaced; each audio chunk
 * carries the text it speaks. It streams audio all the time, silence
 * included, which the reader drops. Its server notices a barge-in itself;
 * a client cannot cancel it.
 */
export const phonicSts: RealtimeApi = {
  id: "phonic-sts",
  label: "Phonic",
  path: PHONIC_PATH,
  endpoint: PHONIC_PATH,
  url: (model, key, location) => {
    const query = new URLSearchParams({ model });
    if (key) query.set("key", key);
    return socketUrl(location, `${PHONIC_PATH}?${query}`);
  },
  protocols: () => [],
  modes: ["conversation"],
  opening: (model) => [
    { type: "config", model, input_format: `pcm_${REALTIME_RATE}`, output_format: `pcm_${REALTIME_RATE}` },
  ],
  audio: (audio) => ({ type: "audio_chunk", audio }),
  reader: () => {
    let turn = 0;
    let heard = false;
    return (message) => {
      const m = message as Json;
      const user = `user-${turn}`;
      const model = `model-${turn}`;
      const done = (status: string): ServerEvent[] => {
        turn += 1;
        return [{ type: "response.done", response: { status } }];
      };
      switch (m.type) {
        case "user_started_speaking":
          heard = false;
          return [
            { type: "input_audio_buffer.speech_started" },
            { type: "conversation.item.created", item: { id: user, role: "user" } },
          ];
        case "input_text": {
          const delta = `${heard ? " " : ""}${m.text ?? ""}`;
          heard = true;
          return [{ type: "conversation.item.input_audio_transcription.delta", item_id: user, delta }];
        }
        case "assistant_started_speaking":
          return [
            { type: "input_audio_buffer.speech_stopped" },
            { type: "conversation.item.created", item: { id: model, role: "assistant" } },
          ];
        case "audio_chunk": {
          const out: ServerEvent[] = [];
          if (!/^A*=*$/.test(String(m.audio ?? ""))) {
            out.push({ type: "response.output_audio.delta", item_id: model, content_index: 0, delta: m.audio });
          }
          if (m.text) out.push({ type: "response.output_audio_transcript.delta", item_id: model, delta: m.text });
          return out;
        }
        case "assistant_finished_speaking":
          return done("completed");
        case "interrupted_response":
          return done("cancelled");
        case "error":
          return [{ type: "error", error: { message: String(m.error?.message ?? "") } }];
        default:
          return [];
      }
    };
  },
};

export const realtimeApis: RealtimeApi[] = [openaiRealtime, openaiLive, geminiLive, mistralRealtime, phonicSts];
