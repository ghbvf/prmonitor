# prmonitor

A Tauri v2 desktop app that monitors GitHub pull requests and, when a PR carries
the right label, dispatches a **codex pr-review** through the [codex
app-server](https://developers.openai.com/codex/app-server) — streaming codex's
review output live in the UI, with a button to stop the session.

It is a GUI front-end for gocell's existing `codex-pr-app-dispatcher` flow: it
reuses the same GitHub label triggers, gating, and de-duplication, but adds the
two things the existing routers lack — **live streaming** of the review and
**active interruption**. The app itself writes no review/label/comment logic;
those side effects belong to the `pr-review` skill that codex runs.

## Status

Early development. Built incrementally across the PRs tracked in the repo issues.
This commit is the scaffold (PR1): Tauri v2 + Vue 3 + TypeScript + Vite, plus CI.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable) + the
  [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS
- [Node.js](https://nodejs.org/) 22+ and [pnpm](https://pnpm.io/) 11+
- [`gh`](https://cli.github.com/) CLI, authenticated (`gh auth login`)
- [`codex`](https://developers.openai.com/codex/) CLI, logged in (`codex login`)
- [`jq`](https://jqlang.github.io/jq/) — used by the `.claude/hooks/` self-audit
  hooks. They **fail open** (skip silently, never block) when `jq` is absent.

## Develop

```bash
pnpm install        # install frontend deps
pnpm tauri dev      # run the desktop app
pnpm build          # type-check + build the frontend
```

Rust backend lives in `src-tauri/`; the Vue frontend in `src/`.

## Third-party CLI discovery

The app manages `gh`, `az`, `codex`, `claude`, and `cloudflared` from the Third-party CLI
settings panel. An empty path uses automatic discovery in the current process `PATH`, the login
shell `PATH`, then common platform install directories. A custom value must be an absolute path to
an existing executable whose filename matches the selected tool. Every managed child receives the
same enhanced `PATH`, so skills launched by Codex or Claude can find the configured `gh` and `az`
even when a desktop launch did not inherit shell environment variables.

Changing a path affects the next one-shot command immediately. A running Codex app-server,
Webhook Quick Tunnel, or Remote Access Quick Tunnel is not interrupted; the settings panel marks
the change as pending until that resident process is stopped/restarted, exits naturally, or the app
restarts. The removed `cloudflaredBin` setting is intentionally not migrated—configure
`cliTools.cloudflaredPath` or leave it empty for automatic discovery.

## Feishu long connection and Codex MCP

Feishu inbound messages use the official long connection; no public callback URL or tunnel is
required. Configure an enabled Feishu messaging integration with App ID, App Secret and at least
one allowed conversation. In the Feishu developer console, enable the bot and subscribe to
`im.message.receive_v1` plus the current `card.action.trigger` event. Do not run the development
build and an installed build with the same Feishu credentials at the same time: Feishu distributes
events across connections instead of broadcasting them.

The settings and Messaging views show `disabled`, `connecting`, `connected`, `reconnecting`,
`error`, or `stopped`, together with the last connection/event/error and reconnect count. The old
Feishu HTTP webhook route returns `410 Gone`; WeCom and DingTalk continue using their HTTP inbound
routes.

The existing loopback Local API also serves a Streamable HTTP MCP endpoint at
`http://127.0.0.1:8788/api/mcp`. The route is mounted only when the Local API entrypoint binds to a
loopback address and no enabled tunnel targets that entrypoint. It uses the existing
`localApiToken` bearer token, rejects non-loopback Host/Origin values, and is optional for Codex
startup. A Codex user configuration can define it once and leave it disabled by default:

```toml
[mcp_servers.prmonitor_human]
url = "http://127.0.0.1:8788/api/mcp"
bearer_token_env_var = "PRMONITOR_LOCAL_API_TOKEN"
enabled = false
required = false
startup_timeout_sec = 5
tool_timeout_sec = 3900
enabled_tools = ["ask_via_feishu"]
default_tools_approval_mode = "approve"

[mcp_servers.prmonitor_human.tools.ask_via_feishu]
approval_mode = "approve"
```

Set `PRMONITOR_LOCAL_API_TOKEN` in the environment that launches Codex to the current
prmonitor `localApiToken`. If an older configuration contains an `Authorization` entry in
`http_headers`, remove that entry after migrating to `bearer_token_env_var`; do not print the
token in shell history, logs, or diagnostic output. To rotate the credential, update
`localApiToken`, update the launch environment, and restart prmonitor and Codex so neither process
continues using the previous value.

A trusted repository enables the inherited server with:

```toml
[mcp_servers.prmonitor_human]
enabled = true
```

`ask_via_feishu` creates one durable request, sends a Feishu card, and opens a Codex elicitation.
The first valid answer wins through a SQLite compare-and-set. Text fallbacks are
`/answer Q-id <answer>` (multiple answers: `q1=A;q2=B`) and `/cancel Q-id`. Tool/sandbox
permissions remain native Codex approvals and are never delegated to Feishu.

## Default branch

The default branch is `develop`. Open PRs against `develop`.
