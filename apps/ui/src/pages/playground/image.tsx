import { useMutation } from "@tanstack/react-query";
import { Copy, Download, ImageIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { ChoiceGroup } from "@/components/choice-group";
import { EmptyState } from "@/components/empty-state";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { useModelChoice } from "@/hooks/use-model-choice";
import { useApiChoice, useLastCall } from "@/hooks/use-playground";
import { postJson } from "@/lib/api";
import { imageApis } from "@/lib/image-apis";
import type { Json } from "@/lib/playground-apis";
import { MediaLayout } from "./media-form";
import { ModelPicker } from "./model-picker";
import { RequestError } from "./request-error";
import { SelectField } from "./select-field";
import { submitOnModEnter } from "./submit-on-mod-enter";

const sizes = [
  { value: "1024x1024", label: "Square", hint: "1024 × 1024", ratio: "aspect-square" },
  { value: "1536x1024", label: "Landscape", hint: "1536 × 1024", ratio: "aspect-[3/2]" },
  { value: "1024x1536", label: "Portrait", hint: "1024 × 1536", ratio: "aspect-[2/3]" },
];
const qualities = ["auto", "low", "medium", "high"];
const examples = [
  { label: "Lighthouse", prompt: "A lighthouse at dusk, painted in soft watercolor" },
  { label: "Robot library", prompt: "A robot reading in a sunlit library, warm light" },
  { label: "Circuit city", prompt: "An isometric city made of circuit boards, neon edges" },
];

function ImageCard({ src, caption, index }: { src: string; caption?: string; index: number }) {
  const copy = async () => {
    try {
      const blob = await (await fetch(src)).blob();
      await navigator.clipboard.write([new ClipboardItem({ [blob.type]: blob })]);
      toast.success("Image copied");
    } catch {
      toast.error("Couldn't copy the image");
    }
  };
  return (
    <figure className="group overflow-hidden rounded-xl border bg-card">
      <div className="relative bg-muted/40">
        <img src={src} alt={caption ?? `Generated image ${index + 1}`} className="w-full" />
        <div className="absolute top-2 right-2 flex gap-1.5 opacity-0 transition-opacity group-focus-within:opacity-100 group-hover:opacity-100 max-sm:opacity-100">
          <Button type="button" size="icon" variant="secondary" className="size-8 shadow-sm" onClick={copy} aria-label={`Copy image ${index + 1}`}>
            <Copy />
          </Button>
          <Button asChild size="icon" variant="secondary" className="size-8 shadow-sm">
            <a href={src} download={`image-${index + 1}.png`} aria-label={`Download image ${index + 1}`}>
              <Download />
            </a>
          </Button>
        </div>
      </div>
      {caption && (
        <figcaption className="border-t p-3 text-xs text-muted-foreground">
          <span className="font-medium text-foreground">Revised prompt. </span>
          {caption}
        </figcaption>
      )}
    </figure>
  );
}

export function ImageTab() {
  const choice = useModelChoice("image", "gpt-image-1");
  const api = useApiChoice("image", imageApis);
  const call = useLastCall();
  const [prompt, setPrompt] = useState("");
  const [size, setSize] = useState(sizes[0].value);
  const [quality, setQuality] = useState("auto");
  const [n, setN] = useState("1");
  const tunes = api.api.tunes;
  const count = tunes ? Number(n) : 1;
  const generate = useMutation({
    mutationFn: () => {
      const { api: adapter } = api;
      const ask = { model: choice.model, prompt: prompt.trim(), size, quality, n: count };
      const { path, body } = adapter.create(ask);
      return call.track({ api: adapter.id, model: choice.model }, async () =>
        adapter.read(await postJson<Json>(path, body)),
      );
    },
  });

  const images = generate.data ?? [];
  const ratio = sizes.find((s) => s.value === size)?.ratio;

  return (
    <MediaLayout
      id="image"
      choice={api}
      record={call.record}
      onSubmit={() => generate.mutate()}
      pending={generate.isPending}
      disabled={!prompt.trim()}
      submitLabel={generate.isPending ? "Generating…" : "Generate"}
      fields={
        <>
          <div className="grid gap-2">
            <Label htmlFor="image-prompt">Prompt</Label>
            <Textarea
              id="image-prompt"
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={submitOnModEnter}
              placeholder="Describe the image you want"
              className="min-h-28"
            />
            <div className="flex flex-wrap items-center gap-1.5" aria-label="Example prompts" role="group">
              <span className="text-xs text-muted-foreground">Try</span>
              {examples.map((e) => (
                <button
                  key={e.label}
                  type="button"
                  onClick={() => setPrompt(e.prompt)}
                  className="truncate rounded-full border px-2.5 py-0.5 text-xs text-muted-foreground outline-none transition-colors hover:bg-muted hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50"
                >
                  {e.label}
                </button>
              ))}
            </div>
          </div>
          <ModelPicker id="image-model" choice={choice} />
          <ChoiceGroup label="Size" value={size} onChange={setSize} choices={sizes} className="grid-cols-3" />
          {tunes && (
            <div className="grid grid-cols-2 gap-3">
              <SelectField id="image-quality" label="Quality" value={quality} onChange={setQuality} options={qualities} className="capitalize" />
              <SelectField id="image-n" label="Images" value={n} onChange={setN} options={["1", "2", "3", "4"]} />
            </div>
          )}
        </>
      }
    >
      {generate.isError ? (
        <RequestError error={generate.error} what="images" />
      ) : generate.isPending ? (
        <div className="grid gap-4 sm:grid-cols-2" aria-busy="true" aria-label="Generating images">
          {Array.from({ length: count }, (_, i) => (
            <Skeleton key={i} className={`${ratio} rounded-xl`} />
          ))}
        </div>
      ) : images.length ? (
        <div className="grid gap-4 sm:grid-cols-2">
          {images.map((img, i) => (
            <ImageCard key={i} src={img.src} caption={img.revised} index={i} />
          ))}
        </div>
      ) : (
        <EmptyState icon={ImageIcon} title="Generate an image">
          <p>
            Sent to <code>{api.api.endpoint}</code>; results appear here, ready to copy or
            download.
          </p>
        </EmptyState>
      )}
    </MediaLayout>
  );
}
