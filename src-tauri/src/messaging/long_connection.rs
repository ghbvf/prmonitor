//! Shared long-connection supervisor helpers for Feishu WS + DingTalk Stream.
//!
//! Protocol-specific connect / frame loops stay in provider modules (no LongConnection
//! trait). Lifecycle fencing, fingerprint, stage timeout, and reconnect backoff live here
//! so both managers cannot drift.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::config::service::MessagingIntegration;
use crate::error::{AppError, AppResult};
use crate::model::MessagingConnectionStatus;

pub fn integration_fingerprint(integration: &MessagingIntegration) -> AppResult<[u8; 32]> {
    let serialized = serde_json::to_vec(integration)
        .map_err(|error| AppError::new(format!("消息集成配置序列化失败: {error}")))?;
    Ok(Sha256::digest(serialized).into())
}

pub fn next_generation(generations: &Arc<Mutex<HashMap<String, u64>>>, id: &str) -> u64 {
    let mut generations = generations.lock().unwrap_or_else(|p| p.into_inner());
    let generation = generations.entry(id.to_string()).or_default();
    *generation = generation.saturating_add(1);
    *generation
}

pub fn update_current(
    statuses: &Arc<Mutex<HashMap<String, MessagingConnectionStatus>>>,
    generations: &Arc<Mutex<HashMap<String, u64>>>,
    id: &str,
    generation: u64,
    f: impl FnOnce(&mut MessagingConnectionStatus),
) {
    let current = generations
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .copied()
        .unwrap_or(0);
    if current != generation {
        return;
    }
    if let Some(value) = statuses
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_mut(id)
    {
        f(value);
    }
}

pub fn lock_current_generation<'a>(
    generations: &'a Arc<Mutex<HashMap<String, u64>>>,
    integration_id: &str,
    generation: u64,
    cancel: &CancellationToken,
) -> AppResult<std::sync::MutexGuard<'a, HashMap<String, u64>>> {
    if cancel.is_cancelled() {
        return Err(AppError::new("长连接 delivery 已取消"));
    }
    let current = generations.lock().unwrap_or_else(|p| p.into_inner());
    if current.get(integration_id).copied() != Some(generation) || cancel.is_cancelled() {
        return Err(AppError::new("长连接 generation 已失效"));
    }
    Ok(current)
}

pub fn clamp_reconnect_secs(value: u64) -> u64 {
    value.clamp(2, 300)
}

pub fn reconnect_delay(reconnect_count: u64) -> Duration {
    // 2, 4, 8, … capped — avoids reconnect storms on bootstrap failure.
    let exp = reconnect_count.min(6);
    let secs = clamp_reconnect_secs(2u64.saturating_mul(1u64 << exp));
    Duration::from_secs(secs)
}

pub async fn await_connection_stage<T, F>(
    cancel: &CancellationToken,
    timeout: Duration,
    stage: &str,
    future: F,
) -> AppResult<T>
where
    F: Future<Output = AppResult<T>>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AppError::new(format!("长连接 {stage} 已取消"))),
        result = tokio::time::timeout(timeout, future) => match result {
            Ok(result) => result,
            Err(_) => Err(AppError::new(format!("长连接 {stage} 超时"))),
        },
    }
}

pub fn sanitize_ws_error(error: &dyn std::fmt::Display) -> String {
    let raw = error.to_string();
    // Never surface ticket= query strings into UI last_error.
    if raw.contains("ticket=") {
        "WebSocket 连接失败".into()
    } else {
        format!("WebSocket 连接失败: {raw}")
    }
}

/// Cancel + abort a connection task, collecting the JoinHandle for [`abort_join_wait`].
pub fn cancel_abort_task(
    cancel: CancellationToken,
    task: tauri::async_runtime::JoinHandle<()>,
    aborted: &mut Vec<tauri::async_runtime::JoinHandle<()>>,
) {
    cancel.cancel();
    task.abort();
    aborted.push(task);
}

/// Await aborted JoinHandles before bootstrapping a replacement (prevents dual connect).
///
/// Callers must not hold locks that async supervisors need to finish teardown; prefer
/// waiting while still holding the task map lock so concurrent reconcile cannot spawn early.
pub fn abort_join_wait(handles: Vec<tauri::async_runtime::JoinHandle<()>>) {
    if handles.is_empty() {
        return;
    }
    let join_all = async move {
        for handle in handles {
            let _ = handle.await;
        }
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(join_all));
    } else {
        tauri::async_runtime::block_on(join_all);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessagingProviderKind;

    #[test]
    fn fingerprint_changes_when_allowlist_changes() {
        let mut integration = MessagingIntegration {
            id: "dt".into(),
            kind: MessagingProviderKind::DingTalk,
            enabled: true,
            app_id: "a".into(),
            app_secret: "s".into(),
            bot_open_id: "r".into(),
            card_template_id: "t".into(),
            allowed_conversation_ids: vec!["c1".into()],
            require_mention: true,
            ..MessagingIntegration::feishu_default()
        };
        let first = integration_fingerprint(&integration).unwrap();
        integration.allowed_conversation_ids.push("c2".into());
        assert_ne!(first, integration_fingerprint(&integration).unwrap());
        integration.require_mention = false;
        assert_ne!(first, integration_fingerprint(&integration).unwrap());
    }

    #[test]
    fn generation_fence_rejects_stale() {
        let generations = Arc::new(Mutex::new(HashMap::new()));
        let active = CancellationToken::new();
        let g2 = next_generation(&generations, "id");
        assert_eq!(g2, 1);
        let g3 = next_generation(&generations, "id");
        assert!(lock_current_generation(&generations, "id", g3, &active).is_ok());
        assert!(lock_current_generation(&generations, "id", g2, &active).is_err());
    }

    #[test]
    fn reconnect_delay_grows_then_caps() {
        assert_eq!(reconnect_delay(0).as_secs(), 2);
        assert_eq!(reconnect_delay(1).as_secs(), 4);
        assert_eq!(reconnect_delay(10).as_secs(), 128);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_join_wait_awaits_cancelled_tasks() {
        let handle = tauri::async_runtime::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        handle.abort();
        abort_join_wait(vec![handle]);
    }
}
