// Locks the add/delete shape of the remote-access (AB#1064) draft ops. Unlike projectOps
// there is NO active-id invariant — listeners/tunnels are flat lists — so the contract is
// just: add appends a default-shaped item with a fresh id; delete removes by stable id and
// is a no-op for an unknown id. Pure (mutates a plain AppConfig) → no Pinia/mocks.
import { describe, it, expect } from "vitest";
import {
  makeListener,
  makeTunnel,
  addListenerToDraft,
  deleteListenerFromDraft,
  addTunnelToDraft,
  deleteTunnelFromDraft,
  updateListenerField,
  updateTunnelField,
  normalizeAllowedOrigins,
} from "./remoteAccessOps";
import type { AppConfig } from "./types";

// A minimal draft carrying empty remote-access lists; the other AppConfig fields are
// irrelevant to these ops but required for the type.
function draft(): AppConfig {
  return {
    projects: [],
    activeProjectId: "",
    webhookEnabled: false,
    webhookPort: 8787,
    webhookSecret: "",
    cloudflaredBin: "cloudflared",
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
    localApiToken: "",
    outbox: { notificationTtlSecs: 7200 },
    notifications: { channels: [] },
    listeners: [],
    tunnels: [],
  };
}

describe("makeListener / makeTunnel", () => {
  it("mints a default-shaped listener with a fresh id and a name", () => {
    const l = makeListener();
    expect(l.id).toBeTruthy();
    expect(l.name).toBeTruthy();
    expect(l.kind).toBe("local-api");
    expect(l.auth).toBe("none");
    expect(l.authToken).toBe("");
    expect(l.terminalRead).toBe(false);
    expect(l.terminalWrite).toBe(false);
    expect(l.terminalCreate).toBe(false);
    expect(l.terminalAdmin).toBe(false);
    expect(l.enabled).toBe(false);
    expect(l.allowedOrigins).toEqual([]);
  });

  it("mints a default-shaped tunnel with a fresh id and a name", () => {
    const t = makeTunnel();
    expect(t.id).toBeTruthy();
    expect(t.name).toBeTruthy();
    expect(t.mode).toBe("quick");
    expect(t.enabled).toBe(false);
    expect(t.targetListenerId).toBe("");
    expect(t.command).toBe("");
  });

  it("gives each minted item a distinct id", () => {
    expect(makeListener().id).not.toBe(makeListener().id);
    expect(makeTunnel().id).not.toBe(makeTunnel().id);
  });
});

describe("addListenerToDraft / deleteListenerFromDraft", () => {
  it("appends a fresh listener and returns it", () => {
    const d = draft();
    const l = addListenerToDraft(d);
    expect(d.listeners).toHaveLength(1);
    expect(d.listeners[0]).toBe(l);
  });

  it("removes a listener by id", () => {
    const d = draft();
    const a = addListenerToDraft(d);
    const b = addListenerToDraft(d);
    expect(deleteListenerFromDraft(d, a.id)).toBe(true);
    expect(d.listeners.map((l) => l.id)).toEqual([b.id]);
  });

  it("is a no-op for an unknown listener id", () => {
    const d = draft();
    addListenerToDraft(d);
    expect(deleteListenerFromDraft(d, "missing")).toBe(false);
    expect(d.listeners).toHaveLength(1);
  });

  // codex F3: deleting a listener must not leave a tunnel pointing at a listener id that no
  // longer exists — the ref is cleared to "", but the tunnel and unrelated refs survive.
  it("clears targetListenerId on tunnels referencing the deleted listener", () => {
    const d = draft();
    const a = addListenerToDraft(d);
    const b = addListenerToDraft(d);
    const pointsAtA = addTunnelToDraft(d);
    pointsAtA.targetListenerId = a.id;
    const pointsAtB = addTunnelToDraft(d);
    pointsAtB.targetListenerId = b.id;
    const unset = addTunnelToDraft(d); // never pointed anywhere

    expect(deleteListenerFromDraft(d, a.id)).toBe(true);
    // the tunnel that referenced the deleted listener now has a cleared ref...
    expect(pointsAtA.targetListenerId).toBe("");
    // ...but the tunnel itself survives, and unrelated refs are untouched.
    expect(d.tunnels).toHaveLength(3);
    expect(pointsAtB.targetListenerId).toBe(b.id);
    expect(unset.targetListenerId).toBe("");
  });
});

describe("addTunnelToDraft / deleteTunnelFromDraft", () => {
  it("appends a fresh tunnel and returns it", () => {
    const d = draft();
    const t = addTunnelToDraft(d);
    expect(d.tunnels).toHaveLength(1);
    expect(d.tunnels[0]).toBe(t);
  });

  it("removes a tunnel by id", () => {
    const d = draft();
    const a = addTunnelToDraft(d);
    const b = addTunnelToDraft(d);
    expect(deleteTunnelFromDraft(d, a.id)).toBe(true);
    expect(d.tunnels.map((t) => t.id)).toEqual([b.id]);
  });

  it("is a no-op for an unknown tunnel id", () => {
    const d = draft();
    addTunnelToDraft(d);
    expect(deleteTunnelFromDraft(d, "missing")).toBe(false);
    expect(d.tunnels).toHaveLength(1);
  });
});

// codex F6: the by-id field-update routing the cards delegate to (RemoteAccessManager →
// updateListenerField / updateTunnelField) — writes land on the matching item only, and an
// unknown id is a no-op (so a concurrent reorder/delete can't misroute the write).
describe("updateListenerField / updateTunnelField", () => {
  it("updates the listener with the matching id, leaving siblings untouched", () => {
    const d = draft();
    const a = addListenerToDraft(d);
    const b = addListenerToDraft(d);
    expect(updateListenerField(d, b.id, "bindHost", "0.0.0.0")).toBe(true);
    expect(b.bindHost).toBe("0.0.0.0");
    expect(a.bindHost).toBe("127.0.0.1"); // makeListener seed, untouched
  });

  it("routes string[] (allowedOrigins) and number (port) values by id", () => {
    const d = draft();
    const l = addListenerToDraft(d);
    updateListenerField(d, l.id, "allowedOrigins", ["https://a", "https://b"]);
    updateListenerField(d, l.id, "port", 8080);
    expect(l.allowedOrigins).toEqual(["https://a", "https://b"]);
    expect(l.port).toBe(8080);
  });

  it("is a no-op (returns false) for an unknown listener id", () => {
    const d = draft();
    const l = addListenerToDraft(d);
    expect(updateListenerField(d, "missing", "bindHost", "x")).toBe(false);
    expect(l.bindHost).toBe("127.0.0.1");
  });

  it("updates the tunnel with the matching id, leaving siblings untouched", () => {
    const d = draft();
    const a = addTunnelToDraft(d);
    const b = addTunnelToDraft(d);
    expect(updateTunnelField(d, b.id, "targetListenerId", "lis-1")).toBe(true);
    expect(b.targetListenerId).toBe("lis-1");
    expect(a.targetListenerId).toBe(""); // makeTunnel seed, untouched
  });

  it("is a no-op (returns false) for an unknown tunnel id", () => {
    const d = draft();
    const t = addTunnelToDraft(d);
    expect(updateTunnelField(d, "missing", "publicUrl", "x")).toBe(false);
    expect(t.publicUrl).toBe("");
  });
});

// codex F6: the allowedOrigins CSV→string[] normalization the ListenerCard csv buffer
// delegates to — split on comma, trim, drop empties (mirrors ProjectCard's authors split).
describe("normalizeAllowedOrigins", () => {
  it("splits on comma, trims, and drops empty entries", () => {
    expect(normalizeAllowedOrigins("https://a , https://b ,, , https://c")).toEqual([
      "https://a",
      "https://b",
      "https://c",
    ]);
  });

  it("returns an empty array for a blank / whitespace-only / commas-only string", () => {
    expect(normalizeAllowedOrigins("")).toEqual([]);
    expect(normalizeAllowedOrigins("   ")).toEqual([]);
    expect(normalizeAllowedOrigins(", ,")).toEqual([]);
  });
});
