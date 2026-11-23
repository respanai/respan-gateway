import type { ComponentType } from "react";
import { createBrowserRouter } from "react-router";
import { AppLayout } from "@/components/layout/app-layout";
import { NotFoundPage } from "@/pages/not-found";

/** A route whose page loads on first visit, so the first load carries only the shell. */
const page = <T extends Record<string, ComponentType>>(load: () => Promise<T>, name: keyof T) => ({
  lazy: () => load().then((m) => ({ Component: m[name] })),
});

export const routes = [
  {
    element: <AppLayout />,
    children: [
      { index: true, ...page(() => import("@/pages/overview"), "OverviewPage") },
      {
        path: "requests",
        ...page(() => import("@/pages/requests"), "RequestsPage"),
        children: [{ path: ":id", ...page(() => import("@/pages/request-detail"), "RequestDetailSheet") }],
      },
      { path: "clients", ...page(() => import("@/pages/clients"), "ClientsPage") },
      { path: "playground", ...page(() => import("@/pages/playground"), "PlaygroundPage") },
      { path: "providers", ...page(() => import("@/pages/providers"), "ProvidersPage") },
      { path: "models", ...page(() => import("@/pages/models"), "ModelsPage") },
      { path: "tools", ...page(() => import("@/pages/tools"), "ToolsPage") },
      { path: "routes", ...page(() => import("@/pages/routes"), "RoutesPage") },
      { path: "config", ...page(() => import("@/pages/config"), "ConfigPage") },
      { path: "settings", ...page(() => import("@/pages/settings"), "SettingsPage") },
      { path: "*", element: <NotFoundPage /> },
    ],
  },
];

export function createRouter() {
  return createBrowserRouter(routes, { basename: "/ui" });
}
