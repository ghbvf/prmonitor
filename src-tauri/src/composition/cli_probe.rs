use crate::{
    config::service::{diagnose_cli_tools_from, CliResolver, CliToolProbeStatus, CliToolsConfig},
    model::CliTool,
};
use strum::IntoEnumIterator;

#[derive(Default)]
pub(crate) struct ActiveCliFingerprints {
    pub codex: Option<String>,
    /// Resident Cursor ACP (`CliTool::Agent` / `agent acp`) fingerprint.
    pub agent: Option<String>,
    pub webhook_cloudflared: Option<String>,
    pub remote_cloudflared: Vec<String>,
}

pub(crate) fn probe_cli_tools(
    resolver: &CliResolver,
    cli_tools: &CliToolsConfig,
    refresh_path: bool,
    active: &ActiveCliFingerprints,
) -> Vec<CliToolProbeStatus> {
    CliTool::iter()
        .zip(diagnose_cli_tools_from(resolver, cli_tools, refresh_path))
        .map(|(tool, diagnostics)| {
            let configured_path = diagnostics.configured_path;
            match diagnostics.resolution {
                Ok(diagnostics) => {
                    let pending_restart = match tool {
                        CliTool::Gh | CliTool::Az | CliTool::Claude => false,
                        CliTool::Codex => active
                            .codex
                            .as_deref()
                            .is_some_and(|fingerprint| fingerprint != diagnostics.fingerprint),
                        CliTool::Agent => active
                            .agent
                            .as_deref()
                            .is_some_and(|fingerprint| fingerprint != diagnostics.fingerprint),
                        CliTool::Cloudflared => active
                            .webhook_cloudflared
                            .as_deref()
                            .into_iter()
                            .chain(active.remote_cloudflared.iter().map(String::as_str))
                            .any(|fingerprint| fingerprint != diagnostics.fingerprint),
                    };
                    CliToolProbeStatus {
                        tool,
                        configured_path,
                        resolved_path: Some(diagnostics.resolved_path),
                        source: Some(diagnostics.source),
                        available: true,
                        pending_restart,
                        message: if pending_restart {
                            "路径已变更，将在常驻进程下次启动时生效".to_string()
                        } else {
                            "已解析".to_string()
                        },
                    }
                }
                Err(error) => {
                    let pending_restart = match tool {
                        CliTool::Gh | CliTool::Az | CliTool::Claude => false,
                        CliTool::Codex => active.codex.is_some(),
                        CliTool::Agent => active.agent.is_some(),
                        CliTool::Cloudflared => {
                            active.webhook_cloudflared.is_some()
                                || !active.remote_cloudflared.is_empty()
                        }
                    };
                    CliToolProbeStatus {
                        tool,
                        configured_path,
                        resolved_path: None,
                        source: None,
                        available: false,
                        pending_restart,
                        message: if pending_restart {
                            format!("{}；旧常驻进程仍在运行，重启后将无法启动", error.message)
                        } else {
                            error.message
                        },
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::config::{model::CliPath, service::resolve_cli_from};

    #[cfg(unix)]
    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fn executable(path: &Path) {
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn probe_returns_all_rows_isolates_failures_and_marks_resident_mismatch() {
        let root = std::env::temp_dir().join(format!(
            "prmonitor-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let name = |tool: &str| {
            if cfg!(windows) {
                format!("{tool}.exe")
            } else {
                tool.to_string()
            }
        };
        for tool in ["gh", "az", "codex", "claude", "agent"] {
            executable(&root.join(name(tool)));
        }
        let path = |tool: &str| {
            CliPath::try_from(root.join(name(tool)).to_string_lossy().into_owned()).unwrap()
        };
        let cli_tools = CliToolsConfig {
            gh_path: path("gh"),
            az_path: path("az"),
            codex_path: path("codex"),
            claude_path: path("claude"),
            agent_path: path("agent"),
            // Correct basename but deliberately absent: this row alone must be unavailable.
            cloudflared_path: path("cloudflared"),
        };
        let rows = probe_cli_tools(
            &CliResolver::default(),
            &cli_tools,
            false,
            &ActiveCliFingerprints {
                codex: Some("different".to_string()),
                agent: Some("different-agent".to_string()),
                webhook_cloudflared: Some("running-old-cloudflared".to_string()),
                remote_cloudflared: Vec::new(),
            },
        );
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter()
                .find(|row| row.tool == CliTool::Codex)
                .unwrap()
                .pending_restart
        );
        assert!(
            rows.iter()
                .find(|row| row.tool == CliTool::Agent)
                .unwrap()
                .pending_restart,
            "Agent pending_restart when agent fingerprint mismatches"
        );
        let cloudflared = rows
            .iter()
            .find(|row| row.tool == CliTool::Cloudflared)
            .unwrap();
        assert!(!cloudflared.available);
        assert!(cloudflared.pending_restart);
        assert!(cloudflared.message.contains("旧常驻进程仍在运行"));
        assert!(cloudflared.message.contains("重启后将无法启动"));
        assert!(rows
            .iter()
            .filter(|row| row.tool != CliTool::Cloudflared)
            .all(|row| row.available));

        executable(&root.join(name("cloudflared")));
        let resolver = CliResolver::default();
        let current_cloudflared =
            resolve_cli_from(&resolver, &cli_tools, CliTool::Cloudflared, false)
                .unwrap()
                .fingerprint()
                .to_string();
        let cloud_pending = probe_cli_tools(
            &resolver,
            &cli_tools,
            false,
            &ActiveCliFingerprints {
                codex: None,
                agent: None,
                webhook_cloudflared: None,
                remote_cloudflared: vec![current_cloudflared, "old-remote".to_string()],
            },
        );
        assert!(
            cloud_pending
                .iter()
                .find(|row| row.tool == CliTool::Cloudflared)
                .unwrap()
                .pending_restart
        );
        let _ = fs::remove_dir_all(root);
    }
}
