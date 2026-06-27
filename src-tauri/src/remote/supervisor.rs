//! Remote Access listener binding supervisor (AB#1225) — reconciles `config.listeners[]` to
//! live loopback listeners.
//!
//! This is the first runtime that turns the declarative AB#1064 listener model into real bound
//! ports. It is a composition horizontal (NOT a slice — it names `crate::review::local_api`
//! directly, like `dispatch.rs` glues `pr`→`review`); the slice-boundary test does not scan it.
//!
//! **Scope this PR (loopback-only, fail-closed):**
//!  - `local-api` and `terminal` are real binders. Both are mounted on the port the supervisor
//!    binds from `listeners[]`, keeping the config as the single source of truth.
//!  - A non-loopback `bindHost` is REFUSED (`BlockedNeeds1073`) for any kind — remote exposure
//!    must happen through `config.tunnels[]`. Whitelist, never blacklist (mirrors
//!    `local_api::security::host_allowed`).
//!  - `event-ingress` is reported `Unsupported` (no listener runtime yet), never
//!    bound.
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
//! new impl + one dispatch arm, NOT a change to the reconcile/diff/status core). The match is
//! exhaustive with NO wildcard, so a new `ListenerKind` without an arm is a COMPILE ERROR (**Hard**
//! carrier, unchanged from the pre-seam single `match`). `classify` stays the R-free
//! bindability+reason source; the two agree on the bindable set, pinned by
//! `binder_registry_consistent_with_classify` (**Medium** carrier).
//! See `.claude/rules/prmonitor/ai-robust.md` §审查要求 for the rating obligation.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tauri::async_runtime::{spawn, JoinHandle};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::oneshot;

use crate::config::model::{
    remote_web_auth_token_is_strong, terminal_auth_token_is_strong, Listener, ListenerKind, Tunnel,
};
use crate::model::WebhookTunnelMode;
use crate::review::local_api::{build_router, Ctx};

use super::status::{ListenerRuntimeStatus, ListenerState};

/// Bounded bind retry to ride out OS socket-release lag on a rapid restart (the same handling
/// the local-api / webhook receivers used). A final failure is logged + recorded as `Error`.
const BIND_RETRIES: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(20);
const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(8);

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
        ListenerKind::LocalApi | ListenerKind::RemoteWeb | ListenerKind::Terminal => {
            Disposition::Bind
        }
        ListenerKind::EventIngress => Disposition::Unsupported(
            "event-ingress 运行时待建（需隧道运行时，AB#1225 后续）".to_string(),
        ),
    }
}

/// Pure status projection (no app handle) given whether the listener is actually bound and whether
/// the local API token is set — the classification→status mapping, unit-tested directly.
///
/// `local_api_token_set` gates `local-api`; terminal uses the per-listener `authToken` strength.
fn status_of(
    l: &Listener,
    bound_port: Option<u16>,
    local_api_token_set: bool,
) -> ListenerRuntimeStatus {
    let (state, bound, port, message) = match classify(l) {
        Disposition::Blocked(msg) => (ListenerState::BlockedNeeds1073, false, None, msg),
        Disposition::Unsupported(msg) => (ListenerState::Unsupported, false, None, msg),
        Disposition::Bind => match bound_port {
            Some(p)
                if (l.kind == ListenerKind::LocalApi && !local_api_token_set)
                    || (l.kind == ListenerKind::Terminal
                        && !terminal_auth_token_is_strong(&l.auth_token))
                    || (l.kind == ListenerKind::RemoteWeb
                        && !remote_web_auth_token_is_strong(&l.auth_token)) =>
            {
                (
                    ListenerState::BoundNoAuth,
                    true,
                    Some(p),
                    bound_no_auth_message(l.kind, p),
                )
            }
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

fn bound_no_auth_message(kind: ListenerKind, port: u16) -> String {
    let field = match kind {
        ListenerKind::Terminal => "终端监听器 authToken",
        ListenerKind::RemoteWeb => "远程面板监听器 authToken",
        ListenerKind::LocalApi => "local API token",
        ListenerKind::EventIngress => "token",
    };
    format!(
        "已绑定 127.0.0.1:{port}，但 token 未设置或强度不足——请求将 401（请在设置中配置 {field}）"
    )
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
#[derive(Clone, Copy, PartialEq, Eq)]
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
/// desired entries' runtime identity. A kind change on an existing id must rebind even when the port
/// is unchanged; otherwise the old router remains mounted behind a new listener kind.
fn diff(
    current: &HashMap<String, DesiredListener>,
    desired: &HashMap<String, DesiredListener>,
) -> (Vec<String>, Vec<String>) {
    let remove = current
        .iter()
        .filter(|(id, current)| desired.get(*id) != Some(*current))
        .map(|(id, _)| id.clone())
        .collect();
    let add = desired
        .iter()
        .filter(|(id, desired)| current.get(*id) != Some(*desired))
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
    kind: ListenerKind,
}

/// The listener binding supervisor. Lives in [`crate::state::AppState`]; `Default` (no bound
/// listeners until [`reconcile`](Self::reconcile)); methods take `&self` (interior mutability).
#[derive(Default)]
pub struct ListenerSupervisor {
    runtimes: StdMutex<HashMap<String, BoundListener>>,
    tunnels: StdMutex<HashMap<String, BoundTunnel>>,
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
    pub fn reconcile<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        listeners: &[Listener],
        tunnels: &[Tunnel],
        cloudflared_bin: &str,
    ) {
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
            let dead: Vec<String> = runtimes
                .iter()
                .filter(|(_, b)| b.server_task.inner().is_finished())
                .map(|(id, _)| id.clone())
                .collect();
            let current: HashMap<String, DesiredListener> = runtimes
                .iter()
                .filter(|(id, _)| !dead.contains(id))
                .map(|(id, b)| {
                    (
                        id.clone(),
                        DesiredListener {
                            port: b.port,
                            kind: b.kind,
                        },
                    )
                })
                .collect();
            let (remove, add) = diff(&current, &desired);
            let removed = remove
                .into_iter()
                .chain(dead)
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
            if let Some(b) = bind_for_kind(kind, app, &id, port) {
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
        self.reconcile_tunnels(tunnels, cloudflared_bin);
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
    /// mutation here, keeping this read-only under the brief lock); the next `reconcile` drains the
    /// dead entry before diffing so the listener is eligible for a fresh bind.
    pub fn status_snapshot(
        &self,
        listeners: &[Listener],
        local_api_token_set: bool,
    ) -> Vec<ListenerRuntimeStatus> {
        let runtimes = self.runtimes.lock().unwrap_or_else(|p| p.into_inner());
        let tunnel_urls = self.tunnel_public_urls_by_listener();
        listeners
            .iter()
            .filter(|l| l.enabled)
            .map(|l| {
                let bound_port = runtimes
                    .get(&l.id)
                    .filter(|b| !b.server_task.inner().is_finished())
                    .map(|b| b.port);
                let mut status = status_of(l, bound_port, local_api_token_set);
                if status.bound {
                    if let Some(urls) = tunnel_urls.get(&l.id).filter(|urls| !urls.is_empty()) {
                        status.message =
                            format!("{}，公网 URL：{}", status.message, urls.join(", "));
                    }
                }
                status
            })
            .collect()
    }

    pub(crate) fn public_urls_for_listener(&self, listener_id: &str) -> Vec<String> {
        self.tunnel_public_urls_by_listener()
            .remove(listener_id)
            .unwrap_or_default()
    }

    fn tunnel_public_urls_by_listener(&self) -> HashMap<String, Vec<String>> {
        let tunnels = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for rt in tunnels.values() {
            let Some(url) = rt
                .public_url
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            else {
                continue;
            };
            out.entry(rt.desired.target_listener_id.clone())
                .or_default()
                .push(url);
        }
        out
    }

    /// App-shutdown cleanup (wired to `RunEvent::Exit`): fire each graceful-shutdown signal + abort
    /// the task so no listener outlives the app (same "软件关闭时一起关闭" contract as the others).
    pub fn shutdown(&self) {
        self.shutdown_tunnels();
        let mut runtimes = self.runtimes.lock().unwrap_or_else(|p| p.into_inner());
        for (_, b) in runtimes.drain() {
            let _ = b.shutdown.send(());
            b.server_task.abort();
        }
    }

    fn reconcile_tunnels(&self, tunnels: &[Tunnel], cloudflared_bin: &str) {
        let bound_ports: HashMap<String, u16> = self
            .runtimes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, b)| !b.server_task.inner().is_finished())
            .map(|(id, b)| (id.clone(), b.port))
            .collect();
        let desired: HashMap<String, TunnelDesired> = tunnels
            .iter()
            .filter(|t| t.enabled)
            .filter_map(|t| {
                let port = *bound_ports.get(&t.target_listener_id)?;
                Some((
                    t.id.clone(),
                    TunnelDesired {
                        mode: t.mode,
                        target_listener_id: t.target_listener_id.clone(),
                        target_port: port,
                        command: t.command.clone(),
                        public_url: t.public_url.clone(),
                    },
                ))
            })
            .collect();

        let add: Vec<(String, TunnelDesired)> = {
            let mut current = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
            let remove: Vec<String> = current
                .iter_mut()
                .filter_map(|(id, rt)| {
                    (desired.get(id) != Some(&rt.desired) || !rt.is_alive()).then(|| id.clone())
                })
                .collect();
            for id in &remove {
                if let Some(rt) = current.remove(id) {
                    rt.teardown();
                }
            }
            let add = desired
                .iter()
                .filter(|(id, d)| current.get(*id).map(|rt| &rt.desired) != Some(*d))
                .map(|(id, d)| (id.clone(), d.clone()))
                .collect();
            add
        };

        let mut started = Vec::new();
        for (id, desired) in add {
            if let Some(rt) = BoundTunnel::start(cloudflared_bin, desired) {
                started.push((id, rt));
            }
        }
        if !started.is_empty() {
            let mut current = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
            for (id, rt) in started {
                current.insert(id, rt);
            }
        }
    }

    fn shutdown_tunnels(&self) {
        let mut tunnels = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
        for (_, rt) in tunnels.drain() {
            rt.teardown();
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TunnelDesired {
    mode: WebhookTunnelMode,
    target_listener_id: String,
    target_port: u16,
    command: String,
    public_url: String,
}

struct BoundTunnel {
    desired: TunnelDesired,
    child: Option<Child>,
    drain_task: Option<JoinHandle<()>>,
    public_url: Arc<StdMutex<Option<String>>>,
}

impl BoundTunnel {
    fn start(cloudflared_bin: &str, desired: TunnelDesired) -> Option<Self> {
        match desired.mode {
            WebhookTunnelMode::Listener => Some(Self {
                public_url: Arc::new(StdMutex::new(non_empty_url(&desired.public_url))),
                desired,
                child: None,
                drain_task: None,
            }),
            WebhookTunnelMode::Command => {
                let (child, drain_task) = tauri::async_runtime::block_on(async {
                    spawn_custom_tunnel(&desired.command, desired.target_port)
                })
                .ok()?;
                Some(Self {
                    public_url: Arc::new(StdMutex::new(non_empty_url(&desired.public_url))),
                    desired,
                    child: Some(child),
                    drain_task: Some(drain_task),
                })
            }
            WebhookTunnelMode::Quick => {
                let (child, drain_task, public_url) = tauri::async_runtime::block_on(
                    spawn_quick_tunnel(cloudflared_bin, desired.target_port),
                )
                .ok()?;
                Some(Self {
                    public_url,
                    desired,
                    child: Some(child),
                    drain_task: Some(drain_task),
                })
            }
        }
    }

    fn teardown(mut self) {
        if let Some(task) = self.drain_task.take() {
            task.abort();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            spawn(async move {
                let _ = child.wait().await;
            });
        }
    }

    fn is_alive(&mut self) -> bool {
        !matches!(self.child.as_mut().map(Child::try_wait), Some(Ok(Some(_))))
    }
}

fn non_empty_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
        listener_id: &str,
        port: u16,
    ) -> Option<BoundListener>;
}

/// Mounts the local-api router (`review::local_api::build_router`) on the bound loopback socket.
/// Other bindable kinds have their own binder structs — see [`bind_for_kind`].
struct LocalApiBinder;

impl ListenerBinder for LocalApiBinder {
    fn bind<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        _listener_id: &str,
        port: u16,
    ) -> Option<BoundListener> {
        bind_loopback(app, port, ListenerKind::LocalApi, |app, port| {
            build_router(Arc::new(Ctx { app, port }))
        })
    }
}

struct TerminalBinder;

impl ListenerBinder for TerminalBinder {
    fn bind<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        listener_id: &str,
        port: u16,
    ) -> Option<BoundListener> {
        let listener_id = listener_id.to_string();
        bind_loopback(app, port, ListenerKind::Terminal, move |app, port| {
            crate::remote::terminal_http::build_router(Arc::new(
                crate::remote::terminal_http::Ctx {
                    app,
                    port,
                    listener_id,
                },
            ))
        })
    }
}

struct RemoteWebBinder;

impl ListenerBinder for RemoteWebBinder {
    fn bind<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        listener_id: &str,
        port: u16,
    ) -> Option<BoundListener> {
        let listener_id = listener_id.to_string();
        bind_loopback(app, port, ListenerKind::RemoteWeb, move |app, port| {
            crate::remote::remote_web_http::build_router(Arc::new(
                crate::remote::remote_web_http::Ctx {
                    app,
                    port,
                    listener_id,
                },
            ))
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
    listener_id: &str,
    port: u16,
) -> Option<BoundListener> {
    match kind {
        ListenerKind::LocalApi => LocalApiBinder.bind(app, listener_id, port),
        ListenerKind::RemoteWeb => RemoteWebBinder.bind(app, listener_id, port),
        ListenerKind::Terminal => TerminalBinder.bind(app, listener_id, port),
        ListenerKind::EventIngress => None,
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
    kind: ListenerKind,
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
        kind,
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

fn build_tunnel_command_argv(command: &str, port: u16) -> Option<(String, Vec<String>)> {
    let port = port.to_string();
    let mut tokens = command
        .split_whitespace()
        .map(|token| token.replace("{port}", &port));
    let program = tokens.next()?;
    Some((program, tokens.collect()))
}

fn spawn_custom_tunnel(
    command: &str,
    port: u16,
) -> crate::error::AppResult<(Child, JoinHandle<()>)> {
    let (program, args) = build_tunnel_command_argv(command, port)
        .ok_or_else(|| crate::error::AppError::new("command 不能为空".to_string()))?;
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        crate::error::AppError::new(format!("无法启动远程隧道命令（{program}）：{e}"))
    })?;
    let stderr = child.stderr.take();
    let drain_task = spawn(async move {
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while matches!(lines.next_line().await, Ok(Some(_))) {}
        }
    });
    Ok((child, drain_task))
}

async fn spawn_quick_tunnel(
    bin: &str,
    port: u16,
) -> crate::error::AppResult<(Child, JoinHandle<()>, Arc<StdMutex<Option<String>>>)> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "tunnel",
        "--no-autoupdate",
        "--url",
        &format!("http://127.0.0.1:{port}"),
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| crate::error::AppError::new(format!("无法启动 cloudflared：{e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| crate::error::AppError::new("cloudflared stderr 不可用".to_string()))?;
    let mut lines = BufReader::new(stderr).lines();
    let initial = tokio::time::timeout(TUNNEL_URL_TIMEOUT, scan_for_url(&mut lines))
        .await
        .ok()
        .flatten();
    if initial.is_none() {
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(crate::error::AppError::new(format!(
                "cloudflared 在解析公网 URL 前已退出（{exit}）"
            )));
        }
    }
    let public_url = Arc::new(StdMutex::new(initial));
    let drain_task = spawn(drain_scanning_url(lines, public_url.clone()));
    Ok((child, drain_task, public_url))
}

async fn scan_for_url(lines: &mut Lines<BufReader<ChildStderr>>) -> Option<String> {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = extract_trycloudflare_url(&line) {
            return Some(url);
        }
    }
    None
}

async fn drain_scanning_url(
    mut lines: Lines<BufReader<ChildStderr>>,
    public_url: Arc<StdMutex<Option<String>>>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        if public_url
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none()
        {
            if let Some(url) = extract_trycloudflare_url(&line) {
                *public_url.lock().unwrap_or_else(|p| p.into_inner()) = Some(url);
            }
        }
    }
}

fn extract_trycloudflare_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|part| part.starts_with("https://") && part.contains(".trycloudflare.com"))
        .map(|part| {
            part.trim_matches(|c: char| c == '|' || c == ',' || c == ';')
                .to_string()
        })
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
            auth_token: String::new(),
            terminal_read: false,
            terminal_write: false,
            terminal_create: false,
            terminal_admin: false,
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
    fn classify_event_ingress_is_unsupported() {
        assert!(matches!(
            classify(&listener(
                "a",
                ListenerKind::EventIngress,
                "127.0.0.1",
                9000,
                true
            )),
            Disposition::Unsupported(_)
        ));
    }

    #[test]
    fn classify_remote_web_loopback_is_bind() {
        assert!(matches!(
            classify(&listener(
                "web",
                ListenerKind::RemoteWeb,
                "127.0.0.1",
                9200,
                true
            )),
            Disposition::Bind
        ));
    }

    #[test]
    fn classify_terminal_loopback_is_bind() {
        assert!(matches!(
            classify(&listener(
                "terminal",
                ListenerKind::Terminal,
                "127.0.0.1",
                9100,
                true
            )),
            Disposition::Bind
        ));
    }

    #[test]
    fn classify_terminal_non_loopback_is_blocked() {
        assert!(matches!(
            classify(&listener(
                "terminal",
                ListenerKind::Terminal,
                "0.0.0.0",
                9100,
                true
            )),
            Disposition::Blocked(_)
        ));
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
    fn status_of_bound_no_auth_for_remote_web_short_token() {
        let mut l = listener("web", ListenerKind::RemoteWeb, "127.0.0.1", 9200, true);
        l.auth_token = "short".to_string();
        let s = status_of(&l, Some(9200), true);
        assert_eq!(s.state, ListenerState::BoundNoAuth);
        assert!(s.bound);
        assert_eq!(s.bound_port, Some(9200));
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
    fn status_of_error_for_unbound_remote_web() {
        let s = status_of(
            &listener("a", ListenerKind::RemoteWeb, "127.0.0.1", 9000, true),
            None,
            true,
        );
        assert_eq!(s.state, ListenerState::Error);
    }

    // --- desired_listeners + diff (the reconcile core) --------------------------------------

    #[test]
    fn desired_listeners_only_includes_enabled_bindable_loopback() {
        let listeners = vec![
            listener("ok", ListenerKind::LocalApi, "127.0.0.1", 8788, true),
            listener("disabled", ListenerKind::LocalApi, "127.0.0.1", 8789, false),
            listener("remote", ListenerKind::LocalApi, "0.0.0.0", 8790, true),
            listener("web", ListenerKind::RemoteWeb, "127.0.0.1", 8791, true),
            listener("terminal", ListenerKind::Terminal, "127.0.0.1", 8792, true),
        ];
        let d = desired_listeners(&listeners);
        assert_eq!(d.len(), 3);
        assert_eq!(
            d.get("ok").map(|x| (x.port, x.kind)),
            Some((8788, ListenerKind::LocalApi))
        );
        assert_eq!(
            d.get("web").map(|x| (x.port, x.kind)),
            Some((8791, ListenerKind::RemoteWeb))
        );
        assert_eq!(
            d.get("terminal").map(|x| (x.port, x.kind)),
            Some((8792, ListenerKind::Terminal))
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
            listener("dup", ListenerKind::EventIngress, "127.0.0.1", 9000, true),
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
        let dl = |port: u16| DesiredListener {
            port,
            kind: ListenerKind::LocalApi,
        };
        let current: HashMap<String, DesiredListener> = [
            ("keep".to_string(), dl(1)),
            ("gone".to_string(), dl(2)),
            ("move".to_string(), dl(3)),
        ]
        .into_iter()
        .collect();
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

    #[test]
    fn diff_rebinds_when_kind_changes_on_same_port() {
        let current: HashMap<String, DesiredListener> = [(
            "web".to_string(),
            DesiredListener {
                port: 9200,
                kind: ListenerKind::LocalApi,
            },
        )]
        .into_iter()
        .collect();
        let desired: HashMap<String, DesiredListener> = [(
            "web".to_string(),
            DesiredListener {
                port: 9200,
                kind: ListenerKind::RemoteWeb,
            },
        )]
        .into_iter()
        .collect();
        let (remove, add) = diff(&current, &desired);
        assert_eq!(remove, vec!["web".to_string()]);
        assert_eq!(add, vec!["web".to_string()]);
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

    #[test]
    fn command_tunnel_substitutes_target_listener_port() {
        let (program, args) = build_tunnel_command_argv(
            "cloudflared tunnel --url http://127.0.0.1:{port} run term",
            9100,
        )
        .expect("command parses");
        assert_eq!(program, "cloudflared");
        assert_eq!(
            args,
            vec![
                "tunnel".to_string(),
                "--url".to_string(),
                "http://127.0.0.1:9100".to_string(),
                "run".to_string(),
                "term".to_string()
            ]
        );
    }

    #[test]
    fn extract_trycloudflare_url_finds_quick_tunnel_banner() {
        assert_eq!(
            extract_trycloudflare_url("INF | https://abc.trycloudflare.com |"),
            Some("https://abc.trycloudflare.com".to_string())
        );
        assert_eq!(extract_trycloudflare_url("INF starting tunnel"), None);
    }

    #[test]
    fn command_tunnel_marks_exited_child_not_alive() {
        let desired = TunnelDesired {
            mode: WebhookTunnelMode::Command,
            target_listener_id: "local-api".to_string(),
            target_port: 9100,
            command: "true".to_string(),
            public_url: String::new(),
        };
        let mut rt = BoundTunnel::start("cloudflared", desired).expect("true command starts");
        for _ in 0..20 {
            if !rt.is_alive() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !rt.is_alive(),
            "exited command tunnel must self-heal as dead"
        );
    }

    #[test]
    fn command_tunnel_exposes_configured_public_url_for_listener() {
        let desired = TunnelDesired {
            mode: WebhookTunnelMode::Command,
            target_listener_id: "terminal".to_string(),
            target_port: 9100,
            command: "sleep 60".to_string(),
            public_url: "https://term.example.com".to_string(),
        };
        let sup = ListenerSupervisor::default();
        let rt = BoundTunnel::start("cloudflared", desired).expect("sleep command starts");
        sup.tunnels.lock().unwrap().insert("tun".to_string(), rt);

        assert_eq!(
            sup.public_urls_for_listener("terminal"),
            vec!["https://term.example.com".to_string()]
        );
        sup.shutdown();
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
    fn status_snapshot_enabled_remote_web_unbound_is_error() {
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
        assert_eq!(snap[0].state, ListenerState::Error);
    }

    // --- ListenerBinder seam (F26 / #1382): dispatch + binder + registry↔classify consistency ---

    #[test]
    fn bind_for_kind_non_runtime_kinds_return_none() {
        // The kind without a runtime binder returns `None` from the dispatch WITHOUT binding any
        // socket (the `None` arm short-circuits before any bind). Pins that the seam never silently
        // binds a non-runtime kind — the Hard exhaustive `match` routes them to `None`, fail-closed.
        let app = tauri::test::mock_app();
        assert!(
            bind_for_kind(ListenerKind::EventIngress, app.handle(), "x", 0).is_none(),
            "non-runtime kind EventIngress must not bind"
        );
    }

    #[test]
    fn local_api_binder_binds_loopback() {
        // This test covers `LocalApiBinder`: it mounts + binds the local-api router on a
        // loopback port. Reserve a free ephemeral port, release it, bind through the binder directly,
        // assert a live serve task, then clean up (mirrors the reconcile-integration CI-safe pattern;
        // no HTTP request, so the router's managed state / DB is never touched).
        let app = tauri::test::mock_app();
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
            l.local_addr().expect("local_addr").port()
        };
        let bound = LocalApiBinder
            .bind(app.handle(), "local-api", port)
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
        // AND `None` (bind_for_kind); bindable kinds are `Bind` (classify) AND `Some` (bind_for_kind). Both
        // matches are exhaustive (Hard), so a NEW kind forces an arm in each; this test pins that an
        // EXISTING kind can't be marked bindable in one and not the other — in BOTH directions, so a
        // regression that drops `bind_for_kind`'s `LocalApi` arm to `None` fails HERE (not only in the
        // reconcile integration test).
        let app = tauri::test::mock_app();
        let l = listener("x", ListenerKind::EventIngress, "127.0.0.1", 9000, true);
        assert!(
            matches!(classify(&l), Disposition::Unsupported(_)),
            "EventIngress must classify Unsupported"
        );
        assert!(
            bind_for_kind(ListenerKind::EventIngress, app.handle(), "x", 9000).is_none(),
            "EventIngress must have no binder"
        );
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
        let bound = bind_for_kind(ListenerKind::LocalApi, app.handle(), "local-api", 0)
            .expect("local-api must have a binder in bind_for_kind, not just Bind in classify");
        let _ = bound.shutdown.send(());
        bound.server_task.abort();

        assert!(
            matches!(
                classify(&listener(
                    "web",
                    ListenerKind::RemoteWeb,
                    "127.0.0.1",
                    9200,
                    true
                )),
                Disposition::Bind
            ),
            "remote-web must classify Bind"
        );
        let bound = bind_for_kind(ListenerKind::RemoteWeb, app.handle(), "web", 0)
            .expect("remote-web must have a binder in bind_for_kind");
        let _ = bound.shutdown.send(());
        bound.server_task.abort();

        assert!(
            matches!(
                classify(&listener(
                    "terminal",
                    ListenerKind::Terminal,
                    "127.0.0.1",
                    9100,
                    true
                )),
                Disposition::Bind
            ),
            "terminal must classify Bind"
        );
        let bound = bind_for_kind(ListenerKind::Terminal, app.handle(), "terminal", 0)
            .expect("terminal must have a binder in bind_for_kind");
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
        sup.reconcile(app.handle(), &listeners, &[], "cloudflared");
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1, "exactly one enabled listener");
        assert_eq!(snap[0].state, ListenerState::Bound, "should be bound");
        assert!(snap[0].bound);
        assert_eq!(snap[0].bound_port, Some(port));

        // Teardown: reconcile to the empty set drops the runtime entry.
        sup.reconcile(app.handle(), &[], &[], "cloudflared");
        assert!(
            sup.status_snapshot(&[], true).is_empty(),
            "reconcile to empty set tears the bound listener down"
        );

        // App-shutdown cleanup is idempotent after teardown.
        sup.shutdown();
    }

    #[test]
    fn reconcile_binds_then_tears_down_terminal() {
        let app = tauri::test::mock_app();
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral bind");
            l.local_addr().expect("local_addr").port()
        };

        let sup = ListenerSupervisor::default();
        let listeners = vec![Listener {
            auth_token: "terminal-token-0123456789".to_string(),
            terminal_read: true,
            ..listener("terminal", ListenerKind::Terminal, "127.0.0.1", port, true)
        }];

        sup.reconcile(app.handle(), &listeners, &[], "cloudflared");
        let snap = sup.status_snapshot(&listeners, true);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].state, ListenerState::Bound);
        assert!(snap[0].bound);
        assert_eq!(snap[0].bound_port, Some(port));

        sup.reconcile(app.handle(), &[], &[], "cloudflared");
        assert!(sup.status_snapshot(&[], true).is_empty());
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
            listener("dup", ListenerKind::EventIngress, "127.0.0.1", 9000, true),
        ];
        sup.reconcile(app.handle(), &listeners, &[], "cloudflared");
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

        sup.reconcile(app.handle(), &listeners, &[], "cloudflared");
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

        sup.reconcile(app.handle(), &listeners, &[], "cloudflared");
        assert_eq!(
            sup.status_snapshot(&listeners, true)[0].state,
            ListenerState::Bound,
            "same-config reconcile must drain dead runtime and re-bind"
        );

        sup.shutdown();
    }

    #[test]
    fn reconcile_restarts_dead_command_tunnel() {
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
        let tunnels = vec![Tunnel {
            id: "tun".to_string(),
            name: "Tunnel".to_string(),
            mode: WebhookTunnelMode::Command,
            target_listener_id: "local-api".to_string(),
            command: "sleep 60".to_string(),
            public_url: String::new(),
            enabled: true,
        }];

        sup.reconcile(app.handle(), &listeners, &tunnels, "cloudflared");
        {
            let mut tunnels = sup.tunnels.lock().unwrap();
            let rt = tunnels.get_mut("tun").expect("tunnel starts");
            assert!(rt.is_alive(), "precondition: sleep tunnel is alive");
            rt.child
                .as_mut()
                .expect("child")
                .start_kill()
                .expect("kill child");
        }
        tauri::async_runtime::block_on(async {
            for _ in 0..200 {
                {
                    let mut tunnels = sup.tunnels.lock().unwrap();
                    if !tunnels.get_mut("tun").expect("tunnel").is_alive() {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("tunnel child did not exit after kill");
        });

        sup.reconcile(app.handle(), &listeners, &tunnels, "cloudflared");
        {
            let mut tunnels = sup.tunnels.lock().unwrap();
            assert!(
                tunnels.get_mut("tun").expect("tunnel restarted").is_alive(),
                "same-config reconcile must restart dead tunnel child"
            );
        }
        sup.shutdown();
    }
}
