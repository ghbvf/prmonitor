// Transport port (AB#1375). The single seam between the frontend (ViewModels /
// components / slice api.ts) and the backend. TauriTransport wraps desktop IPC
// (invoke / listen / opener); HttpTransport wraps a browser HTTP+SSE client. main.ts
// picks one at boot via setTransport(); everything else calls getTransport().
//
// Hard carrier (per .claude/rules/prmonitor/ai-robust.md): this `Transport` interface is
// a TS type — a slice api.ts cannot call a method the port does not declare, so the
// request / subscribe / openExternal surface is locked at compile time (违反不可表达).
// The browser-safety companion (no `@tauri-apps/*` import outside transport/tauri.ts) is
// the Medium closed funnel in slice-boundary.test.ts.

// Our own unlisten handle — intentionally NOT re-exported from `@tauri-apps/api/event`,
// so this module stays free of desktop-only imports. Structurally identical to Tauri's
// `UnlistenFn` (`() => void`), so a TauriTransport can return one directly.
export type UnlistenFn = () => void;

export interface Transport {
  // Invoke a backend command by name. `args` keys map to the Rust command's snake_case
  // params (camelCase JS key → serde rename); `undefined` keys are dropped (parity with
  // Tauri's invoke and JSON.stringify), so optional args pass cleanly as `{ projectId }`.
  request<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  // Subscribe to a backend-pushed event. The handler receives the PAYLOAD directly (the
  // adapter unwraps any transport envelope); the returned UnlistenFn detaches the listener.
  subscribe<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn>;
  // Open a URL in the user's external browser (desktop: opener plugin; web: window.open).
  openExternal(url: string): Promise<void>;
}

// Module singleton. setTransport() runs once in main.ts boot, BEFORE the Vue app mounts,
// so any store init / event subscription (which runs after mount) sees a ready transport.
let current: Transport | null = null;

export function setTransport(transport: Transport): void {
  if (import.meta.env.DEV && current !== null) {
    // Overwriting a live transport (e.g. an HMR re-run of main.ts boot) re-points the
    // singleton; listeners attached to the old instance would be orphaned. Warn in dev.
    console.warn("setTransport: overwriting an existing transport (HMR or double-boot?)");
  }
  current = transport;
}

export function getTransport(): Transport {
  if (current === null) {
    // Fail fast: a slice api.ts called before boot wired the transport is a bug, not a
    // silently-degraded no-op.
    throw new Error("Transport not initialized — setTransport() must run before use");
  }
  return current;
}
