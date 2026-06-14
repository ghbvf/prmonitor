// Config slice types. AppConfig is slice-private (mirrors src-tauri/src/config/model.rs).
export interface AppConfig {
  repo: string;
  repoRoot: string;
  pollIntervalSecs: number;
  authors: string[];
  reviewLabel: string;
  checkLabel: string;
  skillRelPath: string;
  prCooldownSeconds: number;
}
