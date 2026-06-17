// Config slice types. AppConfig is slice-private (mirrors src-tauri/src/config/model.rs).
import type { SourceKind, EngineKind } from "../types";

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

export interface AppConfig {
  repo: string;
  repoRoot: string;
  pollIntervalSecs: number;
  authors: string[];
  reviewLabel: string;
  checkLabel: string;
  skillRelPath: string;
  prCooldownSeconds: number;
  sourceKind: SourceKind;
  engineKind: EngineKind;
  autoReview: boolean;
  webhookEnabled: boolean;
  webhookPort: number;
  webhookSecret: string;
  cloudflaredBin: string;
  webhookTunnelMode: WebhookTunnelMode;
  webhookTunnelCommand: string;
  webhookPublicUrl: string;
}
