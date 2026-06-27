// Config slice types. AppConfig is slice-private (mirrors src-tauri/src/config/model.rs).
import type { SourceKind, EngineKind, UpdateMode, LabelSource } from "../types";

// Webhook receiver tunnel mode (#9), config slice-private — mirrors the Rust
// `WebhookTunnelMode` enum's camelCase wire values. quick = App starts a Cloudflare
// Quick Tunnel (random URL); command = App runs the configured tunnel command ({port}
// placeholder); listener = App only listens on 127.0.0.1:port, tunnel managed
// externally.
//
// Single-sourced as an `as const` array (#50 review G9): the type is DERIVED from
// the array, and fields.ts feeds the same array into the select `options`, so the
// type and the UI's option list can never drift. Adding/renaming a mode = edit this
// one array. (The Rust↔TS mirror remains a separate, golden-locked contract.)
export const WEBHOOK_TUNNEL_MODES = ["quick", "command", "listener"] as const;
export type WebhookTunnelMode = (typeof WEBHOOK_TUNNEL_MODES)[number];

// Remote-access listener kind (AB#1064), config slice-private — mirrors the Rust
// `ListenerKind` enum's kebab-case wire values. local-api = the resident 127.0.0.1
// trigger endpoint; remote-web = the panel served over a tunnel; event-ingress = an
// inbound event/webhook receiver; terminal = a remote shell/PTY listener. Default
// "local-api".
//
// Single-sourced as an `as const` array (same #50 review G9 pattern as
// WEBHOOK_TUNNEL_MODES): the type is DERIVED from the array, and fields.ts feeds the
// same array into the select `options`, so the type and the UI's option list can never
// drift. (The Rust↔TS mirror remains a separate, golden-locked contract.)
export const LISTENER_KINDS = [
  "local-api",
  "remote-web",
  "event-ingress",
  "terminal",
] as const;
export type ListenerKind = (typeof LISTENER_KINDS)[number];

// Runtime listener state (AB#1225 PR1) — mirrors the Rust `ListenerState` enum's
// kebab-case wire values emitted by the `get_listener_runtime_status` command.
// bound = successfully bound; bound-no-auth = bound but the relevant bearer token
// is empty, so requests 401 (bound ≠ usable); blocked-needs-1073 = loopback-only
// gate prevents binding on a non-loopback host; unsupported = the kind can't bind
// yet (remote-web, event-ingress); error = bind attempt failed (see `message` for
// details).
//
// Single-sourced as an `as const` array (same #50 review G9 pattern): the type is
// DERIVED from the array so the Medium `assertNever`穷尽 carrier in
// RemoteAccessRuntimeStatus.vue (listenerStateLabel switch) covers every value here.
// Adding a new state is a compile error in that switch unless a case is added.
export const LISTENER_STATES = [
  "bound",
  "bound-no-auth",
  "blocked-needs-1073",
  "unsupported",
  "error",
] as const;
export type ListenerState = (typeof LISTENER_STATES)[number];

// Runtime status row for one listener (AB#1225 PR1) — mirrors the Rust
// `ListenerRuntimeStatus` struct's camelCase wire shape emitted by
// `get_listener_runtime_status`. `boundPort` is present only when `bound` is true.
// Golden-locked on the Rust side (serde camelCase test); this interface is the
// TS downstream end of that funnel.
export interface ListenerRuntimeStatus {
  id: string;
  kind: ListenerKind;
  bound: boolean;
  boundPort?: number;
  state: ListenerState;
  message: string;
}

// Listener auth mode (AB#1064): none = no credential (loopback only); bearer = an
// Authorization: Bearer <token> gate. Mirrors the Rust `ListenerAuthMode` wire values.
// Default "none". Same single-sourced `as const`-array pattern as LISTENER_KINDS.
export const LISTENER_AUTH_MODES = ["none", "bearer"] as const;
export type ListenerAuthMode = (typeof LISTENER_AUTH_MODES)[number];

// One declarative listener (AB#1064): a named bind endpoint the app exposes. Mirrors
// the Rust `Listener` struct's camelCase wire shape (golden-locked on the Rust side).
// `id` is the stable handle a Tunnel's `targetListenerId` points at.
export interface Listener {
  id: string;
  name: string;
  kind: ListenerKind;
  bindHost: string;
  port: number;
  enabled: boolean;
  auth: ListenerAuthMode;
  authToken: string;
  terminalRead: boolean;
  terminalWrite: boolean;
  terminalCreate: boolean;
  terminalAdmin: boolean;
  allowedOrigins: string[];
  publicUrl: string;
}

// One declarative tunnel (AB#1064): publishes a listener over a public URL. Mirrors the
// Rust `Tunnel` struct's camelCase wire shape (golden-locked on the Rust side). `mode`
// reuses WebhookTunnelMode (quick/command/listener); `targetListenerId` names the
// `Listener.id` this tunnel fronts.
export interface Tunnel {
  id: string;
  name: string;
  mode: WebhookTunnelMode;
  targetListenerId: string;
  command: string;
  publicUrl: string;
  enabled: boolean;
}

// One monitored project (#35): the per-project slice of what used to be the flat
// AppConfig. Mirrors the Rust `Project` struct's camelCase wire shape (golden-locked
// on the Rust side). `id` is the stable handle used by `activeProjectId` and the
// per-project event arms (`projectId` in src/types.ts).
export interface Project {
  id: string;
  name: string;
  enabled: boolean;
  repo: string;
  repoRoot: string;
  pollIntervalSecs: number;
  authors: string[];
  reviewLabel: string;
  checkLabel: string;
  // Where trigger labels come from (717): native = the provider's own PR labels;
  // title = parse `[..]` tags out of the PR title. Default "native"; a Bitbucket source
  // must use "title" (no native labels). Mirrors the Rust `Project.labelSource` wire field.
  labelSource: LabelSource;
  skillRelPath: string;
  prCooldownSeconds: number;
  // Data-update mode (818): webhook-only (default, push-driven) / pull-only / hybrid
  // (both run the CLI poll loop) / manual. Mirrors the Rust `Project.updateMode` wire field.
  updateMode: UpdateMode;
  sourceKind: SourceKind;
  // Azure DevOps org/project (818) — only meaningful when sourceKind === "azure";
  // empty strings for a github source. Mirrors the Rust `Project.azureOrg`/`azureProject`.
  azureOrg: string;
  azureProject: string;
  // Bitbucket Server/Data Center connection (717) — only meaningful when
  // sourceKind === "bitbucket"; empty strings for a github/azure source. Mirrors the Rust
  // `Project.bitbucketHost`/`bitbucketProject`/`bitbucketToken`. host = Server/DC base URL;
  // project = project key (e.g. GOCELL, or ~username for a personal repo); token = HTTP
  // access token (PAT) used as a Bearer credential.
  bitbucketHost: string;
  bitbucketProject: string;
  bitbucketToken: string;
  engineKind: EngineKind;
  // Hand-typed model overrides per engine — only meaningful for the matching engineKind
  // (codexModel when "codex", claudeModel when "claude"); empty = the engine's own default.
  // Mirror the Rust `Project.codexModel`/`claudeModel` wire fields (golden-locked on Rust).
  codexModel: string;
  claudeModel: string;
  autoReview: boolean;
}

// Multi-project config (#35): a list of `Project`s plus the active selection, with
// the webhook/shell fields staying global (one receiver/tunnel serves all projects).
// Mirrors `src-tauri/src/config/model.rs`'s `AppConfig`.
export interface AppConfig {
  projects: Project[];
  activeProjectId: string;
  // Global webhook/shell fields stay top-level — one receiver + tunnel for all projects.
  webhookEnabled: boolean;
  webhookPort: number;
  webhookSecret: string;
  cloudflaredBin: string;
  webhookTunnelMode: WebhookTunnelMode;
  webhookTunnelCommand: string;
  webhookPublicUrl: string;
  // Local REST API token (AB#1043): Bearer credential for the 127.0.0.1 trigger endpoint.
  // `localApiToken` empty = disabled (fail-closed 401). The port is now owned by the
  // `listeners[]` entry of kind "local-api" (AB#1225 PR1) — `localApiPort` is gone from
  // AppConfig. Mirror the Rust `AppConfig.localApiToken` wire field.
  localApiToken: string;
  // Outbox worker policy (AB#1182): per-kind staleness TTLs for the durable action queue. Global
  // (one policy serves every project). Mirrors the Rust `AppConfig.outbox` wire field; the nested
  // struct is `#[serde(default)]` so an older persisted config without it loads with defaults.
  outbox: OutboxConfig;
  // Remote-access resources (AB#1064): declarative lists of listeners (bind endpoints)
  // and tunnels (public publishers) edited by the Remote Access settings page. Mirror
  // the Rust `AppConfig.listeners`/`tunnels` wire fields (golden-locked on the Rust side).
  listeners: Listener[];
  tunnels: Tunnel[];
}

// Mirrors `src-tauri/src/config/model.rs`'s `OutboxConfig` (serde camelCase; locked by the
// `app_config_wire_shape_is_camel_case` golden). `notificationTtlSecs` is the staleness window for
// a `notification` action in SECONDS (default 7200 = 2h); `0` DISABLES the TTL (never expires).
export interface OutboxConfig {
  notificationTtlSecs: number;
}
