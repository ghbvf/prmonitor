//! Cross-slice shared types — the contract boundary between slices.
//!
//! Slices must not import each other's internals; any type that crosses a slice
//! boundary lives here. Serialized fields use camelCase for the frontend.

use serde::{Deserialize, Serialize};
use std::{
    fmt,
    ops::Deref,
    path::{Path, PathBuf},
};

macro_rules! validated_string_newtype {
    ($name:ident, $validator:expr) => {
        #[cfg_attr(test, derive(ts_rs::TS))]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                ($validator)(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn non_empty(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err("value must not be empty".to_string())
    } else {
        Ok(())
    }
}

validated_string_newtype!(InboxDedupeKey, non_empty);
validated_string_newtype!(ReviewActionKey, non_empty);
validated_string_newtype!(OutboxProducerKey, non_empty);

/// Stable idempotency id supplied by an external review trigger.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ExternalRequestId(String);

impl ExternalRequestId {
    pub const LENGTH: usize = 32;

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() != Self::LENGTH
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(
                "requestId must be exactly 32 lowercase hexadecimal characters".to_string(),
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for ExternalRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExternalRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct InboxEventId(i64);

impl InboxEventId {
    pub fn new(value: i64) -> Result<Self, String> {
        (value > 0)
            .then_some(Self(value))
            .ok_or_else(|| "inbox event id must be positive".to_string())
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for InboxEventId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(i64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ReviewReceiptId(i64);

impl ReviewReceiptId {
    pub fn new(value: i64) -> Result<Self, String> {
        (value > 0)
            .then_some(Self(value))
            .ok_or_else(|| "review receipt id must be positive".to_string())
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ReviewReceiptId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(i64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl From<InboxEventId> for ReviewReceiptId {
    fn from(value: InboxEventId) -> Self {
        Self(value.get())
    }
}

/// Public lifecycle of an external review receipt. This is intentionally distinct from inbox,
/// outbox, and review-session statuses: callers see one stable funnel state while the backing row
/// advances across those three stores.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewReceiptStatus {
    Received,
    Queued,
    Blocked,
    Starting,
    Running,
    Interrupting,
    Done,
    Failed,
}

/// Durable status returned for an external review request. The receipt id is the originating
/// inbox id; optional review fields become available as the outbox/session linkage is persisted.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewReceiptSnapshot {
    pub receipt_id: ReviewReceiptId,
    pub status: ReviewReceiptStatus,
    pub thread_id: Option<String>,
    pub comment_url: Option<String>,
    pub outcome: Option<String>,
    pub error: Option<String>,
}

/// Review lifecycle events emitted by the review slice and consumed by horizontal notification
/// orchestration. Kept in `model.rs` so review/state/config do not import each other's internals.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ReviewLifecycleEvent {
    #[default]
    Started,
    Completed,
    Failed,
    Interrupted,
}

impl ReviewLifecycleEvent {
    pub fn as_wire(self) -> &'static str {
        match self {
            ReviewLifecycleEvent::Started => "started",
            ReviewLifecycleEvent::Completed => "completed",
            ReviewLifecycleEvent::Failed => "failed",
            ReviewLifecycleEvent::Interrupted => "interrupted",
        }
    }
}

/// Business payload emitted by review sessions and consumed by the composition-root lifecycle
/// fan-out. It crosses review/state/composition boundaries, so it belongs in this shared model
/// contract instead of the composition state holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewLifecycleDispatch {
    pub project_id: String,
    pub pr_number: u64,
    pub skill_key: String,
    pub thread_id: String,
    pub event: ReviewLifecycleEvent,
    pub comment_url: Option<String>,
}

/// Default skill name for the built-in pr-review skill.
pub const DEFAULT_SKILL_NAME: &str = "pr-review";
/// Default (repo-relative) skill path for the built-in pr-review skill.
pub const DEFAULT_SKILL_PATH: &str = ".codex/skills/pr-review/SKILL.md";
/// Default command template: `/{skill} {pr}` with optional `extra_args` appended.
pub const DEFAULT_COMMAND_TEMPLATE: &str = "/{skill} {pr}";

/// Allowed repo-relative prefixes for skill files (Hard allowlist — no other paths).
pub const SKILL_PATH_PREFIXES: &[&str] = &[".codex/skills/", ".claude/skills/", ".cursor/skills/"];

/// Fully-resolved skill invocation carried on review outbox payloads and engine starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInvocation {
    pub skill_name: String,
    /// Absolute skill file path when the consuming engine needs it ([`EngineKind::requires_skill_path`]);
    /// `None` for command-only engines (Claude / Cursor). Relative values are materialised at
    /// engine start (migration may leave a config-relative path or `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    /// Rendered command ready to execute / attach.
    pub command: String,
    /// Stable identity: `"{skill_name}\0{extra_args.trim()}"`.
    pub skill_key: String,
}

impl SkillInvocation {
    pub fn skill_key(name: &str, extra_args: &str) -> String {
        format!("{name}\0{}", extra_args.trim())
    }

    /// External / manual ingress may only select full review (`""`) or check (`"--check"`).
    pub fn validate_extra_args(extra_args: &str) -> Result<&str, String> {
        match extra_args.trim() {
            "" | "--check" => Ok(extra_args.trim()),
            other => Err(format!(
                "extraArgs must be empty or \"--check\", got {other:?}"
            )),
        }
    }

    /// Tokens embedded in Codex backtick prompts must not break the prompt grammar.
    pub fn validate_prompt_token(field: &str, value: &str) -> Result<(), String> {
        if value.contains('`') || value.contains('\n') || value.contains('\r') {
            return Err(format!(
                "{field} must not contain backticks or newlines: {value:?}"
            ));
        }
        Ok(())
    }

    /// Map legacy persisted wire values (`"review"` / `"check"`) to skill keys on load only.
    pub fn migrate_legacy_skill_key(wire: &str) -> String {
        match wire {
            "review" => Self::skill_key(DEFAULT_SKILL_NAME, ""),
            "check" => Self::skill_key(DEFAULT_SKILL_NAME, "--check"),
            other => other.to_string(),
        }
    }

    /// Fail-closed skill_key parse for durable session rows (reserve/stop identity).
    pub fn parse_skill_key_wire(wire: &str) -> Result<String, String> {
        match wire {
            "review" => Ok(Self::skill_key(DEFAULT_SKILL_NAME, "")),
            "check" => Ok(Self::skill_key(DEFAULT_SKILL_NAME, "--check")),
            other if Self::is_skill_key(other) => Ok(other.to_string()),
            other => Err(format!("unknown skill_key wire value: {other:?}")),
        }
    }

    /// `true` when `wire` is a non-empty skill name plus `\0` separator (extra may be empty).
    pub fn is_skill_key(wire: &str) -> bool {
        match wire.split_once('\0') {
            Some((name, _)) => !name.is_empty(),
            None => false,
        }
    }

    /// Migrate a dispatch ledger key `{pr}@{head}:{skill}` when `skill` is a legacy
    /// `"review"` / `"check"` identity (load-only / one-shot DB rewrite).
    pub fn migrate_legacy_dispatch_key(key: &str) -> String {
        match key.rsplit_once(':') {
            Some((prefix, skill_part)) => {
                format!("{prefix}:{}", Self::migrate_legacy_skill_key(skill_part))
            }
            None => key.to_string(),
        }
    }

    /// Human-readable label for notifications / UI (`pr-review` or `pr-review --check`).
    pub fn display_label(skill_key: &str) -> String {
        let key = Self::migrate_legacy_skill_key(skill_key);
        match key.split_once('\0') {
            Some((name, "")) => name.to_string(),
            Some((name, extra)) => format!("{name} {extra}"),
            None => key,
        }
    }

    pub fn render_command(
        template: &str,
        skill_name: &str,
        pr: u64,
        repo: &str,
        extra_args: &str,
    ) -> String {
        let mut command = template
            .replace("{skill}", skill_name)
            .replace("{pr}", &pr.to_string())
            .replace("{repo}", repo);
        let extra = extra_args.trim();
        if !extra.is_empty() {
            command.push(' ');
            command.push_str(extra);
        }
        command
    }

    /// Resolve a **repo-relative** skill path: allowlisted prefix, join `repo_root`, canonicalize,
    /// require the result stays under `repo_root` and is a file. Absolute paths are rejected.
    pub fn resolve_skill_path(repo_root: &str, skill_path: &str) -> Result<PathBuf, String> {
        let path = Path::new(skill_path);
        if path.is_absolute() {
            return Err(format!(
                "skill path must be relative to repo_root (absolute rejected): {skill_path}"
            ));
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("skill path must not contain '..': {skill_path}"));
        }
        let normalized = skill_path.trim_start_matches("./");
        if !SKILL_PATH_PREFIXES
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
        {
            return Err(format!(
                "skill path must be under {} (got {skill_path})",
                SKILL_PATH_PREFIXES.join(" | ")
            ));
        }
        let root = Path::new(repo_root);
        let joined = root.join(path);
        let root_canon = root
            .canonicalize()
            .map_err(|e| format!("repo_root canonicalize failed: {e}"))?;
        let skill_canon = joined
            .canonicalize()
            .map_err(|e| format!("skill path canonicalize failed: {e}"))?;
        if !skill_canon.starts_with(&root_canon) {
            return Err(format!("skill path escapes repo_root: {skill_path}"));
        }
        if !skill_canon.is_file() {
            return Err(format!(
                "skill path is not a file: {}",
                skill_canon.display()
            ));
        }
        Ok(skill_canon)
    }

    /// Assert an already-absolute skill path is a file under `repo_root` and an allowed prefix.
    pub fn assert_skill_path_confined(repo_root: &str, skill_abs: &str) -> Result<(), String> {
        let root = Path::new(repo_root)
            .canonicalize()
            .map_err(|e| format!("repo_root canonicalize failed: {e}"))?;
        let skill = Path::new(skill_abs)
            .canonicalize()
            .map_err(|e| format!("skill path canonicalize failed: {e}"))?;
        if !skill.starts_with(&root) {
            return Err(format!("skill path escapes repo_root: {skill_abs}"));
        }
        if !skill.is_file() {
            return Err(format!("skill path is not a file: {skill_abs}"));
        }
        let rel = skill
            .strip_prefix(&root)
            .map_err(|_| format!("skill path escapes repo_root: {skill_abs}"))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !SKILL_PATH_PREFIXES
            .iter()
            .any(|prefix| rel_str.starts_with(prefix))
        {
            return Err(format!(
                "skill path must be under {} (got {rel_str})",
                SKILL_PATH_PREFIXES.join(" | ")
            ));
        }
        Ok(())
    }

    pub fn build(
        skill_name: impl Into<String>,
        skill_path: Option<String>,
        command: impl Into<String>,
        extra_args: &str,
    ) -> Self {
        let skill_name = skill_name.into();
        let skill_key = Self::skill_key(&skill_name, extra_args);
        Self {
            skill_name,
            skill_path,
            command: command.into(),
            skill_key,
        }
    }

    /// Render the command template and, when `engine.requires_skill_path()`, resolve `skill_path`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_config(
        repo_root: &str,
        skill_name: &str,
        skill_path: &str,
        command_template: &str,
        pr: u64,
        repo: &str,
        extra_args: &str,
        engine: EngineKind,
    ) -> Result<Self, String> {
        Self::validate_prompt_token("skillName", skill_name)?;
        let command = Self::render_command(command_template, skill_name, pr, repo, extra_args);
        Self::validate_prompt_token("command", &command)?;
        let resolved = if engine.requires_skill_path() {
            Some(
                Self::resolve_skill_path(repo_root, skill_path)?
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            None
        };
        Ok(Self::build(skill_name, resolved, command, extra_args))
    }

    /// Materialise a (possibly relative / missing) skill path for `project`'s engine at start time.
    /// Pending outbox rows migrated before path resolution land here and are confined.
    pub fn materialize_for_engine(
        mut self,
        repo_root: &str,
        engine: EngineKind,
    ) -> Result<Self, String> {
        if !engine.requires_skill_path() {
            self.skill_path = None;
            return Ok(self);
        }
        let path = match self.skill_path.as_deref() {
            Some(p) if Path::new(p).is_absolute() => {
                Self::assert_skill_path_confined(repo_root, p)?;
                p.to_string()
            }
            Some(rel) if !rel.is_empty() => Self::resolve_skill_path(repo_root, rel)?
                .to_string_lossy()
                .into_owned(),
            _ => Self::resolve_skill_path(repo_root, DEFAULT_SKILL_PATH)?
                .to_string_lossy()
                .into_owned(),
        };
        Self::validate_prompt_token("skillName", &self.skill_name)?;
        Self::validate_prompt_token("command", &self.command)?;
        self.skill_path = Some(path);
        Ok(self)
    }

    /// Default pr-review invocation; pass `extra_args = "--check"` for the former check mode.
    pub fn default_pr_review(
        repo_root: &str,
        pr: u64,
        repo: &str,
        extra_args: &str,
        engine: EngineKind,
    ) -> Result<Self, String> {
        Self::from_config(
            repo_root,
            DEFAULT_SKILL_NAME,
            DEFAULT_SKILL_PATH,
            DEFAULT_COMMAND_TEMPLATE,
            pr,
            repo,
            extra_args,
            engine,
        )
    }
}

/// A PR discovered by a [`crate::pr::source::EventSourceProvider`] that may need review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    pub author: String,
    pub is_cross_repository: bool,
    pub is_draft: bool,
    /// Skill identity key; discovery may stamp `""`, and the rule plan fills it.
    pub skill_key: String,
}

impl ReviewActionKey {
    pub fn for_candidate(candidate: &Candidate) -> Result<Self, String> {
        Self::for_parts(candidate.number, &candidate.head_sha, &candidate.skill_key)
    }

    pub fn for_parts(
        pr_number: u64,
        head_sha: &str,
        skill_key: impl AsRef<str>,
    ) -> Result<Self, String> {
        let skill_key = skill_key.as_ref();
        if pr_number == 0 {
            return Err("review action key requires a positive PR number".to_string());
        }
        if head_sha.trim().is_empty() {
            return Err("review action key requires a non-empty head SHA".to_string());
        }
        Self::new(format!("{pr_number}@{head_sha}:{skill_key}"))
    }
}

impl OutboxProducerKey {
    pub fn for_rule_action(
        inbox_id: InboxEventId,
        rule_id: &str,
        action_id: &str,
    ) -> Result<Self, String> {
        if rule_id.trim().is_empty() || action_id.trim().is_empty() {
            return Err("rule and action ids must not be empty".to_string());
        }
        Self::new(format!(
            "inbox:{}:rule:{}:{}:action:{}:{}",
            inbox_id.get(),
            rule_id.len(),
            rule_id,
            action_id.len(),
            action_id
        ))
    }

    pub fn for_dedupe(project_id: &str, dedupe_key: &str) -> Result<Self, String> {
        if project_id.trim().is_empty() || dedupe_key.trim().is_empty() {
            return Err("project id and dedupe key must not be empty".to_string());
        }
        Self::new(format!(
            "dedupe:{}:{}:{}:{}",
            project_id.len(),
            project_id,
            dedupe_key.len(),
            dedupe_key
        ))
    }
}

/// Third-party executables managed by the config slice.
///
/// **Hard carrier (upstream):** this sealed enum is the only tool selector accepted by the CLI
/// resolver. Every path lookup is an exhaustive `match`, so adding a managed executable without
/// adding its config slot and canonical basename is a compile error. **Hard carrier (downstream):**
/// the resolver returns only an opaque `ResolvedCli`, whose sole command builder injects the
/// resolved program and enhanced `PATH`; consumers cannot construct a partially configured launch.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::EnumIter)]
#[serde(rename_all = "camelCase")]
pub enum CliTool {
    Gh,
    Az,
    Codex,
    Claude,
    /// Cursor CLI (`agent`) — ACP review engine binary.
    Agent,
    Cloudflared,
}

/// Provenance of a managed CLI resolution, exposed to diagnostics and the settings UI.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliResolutionSource {
    Custom,
    ProcessPath,
    LoginShellPath,
    PlatformFallback,
}

/// Which backend owns a [`TerminalSession`] (#1372).
///
/// **Hard carrier** (sealed enum): the command layer routes per-session ops through an
/// exhaustive `match TerminalBackendKind { ... }` (`terminal::commands::RoutedBackend`), so
/// adding a third backend without handling it everywhere is a compile error — the missing arm
/// cannot be expressed. The second backend (`WebPty`) realizes the seam the `#1383` slice
/// reserved.
///
/// Wire strings are pinned camelCase (`"iterm"` / `"webPty"`) — a cross-agent contract the
/// frontend's TS union mirrors exactly; the serde golden below
/// (`terminal_backend_kind_wire_strings`) locks them against a `rename_all` / variant drift.
/// `Default` is `Iterm`: the iTerm Python daemon returns rows with NO `backend` key, so a
/// `#[serde(default)]` parse fills in `Iterm` (the daemon contract stays unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TerminalBackendKind {
    /// The iTerm2 Python-API daemon backend (#1383).
    #[default]
    Iterm,
    /// A local pseudo-terminal shell backend (#1372): `portable-pty`-spawned, exposed to the
    /// Remote Web Console; cross-platform (unix openpty + Windows ConPTY).
    WebPty,
}

/// One addressable terminal session in the user's iTerm (#1383). The leaf the
/// frontend xterm panel attaches to: `session_id` is the iTerm session GUID (the
/// attach key); `window_id` / `tab_id` group it in the picker (the frontend
/// flattens this list then re-groups window → tab → session client-side);
/// `rows` / `cols` are iTerm's current grid (seed the xterm size + validate a
/// fit-driven resize).
///
/// Front/back contract (UNLIKE backend-internal [`Candidate`]): the `terminal`
/// slice returns `Vec<TerminalSession>` over the `list_terminal_sessions` Tauri
/// command, so it IS mirrored in `src/types.ts` and the golden below locks the
/// camelCase wire shape both sides depend on. The [`backend`](Self::backend)
/// discriminator (#1372) tells the frontend which backend owns the row (label
/// "iTerm" vs "Web PTY", route close); it is ALWAYS serialized but
/// `#[serde(default)]` on parse, so the iTerm daemon's rows (which carry NO
/// `backend` key) deserialize to the Default `Iterm`. `Deserialize` too: the
/// Python daemon returns this camelCase shape, Rust parses then re-serializes to
/// the frontend (same dual-derive rationale as [`Candidate`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSession {
    pub session_id: String,
    pub window_id: String,
    pub tab_id: String,
    pub title: String,
    pub is_active: bool,
    pub rows: u16,
    pub cols: u16,
    /// Which backend owns this session (#1372). ALWAYS serialized (no `skip`) so the frontend
    /// can label + route; `#[serde(default)]` so the iTerm daemon's `backend`-less rows parse to
    /// `Iterm`.
    #[serde(default)]
    pub backend: TerminalBackendKind,
}

/// Options for `create_terminal_session` (#1383). Both optional: `None` lets the
/// daemon pick (a fresh window with the default profile). `skip_serializing_if`
/// OMITS an absent key so the daemon-side JSON matches the optional `windowId?` /
/// `profile?` the frontend `CreateSessionOpts` mirror declares (an absent key,
/// not a JSON `null`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionOpts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Which backend to create the session on (#1372). `None` routes to the default `Iterm`
    /// backend (the pre-#1372 behavior the iTerm daemon expects — `skip_serializing_if` OMITS the
    /// key so its createSession JSON is byte-unchanged); `Some(WebPty)` spawns a local PTY shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<TerminalBackendKind>,
}

/// Which PR source backs the monitor.
///
/// **Hard carrier** (sealed enum): once PR3+ wires source selection through an
/// exhaustive `match SourceKind { ... }`, adding a variant without handling it
/// is a compile error — the missing arm cannot be expressed. Today it has one
/// variant, so the seam is reserved but not yet load-bearing.
///
/// #11 design reservation: future variant `GitLab`. Wire strings for `Github` /
/// `Azure` / `Bitbucket` are pinned to `"github"` / `"azure"` / `"bitbucket"`
/// (cross-agent contract; the frontend mirrors them and a serde golden test locks them).
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    #[default]
    Github,
    /// Azure DevOps Repos: pulled via the `az repos pr list` CLI (#818) and pushed via
    /// inbound Azure DevOps Service Hooks on `/webhook` (`git.pullrequest.created/updated`,
    /// AB#822) — both feed the same PR list / dispatch path.
    Azure,
    /// Bitbucket Server / Data Center: pulled via the REST API v1.0 over HTTP
    /// (`{host}/rest/api/1.0/projects/{project}/repos/{repo}/pull-requests`, `reqwest`,
    /// AB#717). Bitbucket Server PRs carry NO native labels, so a Bitbucket project
    /// must use [`LabelSource::Title`] (status labels written into the PR title, e.g.
    /// `[pr-status/need-fix]`). No inbound webhook yet (poll/API discovery only).
    Bitbucket,
    // future #11: GitLab
}

/// Where a project's review/check trigger labels come from (AB#717).
///
/// **Hard carrier** (sealed enum): label resolution branches on an exhaustive
/// `match LabelSource { ... }` (`crate::pr::labels::effective_labels`), so adding a
/// variant without handling it is a compile error.
///
/// Wire strings are pinned camelCase (`"native" | "title"`) — a cross-agent contract
/// the frontend's TS union mirrors exactly; a serde golden test below locks it.
///
/// - [`Native`](Self::Native) (default, status quo): labels are the source provider's
///   own PR labels (GitHub labels / Azure DevOps tags).
/// - [`Title`](Self::Title): labels are parsed from bracketed segments in the PR title
///   (`[pr-status/need-fix][wip]` → `["pr-status/need-fix", "wip"]`). The ONLY viable
///   mode for Bitbucket Server, which has no native PR labels.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LabelSource {
    #[default]
    Native,
    Title,
}

/// Per-project data-update mode (#818): how a project's PR list is kept fresh.
///
/// **Hard carrier** (sealed enum): source/poll selection branches on an exhaustive
/// `match UpdateMode { ... }` (the scheduler's `periodic_polling` and `poll_now`),
/// so adding a variant without handling it is a compile error.
///
/// Wire strings are pinned kebab-case (`"webhook-only" | "pull-only" | "hybrid" |
/// "manual"`) — a cross-agent contract the frontend's TS union must mirror exactly;
/// a serde golden test below locks it.
///
/// Modes:
/// - [`WebhookOnly`](Self::WebhookOnly) (default, the safe boot behavior): the list
///   is updated ONLY by inbound webhook deliveries — NO automatic CLI polling. A
///   manual pull is rejected (there is no source to pull from in this mode).
/// - [`PullOnly`](Self::PullOnly): a periodic CLI poll loop is the sole update source.
/// - [`Hybrid`](Self::Hybrid): both a periodic CLI poll loop AND inbound webhooks.
/// - [`Manual`](Self::Manual): no periodic loop; the list updates only on an explicit
///   "立即拉取" (one-shot CLI discovery) or inbound webhook.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMode {
    #[default]
    WebhookOnly,
    PullOnly,
    Hybrid,
    Manual,
}

/// Which review engine runs against a PR.
///
/// **Hard carrier** (sealed enum): engine selection is wired through exhaustive
/// `match EngineKind { ... }` in the review start funnel (`commands.rs::start_via_engine`), so
/// adding a variant without handling it is a compile error — the missing arm cannot be expressed.
/// Load-bearing: `Codex`, `Claude`, and `Cursor` (ACP via `agent acp`).
///
/// Wire strings are a cross-agent contract the frontend mirrors (`ENGINE_KINDS`
/// in `src/types.ts`): `Codex → "codex"`, `Claude → "claude"`, `Cursor → "cursor"`. The serde golden
/// test below (`discriminator_enums_serialize_to_pinned_wire_strings`) is the
/// **Medium** carrier locking those strings against a `rename_all` / variant drift.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EngineKind {
    #[default]
    Codex,
    /// `claude -p` headless (Claude Code) review engine (#718).
    Claude,
    /// Cursor CLI ACP (`agent acp`) review engine.
    Cursor,
}

impl EngineKind {
    /// Codex attaches an on-disk skill file (`UserInput::Skill`); Claude/Cursor only use the
    /// rendered command prompt — path resolve must not gate those engines.
    pub const fn requires_skill_path(self) -> bool {
        matches!(self, Self::Codex)
    }
}

/// Per-turn reasoning effort accepted by the Codex app-server protocol.
///
/// **Hard carrier**: project configuration cannot contain an arbitrary effort string, and the
/// review adapter must exhaustively convert every variant into the private protocol wire type.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningEffort {
    #[default]
    Default,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

/// Reasoning effort accepted by the Claude CLI.
///
/// The sealed enum plus exhaustive argv construction makes unsupported raw strings
/// unrepresentable after the serde boundary.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeEffort {
    #[default]
    Default,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// How the webhook receiver's local port is exposed to the public internet (#9).
///
/// Lives here (not in `pr/webhook.rs`) because, like [`SourceKind`] / [`EngineKind`],
/// it is a config→pr cross-slice kind enum: the `config` slice persists it on
/// `AppConfig` and the `pr` slice's `webhook::start` consumes it to branch the tunnel
/// strategy. Keeping it in `crate::model` is the SINGLE source both slices import,
/// rather than each defining its own — mirroring the `SourceKind` precedent.
///
/// Wire strings are pinned lowercase (`"quick" | "command" | "listener"`) — a
/// cross-agent contract the frontend's TS union must mirror exactly; a serde golden
/// test below locks it.
///
/// Modes:
/// - [`Quick`](Self::Quick) (default, unchanged status quo): spawn a Cloudflare Quick
///   Tunnel via `cloudflared` and scrape the `*.trycloudflare.com` URL.
/// - [`Command`](Self::Command): spawn a user-supplied tunnel command (e.g. a named
///   cloudflared tunnel, `ngrok`, …); `publicUrl` comes from config, not scraped.
/// - [`Listener`](Self::Listener): only bind the local port; the tunnel is fully
///   external (no child process); `publicUrl` comes from config.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebhookTunnelMode {
    #[default]
    Quick,
    Command,
    Listener,
}

/// A PR row shown in the UI (display superset of [`Candidate`]).
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestView {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
    /// Skill identity key for the review action this PR maps to.
    pub skill_key: String,
    /// Why this PR would be skipped (not dispatched), or `None` when it would
    /// dispatch. Serializes to `null` / a string for the frontend.
    pub skip_reason: Option<String>,
}

/// Whether a tracked PR was seen in the latest discovery window or has aged out.
///
/// `Current` = last seen within the presence grace window (the live working set);
/// `Stale` = not seen recently (a transient `gh` miss or a genuinely closed PR),
/// retained so a one-round miss flips presence rather than dropping the row.
///
/// `Serialize`-only by design: this is a frontend projection derived from a
/// [`crate::pr::registry::TrackedPr`]'s `last_seen_epoch` vs the grace window, never
/// persisted or read back, so it intentionally does NOT derive `Deserialize` (the
/// persisted shape is `TrackedPr`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PrPresence {
    Current,
    Stale,
}

/// Tracking-aware PR row emitted to the frontend (persisted-retention view).
///
/// Flattens [`PullRequestView`] so the wire shape stays a flat row plus the two
/// retention fields (`presence` / `archived`) the persisted-list UI renders.
///
/// `Serialize`-only by design: this is the frontend projection of a
/// [`crate::pr::registry::TrackedPr`] (computed per emit), never persisted or read
/// back, so it intentionally does NOT derive `Deserialize` — `TrackedPr` is the
/// persisted type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedPrView {
    #[serde(flatten)]
    pub pr: PullRequestView,
    pub presence: PrPresence,
    pub archived: bool,
}

/// The class of a normalized inbound [`EventEnvelope`] observation: the event
/// pipeline's first cross-slice discriminator. The inbox (AB#1065) persists it; the rule
/// engine (AB#1068) matches on it.
///
/// **Hard carrier** (sealed enum): once a consumer (the inbox normalizer / rule matcher)
/// branches on an exhaustive `match EventType { ... }`, adding a variant without an arm is
/// a compile error — the missing arm cannot be expressed. Today the seam is RESERVED (no
/// consumer yet — the webhook path only emits `PullRequest`), exactly like [`SourceKind`]'s
/// reserved-but-not-yet-load-bearing note; the Hard carrier closes when the inbox lands.
///
/// Wire strings are pinned camelCase (`"pullRequest" | "issue" | "comment" | "label" |
/// "generic"`) — a cross-agent contract the frontend's `EVENT_TYPES` (`src/types.ts`)
/// mirrors; the serde golden test below (`event_type_serializes_to_pinned_wire_strings`)
/// is the **Medium** carrier locking them against a `rename_all` / variant drift.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EventType {
    /// A pull-request event (the only class the current webhook path emits).
    #[default]
    PullRequest,
    /// An issue event (reserved for the inbox's issue ingestion, AB#1065).
    Issue,
    /// An issue/PR comment event.
    Comment,
    /// A label add/remove event.
    Label,
    /// A generic webhook event that does not map to the classes above.
    Generic,
}

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExternalTriggerOrigin {
    Http,
    Cli,
    RemoteWeb,
    DeepLink,
    MessagingBot,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventSubject {
    pub number: Option<u64>,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub url: String,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EventPayload {
    Observation {
        #[serde(rename = "eventType")]
        event_type: EventType,
        subject: EventSubject,
    },
    ReviewRequest {
        #[serde(rename = "prNumber")]
        pr_number: u64,
        #[serde(rename = "skillName")]
        skill_name: String,
        #[serde(rename = "extraArgs")]
        extra_args: String,
        #[serde(rename = "skillPath")]
        skill_path: String,
        #[serde(rename = "commandTemplate")]
        command_template: String,
        #[serde(rename = "requestId")]
        request_id: ExternalRequestId,
        origin: ExternalTriggerOrigin,
        #[serde(rename = "notifyOnCompletion")]
        notify_on_completion: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum EventPayloadWire {
    Observation {
        #[serde(rename = "eventType")]
        event_type: EventType,
        subject: EventSubject,
    },
    ReviewRequest {
        #[serde(rename = "prNumber")]
        pr_number: u64,
        #[serde(rename = "skillName")]
        skill_name: String,
        #[serde(rename = "extraArgs")]
        extra_args: String,
        #[serde(rename = "skillPath")]
        skill_path: String,
        #[serde(rename = "commandTemplate")]
        command_template: String,
        #[serde(rename = "requestId")]
        request_id: ExternalRequestId,
        origin: ExternalTriggerOrigin,
        #[serde(rename = "notifyOnCompletion")]
        notify_on_completion: bool,
    },
}

impl<'de> Deserialize<'de> for EventPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match EventPayloadWire::deserialize(deserializer)? {
            EventPayloadWire::Observation {
                event_type,
                subject,
            } => Self::Observation {
                event_type,
                subject,
            },
            EventPayloadWire::ReviewRequest {
                pr_number,
                skill_name,
                extra_args,
                skill_path,
                command_template,
                request_id,
                origin,
                notify_on_completion,
            } => Self::ReviewRequest {
                pr_number,
                skill_name,
                extra_args,
                skill_path,
                command_template,
                request_id,
                origin,
                notify_on_completion,
            },
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ObservationRef<'a> {
    pub event_type: EventType,
    pub subject: &'a EventSubject,
}

#[derive(Debug, Clone, Copy)]
pub struct ReviewRequestRef<'a> {
    pub pr_number: u64,
    pub skill_name: &'a str,
    pub extra_args: &'a str,
    pub skill_path: &'a str,
    pub command_template: &'a str,
    pub request_id: &'a ExternalRequestId,
    pub origin: ExternalTriggerOrigin,
    pub notify_on_completion: bool,
}

/// Typed inbound envelope. Private fields plus constructors make an invalid payload/envelope
/// pairing unrepresentable; serde rejects the removed flat event shape.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventEnvelope {
    dedupe_key: InboxDedupeKey,
    source: SourceKind,
    project_id: String,
    repo: String,
    payload: EventPayload,
    received_at_epoch: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventEnvelopeWire {
    dedupe_key: InboxDedupeKey,
    source: SourceKind,
    project_id: String,
    repo: String,
    payload: EventPayload,
    received_at_epoch: u64,
}

impl<'de> Deserialize<'de> for EventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = EventEnvelopeWire::deserialize(deserializer)?;
        match wire.payload {
            EventPayload::Observation {
                event_type,
                subject,
            } => Self::observation(
                wire.dedupe_key,
                wire.source,
                wire.project_id,
                wire.repo,
                event_type,
                subject,
                wire.received_at_epoch,
            ),
            EventPayload::ReviewRequest {
                pr_number,
                skill_name,
                extra_args,
                skill_path,
                command_template,
                request_id,
                origin,
                notify_on_completion,
            } => Self::review_request(
                wire.dedupe_key,
                wire.source,
                wire.project_id,
                wire.repo,
                pr_number,
                skill_name,
                extra_args,
                skill_path,
                command_template,
                request_id,
                origin,
                notify_on_completion,
                wire.received_at_epoch,
            ),
        }
        .map_err(serde::de::Error::custom)
    }
}

impl EventEnvelope {
    pub fn observation(
        dedupe_key: InboxDedupeKey,
        source: SourceKind,
        project_id: impl Into<String>,
        repo: impl Into<String>,
        event_type: EventType,
        subject: EventSubject,
        received_at_epoch: u64,
    ) -> Result<Self, String> {
        Self::new(
            dedupe_key,
            source,
            project_id,
            repo,
            EventPayload::Observation {
                event_type,
                subject,
            },
            received_at_epoch,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn review_request(
        dedupe_key: InboxDedupeKey,
        source: SourceKind,
        project_id: impl Into<String>,
        repo: impl Into<String>,
        pr_number: u64,
        skill_name: impl Into<String>,
        extra_args: impl Into<String>,
        skill_path: impl Into<String>,
        command_template: impl Into<String>,
        request_id: ExternalRequestId,
        origin: ExternalTriggerOrigin,
        notify_on_completion: bool,
        received_at_epoch: u64,
    ) -> Result<Self, String> {
        if pr_number == 0 {
            return Err("review request PR number must be positive".to_string());
        }
        Self::new(
            dedupe_key,
            source,
            project_id,
            repo,
            EventPayload::ReviewRequest {
                pr_number,
                skill_name: skill_name.into(),
                extra_args: extra_args.into(),
                skill_path: skill_path.into(),
                command_template: command_template.into(),
                request_id,
                origin,
                notify_on_completion,
            },
            received_at_epoch,
        )
    }

    fn new(
        dedupe_key: InboxDedupeKey,
        source: SourceKind,
        project_id: impl Into<String>,
        repo: impl Into<String>,
        payload: EventPayload,
        received_at_epoch: u64,
    ) -> Result<Self, String> {
        let project_id = project_id.into();
        if project_id.trim().is_empty() {
            return Err("event project id must not be empty".to_string());
        }
        Ok(Self {
            dedupe_key,
            source,
            project_id,
            repo: repo.into(),
            payload,
            received_at_epoch,
        })
    }

    pub fn dedupe_key(&self) -> &InboxDedupeKey {
        &self.dedupe_key
    }
    pub fn source(&self) -> SourceKind {
        self.source
    }
    pub fn project_id(&self) -> &str {
        &self.project_id
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }
    pub fn payload(&self) -> &EventPayload {
        &self.payload
    }
    pub fn received_at_epoch(&self) -> u64 {
        self.received_at_epoch
    }

    pub fn as_observation(&self) -> Option<ObservationRef<'_>> {
        match &self.payload {
            EventPayload::Observation {
                event_type,
                subject,
            } => Some(ObservationRef {
                event_type: *event_type,
                subject,
            }),
            EventPayload::ReviewRequest { .. } => None,
        }
    }

    pub fn as_review_request(&self) -> Option<ReviewRequestRef<'_>> {
        match &self.payload {
            EventPayload::ReviewRequest {
                pr_number,
                skill_name,
                extra_args,
                skill_path,
                command_template,
                request_id,
                origin,
                notify_on_completion,
            } => Some(ReviewRequestRef {
                pr_number: *pr_number,
                skill_name,
                extra_args,
                skill_path,
                command_template,
                request_id,
                origin: *origin,
                notify_on_completion: *notify_on_completion,
            }),
            EventPayload::Observation { .. } => None,
        }
    }
}

/// The processing state of one persisted inbox delivery (AB#1065, epic AB#1078): the
/// inbox's per-entry status, surfaced to the frontend's event-inbox panel.
///
/// **Hard carrier** (sealed enum): the inbox store branches on an exhaustive
/// `match InboxStatus { ... }` ([`crate::inbox::store::status_as_wire`]), so adding a
/// variant without an arm is a compile error — the missing case cannot be expressed.
/// Already load-bearing: the store's `as_wire` is the DB column source and the service's
/// `mark_processed` / `mark_failed` transitions read it back.
///
/// Wire strings are pinned camelCase (`"received" | "processed" | "failed"`) — a
/// cross-agent contract the frontend's `INBOX_STATUSES` (`src/types.ts`) mirrors; the
/// serde golden below (`inbox_status_serializes_to_pinned_wire_strings`) is the
/// **Medium** carrier locking them against a `rename_all` / variant drift. `Default` is
/// [`Received`](Self::Received) — the state every delivery starts in before processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum InboxStatus {
    /// The delivery was persisted (deduped) but not yet processed.
    #[default]
    Received,
    /// The delivery was re-fed through the dispatch path successfully.
    Processed,
    /// Processing raised an error (carried in [`InboxEntry::error`]); replayable.
    Failed,
}

/// One persisted inbox delivery row: a normalized [`EventEnvelope`] plus
/// its processing state, surfaced to the frontend's event-inbox panel and the source of a
/// replay.
///
/// `Serialize`: a front/back contract mirrored in `src/types.ts` (`InboxEntry`); a field
/// change must be synced there in lockstep (the open end of this funnel — future Hard path
/// = codegen `types.ts` from `model.rs` + `git diff --exit-code`). The serde golden below
/// (`inbox_entry_wire_shape_is_camel_case`) is the **Medium** carrier locking the camelCase
/// wire shape.
///
/// The [`EventEnvelope`] is NESTED (a real `event` object), NOT flattened — the panel renders the
/// envelope as a unit and the wire stays `{ id, event: { … }, status, processedAtEpoch,
/// error }`, distinct from [`TrackedPrView`]'s flatten.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEntry {
    /// The inbox row id (the `inbox_event` table PRIMARY KEY) — the replay / get-raw key.
    pub id: i64,
    /// The normalized delivery envelope (nested, not flattened).
    pub event: EventEnvelope,
    /// The processing state of this delivery.
    pub status: InboxStatus,
    /// When processing finished (epoch seconds), or `None` while still `Received`.
    /// Serializes to JSON `null` (not omitted) so the TS mirror's `processedAtEpoch:
    /// number | null` stays a closed contract.
    pub processed_at_epoch: Option<u64>,
    /// The failure message when `status` is `Failed`, else `None` (→ JSON `null`).
    pub error: Option<String>,
}

/// User-visible body text that is safe to send to a notification center or external
/// notification channel.
///
/// **Hard carrier** for the deeplink redaction rule: callers cannot place a raw
/// `String` into [`Notification::body`]. They must choose one of the typed constructors
/// below, making the safety decision explicit at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RedactedNotificationBody(String);

impl RedactedNotificationBody {
    /// Fixed text authored by prmonitor, never derived from an external error message.
    pub fn fixed(text: &'static str) -> Self {
        Self(text.to_string())
    }

    /// Action URL already intended to be visible to the user.
    pub fn action_url(url: String) -> Self {
        Self(url)
    }

    /// Text deliberately authored by a user/API caller for a notification body.
    ///
    /// This constructor keeps the Hard redaction carrier closed: notification send funnels must
    /// still make an explicit typed choice, rather than passing arbitrary error strings into the
    /// persisted/exposed notification body by accident.
    pub fn user_supplied(text: String) -> Self {
        Self(text)
    }

    /// Fixed deeplink failure text whose only dynamic component is the validated PR number.
    pub fn review_trigger_rejected(pr_number: u64) -> Self {
        Self(format!("PR #{pr_number}：项目无效或该 review 已在进行中"))
    }

    #[cfg(test)]
    pub(crate) fn test_only(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The normalized OUTBOUND payload (AB#1070): what the core hands a
/// [`crate::review::notify::NotificationProvider`], the output-side mirror of the
/// inbound [`EventEnvelope`]. **Backend-internal** cross-Rust-slice contract (like
/// [`Candidate`], NOT [`PullRequestView`]): consumed only by Rust providers — today
/// the review slice's desktop notifier; a future outbox (AB#1066) drives it — so per
/// the charter it is intentionally NOT mirrored in `src/types.ts` (the funnel has no
/// open TS end). The serde golden (`notification_wire_shape_is_camel_case`) is the
/// **Medium** carrier locking the camelCase wire shape, so a future channel that
/// (de)serializes it (an email/webhook outbox queue) sees a stable shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    /// Severity, for channels that render it (email subject prefix, log level, icon).
    pub level: NotificationLevel,
    /// Short headline.
    pub title: String,
    /// The actionable URL (the pr-review comment URL today), or `""` when absent.
    pub url: String,
    /// Body text that is safe for persisted/exposed notification sinks. The private
    /// inner field on [`RedactedNotificationBody`] prevents raw `AppError::message`
    /// strings from being placed here by accident.
    pub body: RedactedNotificationBody,
    /// Routing key for a future multi-project / multi-channel outbox (AB#1066); `""` today.
    pub project_id: String,
}

impl Notification {
    pub fn new(
        level: NotificationLevel,
        title: String,
        url: String,
        body: RedactedNotificationBody,
        project_id: String,
    ) -> Self {
        Self {
            level,
            title,
            url,
            body,
            project_id,
        }
    }
}

/// Severity of a [`Notification`] (AB#1070). Sealed enum; the serde golden
/// (`notification_enums_serialize_to_pinned_wire_strings`) is the **Medium** carrier
/// pinning the camelCase wire strings. Default `Info`.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum NotificationLevel {
    #[default]
    Info,
    Warning,
    Error,
}

impl std::str::FromStr for NotificationLevel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(value.to_string()))
            .map_err(|_| "level must be one of: info, warning, error".to_string())
    }
}

/// Which channel a [`Notification`] is delivered through (AB#1070).
///
/// **Hard carrier** (sealed enum): the outbound dispatch ([`crate::review::notify::deliver`]
/// today; a future outbox, AB#1066) branches on an exhaustive `match NotificationKind { ... }`,
/// so adding a variant without an arm is a compile error — the missing channel cannot be
/// expressed. Today ONE variant (`Desktop`), already load-bearing at the review-completion
/// call site (the only outbound today), mirroring how [`SourceKind`] / [`EngineKind`] dispatch.
///
/// AB#1459 adds external channels as concrete variants: `Email` → `"email"`, `Feishu` →
/// `"feishu"`, `Telegram` → `"telegram"`, `WeChatWork` → `"weChatWork"`. Wire string pinned
/// camelCase; the serde golden locks it (Medium carrier).
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum NotificationKind {
    #[default]
    Desktop,
    Email,
    Slack,
    Telegram,
    WeChatWork,
    Feishu,
    DingTalk,
}

/// Which bidirectional messaging provider owns a bot integration (#1559).
///
/// Separate from [`NotificationKind`]: notification channels are one-way egress adapters, while
/// messaging integrations own ingress verification, event parsing, conversation authorization, and
/// replies. The exhaustive provider/router matches in `messaging` and `remote` are the Hard carrier
/// that prevents accidentally treating a bot provider as a notification channel.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MessagingProviderKind {
    #[default]
    Feishu,
    WeChatWork,
    DingTalk,
}

impl MessagingProviderKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            MessagingProviderKind::Feishu => "feishu",
            MessagingProviderKind::WeChatWork => "weChatWork",
            MessagingProviderKind::DingTalk => "dingTalk",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "feishu" => Some(MessagingProviderKind::Feishu),
            "weChatWork" => Some(MessagingProviderKind::WeChatWork),
            "dingTalk" => Some(MessagingProviderKind::DingTalk),
            _ => None,
        }
    }
}

/// Provider capability descriptor surfaced through generated config/types (#1559).
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingProviderCapability {
    pub provider: MessagingProviderKind,
    pub supports_reply: bool,
    pub supports_send: bool,
    pub supports_information_card: bool,
    /// Provider runs a Stream / long-connection ingress (HTTP callback disabled).
    pub supports_long_connection: bool,
    pub requires_allowed_conversations: bool,
}

/// Secret-free integration selector for the messaging panel active-send form.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingIntegrationOption {
    pub id: String,
    pub name: String,
    pub kind: MessagingProviderKind,
    pub allowed_conversation_ids: Vec<String>,
}

/// Runtime state of one messaging long connection (Feishu / DingTalk Stream).
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MessagingConnectionState {
    Disabled,
    Connecting,
    Connected,
    Reconnecting,
    Error,
    #[default]
    Stopped,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingConnectionStatus {
    pub provider: MessagingProviderKind,
    pub integration_id: String,
    pub status: MessagingConnectionState,
    pub last_connected_at_epoch: Option<u64>,
    pub last_event_at_epoch: Option<u64>,
    pub last_error: Option<String>,
    pub reconnect_count: u64,
}

/// Normalized inbound messaging event (#1559).
///
/// Provider payloads (Feishu v1) are normalized to this shape before command parsing. It is stored
/// in `messaging_event.event_json` and mirrored to the audit UI, so the camelCase wire shape is
/// locked by tests and generated TS.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingEvent {
    pub provider: MessagingProviderKind,
    pub integration_id: String,
    pub event_id: String,
    pub conversation_id: String,
    pub thread_id: String,
    pub sender_id: String,
    pub text: String,
    pub mentioned_bot: bool,
    pub raw_payload: String,
    pub received_at_epoch: u64,
}

/// The current processing state of one persisted messaging delivery (#1559).
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MessagingEventStatus {
    #[default]
    Received,
    Processed,
    Failed,
}

impl MessagingEventStatus {
    pub fn as_wire(self) -> &'static str {
        match self {
            MessagingEventStatus::Received => "received",
            MessagingEventStatus::Processed => "processed",
            MessagingEventStatus::Failed => "failed",
        }
    }

    pub fn from_wire_lenient(value: &str) -> Self {
        match value {
            "received" => MessagingEventStatus::Received,
            "processed" => MessagingEventStatus::Processed,
            _ => MessagingEventStatus::Failed,
        }
    }
}

/// Reply side effect linked from one messaging audit row (#1559).
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingReplyAudit {
    pub outbox_id: i64,
    pub kind: String,
    pub summary: String,
    pub status: Option<ActionStatus>,
    pub error: Option<String>,
}

/// Frontend-visible audit row for one messaging delivery (#1559).
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingEventEntry {
    pub id: i64,
    pub event: MessagingEvent,
    pub status: MessagingEventStatus,
    pub processed_at_epoch: Option<u64>,
    pub error: Option<String>,
    pub reply: Option<MessagingReplyAudit>,
}

/// Where a provider reply should be sent (#1559).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingReplyTarget {
    pub conversation_id: String,
    pub message_id: String,
    pub thread_id: String,
}

/// Secret-free outbox payload for a messaging reply (#1559).
///
/// It carries only an integration id and normalized target. Provider credentials are live-loaded
/// from config at execution time, mirroring `NotificationDeliveryPayload`'s secret-exclusion
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingReplyPayload {
    pub integration_id: String,
    pub provider: MessagingProviderKind,
    pub target: MessagingReplyTarget,
    pub text: String,
}

/// Secret-free outbox payload for an operator-authored active messaging send.
///
/// Distinct from [`MessagingReplyPayload`]: replies acknowledge an inbound event and carry a
/// provider reply target, while active sends address a configured conversation directly. Keeping
/// this as a separate payload is the Hard channel-separation carrier with
/// [`ActionKind::MessagingSend`].
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagingSendPayload {
    pub integration_id: String,
    pub provider: MessagingProviderKind,
    pub conversation_id: String,
    pub content: MessagingSendContent,
}

/// Header color supported by non-interactive Feishu information cards.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessagingCardTemplate {
    Blue,
    Orange,
    Grey,
}

impl MessagingCardTemplate {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Orange => "orange",
            Self::Grey => "grey",
        }
    }
}

impl std::str::FromStr for MessagingCardTemplate {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "blue" => Ok(Self::Blue),
            "orange" => Ok(Self::Orange),
            "grey" => Ok(Self::Grey),
            _ => Err(format!(
                "无效卡片模板 {value:?}；可选值：blue、orange、grey"
            )),
        }
    }
}

/// Typed active-send body. The sealed enum makes a text/card field mixture unrepresentable.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessagingSendContent {
    Text {
        text: String,
    },
    Card {
        title: String,
        text: String,
        template: MessagingCardTemplate,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum StrictMessagingSendContentWire {
    Text {
        text: String,
    },
    Card {
        title: String,
        text: String,
        template: MessagingCardTemplate,
    },
}

impl<'de> Deserialize<'de> for MessagingSendContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match StrictMessagingSendContentWire::deserialize(deserializer)? {
                StrictMessagingSendContentWire::Text { text } => Self::Text { text },
                StrictMessagingSendContentWire::Card {
                    title,
                    text,
                    template,
                } => Self::Card {
                    title,
                    text,
                    template,
                },
            },
        )
    }
}

/// Sealed Current wire (`content`). Prefer this for all new writers.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CurrentMessagingSendPayloadWire {
    integration_id: String,
    provider: MessagingProviderKind,
    conversation_id: String,
    content: MessagingSendContent,
}

/// Legacy flat `text` read path for older outbox / callers.
///
/// Sunset: migrate remaining readers to `content`, then delete this struct (no new Legacy writers;
/// target window = next messaging epic after #531).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyMessagingSendPayloadWire {
    integration_id: String,
    provider: MessagingProviderKind,
    conversation_id: String,
    text: String,
}

impl<'de> Deserialize<'de> for MessagingSendPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        // Route by sealed `content` vs legacy `text` so Current deny_unknown stays Hard without
        // treating Legacy's `text` as an unknown Current field.
        if value.get("content").is_some() {
            let wire = CurrentMessagingSendPayloadWire::deserialize(&value)
                .map_err(serde::de::Error::custom)?;
            return Ok(Self {
                integration_id: wire.integration_id,
                provider: wire.provider,
                conversation_id: wire.conversation_id,
                content: wire.content,
            });
        }
        if value.get("text").is_some() {
            let wire = LegacyMessagingSendPayloadWire::deserialize(&value)
                .map_err(serde::de::Error::custom)?;
            return Ok(Self {
                integration_id: wire.integration_id,
                provider: wire.provider,
                conversation_id: wire.conversation_id,
                content: MessagingSendContent::Text { text: wire.text },
            });
        }
        Err(CurrentMessagingSendPayloadWire::deserialize(&value)
            .err()
            .map(serde::de::Error::custom)
            .expect("missing content and text cannot deserialize as Current wire"))
    }
}

impl std::fmt::Debug for MessagingSendContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { .. } => f.debug_struct("Text").field("text", &"[REDACTED]").finish(),
            Self::Card { template, .. } => f
                .debug_struct("Card")
                .field("title", &"[REDACTED]")
                .field("text", &"[REDACTED]")
                .field("template", template)
                .finish(),
        }
    }
}

impl std::fmt::Debug for MessagingSendPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessagingSendPayload")
            .field("integration_id", &self.integration_id)
            .field("provider", &self.provider)
            .field("conversation_id", &self.conversation_id)
            .field("content", &self.content)
            .finish()
    }
}

/// Transport-agnostic request to enqueue one active messaging send.
///
/// `deny_unknown_fields` keeps provider credentials/webhook URLs from being silently accepted into
/// the request funnel. Credentials are always live-loaded from the referenced integration at
/// execution time.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessagingRequest {
    pub integration_id: String,
    pub conversation_id: String,
    pub content: MessagingSendContent,
    pub request_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CurrentSendMessagingRequestWire {
    integration_id: String,
    conversation_id: String,
    content: MessagingSendContent,
    request_id: String,
}

/// Legacy flat `text` request wire. Sunset with [`LegacyMessagingSendPayloadWire`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySendMessagingRequestWire {
    integration_id: String,
    conversation_id: String,
    text: String,
    request_id: String,
}

impl<'de> Deserialize<'de> for SendMessagingRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        // Route by sealed `content` vs legacy `text` so Current deny_unknown stays Hard without
        // treating Legacy's `text` as an unknown Current field.
        if value.get("content").is_some() {
            let wire = CurrentSendMessagingRequestWire::deserialize(&value)
                .map_err(serde::de::Error::custom)?;
            return Ok(Self {
                integration_id: wire.integration_id,
                conversation_id: wire.conversation_id,
                content: wire.content,
                request_id: wire.request_id,
            });
        }
        if value.get("text").is_some() {
            let wire = LegacySendMessagingRequestWire::deserialize(&value)
                .map_err(serde::de::Error::custom)?;
            return Ok(Self {
                integration_id: wire.integration_id,
                conversation_id: wire.conversation_id,
                content: MessagingSendContent::Text { text: wire.text },
                request_id: wire.request_id,
            });
        }
        Err(CurrentSendMessagingRequestWire::deserialize(&value)
            .err()
            .map(serde::de::Error::custom)
            .expect("missing content and text cannot deserialize as Current wire"))
    }
}

impl std::fmt::Debug for SendMessagingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessagingRequest")
            .field("integration_id", &self.integration_id)
            .field("conversation_id", &self.conversation_id)
            .field("content", &self.content)
            .field("request_id", &self.request_id)
            .finish()
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessagingResponse {
    pub outbox_id: i64,
}

/// Persisted payload for one notification delivery row (AB#1459).
///
/// **Hard carrier for secret exclusion:** this is the ONLY shape an outbox
/// `ActionKind::Notification` row executes. It carries the normalized notification plus a channel
/// reference (`channel_id` + `kind`), but it has no field capable of holding a webhook URL, bot
/// token, SMTP password, authorization header, or provider response. Adapters live-load channel
/// config by id at execution time, so the panel-visible raw payload cannot contain channel secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationDeliveryPayload {
    pub notification: Notification,
    pub channel_id: String,
    pub kind: NotificationKind,
}

/// Transport-agnostic user-authored notification intent (#1460).
///
/// **Hard carrier for secret exclusion:** this request has no provider-config fields and
/// `deny_unknown_fields` rejects secret-looking extras (`webhookUrl`, `token`, `smtpPassword`,
/// `authorization`, …) at the serde boundary instead of silently accepting a shape that could be
/// accidentally persisted later. The funnel turns this into [`NotificationDeliveryPayload`] rows.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendNotificationRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<NotificationLevel>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channel_ids: Vec<String>,
}

/// Result of enqueueing one user-authored notification (#1460).
///
/// Success means durable enqueue succeeded. Provider delivery happens later through the outbox
/// worker and is observable through the existing outbox status surface.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendNotificationResponse {
    pub outbox_ids: Vec<i64>,
}

/// Durable workflow type (#1370). V1 has one concrete saga: review completion notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowType {
    #[default]
    ReviewNotify,
}

/// Durable workflow lifecycle (#1370). The current step carries the fine-grained progress; this
/// status is the coarse list/filter state shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowStatus {
    #[default]
    Pending,
    Running,
    Waiting,
    Done,
    Failed,
}

/// Current step of a workflow instance (#1370).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowStep {
    #[default]
    StartReview,
    WaitReview,
    EnqueueNotify,
    Done,
}

/// One persisted workflow instance (#1370). `input` and `state` are intentionally JSON values:
/// workflow-specific payloads remain non-secret and inspectable without adding a second table in v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInstance {
    pub id: i64,
    pub project_id: String,
    #[serde(rename = "type")]
    pub workflow_type: WorkflowType,
    pub status: WorkflowStatus,
    pub current_step: WorkflowStep,
    pub input: serde_json::Value,
    pub state: serde_json::Value,
    pub attempt_count: u32,
    pub next_wake_at: u64,
    pub last_error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Runtime delivery config for one notification channel (AB#1459).
///
/// Horizontal DTO: config owns persisted settings, review owns delivery adapters, and `lib.rs`
/// composes them. Adapters consume this model-level shape so the `review` slice never imports the
/// `config` slice's persisted model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationDeliveryChannel {
    pub id: String,
    pub name: String,
    pub kind: NotificationKind,
    pub webhook_url: String,
    pub webhook_secret: String,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_from: String,
    pub smtp_to: String,
    pub timeout_secs: u64,
}

/// Classified result of one outbox action execution (AB#1459).
///
/// Horizontal because the composition root, outbox worker, and notification adapters all need the
/// same sealed result without any slice importing a sibling. Adapters map provider-specific
/// HTTP/SMTP failures into this type before the generic outbox worker records state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionExecutionResult {
    Done {
        output: ActionExecutionOutput,
    },
    Blocked {
        message: String,
        /// Resume generation observed immediately before the executor classified this action as
        /// blocked. Persisting that observation lets the worker distinguish an already-fired
        /// resume from a future resume without a check-then-sleep race.
        observed_resume_generation: u64,
    },
    Retry {
        message: String,
        retry_after_secs: Option<u64>,
    },
    Dead {
        message: String,
    },
}

impl ActionExecutionResult {
    pub fn done() -> Self {
        Self::Done {
            output: ActionExecutionOutput::None,
        }
    }

    pub fn review(thread_id: impl Into<String>) -> Self {
        Self::Done {
            output: ActionExecutionOutput::Review {
                thread_id: thread_id.into(),
            },
        }
    }
}

/// The kind of side effect a persisted outbox row executes (AB#1066/AB#1069, epic AB#1078).
///
/// **Hard carrier** (sealed enum): the outbox's executor router branches on an exhaustive
/// `match ActionKind { ... }` (installed by the composition root in `lib.rs`, the only place that
/// names `review::notify` / `review::commands`), so adding a variant without an arm is a compile
/// error — the missing action cannot be expressed. The store's wire mapping
/// ([`crate::outbox::store::kind_as_wire`] / `kind_from_wire`) round-trips through serde, so a new
/// variant is carried automatically (no exhaustive store edit). Variants: `Notification` (the
/// review-completion desktop notification, AB#1066) + `Review` / `Check` / `StopReview` (the
/// AB#1069 action executor, each reusing the existing review funnel) + `MessagingReply` (#1559
/// bot reply executor).
///
/// **Email / IM notifications are NOT ActionKinds — they are [`NotificationKind`] channels** under
/// the single `Notification` action: "send via email/Feishu/…" is one notification delivered over a
/// different channel, sharing the unified status/retry/dead-letter lifecycle, not a distinct action
/// class. `MessagingReply` is separate because it acknowledges an inbound bot event with its own
/// dedupe key and provider reply target.
/// AB#1069/1070 design reservation for genuinely-distinct future kinds: `WebhookForward`,
/// `WorkItemComment` — each forcing a new executor arm. Wire string pinned camelCase; the serde
/// golden locks it (Medium carrier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ActionKind {
    /// The review-completion desktop notification (AB#1066) → `"notification"`.
    #[default]
    Notification,
    /// Run a configured skill for a PR via the review funnel → `"runSkill"`.
    RunSkill,
    /// Interrupt an in-flight skill session for a `(project, pr, skill_key)` (AB#1069) →
    /// `"stopReview"`. Idempotent: no live session is a benign no-op success, not a failure.
    StopReview,
    /// Reply to an inbound messaging event (#1559) → `"messagingReply"`.
    MessagingReply,
    /// Send an operator-authored message to a configured messaging conversation → `"messagingSend"`.
    MessagingSend,
    // future genuinely-distinct kinds (AB#1069/1070): WebhookForward, WorkItemComment.
    // Email/IM notification channels are NOT kinds here — see the doc comment.
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::RunSkill => "runSkill",
            Self::StopReview => "stopReview",
            Self::MessagingReply => "messagingReply",
            Self::MessagingSend => "messagingSend",
        }
    }
}

impl OutboxProducerKey {
    pub fn for_manual(
        project_id: &str,
        kind: ActionKind,
        nonce: &ExternalRequestId,
    ) -> Result<Self, String> {
        if project_id.trim().is_empty() {
            return Err("manual producer key requires a project id".to_string());
        }
        Self::new(format!(
            "manual:{}:{}:{}:{nonce}",
            project_id.len(),
            project_id,
            kind.as_str()
        ))
    }
}

/// The outbox payload for a [`ActionKind::RunSkill`] action: the PR the executor reviews via the
/// review funnel (`review::commands::start_for_outbox`), plus the resolved [`SkillInvocation`].
///
/// **Routing key is NOT here (AB#1069 F3).** The owning project is the OUTBOX ROW's single-source
/// routing key ([`crate::outbox::OutboxAction::project_id`] / [`OutboxEntry::project_id`] — what the
/// panel + `outbox:updated` events route by); the executor reads `action.project_id`, never a payload
/// copy. Carrying `project_id` here too would be a dual source of truth: a drifted/forged payload
/// could route a row shown under project A to project B's review.
///
/// Backend-internal (read only by the `lib.rs` executor; the Rule Engine produces it),
/// so NOT mirrored in `src/types.ts`, like [`Notification`] / [`Candidate`]. It IS persisted in the
/// outbox `payload` column and replayed, so its camelCase shape must stay stable. serde camelCase;
/// the golden locks it (Medium carrier).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ReviewActionPayload {
    Automatic {
        candidate: Candidate,
        invocation: SkillInvocation,
    },
    Explicit {
        #[serde(rename = "prNumber")]
        pr_number: u64,
        #[serde(rename = "requestId")]
        request_id: ExternalRequestId,
        origin: ExternalTriggerOrigin,
        invocation: SkillInvocation,
    },
}

impl ReviewActionPayload {
    pub fn automatic_candidate(&self) -> Option<&Candidate> {
        match self {
            Self::Automatic { candidate, .. } => Some(candidate),
            Self::Explicit { .. } => None,
        }
    }

    pub fn invocation(&self) -> &SkillInvocation {
        match self {
            Self::Automatic { invocation, .. } | Self::Explicit { invocation, .. } => invocation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ActionExecutionOutput {
    #[default]
    None,
    Review {
        #[serde(rename = "threadId")]
        thread_id: String,
    },
}

/// The outbox payload for a [`ActionKind::StopReview`] action (AB#1069): which in-flight session to
/// interrupt, keyed (together with the row's `project_id`) by `(project, pr, kind)` — the review
/// funnel's native session key, not an engine-assigned thread id, so it is restart-stable and
/// producible by a future Rule Engine (AB#1068). The executor resolves it to a live `thread_id` via
/// `SessionRegistry::stop_target`; no live session is a benign no-op (idempotent).
///
/// **Routing key is NOT here (AB#1069 F3):** the owning project is the OUTBOX ROW's single-source
/// `project_id` (the executor reads `action.project_id`), never a payload copy — same anti-dual-source
/// rationale as [`ReviewActionPayload`]. Same backend-internal, persisted-and-replayed status (not
/// mirrored in `src/types.ts`). serde camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReviewActionPayload {
    /// The PR / MR number whose session to stop (re-validated `> 0` at the executor boundary).
    pub pr_number: u64,
    /// Which skill session to stop (`SkillInvocation::skill_key`).
    pub skill_key: String,
}

/// The lifecycle state of one persisted outbox action (AB#1066, epic AB#1078): surfaced to the
/// frontend's action-outbox panel and the worker's terminal state.
///
/// **Hard carrier** (sealed enum): the outbox store branches on an exhaustive
/// `match ActionStatus { ... }` ([`crate::outbox::store::status_as_wire`]), so adding a variant
/// without an arm is a compile error — the missing case cannot be expressed.
///
/// Four states (no transient `Processing`): a queued action is [`Pending`](Self::Pending)
/// (`Default` — every action starts here) until the worker executes it; on success it is
/// [`Done`](Self::Done); a failure that exhausts the retry budget is [`Dead`](Self::Dead) — the
/// terminal dead-letter. A transient failure stays `Pending` (the row's `attempt_count` /
/// `last_error` carry the detail and `next_attempt_at` reschedules it), so a crash mid-execute
/// re-runs the action next boot (at-least-once). Wire strings are pinned camelCase
/// (`"pending" | "blocked" | "done" | "dead"`) — generated TypeScript mirrors
/// them; the serde golden below is the **Medium** carrier locking them against a `rename_all` /
/// variant drift.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ActionStatus {
    /// Queued (or awaiting a retry); the worker will execute it when `next_attempt_at` is due.
    #[default]
    Pending,
    /// Waiting for an explicit Codex resume. Blocking does not consume a retry attempt.
    Blocked,
    /// Executed successfully — terminal.
    Done,
    /// The retry budget was exhausted — terminal dead-letter (`last_error` carries the reason).
    Dead,
}

/// One persisted outbox action row (AB#1066, epic AB#1078): the side effect's kind + lifecycle,
/// surfaced to the frontend's action-outbox panel. The raw `payload` is NOT carried here (it is
/// backend-internal — a `Notification` JSON today); the panel fetches it on demand via
/// `outbox_get_raw`, mirroring the inbox's `inbox_get_raw`.
///
/// Front/back contract mirrored in `src/types.ts` (`OutboxEntry`) and reused by the local CLI
/// client for `/messaging/sends`; a field change must be synced there in lockstep. The serde golden
/// below (`outbox_entry_wire_shape_is_camel_case`) is the **Medium** carrier locking the camelCase
/// shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxEntry {
    /// The outbox row id (the `action_outbox` table PRIMARY KEY) — the get-raw / retry key.
    pub id: i64,
    /// Routing key (#35): which project this action belongs to.
    pub project_id: String,
    /// The side effect's kind.
    pub kind: ActionKind,
    /// A short human-readable label the panel renders without deserializing the payload.
    pub summary: String,
    /// The action's lifecycle state.
    pub status: ActionStatus,
    /// How many execution attempts have run (0 until the worker first tries it).
    pub attempt_count: u32,
    /// When the action is next eligible to run (epoch seconds); a retry pushes it forward.
    pub next_attempt_at: u64,
    /// The most recent failure message, or `None` (→ JSON `null`) if it has never failed.
    pub last_error: Option<String>,
    /// When the action was enqueued (epoch seconds).
    pub created_at: u64,
    /// When the row last transitioned (epoch seconds).
    pub updated_at: u64,
}

/// Serde wire-shape locks for `model.rs`'s cross-slice types.
///
/// The **Medium carrier** for these serde shapes per
/// `.claude/rules/prmonitor/ai-robust.md`. Each is a LOCK (characterization)
/// test: it passes on current code and only fails if a field is renamed or the
/// camelCase serialization breaks. The two types differ in their *downstream*,
/// so their contracts are not the same thing:
///
/// - [`PullRequestView`] is a **front/back contract** mirrored in
///   `src/types.ts`; a key change must be synced there in lockstep — the open
///   end of that funnel (no machine check on the TS side yet; future Hard path =
///   codegen `types.ts` from `model.rs` + `git diff --exit-code`).
/// - [`Candidate`] is **backend-internal**, cross-Rust-slice only: per the
///   charter it is intentionally *not* mirrored in `src/types.ts`, so its lock
///   guards the camelCase wire shape the `pr`/`review` slices rely on, **not** a
///   front/back contract — do not sync it to the frontend.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_wire_values_are_closed_and_pinned() {
        let codex = [
            (CodexReasoningEffort::Default, "default"),
            (CodexReasoningEffort::None, "none"),
            (CodexReasoningEffort::Minimal, "minimal"),
            (CodexReasoningEffort::Low, "low"),
            (CodexReasoningEffort::Medium, "medium"),
            (CodexReasoningEffort::High, "high"),
            (CodexReasoningEffort::Xhigh, "xhigh"),
            (CodexReasoningEffort::Max, "max"),
            (CodexReasoningEffort::Ultra, "ultra"),
        ];
        for (value, wire) in codex {
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        assert_eq!(
            CodexReasoningEffort::default(),
            CodexReasoningEffort::Default
        );

        let claude = [
            (ClaudeEffort::Default, "default"),
            (ClaudeEffort::Low, "low"),
            (ClaudeEffort::Medium, "medium"),
            (ClaudeEffort::High, "high"),
            (ClaudeEffort::Xhigh, "xhigh"),
            (ClaudeEffort::Max, "max"),
        ];
        for (value, wire) in claude {
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        assert_eq!(ClaudeEffort::default(), ClaudeEffort::Default);
    }

    #[test]
    fn typed_event_envelope_and_review_request_wire_contract() {
        let request_id = ExternalRequestId::parse("0123456789abcdef0123456789abcdef")
            .expect("32 lowercase hex request id");
        assert!(ExternalRequestId::parse("not-hex").is_err());
        assert!(ExternalRequestId::parse("0123456789ABCDEF0123456789ABCDEF").is_err());

        let event = EventEnvelope::review_request(
            InboxDedupeKey::new("http:0123456789abcdef0123456789abcdef").expect("non-empty key"),
            SourceKind::Github,
            "p1",
            "octocat/hello",
            7,
            crate::model::DEFAULT_SKILL_NAME,
            "--check",
            crate::model::DEFAULT_SKILL_PATH,
            crate::model::DEFAULT_COMMAND_TEMPLATE,
            request_id.clone(),
            ExternalTriggerOrigin::Http,
            true,
            42,
        )
        .expect("valid event");
        let wire = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(wire["dedupeKey"], "http:0123456789abcdef0123456789abcdef");
        assert_eq!(wire["payload"]["kind"], "reviewRequest");
        assert_eq!(wire["payload"]["skillName"], "pr-review");
        assert_eq!(wire["payload"]["extraArgs"], "--check");
        assert_eq!(
            wire["payload"]["skillPath"],
            crate::model::DEFAULT_SKILL_PATH
        );
        assert_eq!(
            wire["payload"]["commandTemplate"],
            crate::model::DEFAULT_COMMAND_TEMPLATE
        );
        assert_eq!(wire["payload"]["requestId"], request_id.as_str());
        assert_eq!(wire["payload"]["origin"], "http");
        assert_eq!(wire["payload"]["notifyOnCompletion"], true);
        assert!(
            wire.get("eventType").is_none(),
            "event type belongs in Observation"
        );
        assert_eq!(event.as_review_request().expect("request").pr_number, 7);
    }

    #[test]
    fn review_receipt_wire_contract_is_exhaustive_and_camel_case() {
        let statuses = [
            (ReviewReceiptStatus::Received, "received"),
            (ReviewReceiptStatus::Queued, "queued"),
            (ReviewReceiptStatus::Blocked, "blocked"),
            (ReviewReceiptStatus::Starting, "starting"),
            (ReviewReceiptStatus::Running, "running"),
            (ReviewReceiptStatus::Interrupting, "interrupting"),
            (ReviewReceiptStatus::Done, "done"),
            (ReviewReceiptStatus::Failed, "failed"),
        ];
        for (status, expected) in statuses {
            assert_eq!(serde_json::to_value(status).unwrap(), expected);
        }

        let snapshot = ReviewReceiptSnapshot {
            receipt_id: ReviewReceiptId::new(9).unwrap(),
            status: ReviewReceiptStatus::Running,
            thread_id: Some("thread-9".into()),
            comment_url: None,
            outcome: None,
            error: None,
        };
        let wire = serde_json::to_value(snapshot).unwrap();
        assert_eq!(wire["receiptId"], 9);
        assert_eq!(wire["status"], "running");
        assert_eq!(wire["threadId"], "thread-9");
        assert_eq!(wire["commentUrl"], serde_json::Value::Null);
    }

    #[test]
    fn typed_action_and_producer_keys_validate_and_frame_components() {
        assert_eq!(
            ReviewActionKey::for_parts(
                7,
                "abc123",
                SkillInvocation::skill_key("pr-review", "--check")
            )
            .unwrap()
            .as_str(),
            &format!(
                "7@abc123:{}",
                SkillInvocation::skill_key("pr-review", "--check")
            )
        );
        assert!(ReviewActionKey::for_parts(
            0,
            "abc123",
            SkillInvocation::skill_key("pr-review", "")
        )
        .is_err());
        assert!(
            ReviewActionKey::for_parts(7, "", SkillInvocation::skill_key("pr-review", "")).is_err()
        );

        let inbox = InboxEventId::new(9).unwrap();
        let left = OutboxProducerKey::for_rule_action(inbox, "a:b", "c").unwrap();
        let right = OutboxProducerKey::for_rule_action(inbox, "a", "b:c").unwrap();
        assert_ne!(left, right, "length framing prevents component collisions");
        assert!(OutboxProducerKey::for_dedupe("", "key").is_err());
        let nonce = ExternalRequestId::parse("00112233445566778899aabbccddeeff").unwrap();
        assert!(OutboxProducerKey::for_manual("", ActionKind::RunSkill, &nonce).is_err());
    }

    #[test]
    fn blocked_execution_carries_observed_resume_generation() {
        let result = ActionExecutionResult::Blocked {
            message: "codex stopped".to_string(),
            observed_resume_generation: 41,
        };
        match result {
            ActionExecutionResult::Blocked {
                message,
                observed_resume_generation,
            } => {
                assert_eq!(message, "codex stopped");
                assert_eq!(observed_resume_generation, 41);
            }
            other => panic!("expected blocked result, got {other:?}"),
        }
    }

    #[test]
    fn typed_observation_and_action_contracts_round_trip() {
        let event = EventEnvelope::observation(
            InboxDedupeKey::new("github:delivery-1").expect("key"),
            SourceKind::Github,
            "p1",
            "octocat/hello",
            EventType::PullRequest,
            EventSubject {
                number: Some(7),
                title: "Ready".into(),
                body: "Body".into(),
                labels: vec!["review".into()],
                url: "https://example.test/7".into(),
            },
            42,
        )
        .expect("valid event");
        let wire = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(wire["payload"]["kind"], "observation");
        assert_eq!(wire["payload"]["eventType"], "pullRequest");
        assert_eq!(wire["payload"]["subject"]["number"], 7);
        assert_eq!(
            event.as_observation().expect("observation").subject.title,
            "Ready"
        );

        let automatic = ReviewActionPayload::Automatic {
            candidate: Candidate {
                number: 7,
                head_sha: "sha".into(),
                head_ref: "main".into(),
                author: "octocat".into(),
                is_cross_repository: false,
                is_draft: false,
                skill_key: SkillInvocation::skill_key("pr-review", ""),
            },
            invocation: SkillInvocation::build(
                "pr-review",
                Some("/tmp/skill.md".into()),
                "/pr-review 7",
                "",
            ),
        };
        assert_eq!(
            serde_json::to_value(&automatic).unwrap()["kind"],
            "automatic"
        );

        let explicit = ReviewActionPayload::Explicit {
            pr_number: 7,
            request_id: ExternalRequestId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            origin: ExternalTriggerOrigin::RemoteWeb,
            invocation: SkillInvocation::build(
                "pr-review",
                Some("/tmp/skill.md".into()),
                "/pr-review 7",
                "",
            ),
        };
        let wire = serde_json::to_value(&explicit).unwrap();
        assert_eq!(wire["kind"], "explicit");
        assert_eq!(wire["prNumber"], 7);
        assert_eq!(wire["origin"], "remoteWeb");

        assert_eq!(
            serde_json::to_value(ActionStatus::Blocked).unwrap(),
            "blocked"
        );
        assert_eq!(
            serde_json::to_value(ActionExecutionOutput::Review {
                thread_id: "thread-1".into(),
            })
            .unwrap(),
            serde_json::json!({"kind":"review","threadId":"thread-1"})
        );
        assert_eq!(
            serde_json::to_value(ActionExecutionOutput::None).unwrap(),
            serde_json::json!({"kind":"none"})
        );
    }

    // Backend-internal cross-slice lock: `Candidate` is not exposed to the
    // frontend and is intentionally absent from `src/types.ts` (per the charter).
    #[test]
    fn candidate_wire_shape_is_camel_case() {
        let candidate = Candidate {
            number: 1,
            head_sha: "abc123".to_string(),
            head_ref: "feature/x".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            skill_key: SkillInvocation::skill_key("pr-review", ""),
        };

        let v = serde_json::to_value(&candidate).expect("Candidate serializes");

        // camelCase keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("headSha").is_some());
        assert!(v.get("headRef").is_some());
        assert!(v.get("author").is_some());
        assert!(v.get("isCrossRepository").is_some());
        assert!(v.get("isDraft").is_some());
        assert!(v.get("skillKey").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("head_sha").is_none());
        assert!(v.get("head_ref").is_none());
        assert!(v.get("is_cross_repository").is_none());
        assert!(v.get("is_draft").is_none());
    }

    // Serde wire-shape lock for the #1383 terminal listing contract (Medium carrier per
    // ai-robust.md): UNLIKE `Candidate`, `TerminalSession` IS a front/back contract (returned
    // by `list_terminal_sessions`), so `src/types.ts` mirrors it in lockstep — this golden is
    // the upstream lock; the TS interface is the open downstream end (future Hard path = codegen
    // `types.ts` from `model.rs` + `git diff --exit-code`). Locks camelCase keys present /
    // snake_case absent so a Rust-side rename surfaces here before it silently breaks the panel.
    #[test]
    fn terminal_session_wire_shape_is_camel_case() {
        let session = TerminalSession {
            session_id: "w0t0p0".to_string(),
            window_id: "w0".to_string(),
            tab_id: "t0".to_string(),
            title: "zsh".to_string(),
            is_active: true,
            rows: 24,
            cols: 80,
            backend: TerminalBackendKind::Iterm,
        };

        let v = serde_json::to_value(&session).expect("TerminalSession serializes");

        // camelCase keys present.
        assert!(v.get("sessionId").is_some());
        assert!(v.get("windowId").is_some());
        assert!(v.get("tabId").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("isActive").is_some());
        assert!(v.get("rows").is_some());
        assert!(v.get("cols").is_some());
        // #1372: the `backend` discriminator is ALWAYS present (no `skip`) and pins to "iterm"
        // for an iTerm row; the frontend reads it to label + route close.
        assert_eq!(v["backend"], "iterm");

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("session_id").is_none());
        assert!(v.get("window_id").is_none());
        assert!(v.get("tab_id").is_none());
        assert!(v.get("is_active").is_none());

        // A WebPty row pins to "webPty" (the second backend, #1372).
        let pty = serde_json::to_value(TerminalSession {
            session_id: "webpty-1".to_string(),
            window_id: "webpty".to_string(),
            tab_id: "webpty".to_string(),
            title: "sh".to_string(),
            is_active: true,
            rows: 24,
            cols: 80,
            backend: TerminalBackendKind::WebPty,
        })
        .expect("TerminalSession serializes");
        assert_eq!(pty["backend"], "webPty");
    }

    // #1372: a daemon row arrives with NO `backend` key; `#[serde(default)]` fills `Iterm`, so an
    // iTerm session re-serialized to the frontend carries `backend == "iterm"` without the daemon
    // ever sending it. This locks the deserialize-default half of the contract.
    #[test]
    fn terminal_session_defaults_backend_to_iterm_when_absent() {
        let daemon_row = serde_json::json!({
            "sessionId": "p0", "windowId": "w0", "tabId": "t0",
            "title": "zsh", "isActive": true, "rows": 24, "cols": 80
        });
        let parsed: TerminalSession =
            serde_json::from_value(daemon_row).expect("daemon row parses without `backend`");
        assert_eq!(parsed.backend, TerminalBackendKind::Iterm);
    }

    // `CreateSessionOpts`: camelCase + `skip_serializing_if` OMITS absent keys (an absent key,
    // not a JSON null), matching the optional `windowId?` / `profile?` TS mirror.
    #[test]
    fn create_session_opts_wire_shape_omits_none() {
        let full = CreateSessionOpts {
            window_id: Some("w0".to_string()),
            profile: Some("Default".to_string()),
            backend: Some(TerminalBackendKind::WebPty),
        };
        let v = serde_json::to_value(&full).expect("CreateSessionOpts serializes");
        assert_eq!(v["windowId"], "w0");
        assert_eq!(v["profile"], "Default");
        assert!(v.get("window_id").is_none());
        // #1372: `Some(WebPty)` pins to "webPty" (a create-time backend pick).
        assert_eq!(v["backend"], "webPty");

        // None → the key is OMITTED entirely (not a JSON null) — `backend: None` routes to Iterm.
        let empty = serde_json::to_value(CreateSessionOpts::default()).expect("serializes");
        assert!(empty.get("windowId").is_none(), "None omits windowId");
        assert!(empty.get("profile").is_none(), "None omits profile");
        assert!(empty.get("backend").is_none(), "None omits backend");
    }

    // Cross-agent wire contract lock for the #1372 backend discriminator: the frontend mirrors
    // these exact strings to pick a create-time backend + label/route sessions. A variant rename
    // or `rename_all` change surfaces here (Medium carrier; the exhaustive `match` in
    // `terminal::commands::RoutedBackend` is the Hard one). Default is `Iterm` ("iterm").
    #[test]
    fn terminal_backend_kind_wire_strings() {
        assert_eq!(
            serde_json::to_value(TerminalBackendKind::Iterm).expect("serializes"),
            "iterm"
        );
        assert_eq!(
            serde_json::to_value(TerminalBackendKind::WebPty).expect("serializes"),
            "webPty"
        );
        assert_eq!(
            serde_json::to_value(TerminalBackendKind::default()).expect("serializes"),
            "iterm"
        );
    }

    // Cross-agent wire contract lock: the frontend mirrors these exact strings.
    // A variant rename or `rename_all` change surfaces here.
    #[test]
    fn discriminator_enums_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(SourceKind::Github).expect("SourceKind serializes"),
            "github"
        );
        // #818: the Azure source variant pins to "azure" (the frontend mirrors it).
        assert_eq!(
            serde_json::to_value(SourceKind::Azure).expect("SourceKind serializes"),
            "azure"
        );
        // AB#717: the Bitbucket Server source variant pins to "bitbucket".
        assert_eq!(
            serde_json::to_value(SourceKind::Bitbucket).expect("SourceKind serializes"),
            "bitbucket"
        );
        assert_eq!(
            serde_json::to_value(EngineKind::Codex).expect("EngineKind serializes"),
            "codex"
        );
        // #718: the Claude review engine variant pins to "claude" (the frontend
        // mirrors it in `ENGINE_KINDS`); a variant rename or `rename_all` change
        // surfaces here (Medium carrier; the exhaustive `match` wiring is the Hard one).
        assert_eq!(
            serde_json::to_value(EngineKind::Claude).expect("EngineKind serializes"),
            "claude"
        );
        // Cursor ACP review engine pins to "cursor" (frontend `ENGINE_KINDS`);
        // Medium carrier against rename_all / variant drift.
        assert_eq!(
            serde_json::to_value(EngineKind::Cursor).expect("EngineKind serializes"),
            "cursor"
        );
        // AB#717: per-project label source. camelCase wire strings the frontend mirrors;
        // a variant rename or `rename_all` change surfaces here. Default is `Native`
        // (the status-quo: provider's own PR labels).
        assert_eq!(
            serde_json::to_value(LabelSource::Native).expect("LabelSource serializes"),
            "native"
        );
        assert_eq!(
            serde_json::to_value(LabelSource::Title).expect("LabelSource serializes"),
            "title"
        );
        assert_eq!(
            serde_json::to_value(LabelSource::default()).expect("LabelSource serializes"),
            "native"
        );
        // #818: per-project data-update modes. kebab-case wire strings the frontend
        // mirrors; a variant rename or `rename_all` change surfaces here. Default is
        // `WebhookOnly` (the safe boot default: NO automatic CLI polling).
        assert_eq!(
            serde_json::to_value(UpdateMode::WebhookOnly).expect("UpdateMode serializes"),
            "webhook-only"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::PullOnly).expect("UpdateMode serializes"),
            "pull-only"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::Hybrid).expect("UpdateMode serializes"),
            "hybrid"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::Manual).expect("UpdateMode serializes"),
            "manual"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::default()).expect("UpdateMode serializes"),
            "webhook-only"
        );
    }

    // Cross-agent wire contract lock for `WebhookTunnelMode` (#9): the frontend's TS
    // union mirrors these exact lowercase strings. A variant rename or a
    // `rename_all` change surfaces here. Default is `Quick` (status-quo behavior).
    #[test]
    fn webhook_tunnel_mode_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Quick).expect("serializes"),
            "quick"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Command).expect("serializes"),
            "command"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Listener).expect("serializes"),
            "listener"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::default()).expect("serializes"),
            "quick"
        );
    }

    // Front/back contract lock: `PullRequestView` is mirrored in `src/types.ts`;
    // a field change here must be synced to that interface in lockstep.
    #[test]
    fn pull_request_view_wire_shape_is_camel_case() {
        let view = PullRequestView {
            number: 1,
            title: "Add feature".to_string(),
            labels: vec!["review".to_string()],
            url: "https://example.com/pr/1".to_string(),
            skill_key: SkillInvocation::skill_key("pr-review", ""),
            skip_reason: Some("draft PR".to_string()),
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");

        // camelCase / flat keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("skillKey").is_some());
        assert!(v.get("skipReason").is_some());

        // snake_case form absent — a rename of the one multi-word field
        // (`skip_reason`) would surface here.
        assert!(v.get("skip_reason").is_none());
    }

    // A non-skipped PR serializes `skipReason` as JSON null (not omitted) so the
    // frontend's `skipReason: string | null` mirror stays a closed contract.
    #[test]
    fn pull_request_view_none_skip_reason_serializes_to_null() {
        let view = PullRequestView {
            number: 2,
            title: "Ready".to_string(),
            labels: vec![],
            url: "https://example.com/pr/2".to_string(),
            skill_key: SkillInvocation::skill_key("pr-review", "--check"),
            skip_reason: None,
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");
        assert_eq!(v["skipReason"], serde_json::Value::Null);
    }

    // Front/back contract lock for the persisted-retention row (Medium carrier per
    // ai-robust.md): `TrackedPrView` is mirrored in `src/types.ts`. It flattens
    // `PullRequestView`, so the inner keys (`number,title,labels,url,kind,skipReason`)
    // must surface at the top level alongside `presence` / `archived`; a flatten
    // regression or field rename surfaces here and must be synced to the TS mirror
    // in lockstep (the open end of this funnel; future Hard path = codegen from
    // `model.rs` + `git diff --exit-code`).
    #[test]
    fn tracked_pr_view_wire_shape_is_camel_case() {
        let view = TrackedPrView {
            pr: PullRequestView {
                number: 1,
                title: "Add feature".to_string(),
                labels: vec!["review".to_string()],
                url: "https://example.com/pr/1".to_string(),
                skill_key: SkillInvocation::skill_key("pr-review", ""),
                skip_reason: None,
            },
            presence: PrPresence::Current,
            archived: false,
        };

        let v = serde_json::to_value(&view).expect("TrackedPrView serializes");

        // Flattened `PullRequestView` keys present at the top level.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("skillKey").is_some());
        assert!(v.get("skipReason").is_some());

        // Retention keys present (camelCase).
        assert!(v.get("presence").is_some());
        assert!(v.get("archived").is_some());

        // `flatten` must hoist the inner fields, NOT nest them under a `pr` wrapper;
        // and the one multi-word field must not leak its snake_case form.
        assert!(v.get("skip_reason").is_none());
        assert!(
            v.get("pr").is_none(),
            "flatten must not nest a 'pr' wrapper"
        );

        // None `skip_reason` serializes as JSON null (not omitted), mirroring
        // `PullRequestView`'s closed `skipReason: string | null` contract at the
        // flattened depth.
        assert_eq!(v["skipReason"], serde_json::Value::Null);

        // `presence` serializes to the pinned lowercase wire strings the TS mirror
        // discriminates on.
        assert_eq!(v["presence"], "current");
        let stale = serde_json::to_value(PrPresence::Stale).expect("PrPresence serializes");
        assert_eq!(stale, "stale");
    }

    // Cross-agent wire contract lock for the AB#1079 event-pipeline discriminator
    // `EventType` (Medium carrier per ai-robust.md): the frontend's `EVENT_TYPES`
    // (`src/types.ts`) mirrors these exact camelCase strings. A variant rename or a
    // `rename_all` change surfaces here (the exhaustive `match` a future inbox/rule
    // consumer adds is the Hard carrier). Default is `PullRequest` (the only class the
    // current webhook path emits).
    #[test]
    fn event_type_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(EventType::PullRequest).expect("EventType serializes"),
            "pullRequest"
        );
        assert_eq!(
            serde_json::to_value(EventType::Issue).expect("EventType serializes"),
            "issue"
        );
        assert_eq!(
            serde_json::to_value(EventType::Comment).expect("EventType serializes"),
            "comment"
        );
        assert_eq!(
            serde_json::to_value(EventType::Label).expect("EventType serializes"),
            "label"
        );
        assert_eq!(
            serde_json::to_value(EventType::Generic).expect("EventType serializes"),
            "generic"
        );
        assert_eq!(
            serde_json::to_value(EventType::default()).expect("EventType serializes"),
            "pullRequest"
        );
    }

    // The generated TS declaration and this golden close both ends of the typed event envelope.
    #[test]
    fn event_wire_shape_is_camel_case() {
        let event = EventEnvelope::observation(
            InboxDedupeKey::new("github:pullRequest:owner/repo#7:labeled").unwrap(),
            SourceKind::Github,
            "p1",
            "owner/repo",
            EventType::PullRequest,
            EventSubject {
                number: Some(7),
                title: "Add feature".into(),
                body: "body text".into(),
                labels: vec!["pr-review".into()],
                url: "https://example.com/pr/7".into(),
            },
            1_700_000_000,
        )
        .unwrap();

        let v = serde_json::to_value(&event).expect("Event serializes");

        // camelCase keys present.
        assert!(v.get("dedupeKey").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("projectId").is_some());
        assert!(v.get("repo").is_some());
        assert!(v.get("payload").is_some());
        assert!(v.get("receivedAtEpoch").is_some());

        // snake_case forms absent — a rename of any multi-word field surfaces here.
        assert!(v.get("dedupe_key").is_none());
        assert!(v.get("project_id").is_none());
        assert!(v.get("received_at_epoch").is_none());

        // The nested kind enums serialize to their pinned wire strings.
        assert_eq!(v["source"], "github");
        assert_eq!(v["payload"]["kind"], "observation");
        assert_eq!(v["payload"]["eventType"], "pullRequest");

        // An absent `number` (a generic / non-numbered event) serializes as JSON null
        // (not omitted), keeping the TS mirror's `number: number | null` a closed contract.
        let generic = EventEnvelope::observation(
            InboxDedupeKey::new("generic:1").unwrap(),
            SourceKind::Github,
            "p1",
            "owner/repo",
            EventType::Generic,
            EventSubject {
                number: None,
                title: String::new(),
                body: String::new(),
                labels: vec![],
                url: String::new(),
            },
            1_700_000_000,
        )
        .unwrap();
        let gv = serde_json::to_value(&generic).expect("Event serializes");
        assert_eq!(gv["payload"]["subject"]["number"], serde_json::Value::Null);
    }

    // Cross-agent wire contract lock for the AB#1065 inbox status (Medium carrier per
    // ai-robust.md): the frontend's `INBOX_STATUSES` (`src/types.ts`) mirrors these exact
    // camelCase strings. A variant rename or a `rename_all` change surfaces here (the
    // exhaustive `match InboxStatus` in `inbox::store::status_as_wire` is the Hard carrier).
    // Default is `Received` (the state every delivery starts in).
    #[test]
    fn inbox_status_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(InboxStatus::Received).expect("InboxStatus serializes"),
            "received"
        );
        assert_eq!(
            serde_json::to_value(InboxStatus::Processed).expect("InboxStatus serializes"),
            "processed"
        );
        assert_eq!(
            serde_json::to_value(InboxStatus::Failed).expect("InboxStatus serializes"),
            "failed"
        );
        assert_eq!(
            serde_json::to_value(InboxStatus::default()).expect("InboxStatus serializes"),
            "received"
        );
    }

    // Front/back contract lock for the AB#1065 `InboxEntry` row (Medium carrier per
    // ai-robust.md): mirrored in `src/types.ts` (`InboxEntry`); a field change must be synced
    // there in lockstep (the open end of the funnel — future Hard path = codegen from
    // `model.rs` + `git diff --exit-code`). Locks camelCase keys present + snake_case absent,
    // the NESTED `event` object (not flattened — its own camelCase keys surface under `event`),
    // the nested `status` wire string, and that an absent `processedAtEpoch` / `error`
    // serializes as JSON null (not omitted) so the TS mirror's `… | null` stays closed.
    #[test]
    fn inbox_entry_wire_shape_is_camel_case() {
        let entry = InboxEntry {
            id: 7,
            event: EventEnvelope::observation(
                InboxDedupeKey::new("github:abc-123").unwrap(),
                SourceKind::Github,
                "p1",
                "owner/repo",
                EventType::PullRequest,
                EventSubject {
                    number: Some(7),
                    title: "Add feature".into(),
                    body: String::new(),
                    labels: vec!["pr-review".into()],
                    url: "https://example.com/pr/7".into(),
                },
                1_700_000_000,
            )
            .unwrap(),
            status: InboxStatus::Processed,
            processed_at_epoch: Some(1_700_000_005),
            error: None,
        };

        let v = serde_json::to_value(&entry).expect("InboxEntry serializes");

        // camelCase keys present at the top level.
        assert!(v.get("id").is_some());
        assert!(v.get("event").is_some());
        assert!(v.get("status").is_some());
        assert!(v.get("processedAtEpoch").is_some());
        assert!(v.get("error").is_some());

        // snake_case form absent — a rename of the multi-word field surfaces here.
        assert!(v.get("processed_at_epoch").is_none());

        // The event is NESTED (a real object), NOT flattened — its camelCase keys live
        // UNDER `event`, and do not leak to the top level (the contrast with `TrackedPrView`).
        let ev = &v["event"];
        assert!(ev.is_object(), "event is a nested object, not flattened");
        assert!(ev.get("dedupeKey").is_some());
        assert!(ev.get("payload").is_some());
        assert!(ev.get("receivedAtEpoch").is_some());
        assert!(
            v.get("dedupeKey").is_none(),
            "nested event keys must not hoist to the top level"
        );

        // The nested `status` enum serializes to its pinned wire string.
        assert_eq!(v["status"], "processed");

        // An absent `processedAtEpoch` / `error` serializes as JSON null (not omitted),
        // keeping the TS mirror's `processedAtEpoch: number | null` / `error: string | null`
        // a closed contract.
        let received = InboxEntry {
            status: InboxStatus::Received,
            processed_at_epoch: None,
            error: None,
            ..entry
        };
        let rv = serde_json::to_value(&received).expect("InboxEntry serializes");
        assert_eq!(rv["status"], "received");
        assert_eq!(rv["processedAtEpoch"], serde_json::Value::Null);
        assert_eq!(rv["error"], serde_json::Value::Null);
    }

    // Backend-internal cross-slice lock for the AB#1070 normalized `Notification` (Medium
    // carrier per ai-robust.md): the output-side mirror of `EventEnvelope`. Like `Candidate` it is
    // NOT mirrored in `src/types.ts` (consumed only by Rust providers / a future outbox), so
    // this lock guards the camelCase wire shape the producer/consumer rely on — the funnel has
    // no open TS end. Locks camelCase keys present + snake_case absent + the nested level string.
    #[test]
    fn notification_wire_shape_is_camel_case() {
        let note = Notification::new(
            NotificationLevel::Info,
            "PR #7 review 完成".to_string(),
            "https://example.com/pr/7".to_string(),
            RedactedNotificationBody::action_url("https://example.com/pr/7".to_string()),
            "p1".to_string(),
        );

        let v = serde_json::to_value(&note).expect("Notification serializes");

        // camelCase keys present.
        assert!(v.get("level").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("body").is_some());
        assert!(v.get("projectId").is_some());

        // snake_case form absent — a rename of the multi-word field surfaces here.
        assert!(v.get("project_id").is_none());

        // The nested level enum serializes to its pinned wire string.
        assert_eq!(v["level"], "info");
        // The body newtype is serde-transparent, preserving the outbound wire shape.
        assert_eq!(v["body"], "https://example.com/pr/7");

        // Roundtrips back (a future outbox queue deserializes it) — exercises `Deserialize`.
        let back: Notification = serde_json::from_value(v).expect("Notification deserializes");
        assert_eq!(back.level, NotificationLevel::Info);
        assert_eq!(back.body.as_str(), "https://example.com/pr/7");
        assert_eq!(back.project_id, "p1");
    }

    #[test]
    fn send_notification_request_response_wire_shape_and_unknown_reject() {
        let req = SendNotificationRequest {
            level: Some(NotificationLevel::Warning),
            title: "Deploy done".to_string(),
            body: Some("Build 42 finished".to_string()),
            url: Some("https://example.com/build/42".to_string()),
            project_id: Some("p1".to_string()),
            channel_ids: vec!["desktop".to_string(), "slack-main".to_string()],
        };

        let v = serde_json::to_value(&req).expect("SendNotificationRequest serializes");
        assert_eq!(v["level"], "warning");
        assert_eq!(v["projectId"], "p1");
        assert_eq!(v["channelIds"][1], "slack-main");
        assert!(v.get("project_id").is_none());
        assert!(v.get("channel_ids").is_none());

        let back: SendNotificationRequest =
            serde_json::from_value(v).expect("SendNotificationRequest deserializes");
        assert_eq!(back, req);

        let minimal: SendNotificationRequest =
            serde_json::from_value(serde_json::json!({"title":"t","body":"b"}))
                .expect("minimal request deserializes");
        assert_eq!(minimal.level, None);
        assert!(minimal.channel_ids.is_empty());

        let err = serde_json::from_value::<SendNotificationRequest>(serde_json::json!({
            "title": "t",
            "body": "b",
            "webhookUrl": "https://hooks.example.com/secret"
        }))
        .expect_err("unknown secret-looking fields are rejected");
        assert!(err.to_string().contains("unknown field"), "{err}");

        let response = SendNotificationResponse {
            outbox_ids: vec![10, 11],
        };
        let rv = serde_json::to_value(&response).expect("SendNotificationResponse serializes");
        assert_eq!(rv["outboxIds"], serde_json::json!([10, 11]));
        assert!(rv.get("outbox_ids").is_none());
    }

    #[test]
    fn send_messaging_request_response_wire_shape_and_secret_exclusion() {
        let req = SendMessagingRequest {
            integration_id: "feishu-main".to_string(),
            conversation_id: "oc_123".to_string(),
            content: MessagingSendContent::Card {
                title: "Build complete".to_string(),
                text: "hello".to_string(),
                template: MessagingCardTemplate::Blue,
            },
            request_id: "req-1".to_string(),
        };
        let v = serde_json::to_value(&req).expect("SendMessagingRequest serializes");
        assert_eq!(v["integrationId"], "feishu-main");
        assert_eq!(v["conversationId"], "oc_123");
        assert_eq!(v["content"]["kind"], "card");
        assert_eq!(v["content"]["title"], "Build complete");
        assert_eq!(v["content"]["text"], "hello");
        assert_eq!(v["content"]["template"], "blue");
        assert_eq!(v["requestId"], "req-1");
        assert!(v.get("integration_id").is_none());

        let err = serde_json::from_value::<SendMessagingRequest>(serde_json::json!({
            "integrationId": "feishu-main",
            "conversationId": "oc_123",
            "content": {"kind": "text", "text": "hello"},
            "requestId": "req-1",
            "webhookUrl": "https://secret.example/hook"
        }))
        .expect_err("unknown secret-looking fields are rejected");
        assert!(
            err.to_string().contains("unknown field"),
            "Current wire deny_unknown must surface, got: {err}"
        );

        let payload = MessagingSendPayload {
            integration_id: "feishu-main".to_string(),
            provider: MessagingProviderKind::Feishu,
            conversation_id: "oc_123".to_string(),
            content: MessagingSendContent::Text {
                text: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&payload).expect("payload serializes");
        for denied in [
            "secret",
            "token",
            "webhookUrl",
            "appSecret",
            "authorization",
        ] {
            assert!(
                !json.contains(denied),
                "messaging send payload must not contain {denied}: {json}"
            );
        }

        let response = SendMessagingResponse { outbox_id: 7 };
        let rv = serde_json::to_value(&response).expect("SendMessagingResponse serializes");
        assert_eq!(rv["outboxId"], serde_json::json!(7));
        assert!(rv.get("outbox_id").is_none());
    }

    #[test]
    fn messaging_send_debug_redacts_text_and_card_body() {
        let text = SendMessagingRequest {
            integration_id: "feishu-main".to_string(),
            conversation_id: "oc_123".to_string(),
            content: MessagingSendContent::Text {
                text: "private text".to_string(),
            },
            request_id: "req-1".to_string(),
        };
        let card = MessagingSendPayload {
            integration_id: "feishu-main".to_string(),
            provider: MessagingProviderKind::Feishu,
            conversation_id: "oc_123".to_string(),
            content: MessagingSendContent::Card {
                title: "private title".to_string(),
                text: "private markdown".to_string(),
                template: MessagingCardTemplate::Orange,
            },
        };
        let debug = format!("{text:?} {card:?}");
        assert!(!debug.contains("private text"));
        assert!(!debug.contains("private title"));
        assert!(!debug.contains("private markdown"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn messaging_send_legacy_wire_is_read_compatible_but_rewrites_to_sealed_content() {
        let request: SendMessagingRequest = serde_json::from_value(serde_json::json!({
            "integrationId": "feishu-main",
            "conversationId": "oc_123",
            "text": "legacy request",
            "requestId": "req-legacy"
        }))
        .expect("legacy request deserializes");
        assert_eq!(
            request.content,
            MessagingSendContent::Text {
                text: "legacy request".to_string()
            }
        );
        let request_wire = serde_json::to_value(&request).expect("request reserializes");
        assert_eq!(request_wire["content"]["kind"], "text");
        assert_eq!(request_wire["content"]["text"], "legacy request");
        assert!(request_wire.get("text").is_none());

        let payload: MessagingSendPayload = serde_json::from_value(serde_json::json!({
            "integrationId": "feishu-main",
            "provider": "feishu",
            "conversationId": "oc_123",
            "text": "legacy persisted payload"
        }))
        .expect("legacy persisted outbox payload deserializes");
        assert_eq!(
            payload.content,
            MessagingSendContent::Text {
                text: "legacy persisted payload".to_string()
            }
        );
        let payload_wire = serde_json::to_value(&payload).expect("payload reserializes");
        assert_eq!(payload_wire["content"]["kind"], "text");
        assert_eq!(payload_wire["content"]["text"], "legacy persisted payload");
        assert!(payload_wire.get("text").is_none());
    }

    #[test]
    fn messaging_send_wire_rejects_mixed_missing_outer_and_nested_extra_fields() {
        let valid_request = serde_json::json!({
            "integrationId": "feishu-main",
            "conversationId": "oc_123",
            "content": {"kind": "text", "text": "hello"},
            "requestId": "req-1"
        });
        for invalid in [
            serde_json::json!({
                "integrationId": "feishu-main", "conversationId": "oc_123",
                "content": {"kind": "text", "text": "hello"},
                "text": "legacy", "requestId": "req-1"
            }),
            serde_json::json!({
                "integrationId": "feishu-main", "conversationId": "oc_123",
                "requestId": "req-1"
            }),
            serde_json::json!({
                "integrationId": "feishu-main", "conversationId": "oc_123",
                "content": {"kind": "text", "text": "hello"},
                "requestId": "req-1", "appSecret": "secret"
            }),
        ] {
            assert!(serde_json::from_value::<SendMessagingRequest>(invalid).is_err());
        }

        for content in [
            serde_json::json!({"kind": "text", "text": "hello", "title": "mixed"}),
            serde_json::json!({"kind": "text", "text": "hello", "token": "secret"}),
            serde_json::json!({
                "kind": "card", "title": "title", "text": "body", "template": "blue",
                "webhookUrl": "https://secret.example"
            }),
            serde_json::json!({
                "kind": "card", "title": "title", "text": "body", "template": "blue",
                "legacyText": "mixed"
            }),
        ] {
            let mut request = valid_request.clone();
            request["content"] = content;
            assert!(serde_json::from_value::<SendMessagingRequest>(request).is_err());
        }

        for invalid in [
            serde_json::json!({
                "integrationId": "feishu-main", "provider": "feishu",
                "conversationId": "oc_123", "content": {"kind": "text", "text": "new"},
                "text": "legacy"
            }),
            serde_json::json!({
                "integrationId": "feishu-main", "provider": "feishu",
                "conversationId": "oc_123"
            }),
            serde_json::json!({
                "integrationId": "feishu-main", "provider": "feishu",
                "conversationId": "oc_123", "text": "legacy", "token": "secret"
            }),
            serde_json::json!({
                "integrationId": "feishu-main", "provider": "feishu",
                "conversationId": "oc_123",
                "content": {"kind": "text", "text": "new", "appSecret": "secret"}
            }),
        ] {
            assert!(serde_json::from_value::<MessagingSendPayload>(invalid).is_err());
        }
    }

    #[test]
    fn messaging_integration_option_wire_shape_and_secret_exclusion() {
        let option = MessagingIntegrationOption {
            id: "wx-main".to_string(),
            name: "企业微信".to_string(),
            kind: MessagingProviderKind::WeChatWork,
            allowed_conversation_ids: vec!["room-1".to_string()],
        };
        let v = serde_json::to_value(&option).expect("MessagingIntegrationOption serializes");
        assert_eq!(v["id"], "wx-main");
        assert_eq!(v["name"], "企业微信");
        assert_eq!(v["kind"], "weChatWork");
        assert_eq!(v["allowedConversationIds"], serde_json::json!(["room-1"]));
        assert!(v.get("allowed_conversation_ids").is_none());
        let json = v.to_string();
        for denied in ["secret", "token", "webhookUrl", "appSecret", "encryptKey"] {
            assert!(
                !json.contains(denied),
                "messaging integration option must not contain {denied}: {json}"
            );
        }
    }

    // Cross-Rust-slice wire contract lock for `NotificationLevel` / `NotificationKind`
    // (AB#1070, Medium carrier): a variant rename or `rename_all` change surfaces here. The
    // exhaustive `match NotificationKind` in `review::notify::deliver` is the Hard carrier.
    #[test]
    fn notification_enums_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(NotificationLevel::Info).expect("NotificationLevel serializes"),
            "info"
        );
        assert_eq!(
            serde_json::to_value(NotificationLevel::Warning).expect("NotificationLevel serializes"),
            "warning"
        );
        assert_eq!(
            serde_json::to_value(NotificationLevel::Error).expect("NotificationLevel serializes"),
            "error"
        );
        assert_eq!(
            serde_json::to_value(NotificationLevel::default())
                .expect("NotificationLevel serializes"),
            "info"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Desktop).expect("NotificationKind serializes"),
            "desktop"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Email).expect("NotificationKind serializes"),
            "email"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Slack).expect("NotificationKind serializes"),
            "slack"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Telegram).expect("NotificationKind serializes"),
            "telegram"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::WeChatWork)
                .expect("NotificationKind serializes"),
            "weChatWork"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Feishu).expect("NotificationKind serializes"),
            "feishu"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::DingTalk).expect("NotificationKind serializes"),
            "dingTalk"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::default()).expect("NotificationKind serializes"),
            "desktop"
        );
    }

    #[test]
    fn notification_level_from_str_uses_wire_values() {
        assert_eq!(
            "info".parse::<NotificationLevel>(),
            Ok(NotificationLevel::Info)
        );
        assert_eq!(
            "warning".parse::<NotificationLevel>(),
            Ok(NotificationLevel::Warning)
        );
        assert_eq!(
            "error".parse::<NotificationLevel>(),
            Ok(NotificationLevel::Error)
        );
        assert!("critical".parse::<NotificationLevel>().is_err());
    }

    #[test]
    fn notification_delivery_payload_excludes_channel_secrets() {
        let payload = NotificationDeliveryPayload {
            notification: Notification::new(
                NotificationLevel::Info,
                "PR #7 review 完成".to_string(),
                "https://example.com/pr/7".to_string(),
                RedactedNotificationBody::action_url("https://example.com/pr/7".to_string()),
                "p1".to_string(),
            ),
            channel_id: "slack-main".to_string(),
            kind: NotificationKind::Slack,
        };

        let json = serde_json::to_string(&payload).expect("payload serializes");
        assert!(json.contains("slack-main"));
        assert!(!json.contains("webhook"));
        assert!(!json.contains("token"));
        assert!(!json.contains("password"));
        assert!(!json.contains("secret"));
    }

    // Cross-agent wire contract lock for the AB#1066 outbox status / kind (Medium carrier per
    // ai-robust.md): the frontend's `OUTBOX_STATUSES` / `ACTION_KINDS` (`src/types.ts`) mirror
    // these exact camelCase strings. A variant rename or a `rename_all` change surfaces here (the
    // exhaustive `match` in `outbox::store::status_as_wire` / `kind_as_wire` is the Hard carrier).
    // Defaults: `Pending` (the state every action starts in) / `Notification` (the only kind).
    #[test]
    fn outbox_status_and_kind_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(ActionStatus::Pending).expect("ActionStatus serializes"),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::Blocked).expect("ActionStatus serializes"),
            "blocked"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::Done).expect("ActionStatus serializes"),
            "done"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::Dead).expect("ActionStatus serializes"),
            "dead"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::default()).expect("ActionStatus serializes"),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::Notification).expect("ActionKind serializes"),
            "notification"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::RunSkill).expect("ActionKind serializes"),
            "runSkill"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::StopReview).expect("ActionKind serializes"),
            "stopReview"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::MessagingReply).expect("ActionKind serializes"),
            "messagingReply"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::MessagingSend).expect("ActionKind serializes"),
            "messagingSend"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::default()).expect("ActionKind serializes"),
            "notification"
        );
    }

    #[test]
    fn messaging_provider_and_status_wire_helpers_match_serde_values() {
        assert_eq!(
            serde_json::to_value(MessagingProviderKind::Feishu)
                .expect("MessagingProviderKind serializes"),
            "feishu"
        );
        assert_eq!(
            serde_json::to_value(MessagingProviderKind::WeChatWork)
                .expect("MessagingProviderKind serializes"),
            "weChatWork"
        );
        assert_eq!(
            serde_json::to_value(MessagingProviderKind::DingTalk)
                .expect("MessagingProviderKind serializes"),
            "dingTalk"
        );
        assert_eq!(MessagingProviderKind::Feishu.as_wire(), "feishu");
        assert_eq!(MessagingProviderKind::WeChatWork.as_wire(), "weChatWork");
        assert_eq!(MessagingProviderKind::DingTalk.as_wire(), "dingTalk");
        assert_eq!(
            MessagingProviderKind::from_wire("feishu"),
            Some(MessagingProviderKind::Feishu)
        );
        assert_eq!(
            MessagingProviderKind::from_wire("weChatWork"),
            Some(MessagingProviderKind::WeChatWork)
        );
        assert_eq!(
            MessagingProviderKind::from_wire("dingTalk"),
            Some(MessagingProviderKind::DingTalk)
        );
        assert_eq!(MessagingProviderKind::from_wire("slack"), None);

        let connection = MessagingConnectionStatus {
            provider: MessagingProviderKind::DingTalk,
            integration_id: "dingtalk-main".to_string(),
            status: MessagingConnectionState::Connected,
            last_connected_at_epoch: Some(1),
            last_event_at_epoch: None,
            last_error: None,
            reconnect_count: 2,
        };
        let cv = serde_json::to_value(&connection).expect("MessagingConnectionStatus serializes");
        assert_eq!(cv["provider"], "dingTalk");
        assert_eq!(cv["integrationId"], "dingtalk-main");
        assert_eq!(cv["status"], "connected");
        assert_eq!(cv["reconnectCount"], 2);
        assert!(cv.get("integration_id").is_none());
        assert!(cv.get("last_connected_at_epoch").is_none());

        for (status, wire) in [
            (MessagingEventStatus::Received, "received"),
            (MessagingEventStatus::Processed, "processed"),
            (MessagingEventStatus::Failed, "failed"),
        ] {
            assert_eq!(status.as_wire(), wire);
            assert_eq!(
                serde_json::to_value(status).expect("MessagingEventStatus serializes"),
                wire
            );
            assert_eq!(MessagingEventStatus::from_wire_lenient(wire), status);
        }
        assert_eq!(
            MessagingEventStatus::from_wire_lenient("corrupt"),
            MessagingEventStatus::Failed
        );
    }

    // Front/back contract lock for the AB#1066 `OutboxEntry` row (Medium carrier per ai-robust.md):
    // mirrored in `src/types.ts` (`OutboxEntry`); a field change must be synced there in lockstep
    // (the open end of the funnel — future Hard path = codegen from `model.rs` + `git diff
    // --exit-code`). Locks camelCase keys present + snake_case absent, the nested `kind` / `status`
    // wire strings, and that an absent `lastError` serializes as JSON null (not omitted) so the TS
    // mirror's `lastError: string | null` stays closed. The raw payload is intentionally NOT a
    // field (fetched via `outbox_get_raw`), mirroring the inbox's raw-payload split.
    #[test]
    fn outbox_entry_wire_shape_is_camel_case() {
        let entry = OutboxEntry {
            id: 7,
            project_id: "p1".to_string(),
            kind: ActionKind::Notification,
            summary: "PR #7 review 完成".to_string(),
            status: ActionStatus::Pending,
            attempt_count: 2,
            next_attempt_at: 1_700_000_060,
            last_error: Some("notify failed".to_string()),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_030,
        };

        let v = serde_json::to_value(&entry).expect("OutboxEntry serializes");

        // camelCase keys present.
        assert!(v.get("id").is_some());
        assert!(v.get("projectId").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("summary").is_some());
        assert!(v.get("status").is_some());
        assert!(v.get("attemptCount").is_some());
        assert!(v.get("nextAttemptAt").is_some());
        assert!(v.get("lastError").is_some());
        assert!(v.get("createdAt").is_some());
        assert!(v.get("updatedAt").is_some());

        // snake_case forms absent — a rename of a multi-word field surfaces here.
        assert!(v.get("project_id").is_none());
        assert!(v.get("attempt_count").is_none());
        assert!(v.get("next_attempt_at").is_none());
        assert!(v.get("last_error").is_none());
        assert!(v.get("created_at").is_none());
        assert!(v.get("updated_at").is_none());

        // The nested enums serialize to their pinned wire strings.
        assert_eq!(v["kind"], "notification");
        assert_eq!(v["status"], "pending");

        // A never-failed action's `lastError` is JSON null (not omitted), keeping the TS mirror's
        // `lastError: string | null` a closed contract.
        let fresh = OutboxEntry {
            attempt_count: 0,
            last_error: None,
            status: ActionStatus::Done,
            ..entry
        };
        let fv = serde_json::to_value(&fresh).expect("OutboxEntry serializes");
        assert_eq!(fv["status"], "done");
        assert_eq!(fv["lastError"], serde_json::Value::Null);
    }

    #[test]
    fn workflow_contract_wire_shape_is_camel_case() {
        assert_eq!(
            serde_json::to_value(WorkflowType::ReviewNotify).expect("WorkflowType serializes"),
            "reviewNotify"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStatus::Pending).expect("WorkflowStatus serializes"),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStatus::Running).expect("WorkflowStatus serializes"),
            "running"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStatus::Waiting).expect("WorkflowStatus serializes"),
            "waiting"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStatus::Done).expect("WorkflowStatus serializes"),
            "done"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStatus::Failed).expect("WorkflowStatus serializes"),
            "failed"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStep::StartReview).expect("WorkflowStep serializes"),
            "startReview"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStep::WaitReview).expect("WorkflowStep serializes"),
            "waitReview"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStep::EnqueueNotify).expect("WorkflowStep serializes"),
            "enqueueNotify"
        );
        assert_eq!(
            serde_json::to_value(WorkflowStep::Done).expect("WorkflowStep serializes"),
            "done"
        );

        let instance = WorkflowInstance {
            id: 7,
            project_id: "p1".to_string(),
            workflow_type: WorkflowType::ReviewNotify,
            status: WorkflowStatus::Waiting,
            current_step: WorkflowStep::WaitReview,
            input: serde_json::json!({"prNumber": 7}),
            state: serde_json::json!({"reviewThreadId": "t1"}),
            attempt_count: 1,
            next_wake_at: 20,
            last_error: None,
            created_at: 10,
            updated_at: 11,
        };
        let v = serde_json::to_value(&instance).expect("WorkflowInstance serializes");

        assert_eq!(v["type"], "reviewNotify");
        assert_eq!(v["status"], "waiting");
        assert_eq!(v["currentStep"], "waitReview");
        assert!(v.get("projectId").is_some());
        assert!(v.get("attemptCount").is_some());
        assert!(v.get("nextWakeAt").is_some());
        assert!(v.get("lastError").is_some());
        assert!(v.get("project_id").is_none());
        assert!(v.get("workflow_type").is_none());
        assert!(v.get("current_step").is_none());
        assert!(v.get("next_wake_at").is_none());
        assert_eq!(v["lastError"], serde_json::Value::Null);
    }

    // Wire lock for the AB#1069 review/check action payload (Medium carrier per ai-robust.md):
    // backend-internal (read by the lib.rs executor, produced by a future Rule Engine), so NOT
    // mirrored in `src/types.ts` — but persisted in the outbox `payload` column and replayed, so its
    // camelCase shape must stay stable. Round-trips (the executor deserializes it).
    #[test]
    fn review_action_payload_wire_shape_is_camel_case() {
        let payload = ReviewActionPayload::Automatic {
            candidate: Candidate {
                number: 7,
                head_sha: "sha".to_string(),
                head_ref: "main".to_string(),
                author: "octocat".to_string(),
                is_cross_repository: false,
                is_draft: false,
                skill_key: SkillInvocation::skill_key("pr-review", ""),
            },
            invocation: SkillInvocation::build(
                "pr-review",
                Some("/tmp/skill.md".into()),
                "/pr-review 7",
                "",
            ),
        };
        let v = serde_json::to_value(&payload).expect("ReviewActionPayload serializes");
        // camelCase present, snake_case absent.
        assert!(v.get("candidate").is_some());
        assert_eq!(v["candidate"]["headSha"], "sha");
        assert_eq!(
            v["candidate"]["skillKey"],
            SkillInvocation::skill_key("pr-review", "")
        );
        assert!(v.get("prNumber").is_none());
        assert!(v["candidate"].get("head_sha").is_none());
        // invocation is a camelCase object with the four SkillInvocation fields.
        let inv = v.get("invocation").expect("invocation present");
        assert_eq!(inv["skillName"], "pr-review");
        assert_eq!(inv["skillPath"], "/tmp/skill.md");
        assert_eq!(inv["command"], "/pr-review 7");
        assert_eq!(inv["skillKey"], SkillInvocation::skill_key("pr-review", ""));
        assert!(inv.get("skill_name").is_none());
        assert!(inv.get("skill_path").is_none());
        assert!(inv.get("skill_key").is_none());
        // F3: the routing key is the outbox ROW's project_id, NOT a payload field — assert it is
        // absent so a producer can't reintroduce a dual project source.
        assert!(v.get("projectId").is_none());
        assert!(v.get("project_id").is_none());
        // Round-trips — the executor reads it back from the stored payload.
        let back: ReviewActionPayload =
            serde_json::from_value(v).expect("ReviewActionPayload round-trips");
        assert_eq!(back, payload);
    }

    // Wire lock for the AB#1069 stop-review action payload (Medium carrier): same backend-internal,
    // persisted-and-replayed status as the review payload; carries the `(pr, kind)` session key (the
    // project is the outbox row's, AB#1069 F3). Round-trips.
    #[test]
    fn stop_review_action_payload_wire_shape_is_camel_case() {
        let payload = StopReviewActionPayload {
            pr_number: 7,
            skill_key: SkillInvocation::skill_key("pr-review", ""),
        };
        let v = serde_json::to_value(&payload).expect("StopReviewActionPayload serializes");
        assert!(v.get("prNumber").is_some());
        assert!(v.get("skillKey").is_some());
        assert!(v.get("pr_number").is_none());
        // F3: routing key is the row's project_id, not a payload field.
        assert!(v.get("projectId").is_none());
        assert!(v.get("project_id").is_none());
        let back: StopReviewActionPayload =
            serde_json::from_value(v).expect("StopReviewActionPayload round-trips");
        assert_eq!(back, payload);
    }

    #[test]
    fn render_command_substitutes_placeholders_and_appends_extra_args() {
        assert_eq!(
            SkillInvocation::render_command("/{skill} {pr}", "pr-review", 7, "o/r", ""),
            "/pr-review 7"
        );
        assert_eq!(
            SkillInvocation::render_command("/{skill} {pr}", "pr-review", 7, "o/r", "--check"),
            "/pr-review 7 --check"
        );
        assert_eq!(
            SkillInvocation::render_command("{skill}@{repo}#{pr}", "s", 9, "a/b", "  x  "),
            "s@a/b#9 x"
        );
    }

    #[test]
    fn resolve_skill_path_rejects_absolute_and_escape() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo_toml = manifest.join("Cargo.toml");
        let abs_err = SkillInvocation::resolve_skill_path("/unused", cargo_toml.to_str().unwrap())
            .expect_err("absolute rejected");
        assert!(
            abs_err.contains("absolute") || abs_err.contains("relative"),
            "{abs_err}"
        );

        let repo = manifest.parent().unwrap();
        let relative = SkillInvocation::resolve_skill_path(
            repo.to_str().unwrap(),
            ".codex/skills/pr-review/SKILL.md",
        )
        .expect("relative under repo");
        assert!(relative.is_file());

        let err = SkillInvocation::resolve_skill_path(repo.to_str().unwrap(), "../etc/passwd")
            .expect_err("escape");
        assert!(
            err.contains("escape")
                || err.contains("canonicalize")
                || err.contains("..")
                || err.contains("must be under"),
            "{err}"
        );

        let not_allowlisted =
            SkillInvocation::resolve_skill_path(repo.to_str().unwrap(), "Cargo.toml")
                .expect_err("not allowlisted");
        assert!(
            not_allowlisted.contains("must be under"),
            "{not_allowlisted}"
        );
    }

    #[test]
    fn skill_key_and_legacy_migrate_are_stable() {
        assert_eq!(SkillInvocation::skill_key("pr-review", ""), "pr-review\0");
        assert_eq!(
            SkillInvocation::skill_key("pr-review", " --check "),
            "pr-review\0--check"
        );
        assert_eq!(
            SkillInvocation::migrate_legacy_skill_key("review"),
            SkillInvocation::skill_key("pr-review", "")
        );
        assert_eq!(
            SkillInvocation::migrate_legacy_skill_key("check"),
            SkillInvocation::skill_key("pr-review", "--check")
        );
        assert_eq!(
            SkillInvocation::display_label("pr-review\0--check"),
            "pr-review --check"
        );
        assert_eq!(
            SkillInvocation::migrate_legacy_dispatch_key("7@sha:review"),
            format!("7@sha:{}", SkillInvocation::skill_key("pr-review", ""))
        );
        assert_eq!(
            SkillInvocation::migrate_legacy_dispatch_key("7@sha:check"),
            format!(
                "7@sha:{}",
                SkillInvocation::skill_key("pr-review", "--check")
            )
        );
    }
}
