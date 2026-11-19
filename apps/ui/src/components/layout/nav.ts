import {
  Activity,
  FileCode,
  Blocks,
  Boxes,
  FlaskConical,
  LayoutDashboard,
  Route,
  Server,
  Settings,
  Users,
  type LucideIcon,
} from "lucide-react";

export interface NavItem {
  to: string;
  label: string;
  icon: LucideIcon;
}

export const monitor: NavItem[] = [
  { to: "/", label: "Overview", icon: LayoutDashboard },
  { to: "/requests", label: "Requests", icon: Activity },
  { to: "/clients", label: "Clients", icon: Users },
];

export const build: NavItem[] = [
  { to: "/playground", label: "Playground", icon: FlaskConical },
  { to: "/providers", label: "Providers", icon: Server },
  { to: "/models", label: "Models", icon: Boxes },
  { to: "/tools", label: "Tools", icon: Blocks },
  { to: "/routes", label: "Routes", icon: Route },
  { to: "/config", label: "Config", icon: FileCode },
];

export const settings: NavItem = { to: "/settings", label: "Settings", icon: Settings };

/** Every page, in the sidebar's order. */
export const pages: NavItem[] = [...monitor, ...build, settings];

/** Whether `pathname` is on the page at `to`: the overview only at `/`. */
export function onPage(to: string, pathname: string): boolean {
  return to === "/" ? pathname === "/" : pathname.startsWith(to);
}

/** The page `pathname` is on, if any. */
export function pageAt(pathname: string): NavItem | undefined {
  return pages.find(({ to }) => onPage(to, pathname));
}
