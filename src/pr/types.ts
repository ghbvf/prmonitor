// PR slice types. GhStatus is slice-private (mirrors the `gh_status` command's
// Rust return shape); it is not a cross-slice contract, so it lives here rather
// than in src/types.ts.
export interface GhStatus {
  authenticated: boolean;
  message: string;
}

// WebhookStatus is slice-private (mirrors the `start_webhook`/`stop_webhook`/
// `webhook_status` commands' Rust return shape, serde camelCase); it is not a
// cross-slice contract, so it lives here rather than in src/types.ts.
export interface WebhookStatus {
  running: boolean;
  // The tunnel ROOT (`https://*.trycloudflare.com`). Not what GitHub gets — see
  // payloadUrl.
  publicUrl: string | null;
  // The full GitHub "Payload URL" = publicUrl + the receiver's `/webhook` route. The
  // UI shows/copies THIS; publicUrl alone 404s every delivery. Derived backend-side.
  payloadUrl: string | null;
  cloudflaredInstalled: boolean;
  message: string;
}
