// Config slice types. AppConfig is slice-private (mirrors src-tauri/src/config/model.rs).
import type { SourceKind, EngineKind, UpdateMode } from "../types";

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
  engineKind: EngineKind;
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
}
