// Review 切片私有 wire 型，镜像 src-tauri 的 CodexStatus（不进 src/types.ts —— 对标 src/pr/types.ts 的 GhStatus）。
export interface CodexStatus {
  available: boolean;
  message: string;
}
