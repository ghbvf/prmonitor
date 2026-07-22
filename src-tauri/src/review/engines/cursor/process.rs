//! Spawn and manage an `agent acp` child process (cwd = `repo_root`), owning its
//! stdin/stdout pipes (wired into an [`RpcClient`]) and draining stderr. Provides
//! the `initialize` → `authenticate(cursor_login)` handshake plus the per-connection
//! RPC ops the engine drives over `client()` (`session_new` / `session_prompt` /
//! `session_cancel`). The process is kept *resident* by
//! [`super::manager::CursorManager`].

use std::path::Path;
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

/// Build `agent … acp` argv for unattended full-access ACP.
/// Always `--force --sandbox disabled` (Claude/Codex-parity dangerous mode).
/// Blank/whitespace `model` → omit `--model`; non-blank → insert trimmed after sandbox flags.
/// **Medium**: locked by `cursor_acp_args_*` + Unix spawn recorder tests.
pub(super) fn cursor_acp_args(model: &str) -> Vec<String> {
    let trimmed = model.trim();
    let mut args = vec![
        "--force".to_string(),
        "--sandbox".to_string(),
        "disabled".to_string(),
    ];
    if !trimmed.is_empty() {
        args.push("--model".to_string());
        args.push(trimmed.to_string());
    }
    args.push("acp".to_string());
    args
}

/// A live, handshaken connection to an `agent acp` child.
pub struct CursorProcess {
    child: Child,
    client: Arc<RpcClient<ChildStdin>>,
    pub info: InitializeResult,
    fingerprint: String,
    /// Trimmed model passed at spawn (empty if blank / omitted `--model`).
    spawn_model: String,
}

impl CursorProcess {
    /// Spawn `agent acp` in `repo_root` and complete `initialize` + `authenticate`.
    /// Empty / relative / non-directory `repo_root` fails closed — never inherits an
    /// arbitrary process cwd.
    pub(super) async fn spawn(
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
    ) -> AppResult<Self> {
        let repo_root = repo_root.trim();
        if repo_root.is_empty() {
            return Err(AppError::new(
                "cursor ACP 需要非空 repo_root（cwd）".to_string(),
            ));
        }
        let repo_path = Path::new(repo_root);
        if !repo_path.is_absolute() || !repo_path.is_dir() {
            return Err(AppError::new(format!(
                "cursor ACP 需要绝对且存在的目录作为 repo_root（cwd）: {repo_root}"
            )));
        }
        let spawn_model = model.trim().to_string();
        let mut cmd = agent.command();
        cmd.args(cursor_acp_args(model))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .current_dir(repo_root);

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
            spawn_model,
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

    pub(super) fn spawn_model(&self) -> &str {
        &self.spawn_model
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
    if result.session_id.trim().is_empty() {
        return Err(AppError::new(
            "cursor ACP session/new 返回空 session_id".to_string(),
        ));
    }
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
pub fn review_prompt(_pr_number: u64, command: &str) -> String {
    command.to_string()
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
    fn cursor_acp_args_omit_model_when_blank() {
        let expected = vec![
            "--force".to_string(),
            "--sandbox".to_string(),
            "disabled".to_string(),
            "acp".to_string(),
        ];
        assert_eq!(cursor_acp_args(""), expected.clone());
        assert_eq!(cursor_acp_args("   "), expected.clone());
        assert_eq!(cursor_acp_args("\t\n"), expected);
    }

    #[test]
    fn cursor_acp_args_append_model_when_set() {
        assert_eq!(
            cursor_acp_args("composer-2-fast"),
            vec![
                "--force".to_string(),
                "--sandbox".to_string(),
                "disabled".to_string(),
                "--model".to_string(),
                "composer-2-fast".to_string(),
                "acp".to_string(),
            ]
        );
    }

    #[test]
    fn cursor_acp_args_trim_padded_model_name() {
        assert_eq!(
            cursor_acp_args("  composer-2-fast  "),
            vec![
                "--force".to_string(),
                "--sandbox".to_string(),
                "disabled".to_string(),
                "--model".to_string(),
                "composer-2-fast".to_string(),
                "acp".to_string(),
            ]
        );
    }

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
        assert_eq!(review_prompt(42, "/pr-review 42"), "/pr-review 42");
        assert_eq!(
            review_prompt(42, "/pr-review 42 --check"),
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

        let proc = CursorProcess::spawn(&resolved, repo.to_str().unwrap(), "")
            .await
            .expect("spawn + handshake");
        assert_eq!(proc.info.protocol_version, 1);
        assert_eq!(proc.spawn_model(), "");

        let args = fs::read_to_string(root.join("args")).unwrap();
        assert_eq!(args.trim(), "--force\n--sandbox\ndisabled\nacp");
        let cwd = fs::read_to_string(root.join("cwd")).unwrap();
        assert_eq!(
            std::fs::canonicalize(cwd.trim()).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );

        proc.kill_and_reap();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_with_model_passes_model_flag_in_argv() {
        // F7: non-empty model → argv includes `--model`, name, `acp`; spawn_model() stamps it.
        use std::os::unix::fs::PermissionsExt;
        use std::{fs, path::PathBuf};

        use crate::{
            config::service::{resolve_cli_from, CliResolver, CliToolsConfig},
            model::CliTool,
        };

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-spawn-model-{}-{}",
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

        let proc = CursorProcess::spawn(&resolved, repo.to_str().unwrap(), "composer-2-fast")
            .await
            .expect("spawn + handshake with model");
        assert_eq!(proc.spawn_model(), "composer-2-fast");

        let args: Vec<String> = fs::read_to_string(root.join("args"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            args,
            vec![
                "--force".to_string(),
                "--sandbox".to_string(),
                "disabled".to_string(),
                "--model".to_string(),
                "composer-2-fast".to_string(),
                "acp".to_string()
            ]
        );

        proc.kill_and_reap();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn spawn_rejects_empty_repo_root() {
        use crate::config::service::ResolvedCli;

        let agent = ResolvedCli::for_test(std::env::temp_dir().join("prmonitor-no-such-agent"));
        let err = match CursorProcess::spawn(&agent, "", "").await {
            Ok(_) => panic!("empty repo_root must fail closed"),
            Err(e) => e,
        };
        assert!(
            err.message.contains("repo_root"),
            "unexpected error: {}",
            err.message
        );
        let err_ws = match CursorProcess::spawn(&agent, "   ", "").await {
            Ok(_) => panic!("whitespace-only repo_root must fail closed"),
            Err(e) => e,
        };
        assert!(err_ws.message.contains("repo_root"));
    }

    #[tokio::test]
    async fn spawn_rejects_relative_or_non_dir_repo_root() {
        use crate::config::service::ResolvedCli;

        let agent = ResolvedCli::for_test(std::env::temp_dir().join("prmonitor-no-such-agent"));
        let err_rel = match CursorProcess::spawn(&agent, "relative/path", "").await {
            Ok(_) => panic!("relative repo_root must fail closed"),
            Err(e) => e,
        };
        assert!(
            err_rel.message.contains("repo_root") || err_rel.message.contains("绝对"),
            "unexpected error: {}",
            err_rel.message
        );

        let missing = std::env::temp_dir().join(format!(
            "prmonitor-cursor-missing-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let err_missing = match CursorProcess::spawn(&agent, missing.to_str().unwrap(), "").await {
            Ok(_) => panic!("non-existent repo_root must fail closed"),
            Err(e) => e,
        };
        assert!(
            err_missing.message.contains("repo_root") || err_missing.message.contains("目录"),
            "unexpected error: {}",
            err_missing.message
        );

        // Absolute path that exists but is a file, not a directory.
        let file_path = std::env::temp_dir().join(format!(
            "prmonitor-cursor-not-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&file_path, b"x").unwrap();
        let err_file = match CursorProcess::spawn(&agent, file_path.to_str().unwrap(), "").await {
            Ok(_) => panic!("file repo_root must fail closed"),
            Err(e) => e,
        };
        assert!(
            err_file.message.contains("repo_root") || err_file.message.contains("目录"),
            "unexpected error: {}",
            err_file.message
        );
        let _ = std::fs::remove_file(&file_path);
    }

    #[tokio::test]
    async fn session_new_rejects_empty_session_id() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let serve = tauri::async_runtime::spawn(async move {
            let mut sr = tokio::io::BufReader::new(server_r);
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let id = req["id"].as_i64().unwrap();
            let mut sw = server_w;
            use tokio::io::AsyncWriteExt;
            let resp = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"  "}}}}"#);
            sw.write_all(resp.as_bytes()).await.unwrap();
            sw.write_all(b"\n").await.unwrap();
            sw.flush().await.unwrap();
        });

        let err = session_new(
            &client,
            SessionNewParams {
                cwd: "/tmp".to_string(),
                mcp_servers: vec![],
            },
        )
        .await
        .expect_err("empty session_id must fail closed");
        assert!(
            err.message.contains("session_id"),
            "unexpected error: {}",
            err.message
        );
        serve.await.unwrap();
    }

    /// Wire lock: `session_cancel` emits a JSON-RPC 2.0 notification with method
    /// `session/cancel` (no response expected).
    #[tokio::test]
    async fn session_cancel_notify_uses_session_cancel_method() {
        let (client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_r).lines();
            let line = reader
                .next_line()
                .await
                .expect("read")
                .expect("session/cancel line");
            let v: serde_json::Value = serde_json::from_str(&line).expect("json");
            assert_eq!(v["jsonrpc"], "2.0");
            assert_eq!(v["method"], rpc_methods::SESSION_CANCEL);
            assert!(v.get("id").is_none(), "cancel must be a notification");
            assert_eq!(v["params"]["sessionId"], "sess_to_cancel");
            drop(server_w);
            line
        });

        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);
        session_cancel(&client, "sess_to_cancel")
            .await
            .expect("session_cancel notify");
        drop(client);
        let seen = server.await.expect("server task");
        assert!(seen.contains("session/cancel"));
    }
}
