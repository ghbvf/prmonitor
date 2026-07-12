//! [`ReviewEngine`] implementation backed by a resident Cursor ACP (`agent acp`)
//! connection. Self-contained session pump (like Claude), reusing
//! [`SessionRegistry`] / history_store / `finalize_turn` rather than coupling
//! `session.rs` to a third protocol.

use tauri::Manager;
use tokio::process::ChildStdin;
use tokio::sync::broadcast;

use super::manager::CursorManager;
use super::process::{self, review_prompt, text_prompt};
use super::protocol::ServerNotification;
use super::protocol::{SessionNewParams, SessionPromptResult};
use super::rpc::RpcClient;
use crate::config::service::ResolvedCli;
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, StreamEvent};
use crate::model::{EngineKind, ReviewKind};
use crate::review::engine::{ReviewEngine, ReviewStartCapability, SessionId, StartReviewOutcome};
use crate::review::history_store::{self, HistoryItemKind};
use crate::review::session::{
    commit_starting_session, CommentUrlContext, SessionInfo, SessionRegistry, SessionStatus,
};

/// Per-request engine handle. Borrows long-lived `AppState` plus the request's
/// `AppHandle`; constructed fresh by each command/dispatch.
pub struct CursorEngine<'a, R: tauri::Runtime> {
    pub app: &'a tauri::AppHandle<R>,
    pub cursor: &'a CursorManager,
    pub registry: &'a SessionRegistry,
    pub agent_cli: &'a ResolvedCli,
    pub project_id: &'a str,
    pub repo: &'a str,
    pub repo_root: &'a str,
    pub(crate) url_ctx: CommentUrlContext,
    /// FOLLOW-UP path only — `start` takes `pr_number` as a method arg.
    pub pr_number: u64,
    pub session_info: Option<SessionInfo>,
    pub outbox_claim_id: Option<i64>,
}

impl<R: tauri::Runtime> ReviewEngine for CursorEngine<'_, R> {
    async fn start(
        &self,
        _capability: &ReviewStartCapability,
        pr_number: u64,
        kind: ReviewKind,
    ) -> AppResult<StartReviewOutcome> {
        start_review(
            self.app,
            self.cursor,
            self.registry,
            self.agent_cli,
            self.project_id,
            self.repo_root,
            pr_number,
            kind,
            self.url_ctx.clone(),
            self.outbox_claim_id,
        )
        .await
    }

    async fn send_message(
        &self,
        session: &SessionId,
        message: &str,
        user_item_id: &str,
    ) -> AppResult<()> {
        resume_review(
            self.app,
            self.cursor,
            self.registry,
            self.agent_cli,
            self.project_id,
            self.repo_root,
            self.pr_number,
            self.session_info
                .as_ref()
                .expect("CursorEngine::send_message requires session_info"),
            session,
            message,
            user_item_id,
            self.url_ctx.clone(),
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    cursor: &CursorManager,
    registry: &SessionRegistry,
    agent_cli: &ResolvedCli,
    project_id: &str,
    repo_root: &str,
    pr_number: u64,
    kind: ReviewKind,
    url_ctx: CommentUrlContext,
    outbox_claim_id: Option<i64>,
) -> AppResult<StartReviewOutcome> {
    if !registry.try_reserve_pair(project_id, pr_number, kind) {
        return Ok(StartReviewOutcome::Deduped);
    }
    let reservation = ReservationGuard {
        registry,
        project_id: project_id.to_string(),
        pr_number,
        kind,
        armed: true,
    };

    let client = cursor.connection(agent_cli, repo_root).await?;
    let cwd = if repo_root.trim().is_empty() {
        String::new()
    } else {
        repo_root.to_string()
    };
    let session_id = process::session_new(
        &client,
        SessionNewParams {
            cwd,
            mcp_servers: vec![],
        },
    )
    .await?;

    let created_at_epoch = history_store::now_epoch();
    let starting = SessionInfo {
        project_id: project_id.to_string(),
        thread_id: session_id.clone(),
        turn_id: session_id.clone(),
        pr_number,
        kind,
        engine_kind: EngineKind::Cursor,
        status: SessionStatus::Starting,
        created_at_epoch,
        comment_url: None,
    };
    commit_starting_session(app, registry, starting.clone(), url_ctx, outbox_claim_id)?;
    reservation.disarm();

    registry.set_running(&session_id, session_id.clone());
    let _ = persist_session(
        app,
        &SessionInfo {
            status: SessionStatus::Running,
            ..starting
        },
    );

    let prompt_text = review_prompt(pr_number, kind);
    let sub = client.subscribe();
    tauri::async_runtime::spawn(pump(
        client,
        sub,
        session_id.clone(),
        prompt_text,
        project_id.to_string(),
        pr_number,
        app.clone(),
        registry.clone(),
    ));

    Ok(StartReviewOutcome::Started(session_id))
}

#[allow(clippy::too_many_arguments)]
async fn resume_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    cursor: &CursorManager,
    registry: &SessionRegistry,
    agent_cli: &ResolvedCli,
    project_id: &str,
    repo_root: &str,
    pr_number: u64,
    durable_info: &SessionInfo,
    thread_id: &str,
    message: &str,
    user_item_id: &str,
    url_ctx: CommentUrlContext,
) -> AppResult<()> {
    // Reuse the SAME ACP sessionId — no session/load (per scaffold contract).
    if registry.get(thread_id).is_none() {
        registry.rehydrate(durable_info.clone(), url_ctx.clone());
    }

    match registry.begin_resume(thread_id) {
        crate::review::session::BeginResume::Proceed => {}
        crate::review::session::BeginResume::Busy => {
            return Err(AppError::new(format!(
                "review 会话仍在进行中，无法续聊: {thread_id}"
            )))
        }
        crate::review::session::BeginResume::NotFound => {
            return Err(AppError::new(format!("未找到 review 会话: {thread_id}")))
        }
    }

    let client = match cursor.connection(agent_cli, repo_root).await {
        Ok(c) => c,
        Err(e) => {
            registry.set_status(thread_id, SessionStatus::Failed);
            persist_session_status(
                app,
                registry,
                project_id,
                thread_id,
                pr_number,
                durable_info,
                SessionStatus::Failed,
            );
            let _ = registry.take_url_context(thread_id);
            return Err(e);
        }
    };

    crate::review::session::persist_user_message(app, project_id, thread_id, user_item_id, message);
    persist_session_status(
        app,
        registry,
        project_id,
        thread_id,
        pr_number,
        durable_info,
        SessionStatus::Running,
    );

    let sub = client.subscribe();
    tauri::async_runtime::spawn(pump(
        client,
        sub,
        thread_id.to_string(),
        message.to_string(),
        project_id.to_string(),
        pr_number,
        app.clone(),
        registry.clone(),
    ));

    Ok(())
}

/// Pump: race `session/prompt` against `session/update` notifications until the
/// prompt returns (terminal) or the connection closes.
#[allow(clippy::too_many_arguments)]
async fn pump<R: tauri::Runtime>(
    client: std::sync::Arc<RpcClient<ChildStdin>>,
    mut sub: broadcast::Receiver<std::sync::Arc<ServerNotification>>,
    session_id: String,
    prompt_text: String,
    project_id: String,
    pr_number: u64,
    app: tauri::AppHandle<R>,
    registry: SessionRegistry,
) {
    let item_id = format!("{session_id}:assistant");
    let prompt_fut = process::session_prompt(&client, text_prompt(&session_id, prompt_text));
    tokio::pin!(prompt_fut);

    loop {
        tokio::select! {
            result = &mut prompt_fut => {
                match result {
                    Ok(SessionPromptResult { stop_reason }) => {
                        let (status, wire_status, error) = map_stop_reason(&stop_reason);
                        finish(
                            &registry,
                            &app,
                            &project_id,
                            pr_number,
                            &session_id,
                            status,
                            wire_status,
                            error,
                        )
                        .await;
                    }
                    Err(e) => {
                        finish(
                            &registry,
                            &app,
                            &project_id,
                            pr_number,
                            &session_id,
                            SessionStatus::Failed,
                            "failed",
                            Some(e.message),
                        )
                        .await;
                    }
                }
                break;
            }
            note = sub.recv() => {
                match note {
                    Ok(note) => match note.as_ref() {
                        ServerNotification::AgentMessageChunk {
                            session_id: sid,
                            text,
                        } if sid == &session_id && !text.is_empty() => {
                            emit_and_persist(
                                &app,
                                &project_id,
                                &session_id,
                                &item_id,
                                text.clone(),
                            );
                        }
                        ServerNotification::ConnectionClosed => {
                            finish(
                                &registry,
                                &app,
                                &project_id,
                                pr_number,
                                &session_id,
                                SessionStatus::Failed,
                                "failed",
                                Some("cursor ACP 连接已关闭".to_string()),
                            )
                            .await;
                            break;
                        }
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        finish(
                            &registry,
                            &app,
                            &project_id,
                            pr_number,
                            &session_id,
                            SessionStatus::Failed,
                            "failed",
                            Some("cursor ACP 通知通道已关闭".to_string()),
                        )
                        .await;
                        break;
                    }
                }
            }
        }
    }
}

fn map_stop_reason(stop_reason: &str) -> (SessionStatus, &'static str, Option<String>) {
    match stop_reason {
        "end_turn" | "endTurn" | "" => (SessionStatus::Done, "completed", None),
        "cancelled" | "canceled" => (SessionStatus::Done, "interrupted", None),
        other => (
            SessionStatus::Failed,
            "failed",
            Some(format!("cursor ACP stopReason={other}")),
        ),
    }
}

fn emit_and_persist<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    session_id: &str,
    item_id: &str,
    text: String,
) {
    let event = ReviewEvent::MessageDelta {
        project_id: project_id.to_string(),
        thread_id: session_id.to_string(),
        item_id: item_id.to_string(),
        text: text.clone(),
    };
    crate::stream::emit(app, StreamEvent::Review(event));
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::append_item(
        db.inner(),
        session_id,
        item_id,
        HistoryItemKind::Message,
        &text,
    ) {
        eprintln!(
            "cursor review history 持久化失败（{session_id}/{item_id}）：{}",
            e.message
        );
        crate::review::session::notify_persist_failure_once(app, project_id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish<R: tauri::Runtime>(
    registry: &SessionRegistry,
    app: &tauri::AppHandle<R>,
    project_id: &str,
    pr_number: u64,
    session_id: &str,
    status: SessionStatus,
    wire_status: &str,
    error: Option<String>,
) {
    crate::review::session::finalize_turn(
        app,
        registry,
        project_id,
        pr_number,
        session_id,
        status,
        wire_status,
        error,
    )
    .await;
}

fn persist_session_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    registry: &SessionRegistry,
    project_id: &str,
    thread_id: &str,
    pr_number: u64,
    durable_info: &SessionInfo,
    status: SessionStatus,
) {
    let info = registry
        .get(thread_id)
        .map(|info| SessionInfo { status, ..info })
        .unwrap_or_else(|| SessionInfo {
            project_id: project_id.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: thread_id.to_string(),
            pr_number,
            kind: durable_info.kind,
            engine_kind: durable_info.engine_kind,
            status,
            created_at_epoch: durable_info.created_at_epoch,
            comment_url: durable_info.comment_url.clone(),
        });
    let _ = persist_session(app, &info);
}

fn persist_session<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    info: &SessionInfo,
) -> AppResult<()> {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::upsert_session(db.inner(), info) {
        eprintln!(
            "cursor review session 持久化失败（{}）：{}",
            info.thread_id, e.message
        );
        crate::review::session::notify_persist_failure_once(app, &info.project_id);
        return Err(e);
    }
    Ok(())
}

struct ReservationGuard<'a> {
    registry: &'a SessionRegistry,
    project_id: String,
    pr_number: u64,
    kind: ReviewKind,
    armed: bool,
}

impl ReservationGuard<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.registry
                .release_pair(&self.project_id, self.pr_number, self.kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_stop_reason_end_turn_is_completed() {
        let (s, w, e) = map_stop_reason("end_turn");
        assert_eq!(s, SessionStatus::Done);
        assert_eq!(w, "completed");
        assert!(e.is_none());
    }

    #[test]
    fn map_stop_reason_end_turn_camel_alias_is_completed() {
        let (s, w, e) = map_stop_reason("endTurn");
        assert_eq!(s, SessionStatus::Done);
        assert_eq!(w, "completed");
        assert!(e.is_none());
    }

    #[test]
    fn map_stop_reason_cancelled_is_interrupted() {
        let (s, w, e) = map_stop_reason("cancelled");
        assert_eq!(s, SessionStatus::Done);
        assert_eq!(w, "interrupted");
        assert!(e.is_none());
    }

    #[test]
    fn map_stop_reason_canceled_alias_is_interrupted() {
        let (s, w, e) = map_stop_reason("canceled");
        assert_eq!(s, SessionStatus::Done);
        assert_eq!(w, "interrupted");
        assert!(e.is_none());
    }

    #[test]
    fn map_stop_reason_unknown_is_failed() {
        let (s, w, e) = map_stop_reason("max_tokens");
        assert_eq!(s, SessionStatus::Failed);
        assert_eq!(w, "failed");
        assert!(e.unwrap().contains("max_tokens"));
    }
}
