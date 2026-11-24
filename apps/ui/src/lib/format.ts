import type { Price } from "@/lib/api";

const compact = new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 1 });
const whole = new Intl.NumberFormat("en");

/** 1234 → "1,234"; 1234567 → "1.2M". */
export function formatCount(n: number): string {
  return Math.abs(n) < 100_000 ? whole.format(n) : compact.format(n);
}

export function formatPercent(ratio: number): string {
  if (!Number.isFinite(ratio) || ratio === 0) return "0%";
  if (ratio < 0.001) return "<0.1%";
  return `${(ratio * 100).toFixed(ratio < 0.1 ? 1 : 0)}%`;
}

/** 900 → "900 B"; 2048 → "2.0 KB"; 5 MiB → "5.0 MB". */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/** Tokens per second as "84 tok/s", or one decimal below ten. */
export function formatTps(tps: number | null | undefined): string {
  if (tps == null) return "—";
  return `${tps >= 100 ? Math.round(tps) : +tps.toFixed(1)} tok/s`;
}

/** Milliseconds as "840 ms", "2.4 s" or "1m 05s". */
export function formatMs(ms: number | null | undefined): string {
  if (ms == null) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
  const s = Math.round(ms / 1000);
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
}

/** Seconds of uptime as "3d 4h", "2h 15m", "4m 12s". */
export function formatUptime(seconds: number): string {
  const d = Math.floor(seconds / 86_400);
  const h = Math.floor((seconds % 86_400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = Math.floor(seconds % 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

const rtf = new Intl.RelativeTimeFormat("en", { numeric: "auto", style: "narrow" });

/** "12s ago", "5 min. ago", … relative to now. */
export function formatAgo(iso: string, now = Date.now()): string {
  const diff = (new Date(iso).getTime() - now) / 1000;
  const abs = Math.abs(diff);
  if (abs < 5) return "just now";
  if (abs < 60) return rtf.format(Math.round(diff), "second");
  if (abs < 3600) return rtf.format(Math.round(diff / 60), "minute");
  if (abs < 86_400) return rtf.format(Math.round(diff / 3600), "hour");
  return rtf.format(Math.round(diff / 86_400), "day");
}

/** Local wall-clock time with milliseconds: "14:03:22.481". */
export function formatClock(iso: string): string {
  const d = new Date(iso);
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

export function formatDateTime(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
  });
}

export function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, { dateStyle: "medium" });
}

/** US dollars: "$0.0042" for small sums, "$12.40" from a dollar up. */
export function formatCost(usd: number): string {
  if (usd === 0) return "$0.00";
  if (usd < 0.0001) return "<$0.0001";
  return `$${usd < 1 ? usd.toFixed(4) : usd.toFixed(2)}`;
}

/**
 * A price: per million tokens, input / output, such as "$0.15 / $0.6", then
 * `perMillion` if given; or per minute of audio for a model billed by the
 * second, "$0.006 / min".
 */
export function formatPrice(price: Price | null | undefined, perMillion = ""): string {
  if (!price) return "—";
  if (price.per_second != null && !price.input && !price.output) {
    return `$${+(price.per_second * 60).toFixed(4)} / min`;
  }
  return `$${+price.input.toFixed(3)} / $${+price.output.toFixed(3)}${perMillion}`;
}
