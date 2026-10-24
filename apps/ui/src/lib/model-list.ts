// The models page's list: what each model's row says, and how the list is
// searched, filtered and sorted. Nothing here fetches.

import type { Kind, Model } from "@/lib/api";

/** What a model can be filtered by: a capability, or what it reads. */
export type Trait =
  | "tools"
  | "reasoning"
  | "structured_outputs"
  | "image"
  | "audio"
  | "video"
  | "document";

export const traits: { value: Trait; label: string }[] = [
  { value: "tools", label: "Tools" },
  { value: "reasoning", label: "Reasoning" },
  { value: "structured_outputs", label: "Structured outputs" },
  { value: "image", label: "Images" },
  { value: "audio", label: "Audio" },
  { value: "video", label: "Video" },
  { value: "document", label: "Documents" },
];

export type Sort = "name" | "price" | "context" | "retires";

export const sorts: { value: Sort; label: string }[] = [
  { value: "name", label: "Name" },
  { value: "price", label: "Cheapest" },
  { value: "context", label: "Largest context" },
  { value: "retires", label: "Retiring soonest" },
];

export interface ModelFilter {
  q: string;
  provider: string | null;
  kind: string | null;
  traits: Trait[];
  sort: Sort;
}

/** How soon a retirement is shown as near. */
export const SOON_DAYS = 90;
const DAY = 86_400_000;

export function hasTrait(m: Model, trait: Trait): boolean {
  return (
    (m.capabilities as string[] | undefined)?.includes(trait) === true ||
    (m.input_modalities as string[] | undefined)?.includes(trait) === true
  );
}

/** What a sort by price ranks: input and output as a typical call weighs them. */
export const cost = (m: Model) => (m.pricing ? m.pricing.input + 3 * m.pricing.output : undefined);

/** Whether `m` retires: `past`, `soon` (within `SOON_DAYS`), `later`, or not at all. */
export function retirement(m: Model, now = Date.now()): "past" | "soon" | "later" | null {
  if (!m.retires_at) return null;
  const at = Date.parse(m.retires_at);
  if (Number.isNaN(at)) return null;
  if (at <= now) return "past";
  return at - now <= SOON_DAYS * DAY ? "soon" : "later";
}

/** The provider a row shows: a route's is the config's. */
export const providerOf = (m: Model) => m.owned_by;

/** `models` as `filter` says: matching all of it, then sorted, unknown values last. */
export function filterModels(models: Model[], filter: ModelFilter): Model[] {
  const q = filter.q.trim().toLowerCase();
  const matches = (m: Model) =>
    (!q || [m.id, m.display_name ?? "", m.owned_by].some((s) => s.toLowerCase().includes(q))) &&
    (!filter.provider || m.owned_by === filter.provider) &&
    (!filter.kind || m.kind === filter.kind) &&
    filter.traits.every((t) => hasTrait(m, t));
  const last = Number.POSITIVE_INFINITY;
  const key: Record<Sort, (m: Model) => number> = {
    name: () => 0,
    price: (m) => cost(m) ?? last,
    context: (m) => -(m.context_window ?? -last),
    retires: (m) => (m.retires_at ? Date.parse(m.retires_at) : last),
  };
  return models
    .filter(matches)
    .sort((a, b) => key[filter.sort](a) - key[filter.sort](b) || a.id.localeCompare(b.id));
}

/** How many of each there are, for the page's summary. */
export function summarize(models: Model[], now = Date.now()) {
  return {
    models: models.filter((m) => !m.route).length,
    routes: models.filter((m) => m.route).length,
    providers: new Set(models.filter((m) => !m.route).map((m) => m.owned_by)).size,
    retiring: models.filter((m) => retirement(m, now) === "soon").length,
  };
}

/** The distinct values of `pick` among `models`, sorted. */
export function distinct(models: Model[], pick: (m: Model) => string | undefined): string[] {
  return [...new Set(models.map(pick).filter((v): v is string => !!v))].sort();
}

/** The playground tab that tries a model of `kind`, if one does. */
export const playgroundTab: Partial<Record<Kind, string>> = {
  chat: "chat",
  realtime: "realtime",
  image: "image",
  transcription: "transcribe",
  speech: "speech",
  embedding: "embeddings",
  rerank: "rerank",
  video: "video",
};
