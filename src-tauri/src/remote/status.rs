//! Remote Access listener runtime status (AB#1225) — the wire types the
//! `get_listener_runtime_status` command returns to the 「远程访问」settings page.
//!
//! These mirror to `src/config/types.ts` (slice-private config↔TS contract, per
//! `.claude/rules/prmonitor/ai-robust.md`). The serde camelCase / kebab-case wire shape is
//! locked by the goldens in this module (**Medium** carrier — a rename surfaces as a failing
//! test); the TS side adds `assertNever` exhaustiveness over the `state` discriminant.

use serde::Serialize;

use crate::config::model::ListenerKind;

/// One enabled listener's runtime state. Sealed enum; kebab-case wire values (mirror the
/// `ListenerKind` style). This is the discriminated-union tag the TS `assertNever` switch covers.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListenerState {
    /// Actually listening on its loopback port.
    Bound,
    /// Bound on its loopback port BUT the relevant bearer token is empty, so requests fail-close
    /// to 401 — bound ≠ usable. For `local-api`, this is `local_api_token`; for `terminal`, this
    /// is the listener's `authToken`.
    BoundNoAuth,
    /// A non-loopback `bindHost` was declared; the runtime refuses to bind it until AB#1073
    /// (auth/secret/audit) lands. Fail-closed — never silently downgraded to loopback.
    // Explicit rename: `rename_all = "kebab-case"` yields `blocked-needs1073` (serde inserts no
    // hyphen before a digit), so pin the readable hyphenated wire value the TS mirror uses.
    #[serde(rename = "blocked-needs-1073")]
    BlockedNeeds1073,
    /// This kind has no listener runtime yet (`remote-web` / `event-ingress`). Reported, not bound.
    Unsupported,
    /// The kind should have bound but binding failed (port occupied / OS).
    Error,
}

/// One enabled listener's runtime status snapshot. serde camelCase — golden-locked below;
/// TS-mirrored in `src/config/types.ts`. `boundPort` is present only when actually bound.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerRuntimeStatus {
    pub id: String,
    pub kind: ListenerKind,
    pub bound: bool,
    #[cfg_attr(test, ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_port: Option<u16>,
    pub state: ListenerState,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::to_value;

    #[test]
    fn listener_state_wire_values_are_kebab() {
        assert_eq!(to_value(ListenerState::Bound).unwrap(), "bound");
        // `rename_all = "kebab-case"` yields `bound-no-auth` (no digit in the variant, so no
        // serde hyphen-before-digit surprise) — golden-locked so a rename surfaces here.
        assert_eq!(
            to_value(ListenerState::BoundNoAuth).unwrap(),
            "bound-no-auth"
        );
        assert_eq!(
            to_value(ListenerState::BlockedNeeds1073).unwrap(),
            "blocked-needs-1073"
        );
        assert_eq!(to_value(ListenerState::Unsupported).unwrap(), "unsupported");
        assert_eq!(to_value(ListenerState::Error).unwrap(), "error");
    }

    #[test]
    fn listener_runtime_status_wire_shape_is_camel_case() {
        let v = to_value(ListenerRuntimeStatus {
            id: "local-api".to_string(),
            kind: ListenerKind::LocalApi,
            bound: true,
            bound_port: Some(8788),
            state: ListenerState::Bound,
            message: "已绑定 127.0.0.1:8788".to_string(),
        })
        .expect("serializes");
        // camelCase keys present.
        assert!(v.get("id").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("bound").is_some());
        assert!(v.get("boundPort").is_some());
        assert!(v.get("state").is_some());
        assert!(v.get("message").is_some());
        // snake_case form absent (a rename to snake would surface here).
        assert!(v.get("bound_port").is_none());
        // kind reuses the kebab-case ListenerKind wire value.
        assert_eq!(v["kind"], "local-api");
        assert_eq!(v["state"], "bound");
    }

    #[test]
    fn bound_port_is_omitted_when_none() {
        let v = to_value(ListenerRuntimeStatus {
            id: "x".to_string(),
            kind: ListenerKind::RemoteWeb,
            bound: false,
            bound_port: None,
            state: ListenerState::Unsupported,
            message: "暂不支持".to_string(),
        })
        .expect("serializes");
        assert!(
            v.get("boundPort").is_none(),
            "boundPort must be omitted (skip_serializing_if), not null"
        );
        assert_eq!(v["bound"], false);
        // A non-`local-api` kind reuses the kebab-case `ListenerKind` wire value — covers the
        // status `kind` field's wire beyond `local-api` (golden-locks the non-default kind).
        assert_eq!(v["kind"], "remote-web");
        assert_eq!(v["state"], "unsupported");
    }

    #[test]
    fn blocked_needs_1073_full_wire_shape() {
        // Full struct golden for a non-bound state: `state` is the hyphenated wire value,
        // `bound` is false, and `boundPort` is omitted (not null).
        let v = to_value(ListenerRuntimeStatus {
            id: "remote".to_string(),
            kind: ListenerKind::LocalApi,
            bound: false,
            bound_port: None,
            state: ListenerState::BlockedNeeds1073,
            message: "bindHost 非 loopback".to_string(),
        })
        .expect("serializes");
        assert_eq!(v["state"], "blocked-needs-1073");
        assert_eq!(v["bound"], false);
        assert!(
            v.get("boundPort").is_none(),
            "boundPort must be omitted in a non-bound state"
        );
    }

    #[test]
    fn bound_no_auth_full_wire_shape() {
        // Full struct golden for the `BoundNoAuth` state: bound on loopback (so `bound` is true and
        // `boundPort` is present) but the local API token is empty → every request 401s. The wire
        // `state` discriminant is the kebab-case `bound-no-auth`.
        let v = to_value(ListenerRuntimeStatus {
            id: "local-api".to_string(),
            kind: ListenerKind::LocalApi,
            bound: true,
            bound_port: Some(8788),
            state: ListenerState::BoundNoAuth,
            message: "已绑定 127.0.0.1:8788，但 token 未设置——请求将 401".to_string(),
        })
        .expect("serializes");
        assert_eq!(v["state"], "bound-no-auth");
        assert_eq!(v["bound"], true);
        assert_eq!(v["boundPort"], 8788);
    }

    #[test]
    fn error_full_wire_shape() {
        // Full struct golden for the `Error` state (bindable kind that failed to bind).
        let v = to_value(ListenerRuntimeStatus {
            id: "local-api".to_string(),
            kind: ListenerKind::LocalApi,
            bound: false,
            bound_port: None,
            state: ListenerState::Error,
            message: "绑定失败".to_string(),
        })
        .expect("serializes");
        assert_eq!(v["state"], "error");
        assert_eq!(v["bound"], false);
        assert!(
            v.get("boundPort").is_none(),
            "boundPort must be omitted in the Error state"
        );
    }
}
