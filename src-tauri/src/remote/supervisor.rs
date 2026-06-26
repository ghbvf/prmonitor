//! Remote Access listener binding supervisor (AB#1225) — reconciles `config.listeners[]` to
//! live loopback listeners.
//!
//! This is the first runtime that turns the declarative AB#1064 listener model into real bound
//! ports. It is a composition horizontal (NOT a slice — it names `crate::review::local_api`
//! directly, like `dispatch.rs` glues `pr`→`review`); the slice-boundary test does not scan it.
//!
//! **Scope this PR (loopback-only, fail-closed):**
//!  - `local-api` is the SOLE real binder — the resident `LocalApiManager` was removed; the
//!    local-api router (`review::local_api::build_router`) is now mounted on the port the
//!    supervisor binds from a `listeners[]` entry of `kind=local-api`. Single source of truth.
//!  - A non-loopback `bindHost` is REFUSED (`BlockedNeeds1073`) for any kind — remote exposure
//!    waits for AB#1073. Whitelist, never blacklist (mirrors `local_api::security::host_allowed`).
//!  - `remote-web` / `terminal` / `event-ingress` are reported `Unsupported` (no runtime yet),
//!    never bound.
//!
//! Reconcile mirrors `pr::scheduler::SchedulerSet::reconcile` (keyed map under a `StdMutex`,
//! tear-down-absent / leave-survivors / bind-new) and the kubelet level-triggered pattern
//! (`ref: kubernetes-sigs/controller-runtime pkg/internal/controller/controller.go`): a
//! per-listener bind failure is captured in status, NEVER propagated (a bad listener must not
//! crash the app or fail a config save).
//!
//! **Per-kind binder seam (F26 / #1382):** the per-kind runtime is a [`ListenerBinder`] — each kind
//! that has a real runtime implements it, and `reconcile` dispatches via the exhaustive
//! [`bind_for_kind`] `match` (mirrors the `ReviewEngine` seam in `review/engine.rs`: a new kind is a
//! new impl + one dispatch arm, NOT a change to the reconcile/diff/status core). `local-api` is the
//! SOLE real binder this PR; `remote-web` / `terminal` (AB#1073) and `event-ingress` (tunnel runtime)
//! have no binder yet and are reported `Unsupported`. The seam is introduced ahead of AB#1073's second
//! binder by deliberate decision (#1382) — a single-impl seam is acknowledged speculative, so it is
//! gated on preserving the prior compile-time guarantee: [`bind_for_kind`]'s `match` is exhaustive with
//! NO wildcard, so a new `ListenerKind` without an arm is a COMPILE ERROR (**Hard** carrier, unchanged
//! from the pre-seam single `match`). `classify` stays the R-free bindability+reason source; the two
//! agree on the bindable set, pinned by `binder_registry_consistent_with_classify` (**Medium** carrier).
//! See `.claude/rules/prmonitor/ai-robust.md` §审查要求 for the rating obligation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tauri::async_runtime::{spawn, JoinHandle};
use tokio::sync::oneshot;

use crate::config::model::{Listener, ListenerKind};
use crate::review::local_api::{build_router, Ctx};

use super::status::{ListenerRuntimeStatus, ListenerState};

/// Bounded bind retry to ride out OS socket-release lag on a rapid restart (the same handling
/// the local-api / webhook receivers used). A final failure is logged + recorded as `Error`.
const BIND_RETRIES: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(20);

/// What the runtime should do with one (enabled) listener — the loopback gate + kind routing.
enum Disposition {
    /// Bind it on loopback (only reachable for `local-api` this PR).
    Bind,
    /// Non-loopback bindHost — refuse until AB#1073.
    Blocked(String),
    /// No runtime for this kind yet.
    Unsupported(String),
}

/// Classify ONE enabled listener (loopback gate first, then kind). The exhaustive `match` over
/// the sealed [`ListenerKind`] is the Hard carrier: a new kind without an arm is a compile error.
/// The fail-closed loopback gate ([`crate::config::model::is_loopback_host`], **Medium** carrier
/// unit-tested at its definition) is the canonical single source — never reimplement it here.
fn classify(l: &Listener) -> Disposition {
    if !crate::config::model::is_loopback_host(&l.bind_host) {
        return Disposition::Blocked(format!(
            "bindHost「{}」非 loopback，已拒绝绑定（远程暴露需 AB#1073）",
            l.bind_host
        ));
    }
    match l.kind {
        ListenerKind::LocalApi => Disposition::Bind,
        ListenerKind::EventIngress => Disposition::Unsupported(
            "event-ingress 运行时待建（需隧道运行时，AB#1225 后续）".to_string(),
        ),
        ListenerKind::RemoteWeb => {
            Disposition::Unsupported("remote-web 运行时待建（需 AB#1073）".to_string())
        }
        ListenerKind::Terminal => {
            Disposition::Unsupported("terminal 运行时待建（需 AB#1073 权限/审计）".to_string())
        }
    }
}

/// Pure status projection (no app handle) given whether the listener is actually bound and whether
/// the local API token is set — the classification→status mapping, unit-tested directly.
///
/// `local_api_token_set` gates ONLY `local-api`: a bound local-api listener with an empty token
/// reports `BoundNoAuth` instead of `Bound`, because `review::local_api::verify_bearer` fail-closes
/// EVERY request to 401 when the token is empty (so bound ≠ usable). Other kinds never reach the
/// `Bind` arm this PR, so the flag is irrelevant to them.
fn status_of(
    l: &Listener,
    bound_port: Option<u16>,
    local_api_token_set: bool,
) -> ListenerRuntimeStatus {
    let (state, bound, port, message) = match classify(l) {
        Disposition::Blocked(msg) => (ListenerState::BlockedNeeds1073, false, None, msg),
        Disposition::Unsupported(msg) => (ListenerState::Unsupported, false, None, msg),
        Disposition::Bind => match bound_port {
            Some(p) if l.kind == ListenerKind::LocalApi && !local_api_token_set => (
                ListenerState::BoundNoAuth,
                true,
                Some(p),
                format!(
                    "已绑定 127.0.0.1:{p}，但 token 未设置——请求将 401（请在设置中配置 local API token）"
                ),
            ),
            Some(p) => (
                ListenerState::Bound,
                true,
                Some(p),
                format!("已绑定 127.0.0.1:{p}"),
            ),
            None => (
                ListenerState::Error,
                false,
                None,
                "绑定失败（端口被占用或不可用）".to_string(),
            ),
        },
    };
    ListenerRuntimeStatus {
        id: l.id.clone(),
        kind: l.kind,
        bound,
        bound_port: port,
        state,
        message,
    }
}

/// One bindable listener's reconcile-relevant fields, kept TOGETHER so the bind step reads a single
/// listener's `(port, kind)` as one unit.
///
/// F1 fix (#1382 review): the bind step previously derived `kind` from a SEPARATE `id -> kind` map
/// built over ALL listeners, decoupled from the `(id -> port)` the desired set selected. Under a
/// DUPLICATE listener id — reachable because the STARTUP reconcile loads config via the LENIENT
/// `config::service::load` (no `validate`, which would reject duplicate ids) — the two `id`-keyed
/// `HashMap` collapses can resolve to DIFFERENT listeners (last-writer-wins), letting a later
/// non-runtime duplicate's kind suppress a bindable `local-api`'s bind. Carrying `kind` IN the
/// desired entry (built from the SAME bindable-filtered pass) makes that divergence unrepresentable.
#[derive(Clone, Copy)]
struct DesiredListener {
    port: u16,
    kind: ListenerKind,
}

/// The desired set of bindable listeners: enabled + loopback + a kind the supervisor binds
/// (`local-api` this PR) + a NON-ZERO port. Pure (id → `DesiredListener`), so the reconcile diff is
/// testable without an app.
///
/// The `l.port != 0` filter (F1) is a runtime backstop, not a duplicate of save-time validation:
/// `config::model::validate` rejects an enabled `port == 0` listener, but the STARTUP reconcile in
/// `lib.rs` loads config via the LENIENT `config::service::load`, which does NOT run `validate`. So a
/// hand-edited / forward-compat config with an enabled `local-api` at `port == 0` reaches reconcile;
/// without this filter `bind_std_with_retry(0)` would bind a RANDOM OS-assigned ephemeral port —
/// silently exposing the trigger API on an unpredictable port instead of failing closed. Excluding
/// it from `desired` means it is never bound → `status_of` reports it `Error` (should-bind-but-isn't),
/// the fail-closed outcome the user can see and correct. The `kind` travels WITH the port (vs a second
/// `id`-keyed lookup) so the SAME lenient-load duplicate-id path can't decouple them (see
/// [`DesiredListener`]).
fn desired_listeners(listeners: &[Listener]) -> HashMap<String, DesiredListener> {
    listeners
        .iter()
        .filter(|l| l.enabled && l.port != 0 && matches!(classify(l), Disposition::Bind))
        .map(|l| {
            (
                l.id.clone(),
                DesiredListener {
                    port: l.port,
                    kind: l.kind,
                },
            )
        })
        .collect()
}

/// Pure reconcile diff: ids to tear down (absent from desired, or port changed) and ids to bind
/// (absent from current, or port changed). Compares the live `id -> port` snapshot against the
/// desired entries' ports (a kind change on an existing id flips its desired membership, so port is
/// the only rebind trigger). Unit-tested directly.
fn diff(
    current: &HashMap<String, u16>,
    desired: &HashMap<String, DesiredListener>,
) -> (Vec<String>, Vec<String>) {
    let remove = current
        .iter()
        .filter(|(id, port)| desired.get(*id).map(|d| d.port) != Some(**port))
        .map(|(id, _)| id.clone())
        .collect();
    let add = desired
        .iter()
        .filter(|(id, d)| current.get(*id) != Some(&d.port))
        .map(|(id, _)| id.clone())
        .collect();
    (remove, add)
}

/// A live bound listener's control handle (mirrors the old `LocalApiRuntime`).
struct BoundListener {
    server_task: JoinHandle<()>,
    /// Graceful-shutdown signal; firing it lets `axum::serve` drop the listener before the task
    /// ends (so a re-bind on the same port is clean).
    shutdown: oneshot::Sender<()>,
    port: u16,
}

/// The listener binding supervisor. Lives in [`crate::state::AppState`]; `Default` (no bound
/// listeners until [`reconcile`](Self::reconcile)); methods take `&self` (interior mutability).
#[derive(Default)]
pub struct ListenerSupervisor {
    runtimes: StdMutex<HashMap<String, BoundListener>>,
    /// Serializes concurrent `reconcile` calls (setup + each `set_config` save). Held for the WHOLE
    /// reconcile so two reconciles can't interleave their bind/teardown, but it is a SEPARATE lock
    /// from `runtimes` — the blocking std bind + retry sleep happen while holding ONLY this guard,
    /// never `runtimes`, so `status_snapshot` / `shutdown` / concurrent saves stay responsive
    /// (F1+F3): `runtimes` is locked only for brief map snapshots / drains / inserts.
    reconcile_guard: StdMutex<()>,
}

impl ListenerSupervisor {
    /// Reconcile `config.listeners[]` → bound loopback runtimes. Idempotent: tears down listeners
    /// no longer desired (removed / disabled / non-loopback / port-changed), leaves survivors,
    /// binds newly-bindable ones. Called from `lib.rs` `setup()` AND after a `set_config` save.
    /// Best-effort: a per-listener bind failure is reflected in status, never propagated.
    pub fn reconcile<R: tauri::Runtime>(&self, app: &tauri::AppHandle<R>, listeners: &[Listener]) {
        // Serialize concurrent reconciles for the whole pass. Poison-safe (`into_inner`): a
        // panicked prior reconcile must not panic-cascade every subsequent save.
        let _serialize = self
            .reconcile_guard
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let desired = desired_listeners(listeners);

        // 1) Briefly lock `runtimes`: snapshot current (id→port), compute the diff, and DRAIN the
        //    removed entries out so we can tear them down with NO lock held. The lock is released
        //    at the end of this block (the guard `runtimes` is dropped), so the blocking bind +
        //    retry sleep below never block `status_snapshot` / `shutdown` / a concurrent save.
        let (add, removed): (Vec<String>, Vec<BoundListener>) = {
            let mut runtimes = self.runtimes.lock().unwrap_or_else(|p| p.into_inner());
            let current: HashMap<String, u16> = runtimes
                .iter()
                .map(|(id, b)| (id.clone(), b.port))
                .collect();
            let (remove, add) = diff(&current, &desired);
            let removed = remove
                .into_iter()
                .filter_map(|id| runtimes.remove(&id))
                .collect();
            (add, removed)
        };

        // 2) Tear down the drained removed entries with NO `runtimes` lock held.
        //
        //    Socket-release-lag invariant (F5): `server_task.abort()` is ASYNCHRONOUS — it requests
        //    cancellation but the task (and thus the OS socket `axum::serve` holds) may not be fully
        //    released by the time step 3 runs. So if this same reconcile pass tears down listener A on
        //    port P and binds listener B on that same port P (a port hand-off in one save), B's bind
        //    can momentarily hit `EADDRINUSE`. That is EXACTLY what `bind_std_with_retry`'s bounded
        //    retry (`BIND_RETRIES` × `BIND_RETRY_DELAY`) rides out — the SAME rapid-restart
        //    socket-release handling the webhook / local-api receivers use. No extra synchronization
        //    is needed here: the retry IS the handling (a final failure still fails closed → `Error`).
        for b in removed {
            let _ = b.shutdown.send(());
            b.server_task.abort();
        }

        // 3) Bind each new listener with NO `runtimes` lock held — `bind_for_kind` routes the kind to
        //    its `ListenerBinder`, which does the blocking std bind + bounded retry sleep here, so a
        //    slow port-release can't stall other callers (and the retry absorbs the step-2 abort's
        //    socket-release lag described above). The kind drives WHICH binder (F26 / #1382); it is read
        //    from the SAME `desired` entry the port came from (`DesiredListener`) — never a second
        //    `id`-keyed map over all listeners, which a duplicate id could decouple (F1 fix, #1382).
        let mut bound: Vec<(String, BoundListener)> = Vec::with_capacity(add.len());
        for id in add {
            let DesiredListener { port, kind } = desired[&id];
            if let Some(b) = bind_for_kind(kind, app, port) {
                bound.push((id, b));
            }
            // A bind failure inserts no entry → status_snapshot reports it as `Error`.
        }

        // 4) Briefly re-lock `runtimes` to insert the successful binds.
        if !bound.is_empty() {
            let mut runtimes = self.runtimes.lock().unwrap_or_else(|p| p.into_inner());
            for (id, b) in bound {
                runtimes.insert(id, b);
            }
        }
    }

    /// Per-(enabled-)listener runtime status, derived FRESH from the passed config + the live
    /// `runtimes` map (never stored, so it cannot go stale). Disabled listeners have no runtime,
    /// so they are omitted (the listener cards still show/edit them).
    ///
    /// `local_api_token_set` is the caller's verdict on whether `config.local_api_token` is
    /// non-empty (after trim). A bound local-api with no token reports `BoundNoAuth`, not `Bound`,
    /// because every request would 401 (`verify_bearer`) — see `status_of`.
    ///
    /// Self-heal (F6): a `runtimes` entry whose spawned `server_task` has FINISHED (the `axum::serve`
    /// future returned — e.g. it errored out abnormally) is treated as NOT bound, so its status falls
    /// to `Error` instead of reporting a phantom `Bound` forever. We probe the inner tokio handle's
    /// `is_finished()` (the tauri `JoinHandle` wrapper only exposes `inner()` + `abort()`, mirroring
    /// `pr::scheduler`'s `task.handle.inner().is_finished()`). The stale entry is left in the map (no
    /// mutation here, keeping this read-only under the brief lock); the next `reconcile` re-binds it
    /// because the dead entry still advertises its old port and a fresh bind on a now-free port wins.
    pub fn status_snapshot(
        &self,
        listeners: &[Listener],
        local_api_token_set: bool,
    ) -> Vec<ListenerRuntimeStatus> {
        let runtimes = self.runtimes.lock().unwrap_or_else(|p| p.into_inner());
        listeners
            .iter()
            .filter(|l| l.enabled)
            .map(|l| {
                let bound_port = runtimes
                    .get(&l.id)
                    .filter(|b| !b.server_task.inner().is_finished())
                    .map(|b| b.port);
                status_of(l, bound_port, local_api_token_set)
            })
            .collect()
    }

    /// App-shutdown cleanup (wired to `RunEvent::Exit`): fire each graceful-shutdown signal + abort
    /// the task so no listener outlives the app (same "软件关闭时一起关闭" contract as the others).
    pub fn shutdown(&self) {
        let mut runtimes = self.runtimes.lock().unwrap_or_else(|p| p.into_inner());
        for (_, b) in runtimes.drain() {
            let _ = b.shutdown.send(());
            b.server_task.abort();
        }
    }
}

/// The per-kind listener runtime binder seam (F26 / #1382). Each [`ListenerKind`] that has a real
/// runtime implements this; the reconcile / diff / status core never names a concrete binder. A new
/// kind plugs in by implementing the trait + flipping one arm of [`bind_for_kind`] — the callsite
/// (`reconcile`) does not change. Mirrors the `ReviewEngine` seam (`review/engine.rs`): monomorphic
/// (no `dyn`), selected by an exhaustive `match` that names the concrete binder type.
trait ListenerBinder {
    /// Bind this kind's runtime on loopback `port`, returning a control handle, or `None` on failure
    /// (→ status `Error`, fail-closed). Method-generic over the Tauri runtime so the trait needs no
    /// object-safety and dispatch stays static.
    fn bind<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        port: u16,
    ) -> Option<BoundListener>;
}

/// The sole real binder this PR: mounts the local-api router (`review::local_api::build_router`) on the
/// bound loopback socket. Other kinds have no binder yet (`remote-web` / `terminal` need AB#1073,
/// `event-ingress` needs the tunnel runtime) — see [`bind_for_kind`].
struct LocalApiBinder;

impl ListenerBinder for LocalApiBinder {
    fn bind<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        port: u16,
    ) -> Option<BoundListener> {
        bind_loopback(app, port, |app, port| {
            build_router(Arc::new(Ctx { app, port }))
        })
    }
}

/// Dispatch a listener kind to its [`ListenerBinder`]. **Hard carrier:** the `match` over the sealed
/// [`ListenerKind`] is exhaustive with NO wildcard — a new kind without an arm is a COMPILE ERROR, never
/// a silent no-bind (this preserves the exact compile-time guarantee the pre-seam single `match` had;
/// the seam must not regress it). Kinds with no runtime yet return `None`; in practice they never reach
/// here (`desired_listeners` excludes them via `classify`), so the `None` is fail-closed defence-in-depth.
///
/// `classify` (R-free) is the source for bindability + the `Unsupported` reason; this (R-specific) is
/// the source for WHICH binder. The two agree on the bindable set — pinned by
/// `binder_registry_consistent_with_classify` (**Medium** carrier) per `.claude/rules/prmonitor/ai-robust.md`.
fn bind_for_kind<R: tauri::Runtime>(
    kind: ListenerKind,
    app: &tauri::AppHandle<R>,
    port: u16,
) -> Option<BoundListener> {
    match kind {
        ListenerKind::LocalApi => LocalApiBinder.bind(app, port),
        ListenerKind::EventIngress | ListenerKind::RemoteWeb | ListenerKind::Terminal => None,
    }
}

/// Generic loopback bind + axum serve scaffolding shared by every axum-based [`ListenerBinder`]:
/// synchronously bind `127.0.0.1:port` (std, works outside a runtime so the caller knows success
/// immediately) with bounded retry, convert to a tokio listener SYNCHRONOUSLY (so success is confirmed
/// before any `BoundListener` exists — no phantom `Bound` status, F2), then hand the socket to a spawned
/// serve task. The binder supplies the router via `make_router(app, port)`; the per-kind mount is the
/// ONLY thing that varies. Returns `None` (→ status `Error`) if the bind or the `from_std` conversion
/// ultimately fails.
///
/// Called (via [`bind_for_kind`]) from `reconcile`, which runs on a sync command/setup thread (NOT
/// inside the async runtime), so `block_on` here is safe — it enters the runtime context just long
/// enough for the reactor to register the socket; the conversion itself is instant.
fn bind_loopback<R, F>(
    app: &tauri::AppHandle<R>,
    port: u16,
    make_router: F,
) -> Option<BoundListener>
where
    R: tauri::Runtime,
    F: FnOnce(tauri::AppHandle<R>, u16) -> axum::Router + Send + 'static,
{
    let std_listener = bind_std_with_retry(port)?;
    if let Err(e) = std_listener.set_nonblocking(true) {
        eprintln!("Remote 监听运行时：端口 {port} set_nonblocking 失败，已跳过：{e}");
        return None;
    }
    // Convert to the tokio listener SYNCHRONOUSLY and confirm success BEFORE creating a
    // `BoundListener`. `from_std` must run inside the tokio runtime context for reactor
    // registration; `block_on` enters it (the conversion is instant). If it errs, no entry is
    // created → `status_snapshot` reports `Error`, never a phantom `Bound` (F2).
    let listener = match tauri::async_runtime::block_on(async {
        tokio::net::TcpListener::from_std(std_listener)
    }) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Remote 监听运行时：端口 {port} from_std 失败，已跳过：{e}");
            return None;
        }
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let app = app.clone();
    let server_task = spawn(async move {
        let router = make_router(app, port);
        if let Err(e) = axum::serve(listener, router.into_make_service())
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
        {
            eprintln!("Remote 监听运行时：端口 {port} serve 异常退出：{e}");
        }
    });
    Some(BoundListener {
        server_task,
        shutdown: shutdown_tx,
        port,
    })
}

/// Sync loopback bind with bounded retry (the std listener is later wrapped by `from_std`).
fn bind_std_with_retry(port: u16) -> Option<std::net::TcpListener> {
    let mut last_err = None;
    for attempt in 0..BIND_RETRIES {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => return Some(l),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < BIND_RETRIES {
                    std::thread::sleep(BIND_RETRY_DELAY);
                }
            }
        }
    }
    let reason = last_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "no bind attempt".to_string());
    eprintln!("Remote 监听运行时：端口 {port} 绑定失败，已跳过（{reason}）");
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{ListenerAuthMode, ListenerKind};

    fn listener(id: &str, kind: ListenerKind, host: &str, port: u16, enabled: bool) -> Listener {
        Listener {
            id: id.to_string(),
            name: "L".to_string(),
            kind,
            bind_host: host.to_string(),
            port,
            enabled,
            auth: ListenerAuthMode::None,
            allowed_origins: vec![],
            public_url: String::new(),
        }
    }

    // The fail-closed loopback gate (`is_loopback_host`) is the canonical single source in
    // `crate::config::model` and is unit-tested at its definition there; `classify` consumes it,
    // so the classify tests below exercise the gate end-to-end through the supervisor.

    // --- classify (loopback gate ahead of kind; kind exhaustiveness) ------------------------

    #[test]
    fn classify_local_api_loopback_is_bind() {
        assert!(matches!(
            classify(&listener(
                "a",
                ListenerKind::LocalApi,
                "127.0.0.1",
                8788,
                true
            )),
            Disposition::Bind
        ));
    }

    #[test]
    fn classify_local_api_non_loopback_is_blocked() {
        // The gate runs ahead of the kind: even local-api is blocked on a non-loopback host.
        assert!(matches!(
            classify(&listener(
                "a",
                ListenerKind::LocalApi,
                "0.0.0.0",
                8788,
                true
            )),
            Disposition::Blocked(_)
        ));
    }

    #[test]
    fn classify_other_kinds_are_unsupported() {
        for kind in [
            ListenerKind::RemoteWeb,
            ListenerKind::EventIngress,
            ListenerKind::Terminal,
        ] {
            assert!(matches!(
                classify(&listener("a", kind, "127.0.0.1", 9000, true)),
                Disposition::Unsupported(_)
            ));
        }
    }

    // --- status_of (classification → wire status) -------------------------------------------

    #[test]
    fn status_of_bound_when_port_present() {
        // token set → a bound local-api is fully usable → `Bound`.
        let s = status_of(
            &listener("local-api", ListenerKind::LocalApi, "127.0.0.1", 8788, true),
            Some(8788),
            true,
        );
        assert_eq!(s.state, ListenerState::Bound);
        assert!(s.bound);
        assert_eq!(s.bound_port, Some(8788));
    }

    #[test]
    fn status_of_bound_no_auth_when_token_empty() {
        // F8: a bound local-api with an EMPTY token reports `BoundNoAuth`, not `Bound` — every
        // request would 401 (`verify_bearer`), so bound ≠ usable. Still `bound=true` with a port
        // (it IS listening), but the state warns the token must be set.
        let s = status_of(
            &listener("local-api", ListenerKind::LocalApi, "127.0.0.1", 8788, true),
            Some(8788),
            false,
        );
        assert_eq!(s.state, ListenerState::BoundNoAuth);
        assert!(s.bound);
        assert_eq!(s.bound_port, Some(8788));
    }

    #[test]
    fn status_of_error_when_bindable_but_not_bound() {
        let s = status_of(
            &listener("local-api", ListenerKind::LocalApi, "127.0.0.1", 8788, true),
            None,
            true,
        );
        assert_eq!(s.state, ListenerState::Error);
        assert!(!s.bound);
        assert_eq!(s.bound_port, None);
    }

    #[test]
    fn status_of_blocked_for_non_loopback() {
        let s = status_of(
            &listener("a", ListenerKind::LocalApi, "0.0.0.0", 8788, true),
            None,
            true,
        );
        assert_eq!(s.state, ListenerState::BlockedNeeds1073);
        assert!(!s.bound);
    }

    #[test]
    fn status_of_unsupported_for_remote_web() {
        let s = status_of(
            &listener("a", ListenerKind::RemoteWeb, "127.0.0.1", 9000, true),
            None,
            true,
        );
        assert_eq!(s.state, ListenerState::Unsupported);
    }

    // --- desired_listeners + diff (the reconcile core) --------------------------------------

    #[test]
    fn desired_listeners_only_includes_enabled_bindable_loopback() {
        let listeners = vec![
            listener("ok", ListenerKind::LocalApi, "127.0.0.1", 8788, true),
            listener("disabled", ListenerKind::LocalApi, "127.0.0.1", 8789, false),
            listener("remote", ListenerKind::LocalApi, "0.0.0.0", 8790, true),
            listener("web", ListenerKind::RemoteWeb, "127.0.0.1", 8791, true),
        ];
        let d = desired_listeners(&listeners);
        assert_eq!(d.len(), 1);
        assert_eq!(
            d.get("ok").map(|x| (x.port, x.kind)),
            Some((8788, ListenerKind::LocalApi))
        );
    }

    #[test]
    fn desired_listeners_carries_bindable_kind_under_duplicate_id() {
        // F1 fix (#1382 review): under a duplicate listener id (reachable via the lenient startup load,
        // no `validate`), the desired entry must carry the BINDABLE listener's kind — a later
        // non-runtime duplicate (filtered out of `desired`) must NOT decouple kind from port. Pins the
        // funnel closed at the pure level (the reconcile-level proof is
        // `reconcile_binds_local_api_despite_duplicate_id_nonruntime_kind`).
        let listeners = vec![
            listener("dup", ListenerKind::LocalApi, "127.0.0.1", 8788, true),
            listener("dup", ListenerKind::RemoteWeb, "127.0.0.1", 9000, true),
        ];
        let d = desired_listeners(&listeners);
        assert_eq!(d.len(), 1);
        let entry = d.get("dup").expect("bindable local-api stays desired");
        assert_eq!(entry.kind, ListenerKind::LocalApi);
        assert_eq!(entry.port, 8788);
    }

    #[test]
    fn desired_listeners_excludes_enabled_local_api_with_port_zero() {
        // F1 runtime backstop: an enabled loopback local-api with port 0 reaches reconcile via the
        // lenient startup `config::service::load` (no `validate`). It must NOT be desired — otherwise
        // `bind_std_with_retry(0)` would bind a RANDOM OS port. Excluded → never bound.
        let listeners = vec![listener(
            "zero",
            ListenerKind::LocalApi,
            "127.0.0.1",
            0,
            true,
        )];
        assert!(
            desired_listeners(&listeners).is_empty(),
            "an enabled local-api with port 0 must be excluded from desired (no random-port bind)"
        );
    }

    #[test]
    fn status_of_error_for_enabled_local_api_with_port_zero() {
        // The flip side of the F1 backstop: since a port-0 local-api is never bound (None passed),
        // its status is `Error` (should-bind-but-isn't) — the fail-closed, user-visible outcome.
        let s = status_of(
            &listener("zero", ListenerKind::LocalApi, "127.0.0.1", 0, true),
            None,
            true,
        );
        assert_eq!(s.state, ListenerState::Error);
        assert!(!s.bound);
        assert_eq!(s.bound_port, None);
    }

    #[test]
    fn diff_adds_new_removes_absent_and_rebinds_changed() {
        let current: HashMap<String, u16> = [
            ("keep".to_string(), 1u16),
            ("gone".to_string(), 2),
            ("move".to_string(), 3),
        ]
        .into_iter()
        .collect();
        let dl = |port: u16| DesiredListener {
            port,
            kind: ListenerKind::LocalApi,
        };
        let desired: HashMap<String, DesiredListener> = [
            ("keep".to_string(), dl(1)),
            ("new".to_string(), dl(4)),
            ("move".to_string(), dl(5)),
        ]
        .into_iter()
        .collect();
        let (mut remove, mut add) = diff(&current, &desired);
        remove.sort();
        add.sort();
        assert_eq!(remove, vec!["gone".to_string(), "move".to_string()]);
        assert_eq!(add, vec!["move".to_string(), "new".to_string()]);
    }

    // --- bind_std_with_retry (CI-safe: ephemeral port) --------------------------------------

    #[test]
    fn bind_std_with_retry_fails_on_occupied_port() {
        // Grab an ephemeral port, keep it bound, then prove the helper cannot re-bind it.
        let occupier = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
        let port = occupier.local_addr().expect("local_addr").port();
        assert!(
            bind_std_with_retry(port).is_none(),
            "an occupied port must not re-bind"
        );
    }

    #[test]
    fn bind_std_with_retry_succeeds_on_free_port() {
        // Port 0 → the OS assigns a guaranteed-free ephemeral port (no reserve-then-release
        // TOCTOU race that could flake under concurrent test runs).
        assert!(
            bind_std_with_retry(0).is_some(),
            "binding to port 0 (OS-assigned free port) should always succeed"
        );
    }

    // --- status_snapshot (no-bound-entry paths; needs no live runtime) -----------------------
    // These cover every `status_snapshot` projection that does NOT require a bound entry; the
    // `Bound` path (which needs a real `BoundListener` with a `JoinHandle`) is covered by the
    // reconcile integration test below — we never fabricate a fake `BoundListener`.

    #[test]
    fn status_snapshot_empty_when_no_listeners() {
        let sup = ListenerSupervisor::default();
        assert!(sup.status_snapshot(&[], true).is_empty());
    }

    #[test]
    fn status_snapshot_omits_disabled_listeners() {
        let sup = ListenerSupervisor::default();
        let listeners = vec![
            listener("a", ListenerKind::LocalApi, "127.0.0.1", 8788, false),
            listener("b", ListenerKind::RemoteWeb, "127.0.0.1", 8789, false),
        ];
        assert!(
            sup.status_snapshot(&listeners, true).is_empty(),
            "disabled listeners are filtered out (no runtime)"
        );
    }

    #[test]
    fn status_snapshot_enabled_local_api_with_empty_runtimes_is_error() {
        // Enabled loopback local-api but nothing bound (empty runtimes) → it SHOULD be bound but
        // isn't → `Error`.
        let sup = ListenerSupervisor::default();
        let listeners = vec![listener(
            "local-api",
            ListenerKind::LocalApi,
            "127.0.0.1",
            8788,
            true,
        )];
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].state, ListenerState::Error);
        assert!(!snap[0].bound);
        assert_eq!(snap[0].bound_port, None);
    }

    #[test]
    fn status_snapshot_enabled_non_loopback_is_blocked() {
        let sup = ListenerSupervisor::default();
        let listeners = vec![listener(
            "remote",
            ListenerKind::LocalApi,
            "0.0.0.0",
            8788,
            true,
        )];
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].state, ListenerState::BlockedNeeds1073);
    }

    #[test]
    fn status_snapshot_enabled_remote_web_is_unsupported() {
        let sup = ListenerSupervisor::default();
        let listeners = vec![listener(
            "web",
            ListenerKind::RemoteWeb,
            "127.0.0.1",
            9000,
            true,
        )];
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].state, ListenerState::Unsupported);
    }

    // --- ListenerBinder seam (F26 / #1382): dispatch + binder + registry↔classify consistency ---

    #[test]
    fn bind_for_kind_non_runtime_kinds_return_none() {
        // The three kinds without a runtime binder return `None` from the dispatch WITHOUT binding any
        // socket (the `None` arm short-circuits before any bind). Pins that the seam never silently
        // binds a non-runtime kind — the Hard exhaustive `match` routes them to `None`, fail-closed.
        let app = tauri::test::mock_app();
        for kind in [
            ListenerKind::RemoteWeb,
            ListenerKind::EventIngress,
            ListenerKind::Terminal,
        ] {
            assert!(
                bind_for_kind(kind, app.handle(), 0).is_none(),
                "non-runtime kind {kind:?} must not bind"
            );
        }
    }

    #[test]
    fn local_api_binder_binds_loopback() {
        // The sole real binder this PR: `LocalApiBinder` mounts + binds the local-api router on a
        // loopback port. Reserve a free ephemeral port, release it, bind through the binder directly,
        // assert a live serve task, then clean up (mirrors the reconcile-integration CI-safe pattern;
        // no HTTP request, so the router's managed state / DB is never touched).
        let app = tauri::test::mock_app();
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
            l.local_addr().expect("local_addr").port()
        };
        let bound = LocalApiBinder
            .bind(app.handle(), port)
            .expect("local-api binder binds a free loopback port");
        assert_eq!(bound.port, port);
        assert!(
            !bound.server_task.inner().is_finished(),
            "serve task should be live right after a successful bind"
        );
        // Clean up the spawned serve task (graceful signal + abort).
        let _ = bound.shutdown.send(());
        bound.server_task.abort();
    }

    #[test]
    fn binder_registry_consistent_with_classify() {
        // Medium carrier (per `.claude/rules/prmonitor/ai-robust.md` §审查要求): the bindable SET must
        // agree between `classify` (R-free — bindability + Unsupported reason) and `bind_for_kind`
        // (R-specific — WHICH binder). The three non-runtime kinds are BOTH `Unsupported` (classify)
        // AND `None` (bind_for_kind); `local-api` is `Bind` (classify) AND `Some` (bind_for_kind). Both
        // matches are exhaustive (Hard), so a NEW kind forces an arm in each; this test pins that an
        // EXISTING kind can't be marked bindable in one and not the other — in BOTH directions, so a
        // regression that drops `bind_for_kind`'s `LocalApi` arm to `None` fails HERE (not only in the
        // reconcile integration test).
        let app = tauri::test::mock_app();
        for kind in [
            ListenerKind::RemoteWeb,
            ListenerKind::EventIngress,
            ListenerKind::Terminal,
        ] {
            let l = listener("x", kind, "127.0.0.1", 9000, true);
            assert!(
                matches!(classify(&l), Disposition::Unsupported(_)),
                "{kind:?} must classify Unsupported"
            );
            assert!(
                bind_for_kind(kind, app.handle(), 9000).is_none(),
                "{kind:?} must have no binder"
            );
        }
        // local-api: bindable in BOTH the R-free source (classify) AND the R-specific source
        // (bind_for_kind). Bind on port 0 (OS-assigned free port — CI-safe, no TOCTOU), assert a real
        // binder, then tear the spawned serve task down.
        assert!(
            matches!(
                classify(&listener(
                    "local-api",
                    ListenerKind::LocalApi,
                    "127.0.0.1",
                    8788,
                    true
                )),
                Disposition::Bind
            ),
            "local-api must classify Bind"
        );
        let bound = bind_for_kind(ListenerKind::LocalApi, app.handle(), 0)
            .expect("local-api must have a binder in bind_for_kind, not just Bind in classify");
        let _ = bound.shutdown.send(());
        bound.server_task.abort();
    }

    // --- reconcile integration (CI-safe: loopback only; user-approved, F24) ------------------
    // Drives a real bind→serve→teardown cycle through a tauri mock app: the only test that
    // exercises the `Bound` status path end-to-end (a real `BoundListener` with a live serve
    // task). No HTTP request is ever made, so the router's managed state / DB is never touched.

    #[test]
    fn reconcile_binds_then_tears_down_local_api() {
        let app = tauri::test::mock_app();

        // Reserve a free ephemeral loopback port, read it, then release it so reconcile can bind.
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
            l.local_addr().expect("local_addr").port()
        };

        let sup = ListenerSupervisor::default();
        let listeners = vec![listener(
            "local-api",
            ListenerKind::LocalApi,
            "127.0.0.1",
            port,
            true,
        )];

        // Bind: one entry, state == Bound, bound_port == the reserved port (token set → usable).
        sup.reconcile(app.handle(), &listeners);
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1, "exactly one enabled listener");
        assert_eq!(snap[0].state, ListenerState::Bound, "should be bound");
        assert!(snap[0].bound);
        assert_eq!(snap[0].bound_port, Some(port));

        // Teardown: reconcile to the empty set drops the runtime entry.
        sup.reconcile(app.handle(), &[]);
        assert!(
            sup.status_snapshot(&[], true).is_empty(),
            "reconcile to empty set tears the bound listener down"
        );

        // App-shutdown cleanup is idempotent after teardown.
        sup.shutdown();
    }

    #[test]
    fn reconcile_binds_local_api_despite_duplicate_id_nonruntime_kind() {
        // F1 regression (#1382 review): the startup reconcile loads config via the LENIENT
        // `config::service::load` (no `validate`), so a hand-edited / forward-compat config with a
        // DUPLICATE listener id reaches reconcile. A bindable local-api followed by a non-runtime kind
        // sharing its id must STILL bind: the binder kind must come from the SAME bindable listener the
        // desired set selected, never a second `id -> kind` map over ALL listeners (where the later
        // non-runtime duplicate wins and silently suppresses the local-api bind). Pre-fix this bound
        // nothing (local-api status → Error); post-fix the local-api binds.
        let app = tauri::test::mock_app();
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
            l.local_addr().expect("local_addr").port()
        };
        let sup = ListenerSupervisor::default();
        let listeners = vec![
            listener("dup", ListenerKind::LocalApi, "127.0.0.1", port, true),
            listener("dup", ListenerKind::RemoteWeb, "127.0.0.1", 9000, true),
        ];
        sup.reconcile(app.handle(), &listeners);
        let snap = sup.status_snapshot(&listeners, true);
        let local_api_bound = snap.iter().any(|s| {
            s.kind == ListenerKind::LocalApi
                && s.state == ListenerState::Bound
                && s.bound_port == Some(port)
        });
        assert!(
            local_api_bound,
            "a duplicate non-runtime kind must not suppress the bindable local-api bind"
        );
        sup.shutdown();
    }

    #[test]
    fn status_self_heals_when_serve_task_finished() {
        // F6: after a successful bind, force the spawned serve task to FINISH (abort it + wait for
        // the handle to observe completion), then assert the status falls to `Error` instead of a
        // phantom `Bound`. We reach into the private `runtimes` map (same module) to abort the live
        // task deterministically — mirrors webhook's `status_self_heals_when_tunnel_child_exited`.
        let app = tauri::test::mock_app();
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
            l.local_addr().expect("local_addr").port()
        };
        let sup = ListenerSupervisor::default();
        let listeners = vec![listener(
            "local-api",
            ListenerKind::LocalApi,
            "127.0.0.1",
            port,
            true,
        )];

        sup.reconcile(app.handle(), &listeners);
        assert_eq!(
            sup.status_snapshot(&listeners, true)[0].state,
            ListenerState::Bound,
            "precondition: a fresh bind is Bound"
        );

        // Kill the serve task and block until its `JoinHandle` reports finished — `abort` is async,
        // so poll `is_finished()` with a bounded wait (block_on a short sleep loop in the runtime).
        {
            let runtimes = sup.runtimes.lock().unwrap();
            runtimes["local-api"].server_task.abort();
        }
        tauri::async_runtime::block_on(async {
            for _ in 0..200 {
                {
                    let runtimes = sup.runtimes.lock().unwrap();
                    if runtimes["local-api"].server_task.inner().is_finished() {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("serve task did not finish after abort");
        });

        // The entry is still in the map, but its task is finished → status self-heals to Error.
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].state,
            ListenerState::Error,
            "a finished serve task must NOT report a phantom Bound"
        );
        assert!(!snap[0].bound);
        assert_eq!(snap[0].bound_port, None);

        sup.shutdown();
    }
}
