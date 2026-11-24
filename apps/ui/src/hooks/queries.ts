import { useQuery } from "@tanstack/react-query";
import { useSyncExternalStore } from "react";
import { api, getGatewayKey, subscribeGatewayKey, type Kind } from "@/lib/api";

export const queryKeys = {
  overview: ["overview"] as const,
  providers: ["providers"] as const,
  providerTps: ["provider-tps"] as const,
  routes: ["routes"] as const,
  requests: ["requests"] as const,
  request: (id: string) => ["request", id] as const,
  models: ["models"] as const,
  config: ["config"] as const,
};

export function useOverview() {
  return useQuery({
    queryKey: queryKeys.overview,
    queryFn: ({ signal }) => api.overview(signal),
    refetchInterval: 5000,
  });
}

export function useConfig() {
  return useQuery({
    queryKey: queryKeys.config,
    queryFn: ({ signal }) => api.config(signal),
  });
}

export function useProviders() {
  return useQuery({
    queryKey: queryKeys.providers,
    queryFn: ({ signal }) => api.providers(signal),
    refetchInterval: 10_000,
  });
}

export function useProviderTps() {
  return useQuery({
    queryKey: queryKeys.providerTps,
    queryFn: ({ signal }) => api.providerTps(signal),
    refetchInterval: 10_000,
  });
}

/** The MCP servers with who uses them; refreshed for their counts. */
export function useMcpOverview() {
  return useQuery({
    queryKey: ["mcp-overview"] as const,
    queryFn: ({ signal }) => api.mcpOverview(signal),
    refetchInterval: 10_000,
  });
}

/**
 * The tools server `name` lists, asked only when `enabled`: asking may start
 * a stdio server. The gateway keeps a list a minute; `refresh` asks anew.
 */
export function useMcpTools(name: string, enabled: boolean) {
  return useQuery({
    queryKey: ["mcp-tools", name] as const,
    queryFn: ({ signal }) => api.mcpTools(name, false, signal),
    enabled,
    staleTime: 60_000,
    retry: false,
  });
}

/** The routes; `live` refetches them for each candidate's latest record. */
export function useRoutes(live = false) {
  return useQuery({
    queryKey: queryKeys.routes,
    queryFn: ({ signal }) => api.routes(signal),
    refetchInterval: live ? 5000 : false,
  });
}

/** Where a request for `model` of `kind` would go now; idle without a model. */
export function useExplain(model: string, kind: string, client?: string) {
  return useQuery({
    queryKey: ["explain", model, kind, client ?? ""] as const,
    queryFn: ({ signal }) => api.explain({ model, kind, client }, signal),
    enabled: model.length > 0,
    placeholderData: (previous) => previous,
  });
}

/** Every model /v1/models lists, routes among them. */
export function useAllModels() {
  return useQuery({
    queryKey: queryKeys.models,
    queryFn: ({ signal }) => api.models(signal),
    staleTime: 60_000,
  });
}

/**
 * Models from /v1/models that serve `kind`. `kind` on a model is an optional
 * extension: when the gateway reports none, every model is taken to be a chat
 * model, as /v1/models lists for OpenAI.
 */
export function useModels(kind: Kind) {
  return useQuery({
    queryKey: queryKeys.models,
    queryFn: ({ signal }) => api.models(signal),
    staleTime: 60_000,
    select: (models) =>
      models.some((m) => m.kind !== undefined)
        ? models.filter((m) => m.kind === kind)
        : kind === "chat"
          ? models
          : [],
  });
}

export function useGatewayKey(): string | null {
  return useSyncExternalStore(subscribeGatewayKey, getGatewayKey, () => null);
}
