import { describe, expect, it } from "vitest";
import {
  addEntrypointToDraft,
  addRouteToEntrypoint,
  addTunnelToDraft,
  deleteEntrypointFromDraft,
  deleteRouteFromEntrypoint,
  deleteTunnelFromDraft,
  generateRemoteBearerToken,
  makeRemoteEntrypoint,
  makeRemoteRoute,
  makeRemoteTunnel,
  messagingCallbackEndpoints,
  normalizeStringList,
} from "./remoteAccessOps";
import {
  DEFAULT_CLI_TOOLS_CONFIG,
  DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
} from "./defaults";
import type { AppConfig, RemoteAccessRuntimeStatus } from "./types";

function draft(): AppConfig {
  return {
    projects: [],
    activeProjectId: "",
    webhookEnabled: false,
    webhookPort: 8787,
    webhookSecret: "",
    cliTools: { ...DEFAULT_CLI_TOOLS_CONFIG },
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
    localApiToken: "",
    outbox: { notificationTtlSecs: 7200 },
    notifications: { channels: [] },
    messaging: { integrations: [] },
    remoteAccess: { entrypoints: [], tunnels: [] },
    rules: [],
    reviewLifecycleNotifications: DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
  };
}

describe("makeRemoteEntrypoint / makeRemoteRoute / makeRemoteTunnel", () => {
  it("mints a disabled entrypoint with only the local-api route by default", () => {
    const entrypoint = makeRemoteEntrypoint();
    expect(entrypoint.id).toBeTruthy();
    expect(entrypoint.name).toBeTruthy();
    expect(entrypoint.enabled).toBe(false);
    expect(entrypoint.bindHost).toBe("127.0.0.1");
    expect(entrypoint.port).toBe(0);
    expect(entrypoint.sourcePolicy).toEqual({ mode: "loopback", allow: [] });
    expect(entrypoint.routes.map((route) => route.capability)).toEqual(["local-api"]);
    expect(entrypoint.routes[0].path).toBe("/api");
  });

  it("mints a terminal route disabled for write/create/admin by default", () => {
    const route = makeRemoteRoute("terminal");
    expect(route.id).toBeTruthy();
    expect(route.name).toBe("Terminal");
    expect(route.path).toBe("/terminal");
    expect(route.capability).toBe("terminal");
    expect(route.enabled).toBe(true);
    expect(route.authToken).toBe("");
    expect(route.terminalRead).toBe(true);
    expect(route.terminalWrite).toBe(false);
    expect(route.terminalCreate).toBe(false);
    expect(route.terminalAdmin).toBe(false);
  });

  it("mints a messaging route under the remote access entrypoint", () => {
    const route = makeRemoteRoute("messaging");
    expect(route.id).toBeTruthy();
    expect(route.name).toBe("Messaging");
    expect(route.path).toBe("/messaging");
    expect(route.capability).toBe("messaging");
    expect(route.enabled).toBe(true);
    expect(route.authToken).toBe("");
  });

  it("mints a LAN tunnel target with a blank entrypoint when none is supplied", () => {
    const tunnel = makeRemoteTunnel();
    expect(tunnel.id).toBeTruthy();
    expect(tunnel.name).toBeTruthy();
    expect(tunnel.mode).toBe("lan");
    expect(tunnel.targetEntrypointId).toBe("");
    expect(tunnel.bindHost).toBe("0.0.0.0");
    expect(tunnel.port).toBe(0);
    expect(tunnel.enabled).toBe(false);
  });

  it("gives each minted item a distinct id", () => {
    expect(makeRemoteEntrypoint().id).not.toBe(makeRemoteEntrypoint().id);
    expect(makeRemoteRoute().id).not.toBe(makeRemoteRoute().id);
    expect(makeRemoteTunnel().id).not.toBe(makeRemoteTunnel().id);
  });
});

describe("generateRemoteBearerToken", () => {
  it("generates a 32+ char URL-safe bearer token", () => {
    const token = generateRemoteBearerToken();
    expect(token.length).toBeGreaterThanOrEqual(32);
    expect(token).toMatch(/^[A-Za-z0-9_-]+$/);
  });

  it("generates fresh values", () => {
    expect(generateRemoteBearerToken()).not.toBe(generateRemoteBearerToken());
  });
});

describe("entrypoint draft ops", () => {
  it("appends and removes entrypoints by id", () => {
    const d = draft();
    const a = addEntrypointToDraft(d);
    const b = addEntrypointToDraft(d);
    expect(d.remoteAccess.entrypoints).toEqual([a, b]);

    expect(deleteEntrypointFromDraft(d, a.id)).toBe(true);
    expect(d.remoteAccess.entrypoints.map((entrypoint) => entrypoint.id)).toEqual([b.id]);
  });

  it("clears tunnel targets that point at a deleted entrypoint", () => {
    const d = draft();
    const a = addEntrypointToDraft(d);
    const b = addEntrypointToDraft(d);
    const pointsAtA = addTunnelToDraft(d);
    pointsAtA.targetEntrypointId = a.id;
    const pointsAtB = addTunnelToDraft(d);
    pointsAtB.targetEntrypointId = b.id;

    expect(deleteEntrypointFromDraft(d, a.id)).toBe(true);
    expect(pointsAtA.targetEntrypointId).toBe("");
    expect(pointsAtB.targetEntrypointId).toBe(b.id);
  });

  it("is a no-op for an unknown entrypoint id", () => {
    const d = draft();
    addEntrypointToDraft(d);
    expect(deleteEntrypointFromDraft(d, "missing")).toBe(false);
    expect(d.remoteAccess.entrypoints).toHaveLength(1);
  });
});

describe("route draft ops", () => {
  it("adds and deletes routes on an entrypoint", () => {
    const entrypoint = makeRemoteEntrypoint();
    const route = addRouteToEntrypoint(entrypoint, "terminal");
    expect(entrypoint.routes.map((item) => item.id)).toContain(route.id);

    expect(deleteRouteFromEntrypoint(entrypoint, route.id)).toBe(true);
    expect(entrypoint.routes.map((item) => item.id)).not.toContain(route.id);
  });

  it("is a no-op for an unknown route id", () => {
    const entrypoint = makeRemoteEntrypoint();
    expect(deleteRouteFromEntrypoint(entrypoint, "missing")).toBe(false);
    expect(entrypoint.routes).toHaveLength(1);
  });
});

describe("tunnel draft ops", () => {
  it("targets the first entrypoint when adding a tunnel", () => {
    const d = draft();
    const entrypoint = addEntrypointToDraft(d);
    const tunnel = addTunnelToDraft(d);
    expect(tunnel.targetEntrypointId).toBe(entrypoint.id);
    expect(d.remoteAccess.tunnels).toEqual([tunnel]);
  });

  it("removes a tunnel by id and no-ops unknown ids", () => {
    const d = draft();
    const a = addTunnelToDraft(d);
    const b = addTunnelToDraft(d);
    expect(deleteTunnelFromDraft(d, a.id)).toBe(true);
    expect(d.remoteAccess.tunnels.map((tunnel) => tunnel.id)).toEqual([b.id]);
    expect(deleteTunnelFromDraft(d, "missing")).toBe(false);
  });
});

describe("normalizeStringList", () => {
  it("splits on comma, trims, and drops empty entries", () => {
    expect(normalizeStringList("10.0.0.1, 192.168.0.0/16 ,, https://a")).toEqual([
      "10.0.0.1",
      "192.168.0.0/16",
      "https://a",
    ]);
  });

  it("returns an empty array for blank input", () => {
    expect(normalizeStringList("")).toEqual([]);
    expect(normalizeStringList(" ,  , ")).toEqual([]);
  });
});

describe("messagingCallbackEndpoints", () => {
  it("projects enabled messaging routes and integrations onto the tunnel public URL", () => {
    const entrypoint = makeRemoteEntrypoint();
    entrypoint.id = "entry-main";
    entrypoint.enabled = true;
    entrypoint.routes = [makeRemoteRoute("messaging")];
    entrypoint.routes[0].id = "route-msg";
    const tunnel = makeRemoteTunnel(entrypoint.id);
    tunnel.id = "tun";
    tunnel.enabled = true;
    tunnel.publicUrl = "https://bot.example.com/";
    const status: RemoteAccessRuntimeStatus = {
      entrypoints: [
        {
          id: entrypoint.id,
          bound: true,
          boundPort: 8788,
          state: "bound",
          message: "bound",
          routes: [
            {
              id: "route-msg",
              path: "/messaging/",
              capability: "messaging",
              enabled: true,
            },
          ],
        },
      ],
      tunnels: [
        {
          id: "tun",
          mode: "lan",
          targetEntrypointId: entrypoint.id,
          state: "running",
          publicUrl: "https://bot.example.com/",
          message: "running",
          logs: [],
        },
      ],
    };

    const endpoints = messagingCallbackEndpoints(status, [entrypoint], [tunnel], [
      {
        id: "feishu-main",
        name: "Feishu",
        kind: "feishu",
        enabled: true,
        appId: "",
        appSecret: "",
        verificationToken: "",
        encryptKey: "",
        botOpenId: "",
        allowedConversationIds: [],
        requireMention: true,
        timeoutSecs: 10,
      },
    ]);

    expect(endpoints).toEqual([
      {
        entrypointId: "entry-main",
        routeId: "route-msg",
        integrationId: "feishu-main",
        url: "https://bot.example.com/messaging/feishu/feishu-main",
      },
    ]);
  });

  it("falls back to the bound local entrypoint URL when no public tunnel is running", () => {
    const entrypoint = makeRemoteEntrypoint();
    entrypoint.id = "entry-local";
    entrypoint.bindHost = "0.0.0.0";
    const status: RemoteAccessRuntimeStatus = {
      entrypoints: [
        {
          id: "entry-local",
          bound: true,
          boundPort: 9010,
          state: "bound",
          message: "bound",
          routes: [{ id: "msg", path: "messaging", capability: "messaging", enabled: true }],
        },
      ],
      tunnels: [],
    };

    const endpoints = messagingCallbackEndpoints(status, [entrypoint], [], [
      {
        id: "fs",
        name: "Feishu",
        kind: "feishu",
        enabled: true,
        appId: "",
        appSecret: "",
        verificationToken: "",
        encryptKey: "",
        botOpenId: "",
        allowedConversationIds: [],
        requireMention: true,
        timeoutSecs: 10,
      },
    ]);

    expect(endpoints[0].url).toBe("http://127.0.0.1:9010/messaging/feishu/fs");
  });
});
