import {
  AudioLines,
  Clapperboard,
  ImageIcon,
  ListOrdered,
  MessageSquare,
  Mic,
  Radio,
  Sigma,
  type LucideIcon,
} from "lucide-react";
import { Suspense, lazy, useState, type ComponentType } from "react";
import { useSearchParams } from "react-router";
import { PageHeader } from "@/components/page-header";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ChatTab } from "./chat";
import { EmbeddingsTab } from "./embeddings";
import { ImageTab } from "./image";
import { RerankTab } from "./rerank";
import { SpeechTab } from "./speech";
import { TranscribeTab } from "./transcribe";
import { VideoTab } from "./video";

// The voice tab carries the audio code, so it loads when first opened.
const RealtimeTab = lazy(() => import("./realtime").then((m) => ({ default: m.RealtimeTab })));

const tabs: { value: string; label: string; icon: LucideIcon; Panel: ComponentType }[] = [
  { value: "chat", label: "Chat", icon: MessageSquare, Panel: ChatTab },
  { value: "realtime", label: "Realtime", icon: Radio, Panel: RealtimeTab },
  { value: "image", label: "Image", icon: ImageIcon, Panel: ImageTab },
  { value: "transcribe", label: "Transcribe", icon: Mic, Panel: TranscribeTab },
  { value: "speech", label: "Speech", icon: AudioLines, Panel: SpeechTab },
  { value: "embeddings", label: "Embeddings", icon: Sigma, Panel: EmbeddingsTab },
  { value: "rerank", label: "Rerank", icon: ListOrdered, Panel: RerankTab },
  { value: "video", label: "Video", icon: Clapperboard, Panel: VideoTab },
];

export function PlaygroundPage() {
  const [params, setParams] = useSearchParams();
  const requested = params.get("tab");
  const tab = tabs.some((t) => t.value === requested) ? requested! : "chat";
  // Panels mount on first visit and then stay, so a lazy one isn't fetched until opened.
  const [visited, setVisited] = useState(() => new Set([tab]));
  if (!visited.has(tab)) setVisited(new Set(visited).add(tab));

  return (
    <div className="grid grid-cols-1 gap-6">
      <PageHeader
        title="Playground"
      />
      <Tabs value={tab} onValueChange={(value) => setParams({ tab: value }, { replace: true })}>
        <TabsList variant="line" className="w-full justify-start overflow-x-auto border-b !h-11 !rounded-none !p-0">
          {tabs.map(({ value, label, icon: Icon }) => (
            <TabsTrigger key={value} value={value} className="h-full flex-none gap-2 rounded-none px-4 data-[state=active]:text-brand after:!bottom-0 after:!bg-brand">
              <Icon /> {label}
            </TabsTrigger>
          ))}
        </TabsList>
        {/* Panels stay mounted so a conversation or a running job survives a tab switch. */}
        {tabs.map(({ value, Panel }) => (
          <TabsContent key={value} value={value} forceMount className="mt-3 min-w-0 data-[state=inactive]:hidden">
            {visited.has(value) && (
              <Suspense fallback={<p className="text-sm text-muted-foreground">Loading…</p>}>
                <Panel />
              </Suspense>
            )}
          </TabsContent>
        ))}
      </Tabs>
    </div>
  );
}
