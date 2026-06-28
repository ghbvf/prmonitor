import type {
  AppConfig,
  MessagingIntegration,
  RemoteAccessRuntimeStatus,
  RemoteCapability,
  RemoteEntrypoint,
  RemoteRoute,
  RemoteTunnel,
} from "./types";

export interface MessagingCallbackEndpoint {
  entrypointId: string;
  routeId: string;
  integrationId: string;
  url: string;
}

export function normalizeStringList(csv: string): string[] {
  return csv
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

export function makeRemoteRoute(capability: RemoteCapability = "local-api"): RemoteRoute {
  const defaults: Record<RemoteCapability, { name: string; path: string }> = {
    terminal: { name: "Terminal", path: "/terminal" },
    "local-api": { name: "Local API", path: "/api" },
    messaging: { name: "Messaging", path: "/messaging" },
  };
  return {
    id: crypto.randomUUID(),
    name: defaults[capability].name,
    path: defaults[capability].path,
    capability,
    enabled: true,
    authToken: "",
    terminalRead: capability === "terminal",
    terminalWrite: false,
    terminalCreate: false,
    terminalAdmin: false,
  };
}

export function makeRemoteEntrypoint(): RemoteEntrypoint {
  return {
    id: crypto.randomUUID(),
    name: "新入口",
    bindHost: "127.0.0.1",
    port: 0,
    enabled: false,
    sourcePolicy: { mode: "loopback", allow: [] },
    allowedOrigins: [],
    trustedProxies: [],
    routes: [makeRemoteRoute("local-api")],
  };
}

export function makeRemoteTunnel(targetEntrypointId = ""): RemoteTunnel {
  return {
    id: crypto.randomUUID(),
    name: "新隧道",
    mode: "lan",
    targetEntrypointId,
    bindHost: "0.0.0.0",
    port: 0,
    command: "",
    publicUrl: "",
    enabled: false,
  };
}

export function addEntrypointToDraft(draft: AppConfig): RemoteEntrypoint {
  const entrypoint = makeRemoteEntrypoint();
  draft.remoteAccess.entrypoints.push(entrypoint);
  return entrypoint;
}

export function deleteEntrypointFromDraft(draft: AppConfig, id: string): boolean {
  const idx = draft.remoteAccess.entrypoints.findIndex((entrypoint) => entrypoint.id === id);
  if (idx === -1) return false;
  draft.remoteAccess.entrypoints.splice(idx, 1);
  for (const tunnel of draft.remoteAccess.tunnels) {
    if (tunnel.targetEntrypointId === id) tunnel.targetEntrypointId = "";
  }
  return true;
}

export function addRouteToEntrypoint(
  entrypoint: RemoteEntrypoint,
  capability: RemoteCapability,
): RemoteRoute {
  const route = makeRemoteRoute(capability);
  entrypoint.routes.push(route);
  return route;
}

export function deleteRouteFromEntrypoint(entrypoint: RemoteEntrypoint, id: string): boolean {
  const idx = entrypoint.routes.findIndex((route) => route.id === id);
  if (idx === -1) return false;
  entrypoint.routes.splice(idx, 1);
  return true;
}

export function addTunnelToDraft(draft: AppConfig): RemoteTunnel {
  const tunnel = makeRemoteTunnel(draft.remoteAccess.entrypoints[0]?.id ?? "");
  draft.remoteAccess.tunnels.push(tunnel);
  return tunnel;
}

export function deleteTunnelFromDraft(draft: AppConfig, id: string): boolean {
  const idx = draft.remoteAccess.tunnels.findIndex((tunnel) => tunnel.id === id);
  if (idx === -1) return false;
  draft.remoteAccess.tunnels.splice(idx, 1);
  return true;
}

export function messagingCallbackEndpoints(
  status: RemoteAccessRuntimeStatus,
  configuredEntrypoints: RemoteEntrypoint[],
  configuredTunnels: RemoteTunnel[],
  integrations: MessagingIntegration[],
): MessagingCallbackEndpoint[] {
  const enabledIntegrations = integrations.filter((integration) => integration.enabled);
  if (enabledIntegrations.length === 0) return [];

  const configuredById = new Map(configuredEntrypoints.map((entrypoint) => [entrypoint.id, entrypoint]));
  const tunnelBaseByEntrypoint = new Map<string, string>();
  for (const tunnel of status.tunnels) {
    if (tunnel.state !== "running" || !tunnel.publicUrl) continue;
    const configured = configuredTunnels.find((item) => item.id === tunnel.id);
    if (configured && !configured.enabled) continue;
    tunnelBaseByEntrypoint.set(tunnel.targetEntrypointId, trimTrailingSlash(tunnel.publicUrl));
  }

  const endpoints: MessagingCallbackEndpoint[] = [];
  for (const entrypoint of status.entrypoints) {
    const configured = configuredById.get(entrypoint.id);
    const base =
      tunnelBaseByEntrypoint.get(entrypoint.id) ??
      localEntrypointBase(configured, entrypoint.boundPort ?? null);
    if (!base) continue;
    for (const route of entrypoint.routes) {
      if (!route.enabled || route.capability !== "messaging") continue;
      const routePath = normalizeRoutePath(route.path);
      for (const integration of enabledIntegrations) {
        endpoints.push({
          entrypointId: entrypoint.id,
          routeId: route.id,
          integrationId: integration.id,
          url: `${base}${routePath}/${integration.kind}/${encodeURIComponent(integration.id)}`,
        });
      }
    }
  }
  return endpoints;
}

function localEntrypointBase(entrypoint: RemoteEntrypoint | undefined, boundPort: number | null): string | null {
  if (!entrypoint || boundPort == null) return null;
  const host = entrypoint.bindHost === "0.0.0.0" ? "127.0.0.1" : entrypoint.bindHost;
  return `http://${host}:${boundPort}`;
}

function normalizeRoutePath(path: string): string {
  const trimmed = path.trim();
  const withSlash = trimmed.startsWith("/") ? trimmed : `/${trimmed}`;
  return trimTrailingSlash(withSlash);
}

function trimTrailingSlash(value: string): string {
  return value.replace(/\/+$/, "");
}
