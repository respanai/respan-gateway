// What every playground tab shares of the client APIs it can speak: each
// tab lists its own, and offers those the gateway serves for its kind.

import type { ServedFormat } from "@/lib/api";

/** A JSON object of an API's, read loosely: the adapters know its shape. */
export type Json = Record<string, any>; // eslint-disable-line @typescript-eslint/no-explicit-any

/** A client API a tab can speak, by the format the gateway names it. */
export interface ApiOption {
  id: string;
  label: string;
  /** The path a client calls, as shown. */
  endpoint: string;
}

/**
 * Those of `apis` the gateway serves as `kind`, in their order; the first
 * alone until the gateway has answered, or if it lists none of them.
 */
export function servedApis<A extends { id: string }>(
  kind: string,
  apis: A[],
  serve: Pick<ServedFormat, "format" | "kind">[] | undefined,
): A[] {
  const served = apis.filter((a) => serve?.some((s) => s.kind === kind && s.format === a.id));
  return served.length > 0 ? served : apis.slice(0, 1);
}

/** The media tabs' APIs: OpenAI's, the one client format of each of their kinds. */
export const mediaApis = {
  transcription: [
    { id: "openai-transcriptions", label: "OpenAI Transcriptions", endpoint: "/v1/audio/transcriptions" },
  ],
  speech: [{ id: "openai-speech", label: "OpenAI Speech", endpoint: "/v1/audio/speech" }],
  embedding: [{ id: "openai-embeddings", label: "OpenAI Embeddings", endpoint: "/v1/embeddings" }],
} satisfies Record<string, ApiOption[]>;
