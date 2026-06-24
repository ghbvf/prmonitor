// Pure draft mutations for the remote-access lists (config slice, AB#1064). Extracted
// from RemoteAccessManager / ListenerCard / TunnelCard so the add/delete/update shape
// stays unit-testable without a Vue component harness — mirrors projectOps.ts. Unlike
// projects there is NO active-id invariant (listeners/tunnels are a flat list with no
// "active" selection), so add just appends and delete just removes by stable id.
import type { AppConfig, Listener, Tunnel } from "./types";
import type { ListenerFieldKey, TunnelFieldKey } from "./fields";

// A fresh listener: identity (id/name) minted here, the rest seeded to the Rust
// `Listener::default` shape (kind "local-api", auth "none", disabled) so an added listener
// is immediately a valid wire shape the backend accepts. ONE deliberate divergence:
// `bindHost` is pre-filled "127.0.0.1" for usability (the common loopback case), whereas
// Rust's `Listener::default().bind_host` is "" — the seed is a UI convenience, not a mirror
// of the Rust default for that field. `id` uses the same `crypto.randomUUID()` generator as
// makeProject (projectOps.ts), keeping the id-minting single-sourced in spirit across the
// config slice.
export function makeListener(): Listener {
  return {
    id: crypto.randomUUID(),
    name: "新监听器",
    kind: "local-api",
    bindHost: "127.0.0.1",
    port: 0,
    enabled: false,
    auth: "none",
    allowedOrigins: [],
    publicUrl: "",
  };
}

// A fresh tunnel: identity minted here, the rest seeded to the Rust `Tunnel::default`
// shape (mode "quick", disabled). Same uuid generator as makeListener/makeProject.
export function makeTunnel(): Tunnel {
  return {
    id: crypto.randomUUID(),
    name: "新隧道",
    mode: "quick",
    targetListenerId: "",
    publicUrl: "",
    enabled: false,
  };
}

// Append a fresh listener and return it (no active-id invariant to maintain).
export function addListenerToDraft(draft: AppConfig): Listener {
  const l = makeListener();
  draft.listeners.push(l);
  return l;
}

// Remove the listener with `id`. Returns false (no-op) if not found. Deleting a listener
// also clears any tunnel's `targetListenerId` that pointed at it (set to "") so the draft
// never persists a dangling reference to a listener that no longer exists (codex F3); the
// tunnels themselves stay — only the now-invalid ref is cleared, matching how a freshly
// minted tunnel starts with an empty targetListenerId.
export function deleteListenerFromDraft(draft: AppConfig, id: string): boolean {
  const idx = draft.listeners.findIndex((x) => x.id === id);
  if (idx === -1) return false;
  draft.listeners.splice(idx, 1);
  for (const t of draft.tunnels) {
    if (t.targetListenerId === id) t.targetListenerId = "";
  }
  return true;
}

// Append a fresh tunnel and return it.
export function addTunnelToDraft(draft: AppConfig): Tunnel {
  const t = makeTunnel();
  draft.tunnels.push(t);
  return t;
}

// Remove the tunnel with `id`. Returns false (no-op) if not found.
export function deleteTunnelFromDraft(draft: AppConfig, id: string): boolean {
  const idx = draft.tunnels.findIndex((x) => x.id === id);
  if (idx === -1) return false;
  draft.tunnels.splice(idx, 1);
  return true;
}

// Apply a single field edit to the draft listener with `id`, addressed by stable id (not
// array position) so a concurrent reorder/delete can't misroute the write. Returns false
// (no-op) if no listener matches. Extracted from RemoteAccessManager.onUpdateListener so
// the by-id routing is unit-testable (codex F6); the value cast bridges the heterogeneous
// FieldDef.kind→value-type pairing the card mirrors.
export function updateListenerField(
  draft: AppConfig,
  id: string,
  key: ListenerFieldKey,
  value: string | number | boolean | string[],
): boolean {
  const l = draft.listeners.find((x) => x.id === id);
  if (!l) return false;
  (l as Record<ListenerFieldKey, unknown>)[key] = value;
  return true;
}

// Apply a single field edit to the draft tunnel with `id`. Same by-id routing contract as
// updateListenerField — returns false (no-op) for an unknown id.
export function updateTunnelField(
  draft: AppConfig,
  id: string,
  key: TunnelFieldKey,
  value: string | number | boolean | string[],
): boolean {
  const t = draft.tunnels.find((x) => x.id === id);
  if (!t) return false;
  (t as Record<TunnelFieldKey, unknown>)[key] = value;
  return true;
}

// Normalize a comma-separated origins string to the `allowedOrigins` string[] wire shape:
// split on comma, trim each, drop empties. Extracted from ListenerCard's csv buffer so the
// CSV→string[] rule is unit-testable (codex F6); mirrors ProjectCard's authors csv split.
export function normalizeAllowedOrigins(csv: string): string[] {
  return csv
    .split(",")
    .map((o) => o.trim())
    .filter((o) => o.length > 0);
}
