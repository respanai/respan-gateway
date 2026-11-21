/** A rerank answer as the gateway's Cohere Rerank writes it. */
export interface RerankResponse {
  results: { index: number; relevance_score: number }[];
  usage?: { prompt_tokens?: number; search_units?: number };
}

/** What a ranking used: its tokens, its search units, or both; none if it says neither. */
export function rerankUsage(res: RerankResponse): string | undefined {
  const said = [];
  if (res.usage?.prompt_tokens != null) said.push(`${res.usage.prompt_tokens} tokens`);
  if (res.usage?.search_units != null) {
    said.push(`${res.usage.search_units} search unit${res.usage.search_units === 1 ? "" : "s"}`);
  }
  return said.length > 0 ? said.join(" · ") : undefined;
}
