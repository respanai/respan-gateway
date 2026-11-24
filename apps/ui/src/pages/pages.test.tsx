import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { Provider, RequestRecord } from "@/lib/api";
import { ProvidersPage } from "./providers";
import { RequestsPage } from "./requests";
import { RoutesPage } from "./routes";

function serve(routes: Record<string, unknown>) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input).split("?")[0];
      return path in routes
        ? new Response(JSON.stringify(routes[path]), { status: 200 })
        : new Response("{}", { status: 404 });
    }),
  );
}

function renderPage(page: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <TooltipProvider>
        <MemoryRouter>{page}</MemoryRouter>
      </TooltipProvider>
    </QueryClientProvider>,
  );
}

const provider: Provider = {
  name: "anthropic",
  vendor: "anthropic",
  formats: ["anthropic-messages"],
  kinds: ["chat"],
  base_url: "https://api.anthropic.com",
  credentials: "client",
  requests: 40,
  errors: 2,
  avg_latency_ms: 1250,
  input_tokens: 123_456,
  output_tokens: 7_890,
  input_tps: 123.4,
  output_tps: 84.6,
  cache_hit_rate: 0.65,
};

const record: RequestRecord = {
  id: "r_000001",
  started_at: "2026-10-10T12:00:00.000Z",
  method: "POST",
  path: "/v1/chat/completions",
  format: "openai-chat",
  kind: "chat",
  model: "claude-sonnet-4-5",
  provider: "anthropic",
  status: 200,
  duration_ms: 812,
  ttfb_ms: 230,
  stream: true,
  usage: { input_tokens: 12, output_tokens: 40 },
  error: null,
};

describe("ProvidersPage", () => {
  it("renders each provider with its formats, credentials and stats", async () => {
    const keyed = { ...provider, name: "keyed", credentials: "gateway" as const, requests: 0, errors: 0, avg_latency_ms: null, input_tokens: 0, output_tokens: 0, input_tps: null, output_tps: null, cache_hit_rate: null };
    serve({ "/ui/api/providers": { providers: [provider, keyed] } });
    renderPage(<ProvidersPage />);
    expect(await screen.findAllByText("anthropic-messages")).toHaveLength(2);
    // Only the provider without a key of its own says whose it uses.
    expect(screen.getAllByText("No key")).toHaveLength(1);
    expect(screen.getByText("5.0%")).toBeInTheDocument();
    expect(screen.getByText("1.25 s")).toBeInTheDocument();
    expect(screen.getByText("123 tok/s")).toBeInTheDocument();
    expect(screen.getByText("84.6 tok/s")).toBeInTheDocument();
    expect(screen.getByText("65%")).toBeInTheDocument();
    // A provider without samples shows a dash, not a zero.
    expect(screen.getAllByText("—")).toHaveLength(5);
  });

  it("teaches how to add a provider when none are configured", async () => {
    serve({ "/ui/api/providers": { providers: [] } });
    renderPage(<ProvidersPage />);
    expect(await screen.findByText("No providers configured")).toBeInTheDocument();
  });

  it("shows an error, not an empty list, when loading fails", async () => {
    serve({});
    renderPage(<ProvidersPage />);
    expect(await screen.findByText("Couldn't load providers")).toBeInTheDocument();
    expect(screen.queryByText("No providers configured")).not.toBeInTheDocument();
  });
});

describe("RequestsPage", () => {
  it("lists requests from the admin API", async () => {
    serve({ "/ui/api/requests": { requests: [record] } });
    renderPage(<RequestsPage />);
    expect(await screen.findByText("claude-sonnet-4-5")).toBeInTheDocument();
    expect(screen.getByText("812 ms")).toBeInTheDocument();
  });

  it("says why a request failed in the list, and when a stream's first byte came", async () => {
    serve({
      "/ui/api/requests": {
        requests: [
          { ...record, id: "r_2", stream: false, status: 404, error: "the provider returned 404, with no body" },
          { ...record, id: "r_3", stream: true, ttfb_ms: 230 },
        ],
      },
    });
    renderPage(<RequestsPage />);
    expect(await screen.findByText("the provider returned 404, with no body")).toBeInTheDocument();
    expect(screen.getByText(/first 230 ms/)).toBeInTheDocument();
  });

  it("shows a copyable first request when the gateway has seen none", async () => {
    serve({ "/ui/api/requests": { requests: [] } });
    renderPage(<RequestsPage />);
    expect(await screen.findByText("No requests yet")).toBeInTheDocument();
    expect(screen.getByText(/curl .*\/v1\/chat\/completions/)).toBeInTheDocument();
  });

  it("shows an error, not an empty filter result, when loading fails", async () => {
    serve({});
    renderPage(<RequestsPage />);
    expect(await screen.findByText("Couldn't load requests")).toBeInTheDocument();
    expect(screen.queryByText("No requests match")).not.toBeInTheDocument();
  });
});

describe("RoutesPage", () => {
  it("tells where a typed model would go", async () => {
    serve({
      "/ui/api/providers": { providers: [provider] },
      "/ui/api/routes": {
        serve: [{ format: "anthropic-messages", kind: "chat", endpoints: ["POST /v1/messages"] }],
        rules: [],
        defaults: [{ kind: "chat", provider: "anthropic" }],
      },
      "/ui/api/routes/explain": {
        by: { type: "listed" },
        pick: null,
        mode: "single",
        to: [{ provider: "anthropic", model: "claude-x", price: null, seen: null }],
        passed: [],
      },
    });
    renderPage(<RoutesPage />);
    await userEvent.type(await screen.findByLabelText("Model", { exact: true }), "claude-x");
    expect(await screen.findByText("the provider's model list")).toBeInTheDocument();
    expect(screen.getByText("POST")).toBeInTheDocument();
  });
});

describe("RequestsPage filters", () => {
  it("narrows the list by status pill and counts each", async () => {
    serve({
      "/ui/api/requests": {
        requests: [record, { ...record, id: "r_2", model: "broken-model", status: 500 }],
      },
    });
    renderPage(<RequestsPage />);
    await screen.findByText("broken-model");
    await userEvent.click(screen.getByRole("button", { name: /Failed\s*1/ }));
    expect(screen.queryByText("claude-sonnet-4-5")).not.toBeInTheDocument();
    expect(screen.getByText("broken-model")).toBeInTheDocument();
  });

  it("narrows the list to a model searched for, and back to all", async () => {
    serve({
      "/ui/api/requests": {
        requests: [record, { ...record, id: "r_2", model: "broken-model", status: 500 }],
      },
    });
    renderPage(<RequestsPage />);
    await screen.findByText("broken-model");
    const filter = screen.getByRole("combobox", { name: "Model" });
    expect(filter).toHaveTextContent("All models");
    await userEvent.click(filter);
    await userEvent.type(screen.getByPlaceholderText("Search 2 models"), "broken");
    await userEvent.click(screen.getByRole("option", { name: "broken-model" }));
    expect(screen.queryByText("claude-sonnet-4-5")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("combobox", { name: "Model" }));
    await userEvent.click(screen.getByRole("option", { name: "All models" }));
    expect(await screen.findByText("claude-sonnet-4-5")).toBeInTheDocument();
  });
});
