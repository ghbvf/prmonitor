// Config slice types. AppConfig is slice-private (mirrors src-tauri/src/config/model.rs).
import type { SourceKind, EngineKind } from "../types";

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
}
