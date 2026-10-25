import { useMutation } from "@tanstack/react-query";
import { ListOrdered } from "lucide-react";
import { useState } from "react";
import { EmptyState } from "@/components/empty-state";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { useModelChoice } from "@/hooks/use-model-choice";
import { useApiChoice, useLastCall } from "@/hooks/use-playground";
import { postJson } from "@/lib/api";
import { mediaApis } from "@/lib/playground-apis";
import { rerankUsage, type RerankResponse } from "@/lib/rerank";
import { MediaLayout } from "./media-form";
import { ModelPicker } from "./model-picker";
import { RequestError } from "./request-error";

export function RerankTab() {
  const choice = useModelChoice("rerank", "rerank-v3.5");
  const api = useApiChoice("rerank", mediaApis.rerank);
  const call = useLastCall();
  const [query, setQuery] = useState("");
  const [text, setText] = useState("");
  const documents = text.split("\n").map((d) => d.trim()).filter(Boolean);
  const rank = useMutation({
    mutationFn: () =>
      call.track({ api: api.api.id, model: choice.model }, async () => {
        // The documents as sent, which a result names by its index.
        const sent = documents;
        const res = await postJson<RerankResponse>(api.api.endpoint, {
          model: choice.model,
          query: query.trim(),
          documents: sent,
        });
        return { sent, res };
      }),
  });

  const results = rank.data?.res.results ?? [];
  const usage = rank.data ? rerankUsage(rank.data.res) : undefined;

  return (
    <MediaLayout
      id="rerank"
      choice={api}
      record={call.record}
      onSubmit={() => rank.mutate()}
      pending={rank.isPending}
      disabled={!query.trim() || documents.length === 0}
      submitLabel={rank.isPending ? "Ranking…" : "Rerank"}
      fields={
        <>
          <div className="grid gap-2">
            <Label htmlFor="rerank-query">Query</Label>
            <Input
              id="rerank-query"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="What is the capital of France?"
            />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="rerank-documents">Documents, one per line</Label>
            <Textarea
              id="rerank-documents"
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder={"Paris is the capital of France.\nBananas are yellow."}
              className="min-h-28"
            />
          </div>
          <ModelPicker id="rerank-model" choice={choice} />
        </>
      }
    >
      {rank.isError ? (
        <RequestError error={rank.error} what="a ranking" />
      ) : rank.isPending ? (
        <Skeleton className="h-40 rounded-xl" />
      ) : rank.data ? (
        <div className="grid gap-3">
          {usage && <p className="text-xs text-muted-foreground">{usage}</p>}
          <ol className="grid gap-2">
            {results.map((r, i) => (
              <li key={r.index}>
                <Card className="flex-row items-center gap-4 p-4">
                  <span className="font-mono text-sm text-muted-foreground">#{i + 1}</span>
                  <p className="min-w-0 flex-1 text-sm break-words">{rank.data.sent[r.index] ?? `Document ${r.index}`}</p>
                  <span className="font-mono text-sm tabular-nums">{r.relevance_score.toFixed(4)}</span>
                </Card>
              </li>
            ))}
          </ol>
        </div>
      ) : (
        <EmptyState icon={ListOrdered} title="Rank documents against a query">
          <p>
            Sent to <code>{api.api.endpoint}</code>; the documents appear here best first, each with its score.
          </p>
        </EmptyState>
      )}
    </MediaLayout>
  );
}
