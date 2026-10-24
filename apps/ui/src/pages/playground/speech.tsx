import { useMutation } from "@tanstack/react-query";
import { AudioLines } from "lucide-react";
import { useState } from "react";
import { AudioPlayer } from "@/components/audio-player";
import { ChoiceGroup } from "@/components/choice-group";
import { EmptyState } from "@/components/empty-state";
import { Card } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { useModelChoice } from "@/hooks/use-model-choice";
import { useApiChoice, useLastCall } from "@/hooks/use-playground";
import { gatewayFetch } from "@/lib/api";
import { mediaApis } from "@/lib/playground-apis";
import { MediaLayout } from "./media-form";
import { ModelPicker } from "./model-picker";
import { RequestError } from "./request-error";
import { SelectField } from "./select-field";

const voices = ["alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer"];
const formats = ["mp3", "opus", "aac", "flac", "wav"];
// Gemini's speech models take its own voices, and speak WAV alone.
const geminiVoices = ["Kore", "Puck", "Charon", "Zephyr", "Fenrir", "Leda", "Orus", "Aoede"];
const geminiFormats = ["wav"];
const choicesOf = (names: string[]) => names.map((v) => ({ value: v, label: v[0].toUpperCase() + v.slice(1) }));

/** Whether `model` is one of Gemini's speech models, such as `gemini/gemini-3.8-flash-tts`. */
const isGeminiSpeech = (model: string) => /(^|\/)gemini-[^/]*tts/i.test(model);
// OpenAI's limit; a provider may allow less.
const MAX_CHARS = 4096;

export function SpeechTab() {
  const choice = useModelChoice("speech", "gpt-4o-mini-tts");
  const api = useApiChoice("speech", mediaApis.speech);
  const call = useLastCall();
  const [text, setText] = useState("");
  const gemini = isGeminiSpeech(choice.model);
  const [picked, setVoice] = useState("alloy");
  const [pickedFormat, setFormat] = useState("mp3");
  const ownVoices = gemini ? geminiVoices : voices;
  const ownFormats = gemini ? geminiFormats : formats;
  const voice = ownVoices.includes(picked) ? picked : ownVoices[0];
  const format = ownFormats.includes(pickedFormat) ? pickedFormat : ownFormats[0];
  const speak = useMutation({
    mutationFn: () =>
      call.track({ api: api.api.id, model: choice.model }, async () => {
        const res = await gatewayFetch("/v1/audio/speech", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            model: choice.model,
            input: text.trim(),
            voice,
            response_format: format,
          }),
        });
        return { audio: await res.blob(), voice, format, model: choice.model };
      }),
  });
  const result = speak.data;

  return (
    <MediaLayout
      id="speech"
      choice={api}
      record={call.record}
      onSubmit={() => speak.mutate()}
      pending={speak.isPending}
      disabled={!text.trim() || text.length > MAX_CHARS}
      submitLabel={speak.isPending ? "Synthesizing…" : "Speak"}
      fields={
        <>
          <div className="grid gap-2">
            <div className="flex items-baseline justify-between">
              <Label htmlFor="speech-text">Text</Label>
              <span
                className={`text-xs tabular ${text.length > MAX_CHARS ? "text-destructive" : "text-muted-foreground"}`}
              >
                {text.length.toLocaleString()} / {MAX_CHARS.toLocaleString()}
              </span>
            </div>
            <Textarea
              id="speech-text"
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder="Hello from the gateway."
              className="min-h-28"
            />
          </div>
          <ModelPicker id="speech-model" choice={choice} />
          <ChoiceGroup
            label="Voice"
            value={voice}
            onChange={setVoice}
            choices={choicesOf(ownVoices)}
            className="grid-cols-2"
          />
          <SelectField id="speech-format" label="Format" value={format} onChange={setFormat} options={ownFormats} />
        </>
      }
    >
      {speak.isError ? (
        <RequestError error={speak.error} what="speech" />
      ) : speak.isPending ? (
        <Card className="gap-3 p-6" aria-busy="true">
          <div className="flex items-center gap-3">
            <Skeleton className="size-10 rounded-full" />
            <Skeleton className="h-12 flex-1" />
          </div>
          <Skeleton className="h-4 w-24" />
        </Card>
      ) : result ? (
        <Card className="gap-4 p-6">
          <div className="flex items-center justify-between text-sm">
            <span className="font-medium capitalize">{result.voice}</span>
            <span className="font-mono text-xs text-muted-foreground">
              {result.model} · {result.format}
            </span>
          </div>
          <AudioPlayer blob={result.audio} filename={`speech.${result.format}`} label="Generated speech" />
        </Card>
      ) : (
        <EmptyState icon={AudioLines} title="Turn text into speech">
          <p>
            Sent to <code>/v1/audio/speech</code>; the audio plays here when it's ready.
          </p>
        </EmptyState>
      )}
    </MediaLayout>
  );
}
