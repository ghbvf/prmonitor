//! Remote Access entrypoint supervisor (#1553).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use tauri::async_runtime::{spawn, JoinHandle};
use tauri::Manager;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::oneshot;
use url::Url;

use crate::config::model::{
    normalize_route_path, route_paths_conflict, terminal_auth_token_is_strong, RemoteAccessConfig,
    RemoteCapability, RemoteEntrypoint, RemoteTunnel, RemoteTunnelMode, SourcePolicyMode,
};
use crate::config::service::{
    managed_cloudflared_for_program, tunnel_command_uses_bare_cloudflared, ResolvedCli,
};
use crate::db::Database;
use crate::messaging::local_api::build_router as build_messaging_local_api_router;
use crate::review::local_api::build_router as build_review_local_api_router;
use crate::state::AppState;

use super::status::{
    RemoteAccessRuntimeStatus, RemoteEntrypointRuntimeStatus, RemoteEntrypointState,
    RemoteRouteRuntimeStatus, RemoteTunnelRuntimeStatus, RemoteTunnelState,
};

const BIND_RETRIES: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(20);
const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(8);
const TUNNEL_LOG_LIMIT: usize = 200;
const TUNNEL_LOG_LINE_LIMIT: usize = 4096;

#[derive(Clone, PartialEq, Eq)]
struct EntrypointDesired {
    bind_host: String,
    port: u16,
    signature: String,
    entrypoint: RemoteEntrypoint,
    mcp_mount: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct TunnelDesired {
    mode: RemoteTunnelMode,
    target_entrypoint_id: String,
    target_bind_host: String,
    target_port: u16,
    lan_bind_host: String,
    lan_port: u16,
    command: String,
    public_url: String,
    signature: String,
    entrypoint: RemoteEntrypoint,
}

#[derive(Clone)]
struct EntrypointGateState<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    entrypoint: RemoteEntrypoint,
    extra_allowed_hosts: Vec<String>,
}

struct BoundEntrypoint {
    server_task: JoinHandle<()>,
    shutdown: oneshot::Sender<()>,
    bind_host: String,
    port: u16,
    signature: String,
}

pub struct BoundTunnel {
    desired: TunnelDesired,
    process_task: Option<JoinHandle<()>>,
    process_shutdown: Option<oneshot::Sender<()>>,
    server_task: Option<JoinHandle<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    drain_task: Option<JoinHandle<()>>,
    public_url: Arc<StdMutex<Option<String>>>,
    state: Arc<StdMutex<TunnelLifecycleState>>,
    logs: Arc<StdMutex<VecDeque<String>>>,
    /// Present when a managed cloudflared child was spawned successfully (`Quick`, or
    /// `Command` with a bare `cloudflared` program token). Keeping the launch credential on
    /// the runtime (instead of the supervisor) lets old and new CLI paths coexist until each
    /// individual child exits or is restarted.
    cloudflared_fingerprint: Option<String>,
}

struct TunnelLifecycleState {
    state: RemoteTunnelState,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerIdentity {
    effective_ip: IpAddr,
    forwarded: bool,
}

#[derive(Default)]
pub struct ListenerSupervisor {
    entrypoints: StdMutex<HashMap<String, BoundEntrypoint>>,
    tunnels: StdMutex<HashMap<String, BoundTunnel>>,
    reconcile_guard: StdMutex<()>,
}

impl ListenerSupervisor {
    pub fn reconcile<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        remote_access: &RemoteAccessConfig,
        cloudflared: crate::error::AppResult<Option<ResolvedCli>>,
    ) {
        let _serialize = self
            .reconcile_guard
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let desired = desired_entrypoints(remote_access);
        let (add, removed) = {
            let mut current = self.entrypoints.lock().unwrap_or_else(|p| p.into_inner());
            let dead: HashSet<String> = current
                .iter()
                .filter(|(_, bound)| bound.server_task.inner().is_finished())
                .map(|(id, _)| id.clone())
                .collect();
            let remove: Vec<String> = current
                .iter()
                .filter(|(id, bound)| {
                    desired.get(*id).is_none_or(|d| {
                        d.bind_host != bound.bind_host
                            || d.port != bound.port
                            || d.signature != bound.signature
                    })
                })
                .map(|(id, _)| id.clone())
                .chain(dead.iter().cloned())
                .collect();
            let add = desired
                .iter()
                .filter(|(id, d)| {
                    dead.contains(*id)
                        || current.get(*id).is_none_or(|bound| {
                            d.bind_host != bound.bind_host
                                || d.port != bound.port
                                || d.signature != bound.signature
                        })
                })
                .map(|(id, d)| (id.clone(), d.clone()))
                .collect::<Vec<_>>();
            let removed = remove
                .into_iter()
                .filter_map(|id| current.remove(&id))
                .collect::<Vec<_>>();
            (add, removed)
        };

        for bound in removed {
            let _ = bound.shutdown.send(());
            bound.server_task.abort();
        }

        let mut started = Vec::new();
        for (id, desired) in add {
            if let Some(bound) = bind_entrypoint(
                app,
                &desired.bind_host,
                desired.port,
                desired.entrypoint.clone(),
                desired.signature.clone(),
                desired.mcp_mount,
            ) {
                started.push((id, bound));
            }
        }
        if !started.is_empty() {
            let mut current = self.entrypoints.lock().unwrap_or_else(|p| p.into_inner());
            for (id, bound) in started {
                current.insert(id, bound);
            }
        }

        let (cloudflared, resolution_error) = match cloudflared {
            Ok(cloudflared) => (cloudflared, None),
            Err(error) => (None, Some(error.message)),
        };
        self.reconcile_tunnels(
            app,
            remote_access,
            cloudflared.as_ref(),
            resolution_error.as_deref(),
        );
    }

    pub fn status_snapshot(
        &self,
        remote_access: &RemoteAccessConfig,
        local_api_token_set: bool,
    ) -> RemoteAccessRuntimeStatus {
        let entrypoints = {
            let bound = self.entrypoints.lock().unwrap_or_else(|p| p.into_inner());
            remote_access
                .entrypoints
                .iter()
                .filter(|entrypoint| entrypoint.enabled)
                .map(|entrypoint| {
                    let bound_port = bound
                        .get(&entrypoint.id)
                        .filter(|b| !b.server_task.inner().is_finished())
                        .map(|b| b.port);
                    let has_unusable_route = entrypoint.routes.iter().any(|route| {
                        route.enabled
                            && match route.capability {
                                RemoteCapability::Terminal => {
                                    !terminal_auth_token_is_strong(&route.auth_token)
                                }
                                RemoteCapability::LocalApi => !local_api_token_set,
                                RemoteCapability::Messaging => false,
                            }
                    });
                    let (state, message) = match bound_port {
                        Some(port) if has_unusable_route => (
                            RemoteEntrypointState::BoundNoAuth,
                            format!(
                                "已绑定 {}:{port}，但至少一个 route token 未配置",
                                entrypoint.bind_host
                            ),
                        ),
                        Some(port) => (
                            RemoteEntrypointState::Bound,
                            format!("已绑定 {}:{port}", entrypoint.bind_host),
                        ),
                        None => (
                            RemoteEntrypointState::Error,
                            "绑定失败（端口被占用或不可用）".to_string(),
                        ),
                    };
                    RemoteEntrypointRuntimeStatus {
                        id: entrypoint.id.clone(),
                        bound: bound_port.is_some(),
                        bound_port,
                        state,
                        message,
                        routes: entrypoint
                            .routes
                            .iter()
                            .map(|route| RemoteRouteRuntimeStatus {
                                id: route.id.clone(),
                                path: route.path.clone(),
                                capability: route.capability,
                                enabled: route.enabled,
                            })
                            .collect(),
                    }
                })
                .collect()
        };

        let tunnels = {
            let bound = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
            remote_access
                .tunnels
                .iter()
                .filter(|tunnel| tunnel.enabled)
                .map(|tunnel| {
                    let (state, message, public_url, logs) = bound
                        .get(&tunnel.id)
                        .map(|rt| {
                            let lifecycle = rt.state.lock().unwrap_or_else(|p| p.into_inner());
                            let state = if rt
                                .server_task
                                .as_ref()
                                .is_some_and(|task| task.inner().is_finished())
                            {
                                RemoteTunnelState::Error
                            } else {
                                lifecycle.state
                            };
                            let message = if state == lifecycle.state {
                                lifecycle.message.clone()
                            } else {
                                tunnel_status_message(tunnel.mode, state)
                            };
                            (
                                state,
                                message,
                                rt.public_url
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .clone(),
                                rt.logs(),
                            )
                        })
                        .unwrap_or_else(|| {
                            (
                                RemoteTunnelState::Stopped,
                                tunnel_status_message(tunnel.mode, RemoteTunnelState::Stopped),
                                None,
                                Vec::new(),
                            )
                        });
                    RemoteTunnelRuntimeStatus {
                        id: tunnel.id.clone(),
                        mode: tunnel.mode,
                        target_entrypoint_id: tunnel.target_entrypoint_id.clone(),
                        state,
                        public_url,
                        message,
                        logs,
                    }
                })
                .collect()
        };

        RemoteAccessRuntimeStatus {
            entrypoints,
            tunnels,
        }
    }

    pub(crate) fn public_urls_for_entrypoint(&self, entrypoint_id: &str) -> Vec<String> {
        let tunnels = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
        tunnels
            .values()
            .filter(|rt| rt.desired.target_entrypoint_id == entrypoint_id)
            .filter_map(|rt| {
                rt.public_url
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            })
            .collect()
    }

    fn forwarded_public_urls_for_entrypoint(&self, entrypoint_id: &str) -> Vec<String> {
        let tunnels = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
        tunnels
            .values()
            .filter(|rt| rt.desired.target_entrypoint_id == entrypoint_id)
            .filter(|rt| rt.desired.mode != RemoteTunnelMode::Lan)
            .filter_map(|rt| {
                rt.public_url
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            })
            .collect()
    }

    pub fn shutdown(&self) {
        {
            let mut tunnels = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
            for (_, tunnel) in tunnels.drain() {
                tunnel.teardown();
            }
        }
        let mut entrypoints = self.entrypoints.lock().unwrap_or_else(|p| p.into_inner());
        for (_, bound) in entrypoints.drain() {
            let _ = bound.shutdown.send(());
            bound.server_task.abort();
        }
    }

    /// Fingerprints of the Quick tunnel processes that are alive right now.
    ///
    /// A set is required because a config save does not interrupt resident tunnels:
    /// a tunnel started after the save may use the new executable while another keeps
    /// running with the old one. Finished tasks are excluded immediately, even before
    /// the next reconcile removes their runtime entry.
    pub fn active_cloudflared_fingerprints(&self) -> Vec<String> {
        let mut fingerprints = self
            .tunnels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|runtime| runtime.is_active_managed_cloudflared())
            .filter_map(|runtime| runtime.cloudflared_fingerprint.clone())
            .collect::<Vec<_>>();
        fingerprints.sort();
        fingerprints.dedup();
        fingerprints
    }

    fn reconcile_tunnels<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        remote_access: &RemoteAccessConfig,
        cloudflared: Option<&ResolvedCli>,
        cloudflared_resolution_error: Option<&str>,
    ) {
        let live_ports: HashMap<String, (String, u16)> = self
            .entrypoints
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, bound)| !bound.server_task.inner().is_finished())
            .map(|(id, bound)| (id.clone(), (bound.bind_host.clone(), bound.port)))
            .collect();
        let desired = remote_access
            .tunnels
            .iter()
            .filter(|tunnel| tunnel.enabled)
            .filter_map(|tunnel| {
                let entrypoint = remote_access
                    .entrypoints
                    .iter()
                    .find(|entrypoint| {
                        entrypoint.id == tunnel.target_entrypoint_id && entrypoint.enabled
                    })?
                    .clone();
                let (target_bind_host, target_port) =
                    live_ports.get(&tunnel.target_entrypoint_id)?.clone();
                Some((
                    tunnel.id.clone(),
                    TunnelDesired {
                        mode: tunnel.mode,
                        target_entrypoint_id: tunnel.target_entrypoint_id.clone(),
                        target_bind_host,
                        target_port,
                        lan_bind_host: tunnel.bind_host.clone(),
                        lan_port: tunnel.port,
                        command: tunnel.command.clone(),
                        public_url: tunnel.public_url.clone(),
                        signature: tunnel_signature(tunnel, &entrypoint),
                        entrypoint,
                    },
                ))
            })
            .collect::<HashMap<_, _>>();

        let add = {
            let mut current = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
            let remove = current
                .iter()
                .filter(|(id, rt)| desired.get(*id) != Some(&rt.desired) || rt.needs_restart())
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &remove {
                if let Some(rt) = current.remove(id) {
                    rt.teardown();
                }
            }
            let add = desired
                .iter()
                .filter(|(id, d)| current.get(*id).map(|rt| &rt.desired) != Some(*d))
                .map(|(id, desired)| (id.clone(), desired.clone()))
                .collect::<Vec<_>>();
            add
        };

        let mut started = Vec::new();
        for (id, desired) in add {
            let runtime =
                BoundTunnel::start(app, cloudflared, cloudflared_resolution_error, desired);
            started.push((id, runtime));
        }
        if !started.is_empty() {
            let mut current = self.tunnels.lock().unwrap_or_else(|p| p.into_inner());
            for (id, rt) in started {
                current.insert(id, rt);
            }
        }
    }
}

fn desired_entrypoints(remote_access: &RemoteAccessConfig) -> HashMap<String, EntrypointDesired> {
    remote_access
        .entrypoints
        .iter()
        .filter(|entrypoint| entrypoint.enabled && entrypoint.port != 0)
        .filter_map(|entrypoint| {
            runtime_checked_entrypoint(entrypoint).map(|entrypoint| {
                let mcp_mount = mcp_mount_allowed(remote_access, &entrypoint);
                (
                    entrypoint.id.clone(),
                    EntrypointDesired {
                        bind_host: entrypoint.bind_host.clone(),
                        port: entrypoint.port,
                        signature: format!(
                            "{}\nmcp={mcp_mount}",
                            entrypoint_signature(&entrypoint)
                        ),
                        entrypoint,
                        mcp_mount,
                    },
                )
            })
        })
        .collect()
}

fn mcp_mount_allowed(remote_access: &RemoteAccessConfig, entrypoint: &RemoteEntrypoint) -> bool {
    entrypoint
        .bind_host
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
        && entrypoint
            .routes
            .iter()
            .any(|route| route.enabled && route.capability == RemoteCapability::LocalApi)
        && !remote_access
            .tunnels
            .iter()
            .any(|tunnel| tunnel.enabled && tunnel.target_entrypoint_id == entrypoint.id)
}

fn runtime_checked_entrypoint(entrypoint: &RemoteEntrypoint) -> Option<RemoteEntrypoint> {
    let mut checked = entrypoint.clone();
    let mut paths: Vec<String> = Vec::new();
    for route in checked.routes.iter_mut().filter(|route| route.enabled) {
        let path = match normalize_route_path(&route.path) {
            Ok(path) => path,
            Err(e) => {
                eprintln!(
                    "Remote Access：入口「{}」route「{}」路径非法，已跳过绑定：{}",
                    checked.name, route.name, e.message
                );
                return None;
            }
        };
        if paths
            .iter()
            .any(|existing| route_paths_conflict(existing, &path))
        {
            eprintln!(
                "Remote Access：入口「{}」routePath 冲突，已跳过绑定：{}",
                checked.name, path
            );
            return None;
        }
        route.path = path.clone();
        paths.push(path);
    }
    Some(checked)
}

fn entrypoint_signature(entrypoint: &RemoteEntrypoint) -> String {
    serde_json::to_string(entrypoint).unwrap_or_default()
}

fn tunnel_signature(tunnel: &RemoteTunnel, entrypoint: &RemoteEntrypoint) -> String {
    format!(
        "{}\n{}",
        serde_json::to_string(tunnel).unwrap_or_default(),
        entrypoint_signature(entrypoint)
    )
}

fn build_entrypoint_router<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    port: u16,
    entrypoint: RemoteEntrypoint,
    extra_allowed_hosts: Vec<String>,
    mcp_mount: bool,
) -> Router {
    let gate_state = Arc::new(EntrypointGateState {
        app: app.clone(),
        entrypoint: entrypoint.clone(),
        extra_allowed_hosts,
    });
    let mut router = Router::new();
    for route in entrypoint.routes.iter().filter(|route| route.enabled) {
        router = match route.capability {
            RemoteCapability::Terminal => router.nest(
                route.path.as_str(),
                crate::remote::terminal_http::build_router(Arc::new(
                    crate::remote::terminal_http::Ctx {
                        app: app.clone(),
                        port,
                        entrypoint_id: entrypoint.id.clone(),
                        route_id: route.id.clone(),
                        base_path: route.path.clone(),
                    },
                )),
            ),
            RemoteCapability::LocalApi => {
                let local_api = build_review_local_api_router(Arc::new(crate::make_local_api_ctx(
                    app.clone(),
                    port,
                    route.path.clone(),
                    Some(entrypoint.id.clone()),
                )))
                .merge(build_messaging_local_api_router(Arc::new(
                    crate::make_messaging_local_api_ctx(
                        app.clone(),
                        port,
                        Some(entrypoint.id.clone()),
                    ),
                )));
                let local_api = if mcp_mount {
                    local_api.merge(crate::messaging::mcp::build_router(app.clone()))
                } else {
                    local_api
                };
                router.nest(route.path.as_str(), local_api)
            }
            RemoteCapability::Messaging => router.nest(
                route.path.as_str(),
                crate::messaging::commands::build_router(Arc::new(
                    crate::messaging::commands::Ctx { app: app.clone() },
                )),
            ),
        };
    }
    router.layer(axum::middleware::from_fn_with_state(
        gate_state,
        remote_access_gate,
    ))
}

async fn remote_access_gate<R: tauri::Runtime>(
    State(state): State<Arc<EntrypointGateState<R>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let entrypoint = &state.entrypoint;
    let direct_ip = peer.ip();
    let identity = effective_peer_identity(req.headers(), direct_ip, entrypoint);
    let public_urls = public_urls_for_gate(&state.app, entrypoint.id.as_str());
    let forwarded_public_urls = forwarded_public_urls_for_gate(&state.app, entrypoint.id.as_str());
    if !host_allowed(
        req.headers().get(header::HOST),
        entrypoint,
        &state.extra_allowed_hosts,
        &public_urls,
    ) {
        audit_remote_denial(
            &state.app,
            entrypoint,
            req.headers(),
            req.uri().path(),
            direct_ip,
            identity.effective_ip,
            "host",
        );
        return Err(StatusCode::FORBIDDEN);
    }
    if !origin_allowed(
        req.headers().get(header::ORIGIN),
        entrypoint,
        &state.extra_allowed_hosts,
        &public_urls,
    ) {
        audit_remote_denial(
            &state.app,
            entrypoint,
            req.headers(),
            req.uri().path(),
            direct_ip,
            identity.effective_ip,
            "origin",
        );
        return Err(StatusCode::FORBIDDEN);
    }
    if !source_request_allowed(req.headers(), direct_ip, entrypoint, &forwarded_public_urls) {
        audit_remote_denial(
            &state.app,
            entrypoint,
            req.headers(),
            req.uri().path(),
            direct_ip,
            identity.effective_ip,
            "source",
        );
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(req).await)
}

fn source_request_allowed(
    headers: &HeaderMap,
    direct_ip: IpAddr,
    entrypoint: &RemoteEntrypoint,
    public_urls: &[String],
) -> bool {
    let identity = effective_peer_identity(headers, direct_ip, entrypoint);
    if public_tunnel_request(headers, public_urls) && !identity.forwarded {
        return false;
    }
    source_allowed(identity.effective_ip, entrypoint)
}

#[cfg(test)]
fn effective_peer_ip(
    headers: &HeaderMap,
    direct_ip: IpAddr,
    entrypoint: &RemoteEntrypoint,
) -> IpAddr {
    effective_peer_identity(headers, direct_ip, entrypoint).effective_ip
}

fn effective_peer_identity(
    headers: &HeaderMap,
    direct_ip: IpAddr,
    entrypoint: &RemoteEntrypoint,
) -> PeerIdentity {
    if !trusted_proxy_matches(direct_ip, entrypoint) {
        return PeerIdentity {
            effective_ip: direct_ip,
            forwarded: false,
        };
    }
    if let Some(effective_ip) = forwarded_peer_ip(headers, entrypoint) {
        return PeerIdentity {
            effective_ip,
            forwarded: true,
        };
    }
    PeerIdentity {
        effective_ip: direct_ip,
        forwarded: false,
    }
}

fn public_tunnel_request(headers: &HeaderMap, public_urls: &[String]) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(host_without_port)
    else {
        return false;
    };
    public_urls
        .iter()
        .filter_map(|url| Url::parse(url).ok())
        .filter_map(|url| url.host_str().map(str::to_string))
        .any(|allowed| host.eq_ignore_ascii_case(&allowed))
}

fn trusted_proxy_matches(direct_ip: IpAddr, entrypoint: &RemoteEntrypoint) -> bool {
    entrypoint
        .trusted_proxies
        .iter()
        .any(|rule| ip_matches_rule(direct_ip, rule))
}

fn forwarded_peer_ip(headers: &HeaderMap, entrypoint: &RemoteEntrypoint) -> Option<IpAddr> {
    header_ip_chain(headers, "forwarded")
        .or_else(|| header_ip_chain(headers, "x-forwarded-for"))
        .and_then(|chain| first_untrusted_forwarded_hop(&chain, entrypoint))
        .or_else(|| header_ip(headers, "x-real-ip"))
        .or_else(|| header_ip(headers, "cf-connecting-ip"))
}

fn first_untrusted_forwarded_hop(
    chain: &[IpAddr],
    entrypoint: &RemoteEntrypoint,
) -> Option<IpAddr> {
    chain
        .iter()
        .rev()
        .copied()
        .find(|ip| !trusted_proxy_matches(*ip, entrypoint))
}

fn header_ip_chain(headers: &HeaderMap, name: &'static str) -> Option<Vec<IpAddr>> {
    let value = headers.get(name)?.to_str().ok()?;
    let ips = match name {
        "forwarded" => parse_forwarded_header(value),
        "x-forwarded-for" => value
            .split(',')
            .filter_map(parse_forwarded_ip_value)
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    (!ips.is_empty()).then_some(ips)
}

fn header_ip(headers: &HeaderMap, name: &'static str) -> Option<IpAddr> {
    let value = headers.get(name)?.to_str().ok()?;
    match name {
        "x-real-ip" | "cf-connecting-ip" => parse_forwarded_ip_value(value),
        _ => None,
    }
}

fn parse_forwarded_header(value: &str) -> Vec<IpAddr> {
    value
        .split(',')
        .filter_map(|node| {
            node.split(';').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                if key.trim().eq_ignore_ascii_case("for") {
                    parse_forwarded_ip_value(value)
                } else {
                    None
                }
            })
        })
        .collect()
}

fn parse_forwarded_ip_value(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() || value.eq_ignore_ascii_case("unknown") || value.starts_with('_') {
        return None;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let (host, _) = rest.split_once(']')?;
        return host.parse::<IpAddr>().ok();
    }
    let host = value
        .rsplit_once(':')
        .filter(|(host, port)| !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()))
        .map(|(host, _)| host)
        .unwrap_or(value);
    host.parse::<IpAddr>().ok()
}

fn source_allowed(ip: IpAddr, entrypoint: &RemoteEntrypoint) -> bool {
    match entrypoint.source_policy.mode {
        SourcePolicyMode::Loopback => ip.is_loopback(),
        SourcePolicyMode::Lan => ip.is_loopback() || is_lan_ip(ip),
        SourcePolicyMode::Custom => entrypoint
            .source_policy
            .allow
            .iter()
            .any(|rule| ip_matches_rule(ip, rule)),
    }
}

fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.octets()[0..2] == [169, 254],
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn ip_matches_rule(ip: IpAddr, rule: &str) -> bool {
    let rule = rule.trim();
    if let Ok(exact) = rule.parse::<IpAddr>() {
        return exact == ip;
    }
    let Some((base, prefix)) = rule.split_once('/') else {
        return false;
    };
    let Ok(base) = base.parse::<IpAddr>() else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match (ip, base) {
        (IpAddr::V4(ip), IpAddr::V4(base)) if prefix <= 32 => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            (u32::from(ip) & mask) == (u32::from(base) & mask)
        }
        (IpAddr::V6(ip), IpAddr::V6(base)) if prefix <= 128 => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            (u128::from(ip) & mask) == (u128::from(base) & mask)
        }
        _ => false,
    }
}

fn host_allowed(
    host: Option<&HeaderValue>,
    entrypoint: &RemoteEntrypoint,
    extra_allowed_hosts: &[String],
    public_urls: &[String],
) -> bool {
    let Some(host) = host.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    host_allowed_str(
        host_without_port(host),
        entrypoint,
        extra_allowed_hosts,
        public_urls,
    )
}

fn host_allowed_str(
    host: &str,
    entrypoint: &RemoteEntrypoint,
    extra_allowed_hosts: &[String],
    public_urls: &[String],
) -> bool {
    if ["127.0.0.1", "localhost", "::1"]
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
    {
        return true;
    }
    if host.eq_ignore_ascii_case(entrypoint.bind_host.as_str()) {
        return true;
    }
    if matches!(entrypoint.bind_host.as_str(), "0.0.0.0" | "::") {
        return host.parse::<IpAddr>().is_ok();
    }
    if extra_allowed_hosts
        .iter()
        .any(|allowed| host_matches_bind_host(host, allowed))
    {
        return true;
    }
    if public_urls
        .iter()
        .filter_map(|url| Url::parse(url).ok())
        .filter_map(|url| url.host_str().map(str::to_string))
        .any(|allowed| host.eq_ignore_ascii_case(&allowed))
    {
        return true;
    }
    false
}

fn origin_allowed(
    origin: Option<&HeaderValue>,
    entrypoint: &RemoteEntrypoint,
    extra_allowed_hosts: &[String],
    public_urls: &[String],
) -> bool {
    let Some(origin) = origin.and_then(|value| value.to_str().ok()) else {
        return true;
    };
    entrypoint.allowed_origins.iter().any(|allowed| {
        allowed
            .trim_end_matches('/')
            .eq_ignore_ascii_case(origin.trim_end_matches('/'))
    }) || public_urls.iter().any(|allowed| {
        allowed
            .trim_end_matches('/')
            .eq_ignore_ascii_case(origin.trim_end_matches('/'))
    }) || Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host_allowed_str(&host, entrypoint, extra_allowed_hosts, public_urls))
}

fn host_matches_bind_host(host: &str, bind_host: &str) -> bool {
    host.eq_ignore_ascii_case(bind_host)
        || (matches!(bind_host, "0.0.0.0" | "::") && host.parse::<IpAddr>().is_ok())
}

fn public_urls_for_gate<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    entrypoint_id: &str,
) -> Vec<String> {
    app.try_state::<AppState>()
        .map(|state| state.remote.public_urls_for_entrypoint(entrypoint_id))
        .unwrap_or_default()
}

fn forwarded_public_urls_for_gate<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    entrypoint_id: &str,
) -> Vec<String> {
    app.try_state::<AppState>()
        .map(|state| {
            state
                .remote
                .forwarded_public_urls_for_entrypoint(entrypoint_id)
        })
        .unwrap_or_default()
}

fn host_without_port(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map(|(host, _)| host).unwrap_or(rest);
    }
    host.rsplit_once(':')
        .filter(|(host, port)| !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()))
        .map(|(host, _)| host)
        .unwrap_or(host)
}

fn audit_remote_denial<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    entrypoint: &RemoteEntrypoint,
    headers: &HeaderMap,
    request_path: &str,
    peer_ip: IpAddr,
    effective_ip: IpAddr,
    gate: &str,
) {
    let Some(db) = app.try_state::<Database>() else {
        return;
    };
    let (route, capability) = route_audit_fields(entrypoint, request_path);
    let host = header_str(headers, header::HOST.as_str())
        .unwrap_or("")
        .to_string();
    let origin = header_str(headers, header::ORIGIN.as_str())
        .unwrap_or("")
        .to_string();
    let _ = db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO remote_access_audit \
             (ts_ms, entrypoint_id, route, capability, gate, decision, peer_ip, effective_ip, host, origin) \
             VALUES (CAST(strftime('%s','now') AS INTEGER) * 1000, ?1, ?2, ?3, ?4, 'deny', ?5, ?6, ?7, ?8)",
            rusqlite::params![
                entrypoint.id.as_str(),
                route,
                capability,
                gate,
                peer_ip.to_string(),
                effective_ip.to_string(),
                host,
                origin
            ],
        )?;
        Ok(())
    });
}

fn route_audit_fields(entrypoint: &RemoteEntrypoint, request_path: &str) -> (String, &'static str) {
    entrypoint
        .routes
        .iter()
        .find(|route| {
            route.enabled
                && (request_path == route.path
                    || request_path
                        .strip_prefix(route.path.as_str())
                        .is_some_and(|rest| rest.starts_with('/')))
        })
        .map(|route| (route.path.clone(), capability_audit_label(route.capability)))
        .unwrap_or_else(|| (request_path.to_string(), "unknown"))
}

fn capability_audit_label(capability: RemoteCapability) -> &'static str {
    match capability {
        RemoteCapability::Terminal => "terminal",
        RemoteCapability::LocalApi => "local-api",
        RemoteCapability::Messaging => "messaging",
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn bind_entrypoint<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    bind_host: &str,
    port: u16,
    entrypoint: RemoteEntrypoint,
    signature: String,
    mcp_mount: bool,
) -> Option<BoundEntrypoint> {
    bind_entrypoint_with_extra_hosts(
        app,
        bind_host,
        port,
        entrypoint,
        signature,
        Vec::new(),
        mcp_mount,
    )
}

fn bind_std_with_retry(bind_host: &str, port: u16) -> Option<std::net::TcpListener> {
    let mut last_err = None;
    for attempt in 0..BIND_RETRIES {
        match std::net::TcpListener::bind((bind_host, port)) {
            Ok(listener) => return Some(listener),
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
    eprintln!(
        "Remote Access：{}:{port} 绑定失败，已跳过（{reason}）",
        bind_host
    );
    None
}

impl BoundTunnel {
    fn start<R: tauri::Runtime>(
        app: &tauri::AppHandle<R>,
        cloudflared: Option<&ResolvedCli>,
        cloudflared_resolution_error: Option<&str>,
        desired: TunnelDesired,
    ) -> Self {
        let logs = Arc::new(StdMutex::new(VecDeque::new()));
        let state = running_tunnel_state(desired.mode);
        match desired.mode {
            RemoteTunnelMode::Listener => {
                push_log(&logs, "listener mode: using externally managed tunnel");
                Self {
                    public_url: Arc::new(StdMutex::new(non_empty_url(&desired.public_url))),
                    state,
                    desired,
                    process_task: None,
                    process_shutdown: None,
                    server_task: None,
                    shutdown: None,
                    drain_task: None,
                    logs,
                    cloudflared_fingerprint: None,
                }
            }
            RemoteTunnelMode::Lan => {
                let Some(bound) = bind_entrypoint_with_extra_hosts(
                    app,
                    &desired.lan_bind_host,
                    desired.lan_port,
                    desired.entrypoint.clone(),
                    desired.signature.clone(),
                    vec![desired.lan_bind_host.clone()],
                    false,
                ) else {
                    let message = format!(
                        "LAN 隧道绑定失败：{}:{}",
                        desired.lan_bind_host, desired.lan_port
                    );
                    push_log(&logs, &message);
                    return failed_tunnel(desired, logs, message);
                };
                push_log(
                    &logs,
                    &format!(
                        "lan tunnel bound {}:{}",
                        desired.lan_bind_host, desired.lan_port
                    ),
                );
                Self {
                    public_url: Arc::new(StdMutex::new(Some(format!(
                        "http://{}:{}",
                        desired.lan_bind_host, desired.lan_port
                    )))),
                    state,
                    desired,
                    process_task: None,
                    process_shutdown: None,
                    server_task: Some(bound.server_task),
                    shutdown: Some(bound.shutdown),
                    drain_task: None,
                    logs,
                    cloudflared_fingerprint: None,
                }
            }
            RemoteTunnelMode::Command => {
                if tunnel_command_uses_bare_cloudflared(&desired.command) && cloudflared.is_none() {
                    let message = cloudflared_resolution_error
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            "未找到 cloudflared，请在「第三方 CLI」配置 cloudflaredPath".to_string()
                        });
                    push_log(&logs, &message);
                    return failed_tunnel(desired, logs, message);
                }
                let (child, drain_task, fingerprint) = match tauri::async_runtime::block_on(async {
                    spawn_custom_tunnel(
                        &desired.command,
                        desired.target_port,
                        cloudflared,
                        logs.clone(),
                    )
                    .await
                }) {
                    Ok(result) => result,
                    Err(e) => {
                        let message = e.message;
                        push_log(&logs, &message);
                        return failed_tunnel(desired, logs, message);
                    }
                };
                let public_url = Arc::new(StdMutex::new(non_empty_url(&desired.public_url)));
                let (process_shutdown, process_task) = spawn_tunnel_process_owner(
                    child,
                    state.clone(),
                    public_url.clone(),
                    logs.clone(),
                );
                Self {
                    public_url,
                    state,
                    desired,
                    process_task: Some(process_task),
                    process_shutdown: Some(process_shutdown),
                    server_task: None,
                    shutdown: None,
                    drain_task: Some(drain_task),
                    logs,
                    cloudflared_fingerprint: fingerprint,
                }
            }
            RemoteTunnelMode::Quick => {
                let Some(cloudflared) = cloudflared else {
                    let message = cloudflared_resolution_error
                        .map(str::to_string)
                        .unwrap_or_else(|| "未找到 cloudflared，请配置可执行文件".to_string());
                    push_log(&logs, &message);
                    return failed_tunnel(desired, logs, message);
                };
                let (child, drain_task, public_url) = match tauri::async_runtime::block_on(
                    spawn_quick_tunnel(cloudflared, desired.target_port, logs.clone()),
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        let message = e.message;
                        push_log(&logs, &message);
                        return failed_tunnel(desired, logs, message);
                    }
                };
                let (process_shutdown, process_task) = spawn_tunnel_process_owner(
                    child,
                    state.clone(),
                    public_url.clone(),
                    logs.clone(),
                );
                Self {
                    public_url,
                    state,
                    desired,
                    process_task: Some(process_task),
                    process_shutdown: Some(process_shutdown),
                    server_task: None,
                    shutdown: None,
                    drain_task: Some(drain_task),
                    logs,
                    cloudflared_fingerprint: Some(cloudflared.fingerprint().to_string()),
                }
            }
        }
    }

    fn teardown(mut self) {
        if let Some(shutdown) = self.process_shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        if let Some(task) = self.drain_task.take() {
            task.abort();
        }
        let _ = self.process_task.take();
    }

    fn needs_restart(&self) -> bool {
        match self.desired.mode {
            RemoteTunnelMode::Quick | RemoteTunnelMode::Command => self
                .process_task
                .as_ref()
                .is_none_or(|task| task.inner().is_finished()),
            RemoteTunnelMode::Lan => self
                .server_task
                .as_ref()
                .is_none_or(|task| task.inner().is_finished()),
            RemoteTunnelMode::Listener => false,
        }
    }

    fn is_active_managed_cloudflared(&self) -> bool {
        self.cloudflared_fingerprint.is_some()
            && self
                .process_task
                .as_ref()
                .is_some_and(|task| !task.inner().is_finished())
    }

    fn logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

fn running_tunnel_state(mode: RemoteTunnelMode) -> Arc<StdMutex<TunnelLifecycleState>> {
    Arc::new(StdMutex::new(TunnelLifecycleState {
        state: RemoteTunnelState::Running,
        message: tunnel_status_message(mode, RemoteTunnelState::Running),
    }))
}

fn failed_tunnel(
    desired: TunnelDesired,
    logs: Arc<StdMutex<VecDeque<String>>>,
    message: String,
) -> BoundTunnel {
    BoundTunnel {
        desired,
        process_task: None,
        process_shutdown: None,
        server_task: None,
        shutdown: None,
        drain_task: None,
        public_url: Arc::new(StdMutex::new(None)),
        state: Arc::new(StdMutex::new(TunnelLifecycleState {
            state: RemoteTunnelState::Error,
            message,
        })),
        logs,
        cloudflared_fingerprint: None,
    }
}

fn spawn_tunnel_process_owner(
    mut child: Child,
    state: Arc<StdMutex<TunnelLifecycleState>>,
    public_url: Arc<StdMutex<Option<String>>>,
    logs: Arc<StdMutex<VecDeque<String>>>,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let task = spawn(async move {
        tokio::select! {
            exit = child.wait() => {
                let message = match exit {
                    Ok(status) => format!("隧道进程退出：{status}"),
                    Err(e) => format!("隧道进程等待失败：{e}"),
                };
                push_log(&logs, &message);
                *public_url.lock().unwrap_or_else(|p| p.into_inner()) = None;
                let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                state.state = RemoteTunnelState::Error;
                state.message = message;
            }
            _ = shutdown_rx => {
                push_log(&logs, "stopping tunnel process");
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    });
    (shutdown_tx, task)
}

fn tunnel_status_message(mode: RemoteTunnelMode, state: RemoteTunnelState) -> String {
    match (mode, state) {
        (_, RemoteTunnelState::Running) => "运行中".to_string(),
        (_, RemoteTunnelState::Stopped) => "未运行".to_string(),
        (_, RemoteTunnelState::Error) => "异常退出或绑定失败".to_string(),
    }
}

fn non_empty_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn build_tunnel_command_argv(command: &str, port: u16) -> Option<(String, Vec<String>)> {
    let port = port.to_string();
    let mut tokens = command
        .split_whitespace()
        .map(|token| token.replace("{port}", &port));
    let program = tokens.next()?;
    Some((program, tokens.collect()))
}

async fn spawn_custom_tunnel(
    command: &str,
    port: u16,
    cloudflared: Option<&ResolvedCli>,
    logs: Arc<StdMutex<VecDeque<String>>>,
) -> crate::error::AppResult<(Child, JoinHandle<()>, Option<String>)> {
    let (program, args) = build_tunnel_command_argv(command, port)
        .ok_or_else(|| crate::error::AppError::new("command 不能为空".to_string()))?;
    let managed = managed_cloudflared_for_program(&program, cloudflared)?;
    let fingerprint = managed.map(|cli| cli.fingerprint().to_string());
    push_log(
        &logs,
        &format!(
            "starting command tunnel: {}",
            if managed.is_some() {
                "cloudflared"
            } else {
                program.as_str()
            }
        ),
    );
    let mut child = if let Some(cli) = managed {
        let mut cmd = cli.command();
        cmd.args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.spawn().map_err(|e| {
            crate::error::AppError::new(format!("无法启动远程隧道命令（cloudflared）：{e}"))
        })?
    } else {
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.spawn().map_err(|e| {
            crate::error::AppError::new(format!("无法启动远程隧道命令（{program}）：{e}"))
        })?
    };
    let stderr = child.stderr.take();
    let drain_task = spawn(async move {
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_log(&logs, &line);
            }
        }
    });
    Ok((child, drain_task, fingerprint))
}

async fn spawn_quick_tunnel(
    cloudflared: &ResolvedCli,
    port: u16,
    logs: Arc<StdMutex<VecDeque<String>>>,
) -> crate::error::AppResult<(Child, JoinHandle<()>, Arc<StdMutex<Option<String>>>)> {
    push_log(&logs, "starting cloudflared quick tunnel");
    let mut cmd = cloudflared.command();
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
    let initial = tokio::time::timeout(TUNNEL_URL_TIMEOUT, scan_for_url(&mut lines, logs.clone()))
        .await
        .ok()
        .flatten();
    let public_url = Arc::new(StdMutex::new(initial));
    let public_url_for_task = public_url.clone();
    let drain_task = spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            push_log(&logs, &line);
            if let Some(url) = extract_cloudflared_url(&line) {
                *public_url_for_task
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(url);
            }
        }
    });
    Ok((child, drain_task, public_url))
}

async fn scan_for_url(
    lines: &mut Lines<BufReader<ChildStderr>>,
    logs: Arc<StdMutex<VecDeque<String>>>,
) -> Option<String> {
    while let Ok(Some(line)) = lines.next_line().await {
        push_log(&logs, &line);
        if let Some(url) = extract_cloudflared_url(&line) {
            push_log(&logs, &format!("public URL parsed: {url}"));
            return Some(url);
        }
    }
    None
}

fn extract_cloudflared_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|token| token.trim_matches(|c| matches!(c, '"' | '\'' | ',')))
        .find(|token| token.starts_with("https://") && token.contains("trycloudflare.com"))
        .map(str::to_string)
}

fn push_log(logs: &Arc<StdMutex<VecDeque<String>>>, line: &str) {
    let line = sanitize_log_line(line);
    let mut logs = logs.lock().unwrap_or_else(|p| p.into_inner());
    if logs.len() >= TUNNEL_LOG_LIMIT {
        logs.pop_front();
    }
    logs.push_back(line);
}

fn sanitize_log_line(line: &str) -> String {
    let mut out = line.to_string();
    let lower = out.to_ascii_lowercase();
    for marker in [
        "authorization:",
        "authorization=",
        "bearer ",
        "token=",
        "access_token=",
        "auth_token=",
        "secret=",
    ] {
        if let Some(idx) = lower.find(marker) {
            out.truncate(idx + marker.len());
            out.push_str("[redacted]");
            return truncate_log_line(out);
        }
    }
    truncate_log_line(out)
}

fn truncate_log_line(mut line: String) -> String {
    if line.len() <= TUNNEL_LOG_LINE_LIMIT {
        return line;
    }
    let suffix = "...[truncated]";
    let max = TUNNEL_LOG_LINE_LIMIT.saturating_sub(suffix.len());
    let mut cut = max;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    line.truncate(cut);
    line.push_str(suffix);
    line
}

fn bind_entrypoint_with_extra_hosts<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    bind_host: &str,
    port: u16,
    entrypoint: RemoteEntrypoint,
    signature: String,
    extra_allowed_hosts: Vec<String>,
    mcp_mount: bool,
) -> Option<BoundEntrypoint> {
    let std_listener = bind_std_with_retry(bind_host, port)?;
    if let Err(e) = std_listener.set_nonblocking(true) {
        eprintln!(
            "Remote Access：{}:{port} set_nonblocking 失败，已跳过：{e}",
            bind_host
        );
        return None;
    }
    let listener = match tauri::async_runtime::block_on(async {
        tokio::net::TcpListener::from_std(std_listener)
    }) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!(
                "Remote Access：{}:{port} 转换 tokio listener 失败，已跳过：{e}",
                bind_host
            );
            return None;
        }
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let app = app.clone();
    let bind_host = bind_host.to_string();
    let bind_host_for_task = bind_host.clone();
    let server_task = spawn(async move {
        let router = build_entrypoint_router(app, port, entrypoint, extra_allowed_hosts, mcp_mount);
        if let Err(e) = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
        {
            eprintln!(
                "Remote Access：{}:{port} serve 异常退出：{e}",
                bind_host_for_task
            );
        }
    });
    Some(BoundEntrypoint {
        server_task,
        shutdown: shutdown_tx,
        bind_host,
        port,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{
            model::{RemoteRoute, SourcePolicy},
            service::{resolve_cli_from, CliPath, CliResolver, CliToolsConfig},
        },
        model::CliTool,
    };

    fn resolved_cloudflared(test_name: &str) -> (std::path::PathBuf, ResolvedCli) {
        let root = std::env::temp_dir().join(format!(
            "prmonitor-remote-cloudflared-{test_name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join(if cfg!(windows) {
            "cloudflared.exe"
        } else {
            "cloudflared"
        });
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tools = CliToolsConfig {
            cloudflared_path: CliPath::try_from(executable.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let resolved =
            resolve_cli_from(&CliResolver::default(), &tools, CliTool::Cloudflared, false).unwrap();
        (root, resolved)
    }

    fn entrypoint() -> RemoteEntrypoint {
        RemoteEntrypoint {
            id: "ep".to_string(),
            name: "Entry".to_string(),
            bind_host: "127.0.0.1".to_string(),
            port: 8788,
            enabled: true,
            source_policy: SourcePolicy::default(),
            allowed_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            routes: vec![RemoteRoute::local_api(), RemoteRoute::terminal()],
        }
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_quick_tunnel_uses_configured_cloudflared_and_tears_it_down() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            process::{Command, Stdio},
            thread,
            time::Instant,
        };

        let root = std::env::temp_dir().join(format!(
            "prmonitor-remote-quick-e2e-自定义 路径-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("cloudflared");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{0}/pid'\nprintf '%s\\n' \"$PATH\" > '{0}/path'\nprintf '%s\\n' \"$@\" > '{0}/args'\nprintf '%s\\n' 'INF https://configured.trycloudflare.com' >&2\nexec sleep 30\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let tools = CliToolsConfig {
            cloudflared_path: CliPath::try_from(executable.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let cloudflared =
            resolve_cli_from(&CliResolver::default(), &tools, CliTool::Cloudflared, false).unwrap();
        let fingerprint = cloudflared.fingerprint().to_string();

        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let remote_access = RemoteAccessConfig {
            entrypoints: vec![RemoteEntrypoint {
                port,
                routes: Vec::new(),
                ..entrypoint()
            }],
            tunnels: vec![RemoteTunnel {
                id: "quick".to_string(),
                name: "Quick".to_string(),
                mode: RemoteTunnelMode::Quick,
                target_entrypoint_id: "ep".to_string(),
                enabled: true,
                ..RemoteTunnel::default()
            }],
        };
        let app = tauri::test::mock_app();
        let supervisor = ListenerSupervisor::default();

        supervisor.reconcile(app.handle(), &remote_access, Ok(Some(cloudflared)));

        let status = supervisor.status_snapshot(&remote_access, true);
        assert_eq!(status.tunnels.len(), 1);
        assert_eq!(status.tunnels[0].state, RemoteTunnelState::Running);
        assert_eq!(
            status.tunnels[0].public_url.as_deref(),
            Some("https://configured.trycloudflare.com")
        );
        assert_eq!(
            supervisor.active_cloudflared_fingerprints(),
            vec![fingerprint]
        );
        let path = fs::read_to_string(root.join("path")).unwrap();
        assert!(!std::env::split_paths(path.trim()).any(|entry| entry == root));
        assert_eq!(
            fs::read_to_string(root.join("args"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec![
                "tunnel",
                "--no-autoupdate",
                "--url",
                &format!("http://127.0.0.1:{port}"),
            ]
        );

        let pid = fs::read_to_string(root.join("pid")).unwrap();
        let pid = pid.trim().to_string();
        supervisor.shutdown();
        assert!(supervisor.active_cloudflared_fingerprints().is_empty());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && Command::new("kill")
                .args(["-0", pid.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !Command::new("kill")
                .args(["-0", pid.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "cloudflared child must be killed and reaped during teardown"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_command_tunnel_uses_resolved_cloudflared_for_bare_program() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };

        let root = std::env::temp_dir().join(format!(
            "prmonitor-remote-command-bare-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("cloudflared");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nprintf '%s\\n' \"$0\" > '{0}/argv0'\nprintf '%s\\n' \"$@\" > '{0}/args'\nprintf '%s\\n' \"$$\" > '{0}/pid'\nexec sleep 30\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let tools = CliToolsConfig {
            cloudflared_path: CliPath::try_from(executable.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let cloudflared =
            resolve_cli_from(&CliResolver::default(), &tools, CliTool::Cloudflared, false).unwrap();
        let fingerprint = cloudflared.fingerprint().to_string();

        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let remote_access = RemoteAccessConfig {
            entrypoints: vec![RemoteEntrypoint {
                port,
                routes: Vec::new(),
                ..entrypoint()
            }],
            tunnels: vec![RemoteTunnel {
                id: "command".to_string(),
                name: "Command".to_string(),
                mode: RemoteTunnelMode::Command,
                target_entrypoint_id: "ep".to_string(),
                enabled: true,
                command: "cloudflared tunnel run --token test-token".to_string(),
                public_url: "https://prmonitor.example.com".to_string(),
                ..RemoteTunnel::default()
            }],
        };
        let app = tauri::test::mock_app();
        let supervisor = ListenerSupervisor::default();

        supervisor.reconcile(app.handle(), &remote_access, Ok(Some(cloudflared)));

        let status = supervisor.status_snapshot(&remote_access, true);
        assert_eq!(status.tunnels.len(), 1);
        assert_eq!(
            status.tunnels[0].state,
            RemoteTunnelState::Running,
            "command tunnel should start: {} {:?}",
            status.tunnels[0].message,
            status.tunnels[0].logs
        );
        assert_eq!(
            status.tunnels[0].public_url.as_deref(),
            Some("https://prmonitor.example.com")
        );
        assert_eq!(
            supervisor.active_cloudflared_fingerprints(),
            vec![fingerprint]
        );
        let argv0_path = root.join("argv0");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !argv0_path.exists() {
            thread::sleep(Duration::from_millis(20));
        }
        let argv0 = fs::read_to_string(&argv0_path).unwrap();
        assert!(
            argv0.contains(executable.to_string_lossy().as_ref()),
            "bare cloudflared must exec configured path, got {argv0}"
        );
        assert_eq!(
            fs::read_to_string(root.join("args"))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec!["tunnel", "run", "--token", "test-token"]
        );

        let pid = fs::read_to_string(root.join("pid")).unwrap();
        let pid = pid.trim().to_string();
        supervisor.shutdown();
        assert!(supervisor.active_cloudflared_fingerprints().is_empty());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && Command::new("kill")
                .args(["-0", pid.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !Command::new("kill")
                .args(["-0", pid.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "command-mode cloudflared child must be killed during teardown"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reconcile_command_tunnel_preserves_cloudflared_resolution_error() {
        use crate::error::AppError;

        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let remote_access = RemoteAccessConfig {
            entrypoints: vec![RemoteEntrypoint {
                port,
                routes: Vec::new(),
                ..entrypoint()
            }],
            tunnels: vec![RemoteTunnel {
                id: "command".to_string(),
                name: "Command".to_string(),
                mode: RemoteTunnelMode::Command,
                target_entrypoint_id: "ep".to_string(),
                enabled: true,
                command: "cloudflared tunnel run --token test-token".to_string(),
                public_url: "https://prmonitor.example.com".to_string(),
                ..RemoteTunnel::default()
            }],
        };
        let app = tauri::test::mock_app();
        let supervisor = ListenerSupervisor::default();
        let resolution = "未找到 cloudflared：path missing: /bad/cloudflared";

        supervisor.reconcile(
            app.handle(),
            &remote_access,
            Err(AppError::new(resolution.to_string())),
        );

        let status = supervisor.status_snapshot(&remote_access, true);
        assert_eq!(status.tunnels.len(), 1);
        assert_eq!(status.tunnels[0].state, RemoteTunnelState::Error);
        assert_eq!(
            status.tunnels[0].message, resolution,
            "command+bare must surface resolve diagnostics, not a generic fallback"
        );
        supervisor.shutdown();
    }

    #[test]
    fn status_snapshot_reports_bound_entrypoint_routes_and_stopped_tunnel() {
        let supervisor = ListenerSupervisor::default();
        let entrypoint = entrypoint();
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let server_task = spawn(async move {
            let _ = shutdown_rx.await;
        });
        supervisor.entrypoints.lock().unwrap().insert(
            entrypoint.id.clone(),
            BoundEntrypoint {
                server_task,
                shutdown,
                bind_host: entrypoint.bind_host.clone(),
                port: entrypoint.port,
                signature: "sig".to_string(),
            },
        );

        let remote_access = RemoteAccessConfig {
            entrypoints: vec![entrypoint],
            tunnels: vec![RemoteTunnel {
                id: "lan".to_string(),
                name: "LAN".to_string(),
                mode: RemoteTunnelMode::Lan,
                target_entrypoint_id: "ep".to_string(),
                enabled: true,
                ..RemoteTunnel::default()
            }],
        };

        let status = supervisor.status_snapshot(&remote_access, false);

        assert_eq!(status.entrypoints.len(), 1);
        let entry_status = &status.entrypoints[0];
        assert!(entry_status.bound);
        assert_eq!(entry_status.bound_port, Some(8788));
        assert_eq!(entry_status.state, RemoteEntrypointState::BoundNoAuth);
        assert_eq!(
            entry_status
                .routes
                .iter()
                .map(|route| route.capability)
                .collect::<Vec<_>>(),
            vec![RemoteCapability::LocalApi, RemoteCapability::Terminal]
        );
        assert_eq!(status.tunnels.len(), 1);
        assert_eq!(status.tunnels[0].state, RemoteTunnelState::Stopped);
        assert_eq!(status.tunnels[0].target_entrypoint_id, "ep");
    }

    #[test]
    fn forwarded_public_urls_excludes_lan_tunnel_status_url() {
        let supervisor = ListenerSupervisor::default();
        let entrypoint = entrypoint();
        let lan_desired = TunnelDesired {
            mode: RemoteTunnelMode::Lan,
            target_entrypoint_id: entrypoint.id.clone(),
            target_bind_host: entrypoint.bind_host.clone(),
            target_port: entrypoint.port,
            lan_bind_host: "192.168.5.10".to_string(),
            lan_port: 8789,
            command: String::new(),
            public_url: String::new(),
            signature: "lan".to_string(),
            entrypoint: entrypoint.clone(),
        };
        let command_desired = TunnelDesired {
            mode: RemoteTunnelMode::Command,
            target_entrypoint_id: entrypoint.id.clone(),
            target_bind_host: entrypoint.bind_host.clone(),
            target_port: entrypoint.port,
            lan_bind_host: String::new(),
            lan_port: 0,
            command: "cloudflared tunnel run".to_string(),
            public_url: "https://prmonitor.example.com".to_string(),
            signature: "command".to_string(),
            entrypoint: entrypoint.clone(),
        };
        let state = running_tunnel_state(RemoteTunnelMode::Lan);
        let logs = Arc::new(StdMutex::new(VecDeque::new()));
        supervisor.tunnels.lock().unwrap().insert(
            "lan".to_string(),
            BoundTunnel {
                desired: lan_desired,
                process_task: None,
                process_shutdown: None,
                server_task: None,
                shutdown: None,
                drain_task: None,
                public_url: Arc::new(StdMutex::new(Some("http://192.168.5.10:8789".to_string()))),
                state: state.clone(),
                logs: logs.clone(),
                cloudflared_fingerprint: None,
            },
        );
        supervisor.tunnels.lock().unwrap().insert(
            "command".to_string(),
            BoundTunnel {
                desired: command_desired,
                process_task: None,
                process_shutdown: None,
                server_task: None,
                shutdown: None,
                drain_task: None,
                public_url: Arc::new(StdMutex::new(Some(
                    "https://prmonitor.example.com".to_string(),
                ))),
                state,
                logs,
                cloudflared_fingerprint: None,
            },
        );

        let mut public_urls = supervisor.public_urls_for_entrypoint("ep");
        public_urls.sort();
        assert_eq!(
            public_urls,
            vec![
                "http://192.168.5.10:8789".to_string(),
                "https://prmonitor.example.com".to_string()
            ]
        );
        assert_eq!(
            supervisor.forwarded_public_urls_for_entrypoint("ep"),
            vec!["https://prmonitor.example.com".to_string()]
        );
    }

    #[test]
    fn effective_peer_ip_ignores_forwarded_headers_without_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));
        let direct = "127.0.0.1".parse::<IpAddr>().unwrap();

        assert_eq!(effective_peer_ip(&headers, direct, &entrypoint()), direct);
    }

    #[test]
    fn effective_peer_ip_uses_forwarded_header_for_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "forwarded",
            HeaderValue::from_static("for=\"203.0.113.10\";proto=https"),
        );
        let mut entrypoint = entrypoint();
        entrypoint.trusted_proxies = vec!["127.0.0.1/32".to_string()];

        assert_eq!(
            effective_peer_ip(
                &headers,
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                &entrypoint
            ),
            "203.0.113.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn effective_peer_ip_uses_first_untrusted_hop_from_right() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.99, 203.0.113.10, 127.0.0.1"),
        );
        let mut entrypoint = entrypoint();
        entrypoint.trusted_proxies = vec!["127.0.0.1".to_string()];

        assert_eq!(
            effective_peer_ip(
                &headers,
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                &entrypoint
            ),
            "203.0.113.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn custom_source_policy_can_match_trusted_proxy_effective_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));
        let mut entrypoint = entrypoint();
        entrypoint.trusted_proxies = vec!["127.0.0.1".to_string()];
        entrypoint.source_policy = SourcePolicy {
            mode: SourcePolicyMode::Custom,
            allow: vec!["203.0.113.0/24".to_string()],
        };
        let effective_ip = effective_peer_ip(
            &headers,
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            &entrypoint,
        );

        assert!(source_allowed(effective_ip, &entrypoint));
        assert!(!source_allowed(
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            &entrypoint
        ));
    }

    #[test]
    fn public_tunnel_source_gate_requires_trusted_forwarded_identity() {
        let public_urls = vec!["https://abc.trycloudflare.com".to_string()];
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            HeaderValue::from_static("abc.trycloudflare.com"),
        );
        let direct = "127.0.0.1".parse::<IpAddr>().unwrap();
        let mut entrypoint = entrypoint();

        assert!(
            !source_request_allowed(&headers, direct, &entrypoint, &public_urls),
            "public tunnel traffic must not satisfy loopback sourcePolicy by direct proxy IP"
        );

        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.10"));
        entrypoint.trusted_proxies = vec!["127.0.0.1".to_string()];
        entrypoint.source_policy = SourcePolicy {
            mode: SourcePolicyMode::Custom,
            allow: vec!["203.0.113.10".to_string()],
        };

        assert!(source_request_allowed(
            &headers,
            direct,
            &entrypoint,
            &public_urls
        ));
    }

    #[test]
    fn lan_tunnel_source_gate_allows_direct_ip_host_without_forwarded_identity() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("192.168.5.10:8789"));
        let mut entrypoint = entrypoint();
        entrypoint.source_policy = SourcePolicy {
            mode: SourcePolicyMode::Custom,
            allow: vec!["0.0.0.0/0".to_string()],
        };

        assert!(source_request_allowed(
            &headers,
            "192.168.5.10".parse::<IpAddr>().unwrap(),
            &entrypoint,
            &[]
        ));
    }

    #[test]
    fn runtime_checked_entrypoint_rejects_invalid_route_path_before_axum_nest() {
        let mut entrypoint = entrypoint();
        entrypoint.routes[0].path = "api".to_string();

        assert!(runtime_checked_entrypoint(&entrypoint).is_none());
    }

    #[test]
    fn runtime_checked_entrypoint_normalizes_and_rejects_route_prefix_conflicts() {
        let mut entrypoint = entrypoint();
        entrypoint.routes[0].path = "/api/".to_string();
        entrypoint.routes[1].path = "/api/sessions".to_string();

        assert!(runtime_checked_entrypoint(&entrypoint).is_none());

        entrypoint.routes[1].path = "/terminal/".to_string();
        let checked = runtime_checked_entrypoint(&entrypoint).expect("routes are valid");
        assert_eq!(checked.routes[0].path, "/api");
        assert_eq!(checked.routes[1].path, "/terminal");
    }

    #[test]
    fn failed_tunnel_status_preserves_error_message_and_logs() {
        let supervisor = ListenerSupervisor::default();
        let entrypoint = entrypoint();
        let desired = TunnelDesired {
            mode: RemoteTunnelMode::Lan,
            target_entrypoint_id: entrypoint.id.clone(),
            target_bind_host: entrypoint.bind_host.clone(),
            target_port: entrypoint.port,
            lan_bind_host: "0.0.0.0".to_string(),
            lan_port: 9090,
            command: String::new(),
            public_url: String::new(),
            signature: "sig".to_string(),
            entrypoint: entrypoint.clone(),
        };
        let logs = Arc::new(StdMutex::new(VecDeque::new()));
        push_log(&logs, "bind failure: address already in use");
        supervisor.tunnels.lock().unwrap().insert(
            "lan".to_string(),
            failed_tunnel(desired, logs, "LAN 隧道绑定失败：0.0.0.0:9090".to_string()),
        );

        let remote_access = RemoteAccessConfig {
            entrypoints: vec![entrypoint],
            tunnels: vec![RemoteTunnel {
                id: "lan".to_string(),
                name: "LAN".to_string(),
                mode: RemoteTunnelMode::Lan,
                target_entrypoint_id: "ep".to_string(),
                enabled: true,
                ..RemoteTunnel::default()
            }],
        };

        let status = supervisor.status_snapshot(&remote_access, true);
        assert_eq!(status.tunnels[0].state, RemoteTunnelState::Error);
        assert!(status.tunnels[0].message.contains("绑定失败"));
        assert_eq!(
            status.tunnels[0].logs,
            vec!["bind failure: address already in use".to_string()]
        );
    }

    #[test]
    fn failed_quick_tunnel_is_not_active_and_is_eligible_for_retry() {
        let entrypoint = entrypoint();
        let desired = TunnelDesired {
            mode: RemoteTunnelMode::Quick,
            target_entrypoint_id: entrypoint.id.clone(),
            target_bind_host: entrypoint.bind_host.clone(),
            target_port: entrypoint.port,
            lan_bind_host: String::new(),
            lan_port: 0,
            command: String::new(),
            public_url: String::new(),
            signature: "sig".to_string(),
            entrypoint,
        };
        let runtime = failed_tunnel(
            desired,
            Arc::new(StdMutex::new(VecDeque::new())),
            "spawn failed".to_string(),
        );

        assert!(runtime.needs_restart());
        assert!(!runtime.is_active_managed_cloudflared());
    }

    #[tokio::test]
    async fn active_quick_fingerprints_follow_each_live_runtime() {
        let supervisor = ListenerSupervisor::default();
        let entrypoint = entrypoint();
        let (old_root, old_cli) = resolved_cloudflared("old");
        let (new_root, new_cli) = resolved_cloudflared("new");

        let quick_runtime = |id: &str, fingerprint: String, finished: bool| {
            let (shutdown, shutdown_rx) = oneshot::channel::<()>();
            let process_task = if finished {
                spawn(async {})
            } else {
                spawn(async move {
                    let _ = shutdown_rx.await;
                })
            };
            BoundTunnel {
                desired: TunnelDesired {
                    mode: RemoteTunnelMode::Quick,
                    target_entrypoint_id: entrypoint.id.clone(),
                    target_bind_host: entrypoint.bind_host.clone(),
                    target_port: entrypoint.port,
                    lan_bind_host: String::new(),
                    lan_port: 0,
                    command: String::new(),
                    public_url: String::new(),
                    signature: id.to_string(),
                    entrypoint: entrypoint.clone(),
                },
                process_task: Some(process_task),
                process_shutdown: Some(shutdown),
                server_task: None,
                shutdown: None,
                drain_task: None,
                public_url: Arc::new(StdMutex::new(None)),
                state: running_tunnel_state(RemoteTunnelMode::Quick),
                logs: Arc::new(StdMutex::new(VecDeque::new())),
                cloudflared_fingerprint: Some(fingerprint),
            }
        };

        let old_fingerprint = old_cli.fingerprint().to_string();
        let new_fingerprint = new_cli.fingerprint().to_string();
        {
            let mut tunnels = supervisor.tunnels.lock().unwrap();
            tunnels.insert(
                "old-live".to_string(),
                quick_runtime("old-live", old_fingerprint.clone(), false),
            );
            tunnels.insert(
                "new-live".to_string(),
                quick_runtime("new-live", new_fingerprint.clone(), false),
            );
            tunnels.insert(
                "old-exited".to_string(),
                quick_runtime("old-exited", old_fingerprint.clone(), true),
            );
        }
        tokio::task::yield_now().await;

        let mut expected = vec![old_fingerprint, new_fingerprint];
        expected.sort();
        assert_eq!(supervisor.active_cloudflared_fingerprints(), expected);

        supervisor.shutdown();
        assert!(supervisor.active_cloudflared_fingerprints().is_empty());
        let _ = std::fs::remove_dir_all(old_root);
        let _ = std::fs::remove_dir_all(new_root);
    }

    #[test]
    fn lan_policy_includes_link_local_v4_range() {
        let mut entrypoint = entrypoint();
        entrypoint.source_policy.mode = SourcePolicyMode::Lan;

        assert!(source_allowed(
            "169.254.99.7".parse::<IpAddr>().unwrap(),
            &entrypoint
        ));
    }

    #[test]
    fn custom_source_policy_allows_wildcard_ipv4_cidr() {
        let mut entrypoint = entrypoint();
        entrypoint.source_policy = SourcePolicy {
            mode: SourcePolicyMode::Custom,
            allow: vec!["0.0.0.0/0".to_string()],
        };

        assert!(source_allowed(
            "192.168.5.10".parse::<IpAddr>().unwrap(),
            &entrypoint
        ));
    }

    #[test]
    fn wildcard_bind_allows_ip_host_headers() {
        let mut entrypoint = entrypoint();
        entrypoint.bind_host = "0.0.0.0".to_string();

        assert!(host_allowed(
            Some(&HeaderValue::from_static("192.168.1.20:8788")),
            &entrypoint,
            &[],
            &[]
        ));
    }

    #[test]
    fn extra_lan_bind_host_allows_lan_tunnel_host_headers() {
        let entrypoint = entrypoint();

        assert!(host_allowed(
            Some(&HeaderValue::from_static("192.168.1.20:8788")),
            &entrypoint,
            &["0.0.0.0".to_string()],
            &[]
        ));
    }

    #[test]
    fn public_url_allows_tunnel_host_and_origin() {
        let entrypoint = entrypoint();
        let public_urls = vec!["https://abc.trycloudflare.com".to_string()];

        assert!(host_allowed(
            Some(&HeaderValue::from_static("abc.trycloudflare.com")),
            &entrypoint,
            &[],
            &public_urls
        ));
        assert!(origin_allowed(
            Some(&HeaderValue::from_static("https://abc.trycloudflare.com")),
            &entrypoint,
            &[],
            &public_urls
        ));
    }

    #[test]
    fn route_audit_fields_match_nested_route_without_prefix_bleed() {
        let entrypoint = entrypoint();

        assert_eq!(
            route_audit_fields(&entrypoint, "/api/sessions"),
            ("/api".to_string(), "local-api")
        );
        assert_eq!(
            route_audit_fields(&entrypoint, "/apiary"),
            ("/apiary".to_string(), "unknown")
        );
    }

    #[test]
    fn tunnel_log_sanitization_redacts_case_insensitive_secrets_and_truncates() {
        let redacted = sanitize_log_line("authorization: Bearer abc");
        assert_eq!(redacted, "authorization:[redacted]");

        let query = sanitize_log_line("https://example.test/path?access_token=abc&x=1");
        assert_eq!(query, "https://example.test/path?access_token=[redacted]");

        let long = sanitize_log_line(&"x".repeat(TUNNEL_LOG_LINE_LIMIT + 100));
        assert_eq!(long.len(), TUNNEL_LOG_LINE_LIMIT);
        assert!(long.ends_with("...[truncated]"));
    }

    #[test]
    fn mcp_mount_requires_untunneled_loopback_local_api_entrypoint() {
        let mut access = RemoteAccessConfig::default();
        let entrypoint = access.entrypoints[0].clone();
        assert!(mcp_mount_allowed(&access, &entrypoint));

        access.tunnels.push(RemoteTunnel {
            id: "public".into(),
            enabled: true,
            target_entrypoint_id: entrypoint.id.clone(),
            ..RemoteTunnel::default()
        });
        assert!(!mcp_mount_allowed(&access, &entrypoint));

        access.tunnels.clear();
        let mut lan = entrypoint;
        lan.bind_host = "0.0.0.0".into();
        assert!(!mcp_mount_allowed(&access, &lan));
    }
}
