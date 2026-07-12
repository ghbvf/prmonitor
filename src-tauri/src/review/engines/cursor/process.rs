//! Spawn and manage an `agent acp` child process (cwd = `repo_root`), owning its
//! stdin/stdout pipes (wired into an [`RpcClient`]) and draining stderr. Provides
//! the `initialize` → `authenticate(cursor_login)` handshake plus the per-connection
//! RPC ops the engine drives over `client()` (`session_new` / `session_prompt` /
//! `session_cancel`). The process is kept *resident* by
//! [`super::manager::CursorManager`].

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin};

use super::protocol::{
    rpc_methods, AuthenticateParams, ClientCapabilities, ClientInfo, ContentBlock, FsCapabilities,
    InitializeParams, InitializeResult, SessionCancelParams, SessionNewParams, SessionNewResult,
    SessionPromptParams, SessionPromptResult, AUTH_METHOD_CURSOR_LOGIN,
};
use super::rpc::RpcClient;
use crate::config::service::ResolvedCli;
use crate::error::{AppError, AppResult};

const NOTIF_CAPACITY: usize = 1024;
/// `session/prompt` spans a full review turn — far longer than handshake RPCs.
const SESSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Cursor ACP availability reported to the StatusBar.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorStatus {
    pub available: bool,
    /// 用户意图的运行状态：true=意图运行，false=用户已显式停止。
    pub desired_running: bool,
    pub message: String,
}

/// A live, handshaken connection to an `agent acp` child.
pub struct CursorProcess {
    child: Child,
    client: Arc<RpcClient<ChildStdin>>,
    pub info: InitializeResult,
    fingerprint: String,
}

impl CursorProcess {
    /// Spawn `agent acp` in `repo_root` and complete `initialize` + `authenticate`.
    /// An empty `repo_root` leaves the child's cwd at the process default.
    pub(super) async fn spawn(agent: &ResolvedCli, repo_root: &str) -> AppResult<Self> {
        let mut cmd = agent.command();
        cmd.args(["acp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if !repo_root.trim().is_empty() {
            cmd.current_dir(repo_root);
        }

        let mut child = cmd.spawn().map_err(|e| {
            AppError::new(format!(
                "无法启动 cursor ACP（agent 未安装或不在 PATH？）: {e}"
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::new("cursor ACP stdin 不可用".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("cursor ACP stdout 不可用".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("cursor ACP stderr 不可用".to_string()))?;

        spawn_stderr_drain(stderr);

        let client = RpcClient::connect(stdin, BufReader::new(stdout), NOTIF_CAPACITY);
        let info = Self::handshake(&client).await?;
        Ok(Self {
            child,
            client: Arc::new(client),
            info,
            fingerprint: agent.fingerprint().to_string(),
        })
    }

    /// `initialize` → `authenticate(methodId: cursor_login)`. Does NOT inject API
    /// keys; relies on CLI login / ambient `CURSOR_API_KEY`.
    pub async fn handshake<W>(client: &RpcClient<W>) -> AppResult<InitializeResult>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let params = InitializeParams {
            protocol_version: 1,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: ClientInfo {
                name: "prmonitor".to_string(),
                title: Some("PR Monitor".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };
        let raw = client
            .request(
                rpc_methods::INITIALIZE,
                serde_json::to_value(params)
                    .map_err(|e| AppError::new(format!("编码 initialize 失败: {e}")))?,
            )
            .await?;
        let info: InitializeResult = serde_json::from_value(raw)
            .map_err(|e| AppError::new(format!("解析 InitializeResult 失败: {e}")))?;

        client
            .request(
                rpc_methods::AUTHENTICATE,
                serde_json::to_value(AuthenticateParams {
                    method_id: AUTH_METHOD_CURSOR_LOGIN.to_string(),
                })
                .map_err(|e| AppError::new(format!("编码 authenticate 失败: {e}")))?,
            )
            .await?;
        Ok(info)
    }

    pub fn client(&self) -> Arc<RpcClient<ChildStdin>> {
        self.client.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn kill_and_reap(mut self) {
        let _ = self.child.start_kill();
        tauri::async_runtime::spawn(async move {
            let _ = self.child.wait().await;
        });
    }
}

/// Open a session, returning its id.
pub async fn session_new<W>(client: &RpcClient<W>, params: SessionNewParams) -> AppResult<String>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let raw = client
        .request(
            rpc_methods::SESSION_NEW,
            serde_json::to_value(params)
                .map_err(|e| AppError::new(format!("编码 session/new 失败: {e}")))?,
        )
        .await?;
    let result: SessionNewResult = serde_json::from_value(raw)
        .map_err(|e| AppError::new(format!("解析 session/new 失败: {e}")))?;
    Ok(result.session_id)
}

/// Prompt a session and await its terminal `stopReason`.
pub async fn session_prompt<W>(
    client: &RpcClient<W>,
    params: SessionPromptParams,
) -> AppResult<SessionPromptResult>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let raw = client
        .request_with_timeout(
            rpc_methods::SESSION_PROMPT,
            serde_json::to_value(params)
                .map_err(|e| AppError::new(format!("编码 session/prompt 失败: {e}")))?,
            SESSION_PROMPT_TIMEOUT,
        )
        .await?;
    serde_json::from_value(raw).map_err(|e| AppError::new(format!("解析 session/prompt 失败: {e}")))
}

/// Cancel a running prompt (fire-and-forget notification).
pub async fn session_cancel<W>(client: &RpcClient<W>, session_id: &str) -> AppResult<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    client
        .notify(
            rpc_methods::SESSION_CANCEL,
            serde_json::to_value(SessionCancelParams {
                session_id: session_id.to_string(),
            })
            .map_err(|e| AppError::new(format!("编码 session/cancel 失败: {e}")))?,
        )
        .await
}

/// Build the review skill prompt (mirrors Claude's `/pr-review` construction).
pub fn review_prompt(pr_number: u64, kind: crate::model::ReviewKind) -> String {
    match kind {
        crate::model::ReviewKind::Review => format!("/pr-review {pr_number}"),
        crate::model::ReviewKind::Check => format!("/pr-review {pr_number} --check"),
    }
}

/// Convenience: text-only prompt params.
pub fn text_prompt(session_id: &str, text: impl Into<String>) -> SessionPromptParams {
    SessionPromptParams {
        session_id: session_id.to_string(),
        prompt: vec![ContentBlock::Text { text: text.into() }],
    }
}

const STDERR_MAX_LINE: usize = 512;

fn spawn_stderr_drain(stderr: ChildStderr) {
    tauri::async_runtime::spawn(drain_stderr(BufReader::new(stderr)));
}

async fn drain_stderr<R: AsyncBufRead + Unpin>(mut reader: R) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = match (&mut reader)
            .take(STDERR_MAX_LINE as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let overflowed = !buf.ends_with(b"\n") && n >= STDERR_MAX_LINE;
        eprintln!(
            "[cursor ACP stderr] {}{}",
            String::from_utf8_lossy(&buf).trim_end(),
            if overflowed { " …(已截断)" } else { "" }
        );
        if overflowed {
            discard_to_newline(&mut reader).await;
        }
    }
}

async fn discard_to_newline<R: AsyncBufRead + Unpin>(reader: &mut R) {
    let mut sink: Vec<u8> = Vec::new();
    loop {
        sink.clear();
        match (&mut *reader)
            .take(STDERR_MAX_LINE as u64)
            .read_until(b'\n', &mut sink)
            .await
        {
            Ok(0) => break,
            Ok(_) if sink.ends_with(b"\n") => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(CursorStatus {
            available: true,
            desired_running: true,
            message: "cursor ACP 已就绪".to_string(),
        })
        .expect("CursorStatus serializes");
        assert!(v.get("available").is_some());
        assert!(v.get("desiredRunning").is_some());
        assert!(v.get("desired_running").is_none());
        assert_eq!(v["available"], true);
    }

    #[test]
    fn review_prompt_matches_kind() {
        assert_eq!(
            review_prompt(42, crate::model::ReviewKind::Review),
            "/pr-review 42"
        );
        assert_eq!(
            review_prompt(42, crate::model::ReviewKind::Check),
            "/pr-review 42 --check"
        );
    }

    #[tokio::test]
    async fn drain_stderr_terminates_on_unterminated_flood() {
        let blob = vec![b'x'; STDERR_MAX_LINE * 4];
        drain_stderr(tokio::io::BufReader::new(&blob[..])).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_launches_agent_with_acp_arg_and_repo_cwd() {
        use std::os::unix::fs::PermissionsExt;
        use std::{fs, path::PathBuf};

        use crate::{
            config::service::{resolve_cli_from, CliResolver, CliToolsConfig},
            model::CliTool,
        };

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-spawn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let agent: PathBuf = root.join("agent");
        // Record argv + cwd, then speak a minimal ACP handshake so spawn completes.
        fs::write(
            &agent,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > '{0}/args'
printf '%s\n' "$PWD" > '{0}/cwd'
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"authMethods":[]}}}}'
      ;;
    *'"method":"authenticate"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{}}}}'
      ;;
  esac
done
"#,
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

        let tools: CliToolsConfig = serde_json::from_value(serde_json::json!({
            "ghPath": "",
            "azPath": "",
            "codexPath": "",
            "claudePath": "",
            "agentPath": agent,
            "cloudflaredPath": ""
        }))
        .unwrap();
        let resolved =
            resolve_cli_from(&CliResolver::default(), &tools, CliTool::Agent, false).unwrap();

        let proc = CursorProcess::spawn(&resolved, repo.to_str().unwrap())
            .await
            .expect("spawn + handshake");
        assert_eq!(proc.info.protocol_version, 1);

        let args = fs::read_to_string(root.join("args")).unwrap();
        assert_eq!(args.trim(), "acp");
        let cwd = fs::read_to_string(root.join("cwd")).unwrap();
        assert_eq!(
            std::fs::canonicalize(cwd.trim()).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );

        proc.kill_and_reap();
        let _ = fs::remove_dir_all(root);
    }
}
