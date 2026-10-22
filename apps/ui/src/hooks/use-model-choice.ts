import { useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import type { Kind, Model } from "@/lib/api";
import { useModels } from "@/hooks/queries";

export interface ModelChoice {
  /** The model to send: the user's pick, else the first listed, else `fallback`. */
  model: string;
  /** What the user typed or picked, possibly empty. */
  chosen: string;
  setModel: (model: string) => void;
  models: Model[] | undefined;
  isPending: boolean;
  fallback: string;
}

/** A dated snapshot id such as `gpt-realtime-2025-08-28` or `...-20250805`. */
const dated = /-(\d{4}-\d{2}-\d{2}|\d{8})$/;

/** The first listed model, preferring an alias to a dated snapshot. */
export function preferred(models: Model[] | undefined): string | undefined {
  return (models?.find((m) => !dated.test(m.id)) ?? models?.[0])?.id;
}

/** The models of `kind`, put in `order` if given, its first the default. */
export function useModelChoice(
  kind: Kind,
  fallback: string,
  order?: (models: Model[]) => Model[],
): ModelChoice {
  const { data: listed, isPending } = useModels(kind);
  const models = useMemo(() => (listed && order ? order(listed) : listed), [listed, order]);
  // A link may name the model to start with, as the models page's do.
  const [params] = useSearchParams();
  const [chosen, setModel] = useState(() => (params.get("kind") === kind ? (params.get("model") ?? "") : ""));
  const model = chosen.trim() || preferred(models) || fallback;
  return { model, chosen, setModel, models, isPending, fallback };
}
