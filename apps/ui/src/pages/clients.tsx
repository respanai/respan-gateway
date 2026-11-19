import { Users } from "lucide-react";
import { CodeBlock } from "@/components/code-block";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { useClients } from "@/hooks/queries";
import type { ClientUsage } from "@/lib/api";
import { formatCost, formatCount, formatMs, formatPercent } from "@/lib/format";
import { describeError } from "@/lib/query-client";

const exampleConfig = `client_keys = [{ name = "app", key = "env:APP_KEY" }]`;

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="grid gap-0.5">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className="font-mono text-sm font-medium tabular">{value}</span>
    </div>
  );
}

function ClientCard({ client: c }: { client: ClientUsage }) {
  return (
    <Card className="min-w-0 gap-5">
      <CardHeader className="gap-1">
        <div className="flex min-w-0 items-center gap-2.5">
          <div className="flex size-9 shrink-0 items-center justify-center rounded-lg bg-brand-soft text-brand">
            <Users className="size-5" />
          </div>
          <CardTitle className="truncate font-mono text-base">{c.name}</CardTitle>
        </div>
      </CardHeader>
      <CardContent className="grid flex-1 content-end gap-4">
        <div className="grid grid-cols-3 gap-3 border-t pt-4">
          <Metric label="Requests" value={formatCount(c.requests)} />
          <Metric label="Errors" value={c.requests ? formatPercent(c.errors / c.requests) : "—"} />
          <Metric label="Avg latency" value={formatMs(c.avg_latency_ms)} />
        </div>
        <div className="grid grid-cols-3 gap-3 border-t pt-4">
          <Metric label="Tokens in" value={formatCount(c.input_tokens)} />
          <Metric label="Tokens out" value={formatCount(c.output_tokens)} />
          <Metric
            label="Cost"
            value={c.cost_usd !== undefined ? formatCost(c.cost_usd) : "—"}
          />
        </div>
      </CardContent>
    </Card>
  );
}

export function ClientsPage() {
  const { data, isPending, error } = useClients();
  return (
    <div className="grid gap-6">
      <PageHeader
        title="Clients"
        description={
          <>
            What each client key has asked of the gateway, and what it spent. The gateway
            records none without <code className="text-xs">client_keys</code>.
          </>
        }
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
          <p className="font-medium">Couldn't load clients</p>
          <p className="text-muted-foreground">{describeError(error)}</p>
        </div>
      ) : data?.length ? (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {data.map((c) => (
            <ClientCard key={c.name} client={c} />
          ))}
        </div>
      ) : (
        <EmptyState icon={Users} title="No client keys recorded">
          <p>Give each caller a named key in gateway.toml and restart the gateway:</p>
          <CodeBlock className="w-full text-left" code={exampleConfig} />
        </EmptyState>
      )}
    </div>
  );
}
