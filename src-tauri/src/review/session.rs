//! Review session state machine + session manager.
//!
//! One resident `codex app-server` process (held in [`crate::state::AppState`]'s
//! `CodexManager`) drives many sessions keyed by `threadId`. Per session:
//! `thread/start` → `turn/start` (the pr-review skill) starts it; a spawned
//! *pump* task maps the codex notification stream into
//! [`crate::events::ReviewEvent`]s and emits them to the frontend; `turn/interrupt`
//! stops it (the terminal `turn/completed`, status `interrupted`, arrives on the
//! stream). The [`SessionRegistry`] tracks each session's lifecycle for
//! `list_review_sessions`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::Emitter;
use tokio::sync::broadcast;

use super::engines::codex::process;
use super::engines::codex::protocol::{
    SandboxPolicy, ServerNotification, ThreadStartParams, TurnInterruptParams, TurnStartParams,
    UserInput,
};
use super::engines::codex::CodexManager;
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, REVIEW_EVENT};

/// A review session is identified by its codex `threadId`.
pub type ThreadId = String;

/// The skill `name` attached to every review turn (matches the local project
/// skill under `<repo_root>/<skillRelPath>`).
const PR_REVIEW_SKILL: &str = "pr-review";

/// Lifecycle of one review session (the state machine). Serialized camelCase for
/// `list_review_sessions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    /// `thread/start` / `turn/start` in flight (not yet observed on the stream).
    Starting,
    /// The turn is running; deltas are streaming.
    Running,
    /// `turn/interrupt` issued; awaiting the terminal `turn/completed`.
    Interrupting,
    /// The turn finished (`completed` or `interrupted`).
    Done,
    /// The turn failed, or the connection dropped mid-session.
    Failed,
}

/// A review session's public snapshot for `list_review_sessions`. Slice-private
/// wire type, mirrored in `src/review/types.ts` (NOT `model.rs` / `src/types.ts`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub thread_id: String,
    pub turn_id: String,
    pub pr_number: u64,
    /// `"review"` or `"check"` — the trigger-label mode the review was started in.
    pub kind: String,
    pub status: SessionStatus,
}

/// In-memory registry of review sessions keyed by `threadId`. Lives in
/// `AppState`; `Clone` (shares one `Arc`) so the command path and each pump task
/// see the same map. Every critical section is synchronous (no `.await` under the
/// lock).
#[derive(Default, Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<ThreadId, SessionInfo>>>,
}

/// Outcome of [`SessionRegistry::begin_interrupt`] — the atomic guard that makes
/// `stop` race-free and idempotent.
enum BeginInterrupt {
    /// Was `Running`; flipped to `Interrupting`. Caller interrupts this `turn_id`.
    Proceed(String),
    /// Already interrupting / done / failed / starting — nothing to (re)interrupt.
    AlreadyHandled,
    /// No session with this id.
    NotFound,
}

impl SessionRegistry {
    fn insert(&self, info: SessionInfo) {
        self.inner
            .lock()
            .unwrap()
            .insert(info.thread_id.clone(), info);
    }

    fn set_status(&self, thread_id: &str, status: SessionStatus) {
        if let Some(info) = self.inner.lock().unwrap().get_mut(thread_id) {
            info.status = status;
        }
    }

    /// Record the turn id and flip to [`SessionStatus::Running`] once `turn/start`
    /// has returned (the session was inserted as `Starting` before the turn began).
    fn set_running(&self, thread_id: &str, turn_id: String) {
        if let Some(info) = self.inner.lock().unwrap().get_mut(thread_id) {
            info.turn_id = turn_id;
            info.status = SessionStatus::Running;
        }
    }

    /// Atomically begin an interrupt. Only a [`SessionStatus::Running`] session
    /// transitions to [`SessionStatus::Interrupting`] and yields its `turn_id` to
    /// interrupt; an already interrupting / terminal / still-starting session is a
    /// no-op (so a double `stop` is idempotent), and a missing session is an error.
    /// The check-and-set is one synchronous critical section, so two concurrent
    /// stops can't both proceed.
    fn begin_interrupt(&self, thread_id: &str) -> BeginInterrupt {
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(thread_id) {
            None => BeginInterrupt::NotFound,
            Some(info) => match info.status {
                SessionStatus::Running => {
                    info.status = SessionStatus::Interrupting;
                    BeginInterrupt::Proceed(info.turn_id.clone())
                }
                _ => BeginInterrupt::AlreadyHandled,
            },
        }
    }

    /// Revert a failed interrupt: [`SessionStatus::Interrupting`] →
    /// [`SessionStatus::Running`], so a retry can stop the still-running turn. A
    /// terminal status that raced in via the pump meanwhile is left untouched.
    fn rollback_interrupt(&self, thread_id: &str) {
        if let Some(info) = self.inner.lock().unwrap().get_mut(thread_id) {
            if info.status == SessionStatus::Interrupting {
                info.status = SessionStatus::Running;
            }
        }
    }

    /// Snapshot of all known sessions (for `list_review_sessions`).
    pub fn list(&self) -> Vec<SessionInfo> {
        self.inner.lock().unwrap().values().cloned().collect()
    }
}

/// Start a review for `pr_number` and stream its output. Returns the codex
/// `threadId` (the [`crate::review::engine::SessionId`]).
///
/// Subscribes to the notification stream BEFORE `turn/start` so no early delta is
/// missed, then spawns a pump task that forwards events until the turn completes.
#[allow(clippy::too_many_arguments)]
pub async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    codex: &CodexManager,
    registry: &SessionRegistry,
    codex_bin: &str,
    repo: &str,
    repo_root: &str,
    skill_abs_path: &str,
    pr_number: u64,
    kind: &str,
) -> AppResult<ThreadId> {
    let client = codex.connection(codex_bin, repo_root).await?;

    // Subscribe before starting the turn: the broadcast buffers from here, so the
    // pump (spawned after `turn/start` returns) sees every delta from turn start.
    let rx = client.subscribe();

    let thread_id = process::start_thread(
        &client,
        ThreadStartParams {
            cwd: Some(repo_root.to_string()),
        },
    )
    .await?;

    // Register as `Starting` now we have a thread id, so a `turn/start` failure is
    // visible to `list_review_sessions` as `Failed` (rather than the session
    // vanishing). `turn_id` is filled once the turn starts.
    registry.insert(SessionInfo {
        thread_id: thread_id.clone(),
        turn_id: String::new(),
        pr_number,
        kind: kind.to_string(),
        status: SessionStatus::Starting,
    });

    let prompt = review_prompt(repo, &skill_command(pr_number, kind));
    let turn_id = match process::start_turn(
        &client,
        TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![
                UserInput::Skill {
                    name: PR_REVIEW_SKILL.to_string(),
                    path: skill_abs_path.to_string(),
                },
                UserInput::Text { text: prompt },
            ],
            // Unattended: never prompt for approval (the reader auto-answers any
            // reverse approval request as a backstop). Workspace-write + network
            // so the pr-review skill can run `git`/`gh` and post comments.
            approval_policy: "never".to_string(),
            sandbox_policy: SandboxPolicy {
                kind: "workspaceWrite".to_string(),
                network_access: true,
                writable_roots: vec![repo_root.to_string()],
            },
            cwd: Some(repo_root.to_string()),
        },
    )
    .await
    {
        Ok(turn_id) => turn_id,
        Err(e) => {
            registry.set_status(&thread_id, SessionStatus::Failed);
            return Err(e);
        }
    };

    registry.set_running(&thread_id, turn_id);

    tauri::async_runtime::spawn(pump(rx, thread_id.clone(), app.clone(), registry.clone()));

    Ok(thread_id)
}

/// Interrupt a running review session. The terminal `turn/completed` (status
/// `interrupted`) arrives on the stream and the pump finishes the session.
pub async fn stop_review(
    codex: &CodexManager,
    registry: &SessionRegistry,
    codex_bin: &str,
    repo_root: &str,
    session_id: &str,
) -> AppResult<()> {
    // Atomic guard: only a `Running` session flips to `Interrupting` (and yields
    // its turn id); a repeat stop is an idempotent no-op, an unknown id an error.
    let turn_id = match registry.begin_interrupt(session_id) {
        BeginInterrupt::Proceed(turn_id) => turn_id,
        BeginInterrupt::AlreadyHandled => return Ok(()),
        BeginInterrupt::NotFound => {
            return Err(AppError::new(format!("未找到 review 会话: {session_id}")))
        }
    };
    // We're now `Interrupting`. Any failure below must roll back to `Running` so a
    // retry can interrupt again — never leave a half-interrupted, un-stoppable
    // session (the pump still owns the real terminal transition on `turn/completed`).
    let client = match codex.connection(codex_bin, repo_root).await {
        Ok(client) => client,
        Err(e) => {
            registry.rollback_interrupt(session_id);
            return Err(e);
        }
    };
    if let Err(e) = process::interrupt_turn(
        &client,
        TurnInterruptParams {
            thread_id: session_id.to_string(),
            turn_id,
        },
    )
    .await
    {
        registry.rollback_interrupt(session_id);
        return Err(e);
    }
    Ok(())
}

/// Pump task: forward this session's notifications to the frontend as
/// [`ReviewEvent`]s until the turn completes (or the connection drops). Filters by
/// `thread_id` since the broadcast carries every session's stream.
async fn pump<R: tauri::Runtime>(
    mut rx: broadcast::Receiver<Arc<ServerNotification>>,
    thread_id: String,
    app: tauri::AppHandle<R>,
    registry: SessionRegistry,
) {
    loop {
        match rx.recv().await {
            Ok(note) => {
                let Some(event) = map_notification(&note, &thread_id) else {
                    continue;
                };
                if let ReviewEvent::TurnCompleted { status, .. } = &event {
                    registry.set_status(&thread_id, terminal_status(status));
                    let _ = app.emit(REVIEW_EVENT, &event);
                    break; // terminal — the turn is over.
                }
                let _ = app.emit(REVIEW_EVENT, &event);
            }
            // The pump fell behind the shared ring and `n` notifications were
            // evicted. The terminal `turn/completed` may have been among them
            // (another session can flood the ring after ours), which would hang
            // this pump on `recv()` forever — and the dropped deltas already make
            // the rendered stream incomplete. So end the session honestly with a
            // Failed terminal + error rather than risk a stuck `Running`.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("review pump（{thread_id}）滞后，丢弃 {n} 条通知");
                registry.set_status(&thread_id, SessionStatus::Failed);
                let _ = app.emit(
                    REVIEW_EVENT,
                    &ReviewEvent::Error {
                        thread_id: thread_id.clone(),
                        message: format!("codex 输出流滞后，丢弃 {n} 条消息（review 中断）"),
                    },
                );
                break;
            }
            // The connection closed before the turn completed.
            Err(broadcast::error::RecvError::Closed) => {
                registry.set_status(&thread_id, SessionStatus::Failed);
                let _ = app.emit(
                    REVIEW_EVENT,
                    &ReviewEvent::Error {
                        thread_id: thread_id.clone(),
                        message: "codex 连接已关闭".to_string(),
                    },
                );
                break;
            }
        }
    }
}

/// Map one codex notification to a [`ReviewEvent`] for this session, or `None`
/// if it belongs to another thread / is not a streamed unit we forward. Pure —
/// unit-tested below.
fn map_notification(note: &ServerNotification, thread_id: &str) -> Option<ReviewEvent> {
    match note {
        ServerNotification::AgentMessageDelta(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::MessageDelta {
                thread_id: d.thread_id.clone(),
                item_id: d.item_id.clone(),
                text: d.delta.clone(),
            })
        }
        ServerNotification::ReasoningTextDelta(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::ReasoningDelta {
                thread_id: d.thread_id.clone(),
                item_id: d.item_id.clone(),
                text: d.delta.clone(),
            })
        }
        ServerNotification::TurnCompleted(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::TurnCompleted {
                thread_id: d.thread_id.clone(),
                status: d.turn.status.clone(),
            })
        }
        _ => None,
    }
}

/// Map a codex `turn.status` to the session's terminal state. `completed` /
/// `interrupted` are both successful endings (the latter is a user stop);
/// anything else (`failed`, …) is [`SessionStatus::Failed`].
fn terminal_status(status: &str) -> SessionStatus {
    match status {
        "completed" | "interrupted" => SessionStatus::Done,
        _ => SessionStatus::Failed,
    }
}

/// The skill command the review turn instructs codex to run: `/pr-review <N>` for
/// a review, `/pr-review <N> --check` for a check.
fn skill_command(pr_number: u64, kind: &str) -> String {
    if kind == "check" {
        format!("/{PR_REVIEW_SKILL} {pr_number} --check")
    } else {
        format!("/{PR_REVIEW_SKILL} {pr_number}")
    }
}

/// The turn's instruction text. Ported from `router.py:590-597`; the
/// machine-block clause is dropped because prmonitor's pr-review skill posts plain
/// `pm:` comments (no machine block — see #24).
fn review_prompt(repo: &str, skill_command: &str) -> String {
    format!(
        "Use the attached local project skill `{PR_REVIEW_SKILL}` exactly. \
         Execute `{skill_command}` for repository `{repo}`. \
         Complete the full skill workflow, including posting the pm:pr-review \
         comment and actually applying the label transition required by the skill. \
         Do not stop at a label-transition suggestion. Do not use the built-in \
         Codex review command."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::engines::codex::protocol::{
        AgentMessageDelta, OutputDelta, ReasoningTextDelta, TurnCompletedNotification,
        TurnStatusRef,
    };

    fn msg_delta(thread: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(AgentMessageDelta {
            thread_id: thread.to_string(),
            turn_id: "tn".to_string(),
            item_id: "it".to_string(),
            delta: "hello".to_string(),
        })
    }

    #[test]
    fn map_notification_forwards_own_thread_message_delta() {
        match map_notification(&msg_delta("t1"), "t1") {
            Some(ReviewEvent::MessageDelta {
                thread_id,
                item_id,
                text,
            }) => {
                assert_eq!(thread_id, "t1");
                assert_eq!(item_id, "it");
                assert_eq!(text, "hello");
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn map_notification_drops_other_thread() {
        // A delta for a different session must not leak into this pump.
        assert!(map_notification(&msg_delta("other"), "t1").is_none());
    }

    #[test]
    fn map_notification_ignores_non_forwarded_notifications() {
        // OutputDelta (command/exec output) and Other (unknown) are not streamed
        // to the UI in PR6 — the `_ => None` arm must hold for them.
        let output = ServerNotification::OutputDelta(OutputDelta {
            process_id: Some("p".to_string()),
            process_handle: None,
            stream: "stdout".to_string(),
            delta_base64: "dGhl".to_string(),
            cap_reached: false,
        });
        assert!(map_notification(&output, "t1").is_none());

        let other = ServerNotification::Other {
            method: "thread/futureThing".to_string(),
            params: serde_json::json!({}),
        };
        assert!(map_notification(&other, "t1").is_none());
    }

    #[test]
    fn map_notification_maps_reasoning_delta() {
        let n = ServerNotification::ReasoningTextDelta(ReasoningTextDelta {
            thread_id: "t1".to_string(),
            turn_id: "tn".to_string(),
            item_id: "r1".to_string(),
            delta: "thinking".to_string(),
        });
        assert!(matches!(
            map_notification(&n, "t1"),
            Some(ReviewEvent::ReasoningDelta { .. })
        ));
    }

    #[test]
    fn map_notification_maps_turn_completed_status() {
        let n = ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "t1".to_string(),
            turn: TurnStatusRef {
                status: "interrupted".to_string(),
            },
        });
        match map_notification(&n, "t1") {
            Some(ReviewEvent::TurnCompleted { status, .. }) => assert_eq!(status, "interrupted"),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn terminal_status_maps_codex_status() {
        assert_eq!(terminal_status("completed"), SessionStatus::Done);
        assert_eq!(terminal_status("interrupted"), SessionStatus::Done);
        assert_eq!(terminal_status("failed"), SessionStatus::Failed);
        assert_eq!(terminal_status("anythingElse"), SessionStatus::Failed);
    }

    #[test]
    fn skill_command_matches_kind() {
        assert_eq!(skill_command(7, "review"), "/pr-review 7");
        assert_eq!(skill_command(7, "check"), "/pr-review 7 --check");
    }

    #[test]
    fn review_prompt_names_skill_command_and_repo() {
        let p = review_prompt("owner/name", "/pr-review 7");
        assert!(p.contains("/pr-review 7"));
        assert!(p.contains("owner/name"));
        assert!(p.contains("pr-review"));
        // The dropped router.py machine-block clause must stay dropped.
        assert!(!p.to_lowercase().contains("machine block"));
    }

    #[test]
    fn registry_insert_status_and_list_roundtrip() {
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        assert_eq!(reg.list()[0].turn_id, "tn1");
        reg.set_status("t1", SessionStatus::Done);
        let list = reg.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, SessionStatus::Done);
        assert!(matches!(
            reg.begin_interrupt("missing"),
            BeginInterrupt::NotFound
        ));
    }

    #[test]
    fn begin_interrupt_is_atomic_and_idempotent() {
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        // Running → Proceed(turn_id), status flips to Interrupting.
        match reg.begin_interrupt("t1") {
            BeginInterrupt::Proceed(turn_id) => assert_eq!(turn_id, "tn1"),
            _ => panic!("a Running session must Proceed"),
        }
        assert_eq!(reg.list()[0].status, SessionStatus::Interrupting);
        // A repeat stop while Interrupting is an idempotent no-op.
        assert!(matches!(
            reg.begin_interrupt("t1"),
            BeginInterrupt::AlreadyHandled
        ));
        // Unknown id is an error.
        assert!(matches!(
            reg.begin_interrupt("missing"),
            BeginInterrupt::NotFound
        ));
    }

    #[test]
    fn rollback_interrupt_reverts_only_interrupting() {
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        reg.begin_interrupt("t1"); // → Interrupting
        reg.rollback_interrupt("t1"); // failed interrupt → back to Running (retry-able)
        assert_eq!(reg.list()[0].status, SessionStatus::Running);

        // A terminal status that raced in via the pump must NOT be reverted.
        reg.begin_interrupt("t1");
        reg.set_status("t1", SessionStatus::Done);
        reg.rollback_interrupt("t1");
        assert_eq!(reg.list()[0].status, SessionStatus::Done);
    }

    #[test]
    fn session_info_wire_shape_is_camel_case() {
        // Locks the contract with `src/review/types.ts` (Medium carrier).
        let v = serde_json::to_value(SessionInfo {
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        })
        .expect("SessionInfo serializes");
        assert_eq!(v["threadId"], "t1");
        assert_eq!(v["turnId"], "tn1");
        assert_eq!(v["prNumber"], 7);
        assert_eq!(v["status"], "running");
        assert!(v.get("thread_id").is_none());
    }

    #[test]
    fn session_status_wire_strings_are_pinned() {
        // Every variant's wire string is mirrored by `SessionStatus` in
        // `src/review/types.ts`; a rename here must be matched there (Medium lock).
        for (status, wire) in [
            (SessionStatus::Starting, "starting"),
            (SessionStatus::Running, "running"),
            (SessionStatus::Interrupting, "interrupting"),
            (SessionStatus::Done, "done"),
            (SessionStatus::Failed, "failed"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), wire);
        }
    }

    #[test]
    fn start_failure_marks_session_failed() {
        // The two-phase start: a session inserted as `Starting` is flipped to
        // `Failed` if the turn never starts (so it stays visible, not vanished).
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            thread_id: "t1".to_string(),
            turn_id: String::new(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Starting,
        });
        reg.set_status("t1", SessionStatus::Failed);
        assert_eq!(reg.list()[0].status, SessionStatus::Failed);

        // The success path fills the turn id and flips to Running.
        reg.set_running("t1", "tn9".to_string());
        assert_eq!(reg.list()[0].turn_id, "tn9");
        assert_eq!(reg.list()[0].status, SessionStatus::Running);
    }
}
