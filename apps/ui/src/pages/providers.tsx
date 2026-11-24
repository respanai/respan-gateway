import { Activity, KeyRound, Server } from "lucide-react";
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
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useProviderTps, useProviders } from "@/hooks/queries";
import type { Provider, ProviderTpsMetric } from "@/lib/api";
import { formatCount, formatMs, formatPercent } from "@/lib/format";
import { describeError } from "@/lib/query-client";

const exampleConfig = `[providers.openai]
vendor = "openai"
key = "env:OPENAI_API_KEY"`;

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="grid gap-0.5">
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
      </CardContent>
    </Card>
  );
}

function formatRate(rate: number | null): string {
  return rate == null || !Number.isFinite(rate) ? "—" : `${rate.toFixed(rate >= 100 ? 0 : 1)} tok/s`;
}

function formatCacheHit(metric: ProviderTpsMetric): string {
  if (metric.cache_hit_rate == null) return "—";
  const coverage = `${formatCount(metric.cache_known_calls)}/${formatCount(metric.cache_total_calls)}`;
  return `${formatPercent(metric.cache_hit_rate)} (${coverage})`;
}

function ProviderTpsTable({ metrics }: { metrics: ProviderTpsMetric[] }) {
  return (
    <section className="grid gap-3 border-t pt-6" aria-labelledby="provider-tps-title">
      <div className="flex items-start gap-2">
        <Activity className="mt-0.5 size-4 text-muted-foreground" aria-hidden="true" />
        <div>
          <h2 id="provider-tps-title" className="font-medium">Provider call metrics</h2>
          <p className="text-sm text-muted-foreground">
            Gateway observations by provider and model. Rates include network and provider queue time.
          </p>
        </div>
      </div>
      <div className="overflow-hidden rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Provider / model</TableHead>
              <TableHead>Calls</TableHead>
              <TableHead>Success / failed / cancelled</TableHead>
              <TableHead>Usage</TableHead>
              <TableHead>Timed input / output</TableHead>
              <TableHead>Avg context</TableHead>
              <TableHead>Input TPS</TableHead>
              <TableHead>Output TPS</TableHead>
              <TableHead>Cache hit</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {metrics.map((metric) => (
              <TableRow key={`${metric.provider}/${metric.model}`}>
                <TableCell>
                  <div className="grid min-w-44 gap-0.5">
                    <span className="font-medium">{metric.provider}</span>
                    <span className="truncate font-mono text-xs text-muted-foreground" title={metric.model}>
                      {metric.model}
                    </span>
                  </div>
                </TableCell>
                <TableCell className="font-mono tabular">{formatCount(metric.calls)}</TableCell>
                <TableCell className="font-mono tabular">
                  {formatCount(metric.successful_calls)} / {formatCount(metric.failed_calls - metric.abandoned_calls)} / {formatCount(metric.abandoned_calls)}
                </TableCell>
                <TableCell className="font-mono tabular">{formatCount(metric.usage_calls)}</TableCell>
                <TableCell className="font-mono tabular">
                  {formatCount(metric.input_timed_calls)} / {formatCount(metric.output_timed_calls)}
                </TableCell>
                <TableCell className="font-mono tabular">{metric.average_context_tokens == null ? "—" : `${formatCount(Math.round(metric.average_context_tokens))} tok`}</TableCell>
                <TableCell className="font-mono tabular">{formatRate(metric.input_tps)}</TableCell>
                <TableCell className="font-mono tabular">{formatRate(metric.output_tps)}</TableCell>
                <TableCell className="font-mono tabular">{formatCacheHit(metric)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
    </section>
  );
}

export function ProvidersPage() {
  const { data, isPending, error } = useProviders();
  const tps = useProviderTps();
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
      {tps.isPending ? (
        <Skeleton className="h-40 rounded-lg" />
      ) : tps.error && !tps.data ? (
        <div role="alert" className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm">
          <p className="font-medium">Couldn't load provider call metrics</p>
          <p className="text-muted-foreground">{describeError(tps.error)}</p>
        </div>
      ) : tps.data?.length ? (
        <ProviderTpsTable metrics={tps.data} />
      ) : (
        <section className="grid gap-1 border-t pt-6" aria-labelledby="provider-tps-title">
          <h2 id="provider-tps-title" className="font-medium">Provider call metrics</h2>
          <p className="text-sm text-muted-foreground">No provider calls observed yet.</p>
        </section>
      )}
    </div>
  );
}
