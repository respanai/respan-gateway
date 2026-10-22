import { AudioLines, Hand, Mic, PhoneOff } from "lucide-react";
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import { EmptyState } from "@/components/empty-state";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { useGatewayKey, useRoutes } from "@/hooks/queries";
import { useModelChoice } from "@/hooks/use-model-choice";
import { useApiChoice } from "@/hooks/use-playground";
import {
  initialRealtime,
  openRealtime,
  realtimeReducer,
  type Line,
  type RealtimeSocket,
  type ServerEvent,
  type RealtimeState,
} from "@/lib/realtime";
import type { RequestRecord } from "@/lib/api";
import { realtimeApis, transcribersFirst, type RealtimeApi, type RealtimeMode } from "@/lib/realtime-apis";
import { locateCall } from "@/lib/served";
import { Player, startMic, type Mic as MicHandle } from "@/lib/realtime-io";
import { cn } from "@/lib/utils";
import { ModelPicker } from "./model-picker";
import { ConfigCard, PlaygroundLayout } from "./layout";
import { SelectField } from "./select-field";

const REFUSED =
  "The gateway refused the session. Check that a provider serves realtime for this model and that the gateway key is right.";

/** A realtime session in `api`, for `mode`: the socket, the mic, the speaker, and the transcript they make. */
function useRealtimeSession(api: RealtimeApi, model: string, mode: RealtimeMode) {
  const key = useGatewayKey();
  const [state, dispatch] = useReducer(realtimeReducer, initialRealtime);
  const [level, setLevel] = useState(0);
  const socket = useRef<RealtimeSocket | null>(null);
  const mic = useRef<MicHandle | null>(null);
  const player = useRef<Player | null>(null);
  /** The item whose audio is playing, so a barge-in can say how much was heard. */
  const playing = useRef<{ id: string; index: number } | null>(null);

  const cutOff = useCallback(() => {
    // A reply that already played out is not cut.
    const wasPlaying = player.current?.playing ?? false;
    const heardMs = player.current?.stop() ?? 0;
    if (wasPlaying && playing.current && api.truncate) {
      socket.current?.send(api.truncate({ ...playing.current, heardMs }));
    }
    playing.current = null;
  }, [api]);

  const teardown = useCallback(() => {
    mic.current?.stop();
    mic.current = null;
    player.current?.close();
    player.current = null;
    playing.current = null;
    setLevel(0);
  }, []);

  /** Ends the session, if one is open or opening: failed with `error`, else ended by the user. */
  const end = useCallback(
    (error?: string) => {
      const ws = socket.current;
      socket.current = null;
      if (!ws) return;
      // Its close, once it comes, is no longer this session's to report.
      ws.close();
      teardown();
      dispatch({ type: "close", error });
    },
    [teardown],
  );
  // Not `end` itself, which a click handler would pass the click as its error.
  const stop = useCallback(() => end(), [end]);

  useEffect(() => stop, [stop]);

  const start = useCallback(() => {
    stop();
    dispatch({ type: "connecting" });
    const speaker = new Player();
    player.current = speaker;
    const ws = openRealtime({
      api,
      model,
      mode,
      key,
      onOpen: () => {
        dispatch({ type: "open" });
        api.opening(model, mode).forEach(ws.send);
        startMic(
          speaker,
          (audio) => ws.send(api.audio(audio)),
          setLevel,
        ).then(
          (m) => {
            if (socket.current === ws) mic.current = m;
            else m.stop();
          },
          (error: unknown) => {
            // A session already ended or replaced has nothing to report.
            if (socket.current !== ws) return;
            end(
              error instanceof DOMException && error.name === "NotAllowedError"
                ? "Microphone access was blocked. Allow it for this page and start again."
                : `The microphone could not be opened: ${error instanceof Error ? error.message : String(error)}`,
            );
          },
        );
      },
      onEvent: (event: ServerEvent) => {
        dispatch({ type: "event", event });
        if (event.type === "input_audio_buffer.speech_started") cutOff();
        if (
          (event.type === "response.output_audio.delta" || event.type === "response.audio.delta") &&
          typeof event.delta === "string"
        ) {
          speaker.play(event.delta);
          playing.current = {
            id: String(event.item_id ?? ""),
            index: Number(event.content_index ?? 0),
          };
        }
      },
      onClose: ({ opened, code, reason }) => {
        if (socket.current !== ws) return;
        socket.current = null;
        teardown();
        // A session the gateway refused once open, such as a conversation
        // with a model that only transcribes, says why in its close.
        const closed = code !== 1000 && reason ? `The gateway closed the session: ${reason}.` : undefined;
        dispatch({ type: "close", error: opened ? closed : reason || REFUSED });
      },
    });
    socket.current = ws;
  }, [api, cutOff, end, key, mode, model, stop, teardown]);

  const interrupt = useCallback(() => {
    cutOff();
    if (api.cancel) socket.current?.send(api.cancel);
  }, [api, cutOff]);

  return { state, level, start, stop, interrupt };
}

function status(state: RealtimeState): { label: string; hint: string } {
  if (state.phase === "connecting") return { label: "Connecting…", hint: "Opening the session" };
  if (state.phase === "live") {
    if (state.speaker === "user") return { label: "You're speaking", hint: "Listening to you" };
    if (state.speaker === "assistant") return { label: "Model is speaking", hint: "Speak to interrupt" };
    return { label: "Listening", hint: "Say something" };
  }
  if (state.phase === "ended") return { label: "Session ended", hint: "Start again to continue" };
  if (state.phase === "failed") return { label: "Couldn't connect", hint: "See the message below" };
  return { label: "Not connected", hint: "Start a session to talk" };
}

/** A disc that pulses with whoever is speaking: brand for the model, foreground for the user. */
function Presence({ state, level }: { state: RealtimeState; level: number }) {
  const live = state.phase === "live";
  const { label, hint } = status(state);
  return (
    <div className="flex flex-col items-center gap-3 py-2">
      <div className="relative flex size-24 items-center justify-center" aria-hidden="true">
        <span
          className={cn(
            "absolute inset-0 rounded-full transition-all duration-150",
            state.speaker === "assistant" ? "animate-pulse bg-brand/25" : "bg-muted",
          )}
          style={
            state.speaker === "user" || (live && state.speaker === null)
              ? { transform: `scale(${1 + level * 0.35})` }
              : undefined
          }
        />
        <span
          className={cn(
            "relative flex size-14 items-center justify-center rounded-full",
            state.speaker === "assistant"
              ? "bg-brand text-white"
              : live
                ? "bg-secondary text-secondary-foreground"
                : "bg-muted text-muted-foreground",
          )}
        >
          {state.speaker === "assistant" ? <AudioLines className="size-6" /> : <Mic className="size-6" />}
        </span>
      </div>
      <div className="text-center">
        <p role="status" className="text-sm font-medium">
          {label}
        </p>
        <p className="text-xs text-muted-foreground">{hint}</p>
      </div>
    </div>
  );
}

function Transcript({ lines }: { lines: Line[] }) {
  const bottom = useRef<HTMLDivElement>(null);
  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "nearest" });
  }, [lines]);
  return (
    <ol className="grid grid-cols-[minmax(0,1fr)] gap-4" aria-label="Transcript">
      {lines.map((line) =>
        line.role === "user" ? (
          <li key={line.id} className="flex justify-end">
            <div className="max-w-[85%] rounded-2xl rounded-br-md bg-secondary px-4 py-2.5 text-sm text-secondary-foreground">
              {line.text || <span className="text-muted-foreground">Listening…</span>}
            </div>
          </li>
        ) : (
          <li key={line.id} className="max-w-[92%] text-sm">
            {line.text}
            {!line.done && (
              <span className="ml-0.5 inline-block h-4 w-1.5 translate-y-0.5 animate-pulse rounded-sm bg-brand" />
            )}
            {line.interrupted && <span className="ml-2 text-xs text-muted-foreground">interrupted</span>}
          </li>
        ),
      )}
      <div ref={bottom} />
    </ol>
  );
}

export function RealtimeTab() {
  const { data: routes } = useRoutes();
  const apiChoice = useApiChoice("realtime", realtimeApis);
  const api = apiChoice.api;
  const [chosenMode, setMode] = useState<RealtimeMode>("conversation");
  const mode = api.modes.includes(chosenMode) ? chosenMode : api.modes[0];
  const transcribing = mode === "transcription";
  const choice = useModelChoice(
    "realtime",
    transcribing ? "gpt-realtime-whisper" : "gpt-realtime",
    transcribing ? transcribersFirst : undefined,
  );
  const { state, level, start, stop, interrupt } = useRealtimeSession(api, choice.model, mode);
  const active = state.phase === "connecting" || state.phase === "live";
  // A session's record is finished when it ends.
  const [record, setRecord] = useState<RequestRecord>();
  const began = useRef<number | null>(null);
  const begin = () => {
    began.current = Date.now();
    setRecord(undefined);
    start();
  };
  useEffect(() => {
    if ((state.phase === "ended" || state.phase === "failed") && began.current !== null) {
      const since = began.current;
      began.current = null;
      void locateCall({ api: api.id, model: choice.model, since })
        .then((found) => found && setRecord(found))
        .catch(() => undefined);
    }
  }, [state.phase, api.id, choice.model]);
  const unserved =
    !choice.isPending &&
    !choice.models?.length &&
    routes !== undefined &&
    !routes.defaults.some((d) => d.kind === "realtime");

  if (unserved) {
    return (
      <Card className="py-0">
        <EmptyState icon={AudioLines} title="No realtime provider" className="border-none">
          <p>
            No provider here serves realtime voice. Add one that speaks OpenAI Realtime, Gemini Live or
            Mistral Realtime (kind <code>realtime</code>) to the gateway's config, and it shows up here.
          </p>
        </EmptyState>
      </Card>
    );
  }

  return (
    <PlaygroundLayout
      config={
        <ConfigCard
          id="realtime"
          kind="realtime"
          apis={apiChoice.apis}
          api={api}
          onApi={apiChoice.choose}
          locked={active}
          record={record}
        >
        <ModelPicker id="realtime-model" choice={choice} disabled={active} />
        {api.modes.length > 1 && (
          <SelectField
            id="realtime-mode"
            label="Mode"
            value={mode}
            onChange={(m) => {
              setMode(m as RealtimeMode);
              // Each mode starts from its own first model.
              choice.setModel("");
            }}
            options={api.modes}
            className="capitalize"
            disabled={active}
          />
        )}
        {active ? (
          <div className="grid gap-2">
            {api.cancel && !transcribing && (
              <Button variant="outline" onClick={interrupt} disabled={state.speaker !== "assistant"}>
                <Hand /> Interrupt
              </Button>
            )}
            <Button variant="destructive" onClick={stop}>
              <PhoneOff /> End session
            </Button>
          </div>
        ) : (
          <Button onClick={begin}>
            <Mic /> {state.phase === "idle" ? "Start session" : "Start a new session"}
          </Button>
        )}
        <p className="text-xs text-muted-foreground">
          Talks to the gateway's {api.label} endpoint over a WebSocket; any realtime model works,
          as the gateway translates.{" "}
          {transcribing &&
            "In transcription, your speech is transcribed and nothing answers; the model is the one that transcribes, so pick one that does. "}
          The browser asks for your microphone once the session opens; audio is sent as 24 kHz PCM16.
        </p>
        </ConfigCard>
      }
    >

      <Card className="flex h-[calc(100dvh-17rem)] min-h-[28rem] flex-col gap-0 overflow-hidden py-0">
        <div className="border-b bg-muted/20 p-3">
          <Presence state={state} level={level} />
        </div>
        <div className="flex-1 overflow-y-auto p-4 sm:p-6">
          {state.error && (
            <p role="alert" className="mb-4 rounded-lg border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm text-destructive">
              {state.error}
            </p>
          )}
          {state.lines.length === 0 ? (
            <p className="text-center text-sm text-muted-foreground">
              {transcribing
                ? "Your transcript appears here as you talk."
                : "Transcripts of both sides appear here as you talk."}
            </p>
          ) : (
            <Transcript lines={state.lines} />
          )}
        </div>
      </Card>
    </PlaygroundLayout>
  );
}
