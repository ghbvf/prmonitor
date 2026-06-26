# prmonitor iTerm daemon (#1383)

`iterm_daemon.py` is a long-resident bridge between prmonitor (Rust, the `terminal` slice)
and the [iTerm2 Python API](https://iterm2.com/python-api/). prmonitor spawns it as
`python3 iterm_daemon.py` and speaks newline-delimited JSON-RPC over stdio.

## Preconditions

The daemon connects to iTerm INSIDE the `initialize` handler, so each missing precondition
comes back as a structured JSON-RPC error (with an actionable Chinese message), never a
silent crash:

1. **Enable the Python API** — iTerm → Preferences → General → Magic → *Enable Python API*.
2. **First-run authorization** — the first time the daemon connects, iTerm shows a dialog to
   authorize the script; approve it.
3. **`iterm2` library** — install it for the interpreter `python3` resolves to:
   `pip install iterm2` (also pulls in `websockets` / `protobuf`).

## Protocol

NDJSON, one JSON object per line, **no `jsonrpc` field** (MCP-style). Must stay in lock-step
with `src-tauri/src/terminal/protocol.rs`.

```
request  : {"id": N, "method": "...", "params": {...}}
response : {"id": N, "result": ...}  |  {"id": N, "error": {"code": C, "message": "..."}}
notify   : {"method": "...", "params": {...}}            (no id; daemon -> client only)
```

### Requests (client → daemon)

| method          | params                                              | result                          |
| --------------- | --------------------------------------------------- | ------------------------------- |
| `initialize`    | `{}`                                                | `{ itermVersion }`              |
| `listSessions`  | `{}`                                                | `[ TerminalSession, … ]`        |
| `createSession` | `{ windowId?, profile? }`                            | `TerminalSession`               |
| `sendText`      | `{ sessionId, text }`                               | `{}`                            |
| `subscribe`     | `{ sessionId }`                                     | `{ cols, rows }`                |
| `unsubscribe`   | `{ sessionId }`                                     | `{}`                            |
| `resize`        | `{ sessionId, cols, rows }`                         | `{}`                            |

`TerminalSession` = `{ sessionId, windowId, tabId, title, isActive, rows, cols }`.

### Notifications (daemon → client)

| method         | params                                                              |
| -------------- | ------------------------------------------------------------------- |
| `screenUpdate` | `{ sessionId, cols, rows, contents, cursorRow?, cursorCol? }`       |
| `sessionEnded` | `{ sessionId, reason }`                                             |
| `error`        | `{ sessionId?, message }`                                           |

`screenUpdate` carries a FULL visible-screen snapshot (`contents` is the rendered grid, the
iTerm2 `ScreenStreamer` output), not a PTY byte delta — the frontend renders each frame with
`term.reset()` + `term.write(contents)`. The daemon emits no `attached` notification; the
Rust backend synthesizes the one-shot `Attached` event from the `subscribe` ack.

stdout carries ONLY protocol frames; all human diagnostics go to stderr (prmonitor drains and
logs them, line-capped).
