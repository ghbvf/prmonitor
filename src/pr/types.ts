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

// DeliveryStatus is the terminal classification of one received webhook delivery
// (#62), mirrored from `pr/webhook.rs::DeliveryStatus` (serde camelCase). Slice-
// private command-return mirror — not a cross-slice contract — so it lives here,
// not in src/types.ts. The Rust side pins these exact strings with a wire-shape
// golden test, so this union must stay in lockstep.
export type DeliveryStatus =
  | "unauthorized"
  | "badPayload"
  | "ignored"
  | "wrongRepo"
  | "noTriggerLabel"
  | "notOpen"
  | "gated"
  | "dispatched"
  | "listUpdated";

// WebhookDelivery mirrors `pr/webhook.rs::WebhookDelivery` (the `webhook_deliveries`
// command's Rust return shape, serde camelCase). Slice-private diagnostics row, not
// a cross-slice contract — same rationale as WebhookStatus. The `*Epoch` field is
// UNIX seconds (u64); multiply by 1000 for `new Date(...)`.
export interface WebhookDelivery {
  receivedAtEpoch: number;
  event: string;
  action: string | null;
  repo: string | null;
  prNumber: number | null;
  kind: string | null;
  status: DeliveryStatus;
  message: string | null;
}

// PollStatus mirrors `pr/scheduler.rs::PollStatus` (the `poll_status` command's Rust
// return shape, serde camelCase). Slice-private command-return mirror — same
// rationale as WebhookStatus. The `*Epoch` fields are UNIX seconds (u64); multiply
// by 1000 for `new Date(...)`.
export interface PollStatus {
  running: boolean;
  intervalSecs: number;
  lastStartedEpoch: number | null;
  lastSuccessEpoch: number | null;
  lastErrorEpoch: number | null;
  lastErrorMessage: string | null;
  lastPersistEpoch: number | null;
  lastDiscoveredCount: number | null;
}
