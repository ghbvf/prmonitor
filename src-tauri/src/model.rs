//! Cross-slice shared types — the contract boundary between slices.
//!
//! Slices must not import each other's internals; any type that crosses a slice
//! boundary lives here. Serialized fields use camelCase for the frontend.

use serde::{Deserialize, Serialize};

/// A PR discovered by a [`crate::pr::source::PrSource`] that may need review.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    pub author: String,
    pub is_cross_repository: bool,
    pub is_draft: bool,
    /// `"review"` or `"check"` — which pr-review mode the trigger label maps to.
    pub kind: String,
}

/// Which PR source backs the monitor.
///
/// **Hard carrier** (sealed enum): once PR3+ wires source selection through an
/// exhaustive `match SourceKind { ... }`, adding a variant without handling it
/// is a compile error — the missing arm cannot be expressed. Today it has one
/// variant, so the seam is reserved but not yet load-bearing.
///
/// #11 design reservation: future variants `GitLab` / `Bitbucket`. Wire strings
/// for `Github` / `Azure` are pinned to `"github"` / `"azure"` (cross-agent
/// contract; the frontend mirrors them and a serde golden test locks them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    #[default]
    Github,
    /// Azure DevOps Repos, discovered via the `az repos pr list` CLI (#818).
    Azure,
    // future #11: GitLab, Bitbucket
}

/// Per-project data-update mode (#818): how a project's PR list is kept fresh.
///
/// **Hard carrier** (sealed enum): source/poll selection branches on an exhaustive
/// `match UpdateMode { ... }` (the scheduler's `periodic_polling` and `poll_now`),
/// so adding a variant without handling it is a compile error.
///
/// Wire strings are pinned kebab-case (`"webhook-only" | "pull-only" | "hybrid" |
/// "manual"`) — a cross-agent contract the frontend's TS union must mirror exactly;
/// a serde golden test below locks it.
///
/// Modes:
/// - [`WebhookOnly`](Self::WebhookOnly) (default, the safe boot behavior): the list
///   is updated ONLY by inbound webhook deliveries — NO automatic CLI polling. A
///   manual pull is rejected (there is no source to pull from in this mode).
/// - [`PullOnly`](Self::PullOnly): a periodic CLI poll loop is the sole update source.
/// - [`Hybrid`](Self::Hybrid): both a periodic CLI poll loop AND inbound webhooks.
/// - [`Manual`](Self::Manual): no periodic loop; the list updates only on an explicit
///   "立即拉取" (one-shot CLI discovery) or inbound webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMode {
    #[default]
    WebhookOnly,
    PullOnly,
    Hybrid,
    Manual,
}

/// Which review engine runs against a PR.
///
/// **Hard carrier** (sealed enum): once PR3+ wires engine selection through an
/// exhaustive `match EngineKind { ... }`, adding a variant without handling it
/// is a compile error — the missing arm cannot be expressed. Today it has one
/// variant, so the seam is reserved but not yet load-bearing.
///
/// #11 design reservation: future variant `Claude`. Wire string for `Codex` is
/// pinned to `"codex"` (cross-agent contract; the frontend mirrors it and a
/// serde golden test locks it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EngineKind {
    #[default]
    Codex,
    // future #11: Claude
}

/// How the webhook receiver's local port is exposed to the public internet (#9).
///
/// Lives here (not in `pr/webhook.rs`) because, like [`SourceKind`] / [`EngineKind`],
/// it is a config→pr cross-slice kind enum: the `config` slice persists it on
/// `AppConfig` and the `pr` slice's `webhook::start` consumes it to branch the tunnel
/// strategy. Keeping it in `crate::model` is the SINGLE source both slices import,
/// rather than each defining its own — mirroring the `SourceKind` precedent.
///
/// Wire strings are pinned lowercase (`"quick" | "command" | "listener"`) — a
/// cross-agent contract the frontend's TS union must mirror exactly; a serde golden
/// test below locks it.
///
/// Modes:
/// - [`Quick`](Self::Quick) (default, unchanged status quo): spawn a Cloudflare Quick
///   Tunnel via `cloudflared` and scrape the `*.trycloudflare.com` URL.
/// - [`Command`](Self::Command): spawn a user-supplied tunnel command (e.g. a named
///   cloudflared tunnel, `ngrok`, …); `publicUrl` comes from config, not scraped.
/// - [`Listener`](Self::Listener): only bind the local port; the tunnel is fully
///   external (no child process); `publicUrl` comes from config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebhookTunnelMode {
    #[default]
    Quick,
    Command,
    Listener,
}

/// A PR row shown in the UI (display superset of [`Candidate`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestView {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
    /// `"review"` or `"check"` — the trigger-label mode this PR maps to.
    pub kind: String,
    /// Why this PR would be skipped (not dispatched), or `None` when it would
    /// dispatch. Serializes to `null` / a string for the frontend.
    pub skip_reason: Option<String>,
}

/// Whether a tracked PR was seen in the latest discovery window or has aged out.
///
/// `Current` = last seen within the presence grace window (the live working set);
/// `Stale` = not seen recently (a transient `gh` miss or a genuinely closed PR),
/// retained so a one-round miss flips presence rather than dropping the row.
///
/// `Serialize`-only by design: this is a frontend projection derived from a
/// [`crate::pr::registry::TrackedPr`]'s `last_seen_epoch` vs the grace window, never
/// persisted or read back, so it intentionally does NOT derive `Deserialize` (the
/// persisted shape is `TrackedPr`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PrPresence {
    Current,
    Stale,
}

/// Tracking-aware PR row emitted to the frontend (persisted-retention view).
///
/// Flattens [`PullRequestView`] so the wire shape stays a flat row plus the two
/// retention fields (`presence` / `archived`) the persisted-list UI renders.
///
/// `Serialize`-only by design: this is the frontend projection of a
/// [`crate::pr::registry::TrackedPr`] (computed per emit), never persisted or read
/// back, so it intentionally does NOT derive `Deserialize` — `TrackedPr` is the
/// persisted type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedPrView {
    #[serde(flatten)]
    pub pr: PullRequestView,
    pub presence: PrPresence,
    pub archived: bool,
}

/// Serde wire-shape locks for `model.rs`'s cross-slice types.
///
/// The **Medium carrier** for these serde shapes per
/// `.claude/rules/prmonitor/ai-robust.md`. Each is a LOCK (characterization)
/// test: it passes on current code and only fails if a field is renamed or the
/// camelCase serialization breaks. The two types differ in their *downstream*,
/// so their contracts are not the same thing:
///
/// - [`PullRequestView`] is a **front/back contract** mirrored in
///   `src/types.ts`; a key change must be synced there in lockstep — the open
///   end of that funnel (no machine check on the TS side yet; future Hard path =
///   codegen `types.ts` from `model.rs` + `git diff --exit-code`).
/// - [`Candidate`] is **backend-internal**, cross-Rust-slice only: per the
///   charter it is intentionally *not* mirrored in `src/types.ts`, so its lock
///   guards the camelCase wire shape the `pr`/`review` slices rely on, **not** a
///   front/back contract — do not sync it to the frontend.
#[cfg(test)]
mod tests {
    use super::*;

    // Backend-internal cross-slice lock: `Candidate` is not exposed to the
    // frontend and is intentionally absent from `src/types.ts` (per the charter).
    #[test]
    fn candidate_wire_shape_is_camel_case() {
        let candidate = Candidate {
            number: 1,
            head_sha: "abc123".to_string(),
            head_ref: "feature/x".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: "review".to_string(),
        };

        let v = serde_json::to_value(&candidate).expect("Candidate serializes");

        // camelCase keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("headSha").is_some());
        assert!(v.get("headRef").is_some());
        assert!(v.get("author").is_some());
        assert!(v.get("isCrossRepository").is_some());
        assert!(v.get("isDraft").is_some());
        assert!(v.get("kind").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("head_sha").is_none());
        assert!(v.get("head_ref").is_none());
        assert!(v.get("is_cross_repository").is_none());
        assert!(v.get("is_draft").is_none());
    }

    // Cross-agent wire contract lock: the frontend mirrors these exact strings.
    // A variant rename or `rename_all` change surfaces here.
    #[test]
    fn discriminator_enums_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(SourceKind::Github).expect("SourceKind serializes"),
            "github"
        );
        // #818: the Azure source variant pins to "azure" (the frontend mirrors it).
        assert_eq!(
            serde_json::to_value(SourceKind::Azure).expect("SourceKind serializes"),
            "azure"
        );
        assert_eq!(
            serde_json::to_value(EngineKind::Codex).expect("EngineKind serializes"),
            "codex"
        );
        // #818: per-project data-update modes. kebab-case wire strings the frontend
        // mirrors; a variant rename or `rename_all` change surfaces here. Default is
        // `WebhookOnly` (the safe boot default: NO automatic CLI polling).
        assert_eq!(
            serde_json::to_value(UpdateMode::WebhookOnly).expect("UpdateMode serializes"),
            "webhook-only"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::PullOnly).expect("UpdateMode serializes"),
            "pull-only"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::Hybrid).expect("UpdateMode serializes"),
            "hybrid"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::Manual).expect("UpdateMode serializes"),
            "manual"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::default()).expect("UpdateMode serializes"),
            "webhook-only"
        );
    }

    // Cross-agent wire contract lock for `WebhookTunnelMode` (#9): the frontend's TS
    // union mirrors these exact lowercase strings. A variant rename or a
    // `rename_all` change surfaces here. Default is `Quick` (status-quo behavior).
    #[test]
    fn webhook_tunnel_mode_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Quick).expect("serializes"),
            "quick"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Command).expect("serializes"),
            "command"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Listener).expect("serializes"),
            "listener"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::default()).expect("serializes"),
            "quick"
        );
    }

    // Front/back contract lock: `PullRequestView` is mirrored in `src/types.ts`;
    // a field change here must be synced to that interface in lockstep.
    #[test]
    fn pull_request_view_wire_shape_is_camel_case() {
        let view = PullRequestView {
            number: 1,
            title: "Add feature".to_string(),
            labels: vec!["review".to_string()],
            url: "https://example.com/pr/1".to_string(),
            kind: "review".to_string(),
            skip_reason: Some("draft PR".to_string()),
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");

        // camelCase / flat keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("skipReason").is_some());

        // snake_case form absent — a rename of the one multi-word field
        // (`skip_reason`) would surface here.
        assert!(v.get("skip_reason").is_none());
    }

    // A non-skipped PR serializes `skipReason` as JSON null (not omitted) so the
    // frontend's `skipReason: string | null` mirror stays a closed contract.
    #[test]
    fn pull_request_view_none_skip_reason_serializes_to_null() {
        let view = PullRequestView {
            number: 2,
            title: "Ready".to_string(),
            labels: vec![],
            url: "https://example.com/pr/2".to_string(),
            kind: "check".to_string(),
            skip_reason: None,
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");
        assert_eq!(v["skipReason"], serde_json::Value::Null);
    }

    // Front/back contract lock for the persisted-retention row (Medium carrier per
    // ai-robust.md): `TrackedPrView` is mirrored in `src/types.ts`. It flattens
    // `PullRequestView`, so the inner keys (`number,title,labels,url,kind,skipReason`)
    // must surface at the top level alongside `presence` / `archived`; a flatten
    // regression or field rename surfaces here and must be synced to the TS mirror
    // in lockstep (the open end of this funnel; future Hard path = codegen from
    // `model.rs` + `git diff --exit-code`).
    #[test]
    fn tracked_pr_view_wire_shape_is_camel_case() {
        let view = TrackedPrView {
            pr: PullRequestView {
                number: 1,
                title: "Add feature".to_string(),
                labels: vec!["review".to_string()],
                url: "https://example.com/pr/1".to_string(),
                kind: "review".to_string(),
                skip_reason: None,
            },
            presence: PrPresence::Current,
            archived: false,
        };

        let v = serde_json::to_value(&view).expect("TrackedPrView serializes");

        // Flattened `PullRequestView` keys present at the top level.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("skipReason").is_some());

        // Retention keys present (camelCase).
        assert!(v.get("presence").is_some());
        assert!(v.get("archived").is_some());

        // `flatten` must hoist the inner fields, NOT nest them under a `pr` wrapper;
        // and the one multi-word field must not leak its snake_case form.
        assert!(v.get("skip_reason").is_none());
        assert!(
            v.get("pr").is_none(),
            "flatten must not nest a 'pr' wrapper"
        );

        // None `skip_reason` serializes as JSON null (not omitted), mirroring
        // `PullRequestView`'s closed `skipReason: string | null` contract at the
        // flattened depth.
        assert_eq!(v["skipReason"], serde_json::Value::Null);

        // `presence` serializes to the pinned lowercase wire strings the TS mirror
        // discriminates on.
        assert_eq!(v["presence"], "current");
        let stale = serde_json::to_value(PrPresence::Stale).expect("PrPresence serializes");
        assert_eq!(stale, "stale");
    }
}
