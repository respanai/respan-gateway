import { KeyRound, Server } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router";
import { CodeBlock } from "@/components/code-block";
import { CopyButton } from "@/components/copy-button";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { ProviderIcon } from "@/components/provider-icon";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { useProviders } from "@/hooks/queries";
import type { Provider } from "@/lib/api";
import { formatCount, formatMs, formatPercent, formatTps } from "@/lib/format";
import { describeError } from "@/lib/query-client";

const exampleConfig = `[providers.openai]
vendor = "openai"
key = "env:OPENAI_API_KEY"`;

function Metric({ label, value, title }: { label: string; value: string; title?: string }) {
  return (
    <div className="grid gap-0.5" title={title}>
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className="font-mono text-sm font-medium tabular">{value}</span>
    </div>
  );
}

function Chips({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid gap-1.5">
      <span className="text-[11px] font-medium tracking-wide text-muted-foreground uppercase">{label}</span>
      <div className="flex flex-wrap gap-1.5">{children}</div>
    </div>
  );
}

function ProviderCard({ provider: p }: { provider: Provider }) {
  const gatewayKey = p.credentials === "gateway";
  return (
    <Card className="min-w-0 gap-5">
      <CardHeader className="gap-1">
        <div className="flex items-start justify-between gap-3">
          <div className="flex min-w-0 items-center gap-2.5">
            <ProviderIcon name={p.name} vendor={p.vendor} baseUrl={p.base_url} />
            <div className="min-w-0">
              <CardTitle className="truncate text-base">{p.name}</CardTitle>
              {p.vendor && p.vendor !== p.name && (
                <div className="truncate text-xs text-muted-foreground">{p.vendor}</div>
              )}
            </div>
          </div>
          {!gatewayKey && (
            <Badge
              variant="outline"
              className="shrink-0 font-normal text-muted-foreground"
              title="No key in the config: it gets none from the gateway, only each caller's own credentials (Authorization, x-api-key or x-goog-api-key) if they bring any beyond their gateway key"
            >
              <KeyRound /> No key
            </Badge>
          )}
        </div>
        <div className="flex min-w-0 items-center gap-1 pt-1">
          <p className="truncate font-mono text-xs text-muted-foreground" title={p.base_url}>
            {p.base_url}
          </p>
          {p.base_url && (
            <CopyButton value={p.base_url} label="Copy base URL" className="size-6 shrink-0" />
          )}
        </div>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col gap-4">
        <Chips label="Serves">
          {p.kinds.map((k) => (
            <Badge key={k} className="bg-brand-soft font-normal text-brand">
              {k}
            </Badge>
          ))}
        </Chips>
        <Chips label="Formats">
          {p.formats.map((f) => (
            <Badge key={f} variant="secondary" className="font-mono text-[11px] font-normal">
              {f}
            </Badge>
          ))}
        </Chips>
        <div className="mt-auto grid grid-cols-3 gap-3 border-t pt-4">
          <Metric label="Requests" value={formatCount(p.requests)} />
          <Metric label="Errors" value={p.requests ? formatPercent(p.errors / p.requests) : "—"} />
          <Metric label="Avg latency" value={formatMs(p.avg_latency_ms)} />
        </div>
        <div className="grid grid-cols-3 gap-3">
          <Metric
            label="Input tps"
            value={formatTps(p.input_tps)}
            title="Input tokens per second of each call's wait for its first token, as the gateway saw it: encoding, network, queueing and cache work included. Only streaming answers with usage are sampled."
          />
          <Metric
            label="Output tps"
            value={formatTps(p.output_tps)}
            title="Output tokens per second after the first token, as the gateway saw it. Only streaming answers with usage are sampled."
          />
          <Metric
            label="Cache hit"
            value={p.cache_hit_rate == null ? "—" : formatPercent(p.cache_hit_rate)}
            title="Of the input tokens successful calls reported, the share read from the provider's cache. A provider that does not report cache counts reads as zero."
          />
        </div>
      </CardContent>
    </Card>
  );
}

export function ProvidersPage() {
  const { data, isPending, error } = useProviders();
  return (
    <div className="grid gap-6">
      <PageHeader
        title="Providers"
        description={<>Upstreams the gateway can call, what they speak and how they're doing. Tool servers are under <Link to="/tools" className="underline">Tools</Link>.</>}
      />
      {isPending ? (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {Array.from({ length: 3 }, (_, i) => (
            <Skeleton key={i} className="h-52 rounded-xl" />
          ))}
        </div>
      ) : error && !data ? (
        <div
          role="alert"
          className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm"
        >
          <p className="font-medium">Couldn't load providers</p>
          <p className="text-muted-foreground">{describeError(error)}</p>
        </div>
      ) : data?.length ? (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {data.map((p) => (
            <ProviderCard key={p.name} provider={p} />
          ))}
        </div>
      ) : (
        <EmptyState icon={Server} title="No providers configured">
          <p>Add a provider to gateway.toml and restart the gateway:</p>
          <CodeBlock className="w-full text-left" code={exampleConfig} />
        </EmptyState>
      )}
    </div>
  );
}
