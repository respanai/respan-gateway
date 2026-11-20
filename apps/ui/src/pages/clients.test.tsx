import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ClientUsage } from "@/lib/api";
import { json, renderApp, serve } from "@/test/render";
import { ClientsPage } from "./clients";

const client: ClientUsage = {
  name: "app",
  requests: 40,
  errors: 2,
  avg_latency_ms: 1250,
  input_tokens: 120_000,
  output_tokens: 30_000,
  cost_usd: 0.42,
};

describe("ClientsPage", () => {
  it("shows each client key's requests, latency, tokens and cost", async () => {
    serve({ "GET /ui/api/clients": () => json({ clients: [client] }) });
    renderApp(<ClientsPage />);
    expect(await screen.findByText("app")).toBeInTheDocument();
    expect(screen.getByText("40")).toBeInTheDocument();
    expect(screen.getByText("5.0%")).toBeInTheDocument();
    expect(screen.getByText("1.25 s")).toBeInTheDocument();
    expect(screen.getByText("120K")).toBeInTheDocument();
    expect(screen.getByText("$0.4200")).toBeInTheDocument();
  });

  it("shows a dash for the cost when nothing is priced", async () => {
    serve({ "GET /ui/api/clients": () => json({ clients: [{ ...client, cost_usd: undefined }] }) });
    renderApp(<ClientsPage />);
    expect(await screen.findByText("app")).toBeInTheDocument();
    const cost = screen.getByText("Cost").closest("div")!;
    expect(cost).toHaveTextContent("—");
  });

  it("teaches how to add a client key when the gateway records none", async () => {
    serve({ "GET /ui/api/clients": () => json({ clients: [] }) });
    renderApp(<ClientsPage />);
    expect(await screen.findByText("No client keys recorded")).toBeInTheDocument();
    expect(screen.getByText(/client_keys = \[/)).toBeInTheDocument();
  });

  it("shows an error, not an empty list, when loading fails", async () => {
    serve({});
    renderApp(<ClientsPage />);
    expect(await screen.findByText("Couldn't load clients")).toBeInTheDocument();
    expect(screen.queryByText("No client keys recorded")).not.toBeInTheDocument();
  });
});
