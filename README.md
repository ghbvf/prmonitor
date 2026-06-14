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

## Default branch

The default branch is `develop`. Open PRs against `develop`.
