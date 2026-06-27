//! Remote Access runtime status for #1553 entrypoints and tunnels.

use serde::Serialize;

use crate::config::model::{RemoteCapability, RemoteTunnelMode};

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteEntrypointState {
    Bound,
    BoundNoAuth,
    Error,
}

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteTunnelState {
    Running,
    Stopped,
    Error,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRouteRuntimeStatus {
    pub id: String,
    pub path: String,
    pub capability: RemoteCapability,
    pub enabled: bool,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntrypointRuntimeStatus {
    pub id: String,
    pub bound: bool,
    #[cfg_attr(test, ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_port: Option<u16>,
    pub state: RemoteEntrypointState,
    pub message: String,
    pub routes: Vec<RemoteRouteRuntimeStatus>,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTunnelRuntimeStatus {
    pub id: String,
    pub mode: RemoteTunnelMode,
    pub target_entrypoint_id: String,
    pub state: RemoteTunnelState,
    #[cfg_attr(test, ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    pub message: String,
    pub logs: Vec<String>,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccessRuntimeStatus {
    pub entrypoints: Vec<RemoteEntrypointRuntimeStatus>,
    pub tunnels: Vec<RemoteTunnelRuntimeStatus>,
}
