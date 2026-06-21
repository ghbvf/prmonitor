//! Spawn a one-shot `claude -p` (Claude Code headless) child (cwd = `repo_root`)
//! and parse its line-delimited `stream-json` output (#718).
//!
//! Two concerns live here:
//! 1. [`spawn_claude`] — the subprocess discipline (piped stdio, `kill_on_drop`,
//!    stderr drained concurrently, bounded line reads), mirroring the codex
//!    `process` module.
//! 2. [`parse_line`] — a PURE function mapping ONE `stream-json` line to a
//!    [`ParsedEvent`], the engine's testable seam: the whole envelope mapping is
//!    unit-tested by feeding sample JSONL lines WITHOUT spawning a process. The
//!    engine layer then stamps `project_id`/`thread_id` onto the resulting
//!    [`crate::events::ReviewEvent`] — kept out of the parser so it stays a pure,
//!    context-free transform.

use std::process::Stdio;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use crate::error::{AppError, AppResult};

/// The `claude` binary name (PATH-resolved). Single source mirrored by the review
/// command + composition root, like codex's `CODEX_BIN`.
pub const CLAUDE_BIN: &str = "claude";

/// Max bytes buffered for a single stdout line. One `stream-json` line is a whole
/// JSON object; a delta line stays small, but this bounds memory against a
/// pathological unterminated flood (the line is then skipped, not parsed).
const STDOUT_MAX_LINE: usize = 1024 * 1024;

/// Max bytes logged per stderr line (bounds memory + log volume; `claude` is a
/// trusted local child, so truncation suffices — same discipline as codex).
const STDERR_MAX_LINE: usize = 512;

/// One semantic unit parsed from a `claude -p` `stream-json` line — the engine maps
/// each onto an existing [`crate::events::ReviewEvent`] (stamping project/thread).
/// Pure data; no `project_id`/`thread_id` so [`parse_line`] stays context-free and
/// table-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    /// The first `system`/`init` line — carries the session id (the `SessionId` /
    /// `thread_id` for the whole review). Captured to drive `promote_reservation`.
    SessionStarted { session_id: String },
    /// An assistant message text delta (`content_block_delta` / `text_delta`).
    /// `item_id` is stable per content block so `history_store::append_item`
    /// coalescing works (the engine builds it from the message id + block index).
    MessageDelta { item_id: String, text: String },
    /// A reasoning (thinking) text delta (`content_block_delta` / `thinking_delta`).
    ReasoningDelta { item_id: String, text: String },
    /// The terminal `result` line. `is_error` distinguishes a failed run (the engine
    /// emits a session `Error` first, then a `failed` `TurnCompleted`) from success
    /// (a `completed` `TurnCompleted`). `message` carries the result text for the
    /// error case.
    Result { is_error: bool, message: String },
}

/// Per-stream parser state threaded across [`parse_line`] calls: the current
/// `message.id` (set by `message_start`) used to build a stable per-content-block
/// `item_id`. `claude` does not repeat the message id on each delta, so we carry it.
#[derive(Debug, Default)]
pub struct ParserState {
    /// The most recent `message.id` seen on a `message_start` event. Falls back to
    /// the session id when absent so an `item_id` is always non-empty.
    message_id: Option<String>,
}

/// Map ONE `stream-json` line to a [`ParsedEvent`], or `None` for a line we ignore
/// (status/rate-limit/consolidated message objects/unhandled stream events). PURE:
/// no IO, no process — the engine's whole envelope mapping is exercised by feeding
/// sample lines in the unit tests below (the key testable seam, no real subprocess).
///
/// The `session_id` fallback for `item_id` keeps coalescing stable even if a delta
/// arrives before any `message_start` (defensive — the live envelope always sends
/// `message_start` first).
pub fn parse_line(line: &str, state: &mut ParserState, session_id: &str) -> Option<ParsedEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v.get("type").and_then(|t| t.as_str())? {
        // First line: `{"type":"system","subtype":"init",...,"session_id":"<uuid>"}`.
        // Other system subtypes (status, …) carry no stream content → ignored.
        "system" => {
            if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                let session_id = v.get("session_id").and_then(|s| s.as_str())?;
                return Some(ParsedEvent::SessionStarted {
                    session_id: session_id.to_string(),
                });
            }
            None
        }
        // Anthropic streaming events wrapped as `{"type":"stream_event","event":{…}}`.
        "stream_event" => parse_stream_event(v.get("event")?, state, session_id),
        // Terminal: `{"type":"result","subtype":"…","is_error":bool,"result":…}`.
        "result" => {
            let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            // `result` is usually a string; tolerate a missing/non-string value.
            let message = v
                .get("result")
                .and_then(|r| r.as_str())
                .map(str::to_string)
                .unwrap_or_default();
            Some(ParsedEvent::Result { is_error, message })
        }
        // Consolidated message-level objects (`assistant`/`user`) — we already
        // streamed the deltas, so ignore to avoid double-counting. Likewise
        // `rate_limit_event` and any other top-level type.
        _ => None,
    }
}

/// Map the inner Anthropic `event` object of a `stream_event` line. Records the
/// message id on `message_start`; turns `content_block_delta` text/thinking deltas
/// into [`ParsedEvent`]s; ignores the structural events (start/stop/message_delta).
fn parse_stream_event(
    event: &serde_json::Value,
    state: &mut ParserState,
    session_id: &str,
) -> Option<ParsedEvent> {
    match event.get("type").and_then(|t| t.as_str())? {
        // Record the message id so each content block gets a stable `item_id`.
        "message_start" => {
            if let Some(id) = event
                .get("message")
                .and_then(|m| m.get("id"))
                .and_then(|i| i.as_str())
            {
                state.message_id = Some(id.to_string());
            }
            None
        }
        "content_block_delta" => {
            let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
            let delta = event.get("delta")?;
            let item_id = item_id_for(state, session_id, index);
            match delta.get("type").and_then(|t| t.as_str())? {
                "text_delta" => {
                    let text = delta.get("text").and_then(|t| t.as_str())?;
                    Some(ParsedEvent::MessageDelta {
                        item_id,
                        text: text.to_string(),
                    })
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(|t| t.as_str())?;
                    Some(ParsedEvent::ReasoningDelta {
                        item_id,
                        text: text.to_string(),
                    })
                }
                // input_json_delta (tool input) etc. are not streamed to the UI.
                _ => None,
            }
        }
        // message_stop / content_block_start / content_block_stop / message_delta:
        // structural framing, no UI-streamed text → ignored for MVP.
        _ => None,
    }
}

/// Build the stable per-content-block `item_id` `"<message_id_or_session>:<index>"`,
/// so `history_store::append_item` coalesces every delta of one block into one row.
fn item_id_for(state: &ParserState, session_id: &str, index: u64) -> String {
    let base = state.message_id.as_deref().unwrap_or(session_id);
    format!("{base}:{index}")
}

/// Build the review prompt the headless `claude -p` runs. For a `check` →
/// `/pr-review <N> --check`; otherwise `/pr-review <N>`. `claude` auto-discovers the
/// local `.claude/skills/pr-review/` skill (cwd = repo_root), so no skill path is
/// passed. These strings contain none of the gh-write denylist substrings (the
/// `/pr-review` hyphen form is safe vs `dispatch.rs`'s governance scan).
pub fn review_prompt(pr_number: u64, kind: &str) -> String {
    if kind == "check" {
        format!("/pr-review {pr_number} --check")
    } else {
        format!("/pr-review {pr_number}")
    }
}

/// A spawned one-shot `claude -p` child plus its taken stdout/stderr handles. The
/// engine reads `stdout` line by line and drains `stderr` concurrently. `kill_on_drop`
/// is set, so dropping this (e.g. the [`super::manager::ClaudeManager`] removing it on
/// `stop`/`shutdown`) kills the child.
pub struct ClaudeProcess {
    pub child: Child,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// Build the `claude -p` argument vector. Pure (no spawn) so the `--model` insertion is
/// unit-testable — `spawn_claude` itself needs a real child and can't be. A non-blank
/// `model` appends `--model <name>`; a blank one (config left empty) is omitted, so claude
/// falls back to its own default model.
fn claude_cli_args(model: &str, prompt: &str) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        prompt.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--include-partial-messages".to_string(),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
    ];
    if !model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args
}

/// Spawn `claude -p "<prompt>" --output-format stream-json --verbose
/// --include-partial-messages --permission-mode bypassPermissions [--model <name>]` in
/// `repo_root`, with piped stdout/stderr and `kill_on_drop(true)`. Headless print mode is
/// unattended (`bypassPermissions`), so a review never blocks on an approval prompt. A
/// blank `model` omits `--model` (claude CLI default).
pub fn spawn_claude(
    claude_bin: &str,
    repo_root: &str,
    model: &str,
    prompt: &str,
) -> AppResult<ClaudeProcess> {
    let mut cmd = Command::new(claude_bin);
    cmd.args(claude_cli_args(model, prompt))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !repo_root.trim().is_empty() {
        cmd.current_dir(repo_root);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new(format!("无法启动 claude（未安装或不在 PATH？）: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::new("claude stdout 不可用".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("claude stderr 不可用".to_string()))?;
    Ok(ClaudeProcess {
        child,
        stdout,
        stderr,
    })
}

/// Drain and log the child's stderr, capping each line so an unterminated flood can't
/// grow the buffer without bound (same discipline as codex's stderr drain). Generic
/// over the reader so it is unit-testable without a real child.
pub async fn drain_stderr<R: AsyncBufRead + Unpin>(mut reader: R) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = match (&mut reader)
            .take(STDERR_MAX_LINE as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => break, // EOF or read error: child gone.
            Ok(n) => n,
        };
        let overflowed = !buf.ends_with(b"\n") && n >= STDERR_MAX_LINE;
        eprintln!(
            "[claude stderr] {}{}",
            String::from_utf8_lossy(&buf).trim_end(),
            if overflowed { " …(已截断)" } else { "" }
        );
        if overflowed {
            discard_to_newline(&mut reader).await;
        }
    }
}

/// Drop the rest of an over-long stderr line so it is logged once, not as a flood of
/// fixed-size chunks (mirrors codex's `discard_to_newline`).
async fn discard_to_newline<R: AsyncBufRead + Unpin>(reader: &mut R) {
    let mut sink: Vec<u8> = Vec::new();
    loop {
        sink.clear();
        match (&mut *reader)
            .take(STDERR_MAX_LINE as u64)
            .read_until(b'\n', &mut sink)
            .await
        {
            Ok(0) => break,                          // EOF
            Ok(_) if sink.ends_with(b"\n") => break, // consumed through the newline
            Ok(_) => continue,                       // more of the long line
            Err(_) => break,
        }
    }
}

/// Read the next `stream-json` stdout line, bounding the buffer at [`STDOUT_MAX_LINE`].
/// Returns `Ok(None)` at EOF (child finished / closed stdout). An over-long line is
/// truncated at the cap (the rest discarded) so a pathological line can't OOM — such a
/// line then fails JSON parse and is skipped by [`parse_line`]. Generic over the reader
/// for unit-testability without a real child.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    let n = (&mut *reader)
        .take(STDOUT_MAX_LINE as u64)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    // Drop the rest of an over-cap line so the next read starts at the next line.
    if !buf.ends_with(b"\n") && n >= STDOUT_MAX_LINE {
        discard_to_newline(reader).await;
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Convenience to wrap a child's stdout in a buffered reader for [`read_line`].
pub fn stdout_reader(stdout: ChildStdout) -> BufReader<ChildStdout> {
    BufReader::new(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── prompt builder ──────────────────────────────────────────────────────────
    #[test]
    fn review_prompt_matches_kind() {
        assert_eq!(review_prompt(7, "review"), "/pr-review 7");
        assert_eq!(review_prompt(7, "check"), "/pr-review 7 --check");
        // Any non-"check" kind is a review (mirrors session.rs::skill_command).
        assert_eq!(review_prompt(9, "anything"), "/pr-review 9");
    }

    #[test]
    fn review_prompt_has_no_gh_write_substrings() {
        // Defense-in-depth against the dispatch.rs governance scan: the prompt the app
        // spawns must contain none of the forbidden gh-write substrings. The hyphenated
        // `/pr-review` is safe (the scan forbids the SPACE form). The patterns are built
        // from fragments so the forbidden literals don't appear verbatim in THIS source —
        // otherwise the scan (which walks all of `src`, this file included) would match
        // its own test, the same trick `dispatch.rs` uses.
        let forbidden = [
            format!("pr rev{}", "iew"),
            format!("pr com{}", "ment"),
            format!("pr ed{}", "it"),
        ];
        for p in [review_prompt(7, "review"), review_prompt(7, "check")] {
            for pat in &forbidden {
                assert!(!p.contains(pat.as_str()), "{p:?} must not contain {pat:?}");
            }
        }
    }

    // ── CLI arg builder (the --model injection seam — NO subprocess) ─────────────
    #[test]
    fn claude_cli_args_omit_model_when_blank() {
        for blank in ["", "   "] {
            let args = claude_cli_args(blank, "/pr-review 7");
            assert!(
                !args.iter().any(|a| a == "--model"),
                "blank model must not add --model: {args:?}"
            );
            // The base flags are still present.
            assert_eq!(args[0], "-p");
            assert_eq!(args[1], "/pr-review 7");
            assert!(args.iter().any(|a| a == "bypassPermissions"));
        }
    }

    #[test]
    fn claude_cli_args_append_model_when_set() {
        let args = claude_cli_args("claude-opus-4-1", "/pr-review 7");
        // `--model <name>` is appended as the trailing pair.
        let n = args.len();
        assert_eq!(args[n - 2], "--model");
        assert_eq!(args[n - 1], "claude-opus-4-1");
    }

    // ── pure stream-json line parser (the testable seam — NO subprocess) ─────────
    const SID: &str = "sess-uuid";

    fn parse(line: &str, state: &mut ParserState) -> Option<ParsedEvent> {
        parse_line(line, state, SID)
    }

    #[test]
    fn init_line_yields_session_id() {
        let mut st = ParserState::default();
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123","tools":[]}"#;
        assert_eq!(
            parse(line, &mut st),
            Some(ParsedEvent::SessionStarted {
                session_id: "abc-123".to_string()
            })
        );
    }

    #[test]
    fn system_status_line_is_ignored() {
        let mut st = ParserState::default();
        let line = r#"{"type":"system","subtype":"status","message":"working"}"#;
        assert_eq!(parse(line, &mut st), None);
    }

    #[test]
    fn rate_limit_line_is_ignored() {
        let mut st = ParserState::default();
        let line = r#"{"type":"rate_limit_event","limit":100}"#;
        assert_eq!(parse(line, &mut st), None);
    }

    #[test]
    fn text_delta_yields_message_delta_with_message_scoped_item_id() {
        let mut st = ParserState::default();
        // message_start records the id used for the item_id.
        let start =
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}}}"#;
        assert_eq!(parse(start, &mut st), None);
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#;
        assert_eq!(
            parse(delta, &mut st),
            Some(ParsedEvent::MessageDelta {
                item_id: "msg_1:0".to_string(),
                text: "Hello".to_string()
            })
        );
        // A second content block (index 1) coalesces under a distinct item_id.
        let delta2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"!"}}}"#;
        assert_eq!(
            parse(delta2, &mut st),
            Some(ParsedEvent::MessageDelta {
                item_id: "msg_1:1".to_string(),
                text: "!".to_string()
            })
        );
    }

    #[test]
    fn thinking_delta_yields_reasoning_delta() {
        let mut st = ParserState::default();
        let start =
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_9"}}}"#;
        parse(start, &mut st);
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}}"#;
        assert_eq!(
            parse(delta, &mut st),
            Some(ParsedEvent::ReasoningDelta {
                item_id: "msg_9:0".to_string(),
                text: "hmm".to_string()
            })
        );
    }

    #[test]
    fn delta_before_message_start_falls_back_to_session_id() {
        // Defensive: a delta with no prior message_start still yields a stable, non-empty
        // item_id keyed on the session id, so coalescing never collapses to an empty key.
        let mut st = ParserState::default();
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"x"}}}"#;
        assert_eq!(
            parse(delta, &mut st),
            Some(ParsedEvent::MessageDelta {
                item_id: format!("{SID}:2"),
                text: "x".to_string()
            })
        );
    }

    #[test]
    fn structural_stream_events_are_ignored() {
        let mut st = ParserState::default();
        for line in [
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{}}}"#,
            // input_json_delta (tool input) is not UI-streamed.
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}}"#,
        ] {
            assert_eq!(parse(line, &mut st), None, "line should be ignored: {line}");
        }
    }

    #[test]
    fn consolidated_assistant_and_user_objects_are_ignored() {
        // We already streamed the deltas, so the consolidated message objects must NOT
        // be re-emitted (double-counting).
        let mut st = ParserState::default();
        let assistant = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"Hello"}]}}"#;
        assert_eq!(parse(assistant, &mut st), None);
        let user = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        assert_eq!(parse(user, &mut st), None);
    }

    #[test]
    fn result_success_yields_non_error_result() {
        let mut st = ParserState::default();
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"abc"}"#;
        assert_eq!(
            parse(line, &mut st),
            Some(ParsedEvent::Result {
                is_error: false,
                message: "done".to_string()
            })
        );
    }

    #[test]
    fn result_error_yields_error_result_with_message() {
        let mut st = ParserState::default();
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","session_id":"abc"}"#;
        assert_eq!(
            parse(line, &mut st),
            Some(ParsedEvent::Result {
                is_error: true,
                message: "boom".to_string()
            })
        );
    }

    #[test]
    fn result_without_is_error_field_defaults_to_non_error() {
        // A `result` line missing `is_error` must NOT panic and must default to a
        // non-error completion (`is_error:false`) — defends the `.unwrap_or(false)` so a
        // shape change can't silently flip a success into a failure.
        let mut st = ParserState::default();
        let line = r#"{"type":"result","subtype":"success","result":"done"}"#;
        assert_eq!(
            parse(line, &mut st),
            Some(ParsedEvent::Result {
                is_error: false,
                message: "done".to_string()
            })
        );
    }

    #[test]
    fn malformed_or_empty_lines_are_skipped() {
        let mut st = ParserState::default();
        assert_eq!(parse("", &mut st), None);
        assert_eq!(parse("   ", &mut st), None);
        assert_eq!(parse("not json", &mut st), None);
        // A JSON value without a string `type` field is skipped, not panicked.
        assert_eq!(parse("123", &mut st), None);
        assert_eq!(parse(r#"{"no_type":true}"#, &mut st), None);
    }

    // ── stderr / stdout reader bounds (no real child) ───────────────────────────
    #[tokio::test]
    async fn drain_stderr_terminates_on_unterminated_flood() {
        let blob = vec![b'x'; STDERR_MAX_LINE * 4];
        drain_stderr(tokio::io::BufReader::new(&blob[..])).await;
    }

    #[tokio::test]
    async fn read_line_reads_lines_then_eof() {
        let data = b"line one\nline two\n".to_vec();
        let mut reader = tokio::io::BufReader::new(&data[..]);
        assert_eq!(
            read_line(&mut reader).await.unwrap().as_deref(),
            Some("line one\n")
        );
        assert_eq!(
            read_line(&mut reader).await.unwrap().as_deref(),
            Some("line two\n")
        );
        assert_eq!(read_line(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_line_truncates_over_long_line_then_continues_at_next() {
        // A pathological over-cap line must be truncated at the cap (so it can't OOM): the
        // first read yields exactly the capped chunk (no trailing newline — the cap is hit
        // mid-line), and the line's remainder (through its terminating newline) is
        // discarded so the NEXT read resumes cleanly at the following intact `"ok\n"` line.
        // The over-long line IS newline-terminated here (a full logical line longer than
        // the cap), followed by a separate short line.
        let mut data = vec![b'x'; STDOUT_MAX_LINE + 16];
        data.push(b'\n'); // terminates the over-long line
        data.extend_from_slice(b"ok\n"); // a separate, intact following line
        let mut reader = tokio::io::BufReader::new(&data[..]);

        let first = read_line(&mut reader).await.unwrap().expect("a chunk");
        assert_eq!(
            first.len(),
            STDOUT_MAX_LINE,
            "first read capped at the limit"
        );
        assert!(!first.ends_with('\n'), "the capped chunk has no newline");
        assert!(first.bytes().all(|b| b == b'x'), "all the over-long filler");

        // The remainder of the over-long line (its leftover filler + terminating newline)
        // was discarded, so the next read returns the intact following line, not filler.
        assert_eq!(
            read_line(&mut reader).await.unwrap().as_deref(),
            Some("ok\n")
        );
        assert_eq!(read_line(&mut reader).await.unwrap(), None);
    }
}
