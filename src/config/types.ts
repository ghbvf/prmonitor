// Config slice types. AppConfig is slice-private (mirrors src-tauri/src/config/model.rs).
import type { SourceKind, EngineKind } from "../types";

// Webhook receiver tunnel mode (#9), config slice-private — mirrors the Rust
// `WebhookTunnelMode` enum's camelCase wire values. Named (like SourceKind/EngineKind)
// so the literal union has a single home. quick = App starts a Cloudflare Quick
// Tunnel (random URL); command = App runs the configured tunnel command ({port}
// placeholder); listener = App only listens on 127.0.0.1:port, tunnel managed
// externally.
export type WebhookTunnelMode = "quick" | "command" | "listener";

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
