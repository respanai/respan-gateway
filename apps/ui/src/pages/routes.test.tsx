import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Explained, Provider, Routes } from "@/lib/api";
import { json, renderApp, serve } from "@/test/render";
import { RoutesPage } from "./routes";

const provider = (name: string): Provider => ({
  name,
  vendor: name,
  formats: [],
  kinds: ["chat"],
  base_url: `https://api.${name}.test`,
  credentials: "gateway",
  requests: 0,
  errors: 0,
  avg_latency_ms: null,
  input_tokens: 0,
  output_tokens: 0,
  input_tps: null,
  output_tps: null,
  cache_hit_rate: null,
});

const routes: Routes = {
  serve: [
    { format: "openai-chat", kind: "chat", endpoints: ["POST /v1/chat/completions"] },
    { format: "openai-embeddings", kind: "embedding", endpoints: ["POST /v1/embeddings"] },
  ],
  defaults: [
    { kind: "chat", provider: "openai" },
    { kind: "embedding", provider: "openai" },
  ],
  rules: [
    {
      name: "smart", glob: false, pick: "cheapest", needs: ["tools"], clients: ["ops"], kinds: [],
      fallback: true, hedge_ms: null, tries: 1,
      to: [
        {
          target: "groq/llama-3.3", provider: "groq", model: "llama-3.3", weight: 1,
          price: { input: 0.59, output: 0.79 },
          seen: { latency_ms: 240, error_rate: 0.5, calls: 4, failures: 3, cooling: true },
        },
        { target: "anthropic/claude", provider: "anthropic", model: "claude", weight: 1, price: null, seen: null },
      ],
    },
    {
      name: "llama-*", glob: true, pick: "first", needs: [], clients: [], kinds: [],
      fallback: true, hedge_ms: 200, tries: 1,
      to: [
        { target: "groq", provider: "groq", model: null, weight: 1, price: null, seen: null },
        { target: "deepseek/{model}-chat", provider: "deepseek", model: null, weight: 1, price: null, seen: null },
      ],
    },
  ],
};

const explained: Explained = {
  by: { type: "rule", name: "smart" },
  pick: "cheapest",
  mode: "fallback",
  to: [
    { provider: "anthropic", model: "claude", price: null, seen: null },
    { provider: "groq", model: "llama-3.3", price: { input: 0.59, output: 0.79 }, seen: null },
  ],
  passed: [{ candidate: "smart → openai", why: "lacks tools" }],
};

const api = (body: Routes, explain: (path: string) => Explained = () => explained) =>
  serve({
    "GET /ui/api/routes": () => json(body),
    "GET /ui/api/routes/explain": ({ path }) => json(explain(path)),
    "GET /ui/api/providers": () =>
      json({ providers: [provider("openai"), provider("anthropic"), provider("groq")] }),
  });

describe("RoutesPage", () => {
  it("shows each route's candidates with price and record, then defaults", async () => {
    api(routes);
    renderApp(<RoutesPage />);
    const smart = await screen.findByRole("list", { name: "Candidates of smart" });
    expect(within(smart).getByText("llama-3.3")).toBeInTheDocument();
    expect(within(smart).getByText("$0.59 / $0.79")).toBeInTheDocument();
    expect(within(smart).getByText("240 ms")).toBeInTheDocument();
    expect(within(smart).getByText("cooling")).toBeInTheDocument();
    expect(within(smart).getByText("no calls yet")).toBeInTheDocument();
    expect(screen.getByText("Cheapest")).toBeInTheDocument();
    expect(screen.getByText("needs tools")).toBeInTheDocument();
    expect(screen.getByText("for ops")).toBeInTheDocument();
    expect(screen.getByText(/raced, 200 ms apart/)).toBeInTheDocument();
    const llama = within(screen.getByRole("list", { name: "Candidates of llama-*" }));
    expect(llama.getByText("as asked")).toBeInTheDocument();
    expect(llama.getByText("{model}-chat")).toBeInTheDocument();
    expect(screen.getByText("/v1/embeddings")).toBeInTheDocument();
  });

  it("explains an empty configuration instead of showing blank tables", async () => {
    api({ serve: [], rules: [], defaults: [] });
    renderApp(<RoutesPage />);
    expect(await screen.findByText("No routes yet")).toBeInTheDocument();
    expect(document.querySelector("pre")).toHaveTextContent("[routes.smart]");
    expect(screen.getByText(/No default provider/)).toBeInTheDocument();
    expect(screen.getByText(/No endpoints configured/)).toBeInTheDocument();
  });

  it("says the routes could not be loaded, and does not claim there are none", async () => {
    serve({});
    renderApp(<RoutesPage />);
    expect(await screen.findByText("Couldn't load routes")).toBeInTheDocument();
    expect(screen.queryByText("No routes yet")).not.toBeInTheDocument();
  });

  describe("tester", () => {
    it("asks the router where a model goes and shows the order and what was passed over", async () => {
      const asked: string[] = [];
      api(routes, (path) => {
        asked.push(path);
        return explained;
      });
      renderApp(<RoutesPage />);
      await userEvent.type(await screen.findByLabelText("Model", { exact: true }), "smart");
      const order = await screen.findByRole("list", { name: "Where it goes" });
      const rows = within(order).getAllByRole("listitem");
      expect(rows[0]).toHaveTextContent("anthropic");
      expect(rows[1]).toHaveTextContent("groq");
      expect(screen.getByText("route smart")).toBeInTheDocument();
      expect(screen.getByText(/in this order until one answers/)).toBeInTheDocument();
      expect(screen.getByText("lacks tools", { exact: false })).toBeInTheDocument();
      expect(asked.at(-1)).toContain("model=smart");
      expect(asked.at(-1)).toContain("kind=chat");
    });

    it("says when no provider serves the kind", async () => {
      api(routes, () => ({ by: null }));
      renderApp(<RoutesPage />);
      await userEvent.type(await screen.findByLabelText("Model", { exact: true }), "x");
      expect(await screen.findByText(/No provider serves chat requests/)).toBeInTheDocument();
    });

    it("offers the routes' names as examples and fills one on a click", async () => {
      api(routes);
      renderApp(<RoutesPage />);
      const examples = await screen.findByRole("group", { name: "Example models" });
      await userEvent.click(within(examples).getByRole("button", { name: "llama-x" }));
      expect(screen.getByLabelText("Model", { exact: true })).toHaveValue("llama-x");
    });
  });
});
