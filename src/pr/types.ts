// PR slice types. GhStatus is slice-private (mirrors the `gh_status` command's
// Rust return shape); it is not a cross-slice contract, so it lives here rather
// than in src/types.ts.
export interface GhStatus {
  authenticated: boolean;
  message: string;
}
