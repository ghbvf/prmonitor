import type {
  AppConfig,
  RemoteCapability,
  RemoteEntrypoint,
  RemoteRoute,
  RemoteTunnel,
} from "./types";

export function normalizeStringList(csv: string): string[] {
  return csv
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

export function makeRemoteRoute(capability: RemoteCapability = "local-api"): RemoteRoute {
  return {
    id: crypto.randomUUID(),
    name: capability === "terminal" ? "Terminal" : "Local API",
    path: capability === "terminal" ? "/terminal" : "/api",
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
