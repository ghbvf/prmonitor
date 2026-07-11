//! Rust slice-boundary enforcement test (Medium, per `.claude/rules/prmonitor/ai-robust.md`).
//!
//! The backend vertical slices (`config` / `pr` / `review` / `inbox` / `outbox` / `rule` /
//! `messaging` / `workflow`) must be
//! self-contained: a file in one slice must NOT make a runtime VALUE reference (`crate::<sibling>::`)
//! into a SIBLING slice. Cross-slice wiring is the composition root's job (`lib.rs` + the horizontal
//! modules `model` / `error` / `events` / `db` / `state`, none of which are slices).
//! This is the Rust-side mirror of the frontend `src/slice-boundary.test.ts`; before it, the backend
//! boundary was a Soft convention with no machine guard (AB#1066 F1) — this test makes a sibling-slice
//! value reference a **Medium** machine-detected violation (a failing test), the carrier the charter
//! requires for the review→outbox decoupling.
//!
//! **`config::service` is the one allowlisted cross-slice consumable.** It is the shared
//! settings/project provider `pr`/`review`/`outbox` legitimately read (pre-existing architecture);
//! a sibling may reference `crate::config::service`, but NOT any other config submodule. A reach
//! into `crate::config::model` (etc.) is the SAME boundary violation as referencing a sibling slice
//! (AB#1182 F1: this closed the previous downstream opening, where the WHOLE `crate::config::` was
//! allowlisted and the outbox worker grabbed a `config::model` constant directly). EVERY OTHER
//! slice→sibling-slice reference is forbidden — e.g. `review` must reach the outbox through the
//! composition-root-injected sink, never `crate::outbox::`.
//!
//! **Carrier rating (charter `.claude/rules/prmonitor/ai-robust.md`): Medium.** This is a CI-run
//! type-aware scan (a failing test), not a compile-time Hard barrier — a determined edit could
//! still write the violating path, but CI catches it. Funnel analysis: UPSTREAM (which slices may
//! consume config) AND DOWNSTREAM (which config SUBMODULE they may name) are now BOTH closed to
//! `crate::config::service`. Before AB#1182 F1 the downstream was OPEN (any `crate::config::*`
//! passed), so the funnel leaked into config internals — a non-closed funnel the charter forbids.

use std::fs;
use std::path::Path;

/// The backend vertical slices scanned for boundary violations.
const SLICES: [&str; 9] = [
    "config",
    "pr",
    "review",
    "inbox",
    "outbox",
    "rule",
    "terminal",
    "messaging",
    "workflow",
];

/// The ONE cross-slice consumable. `config` is the shared configuration provider consumed by
/// `pr`/`review`/`outbox`; widening this is a deliberate architecture decision, not a silent
/// allowance. A sibling may consume it ONLY through [`CONSUMABLE_ALLOWED_SUBMODULE`].
const CONSUMABLE_SLICE: &str = "config";

/// The ONLY `config` submodule a sibling slice may name (AB#1182 F1). `config::service` is the
/// public settings/project seam; a reach into any OTHER submodule (e.g. `crate::config::model`)
/// goes past the seam into config internals and is the SAME boundary violation as referencing a
/// sibling slice — so the cross-slice config funnel is closed to the service seam on BOTH ends.
const CONSUMABLE_ALLOWED_SUBMODULE: &str = "service";

#[test]
fn no_backend_slice_references_a_sibling_slice() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    let mut files_scanned = 0usize;
    for slice in SLICES {
        scan_dir(&src.join(slice), slice, &mut violations, &mut files_scanned);
    }
    // Guard against the scan silently matching nothing (a moved src dir would make the assertion
    // vacuously pass and let a real violation through), mirroring the frontend test's glob guard.
    assert!(
        files_scanned > 0,
        "slice-boundary scan found no .rs files — src layout changed?"
    );
    assert!(
        violations.is_empty(),
        "backend slice-boundary violations (a slice file references a sibling slice — wire it \
         through the composition root instead):\n{}",
        violations.join("\n")
    );
}

#[test]
fn no_backend_slice_references_composition_root_helpers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    let mut files_scanned = 0usize;
    for slice in SLICES {
        scan_dir_for_needle(
            &src.join(slice),
            "crate::composition::",
            &mut violations,
            &mut files_scanned,
        );
    }
    assert!(
        files_scanned > 0,
        "composition-root scan found no .rs files — src layout changed?"
    );
    assert!(
        violations.is_empty(),
        "backend slice-boundary violations (a slice references composition-root helpers directly; \
         install an opaque AppState seam instead):\n{}",
        violations.join("\n")
    );
}

/// #1374 Medium carrier: once every automatic/external producer enters through the durable
/// inbox, the legacy dispatcher bridge must not be reintroduced. The strings are assembled so
/// this guard does not match its own source.
#[test]
fn no_legacy_dispatch_bridge_symbols() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let needles = [
        ["run_auto", "_dispatch"].concat(),
        ["make_", "dispatcher"].concat(),
        ["Project", "Dispatcher"].concat(),
        ["mod ", "dispatch;"].concat(),
        ["DeliveryStatus::", "Dispatched"].concat(),
        ["trigger_", "review"].concat(),
    ];
    let mut violations = Vec::new();
    let mut files_scanned = 0usize;
    for needle in &needles {
        scan_dir_for_needle(&src, needle, &mut violations, &mut files_scanned);
    }
    assert!(
        files_scanned > 0,
        "legacy-dispatch scan found no Rust files"
    );
    assert!(
        violations.is_empty(),
        "legacy dispatch bridge remains after #1374:\n{}",
        violations.join("\n")
    );
}

/// App code may read provider state, but only the external pr-review skill is allowed to write to
/// GitHub. This governance carrier moved here when the legacy dispatch module was deleted.
#[test]
fn app_code_uses_no_gh_write_subcommands() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        ["pr ed", "it"].concat(),
        ["pr com", "ment"].concat(),
        ["pr rev", "iew"].concat(),
        ["issue com", "ment"].concat(),
        ["issue ed", "it"].concat(),
        ["--add-l", "abel"].concat(),
        ["--remove-l", "abel"].concat(),
    ];
    let mut violations = Vec::new();
    let mut files_scanned = 0usize;
    for needle in &forbidden {
        scan_dir_for_needle(&src, needle, &mut violations, &mut files_scanned);
    }
    assert!(files_scanned > 0, "gh-write scan found no Rust files");
    assert!(
        violations.is_empty(),
        "app-side gh WRITE detected (only the pr-review skill may write to GitHub):\n{}",
        violations.join("\n")
    );
}

/// #1374 external-ingress funnel (Medium callgraph carrier). These adapters receive untrusted or
/// remote intent and may only enqueue an ExternalReviewIngress receipt; the desktop/internal
/// manual start funnel is intentionally unavailable to them by policy and machine-checked here.
/// Scan the load-bearing callees that exist today, rather than relying on deleted bridge names:
/// Rust adapters may not call `start_review` / `start_for_outbox`, and the remote console may not
/// send the desktop-only `start_review` command. Rust call detection strips comments + literals and
/// distinguishes a function definition from a call expression, so declarations and documentation
/// do not create false positives.
#[test]
fn external_review_adapters_do_not_start_reviews_directly() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let paths = [
        src.join("review/local_api.rs"),
        src.join("review/deeplink.rs"),
        src.join("messaging/service.rs"),
        src.join("cli.rs"),
        src.join("workflow/service.rs"),
    ];
    let forbidden_callees = ["start_review", "start_for_outbox"];
    let mut violations = Vec::new();
    let mut rust_files_scanned = 0usize;
    for path in paths {
        scan_rust_file_for_callees(
            &path,
            &forbidden_callees,
            &mut violations,
            &mut rust_files_scanned,
        );
    }
    // Remote HTTP adapters are a directory, not one stable file. Recursive scanning closes the
    // easy bypass of moving a handler under `remote/**` and calling the internal start funnel there.
    scan_rust_dir_for_callees(
        &src.join("remote"),
        &forbidden_callees,
        &mut violations,
        &mut rust_files_scanned,
    );

    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri has a project-root parent");
    let mut frontend_files_scanned = 0usize;
    scan_frontend_dir_for_command(
        &project_root.join("src/remoteConsole"),
        "start_review",
        &mut violations,
        &mut frontend_files_scanned,
    );
    assert!(
        rust_files_scanned > 0 && frontend_files_scanned > 0,
        "external-review scan found no Rust or remote-console files — source layout changed?"
    );
    assert!(
        violations.is_empty(),
        "external review adapter bypasses durable ingress:\n{}",
        violations.join("\n")
    );
}

/// #1374 composition-root ownership carrier. `ExternalReviewIngress::set_sinks` installs the
/// durable inbox submit/get implementation and therefore belongs only in `lib.rs`; allowing a
/// slice or remote adapter to replace it would reopen the funnel even if every adapter currently
/// calls `submit`. Definitions, comments, and string examples are excluded by the callee scanner.
#[test]
fn external_review_ingress_sinks_are_installed_only_by_composition_root() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut calls = Vec::new();
    let mut files_scanned = 0usize;
    scan_rust_dir_for_callees(&src, &["set_sinks"], &mut calls, &mut files_scanned);
    assert!(
        files_scanned > 0,
        "ExternalReviewIngress::set_sinks scan found no Rust files"
    );

    let lib = src.join("lib.rs");
    let outside_root: Vec<_> = calls
        .iter()
        .filter(|call| !call.starts_with(&format!("{}:", lib.display())))
        .cloned()
        .collect();
    assert!(
        outside_root.is_empty(),
        "ExternalReviewIngress::set_sinks may only be called by composition-root lib.rs:\n{}",
        outside_root.join("\n")
    );
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one composition-root ExternalReviewIngress::set_sinks installation; \
         found:\n{}",
        calls.join("\n")
    );
}

fn scan_rust_file_for_callees(
    path: &Path,
    callees: &[&str],
    violations: &mut Vec<String>,
    files_scanned: &mut usize,
) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read Rust architecture input {}: {e}", path.display()));
    *files_scanned += 1;
    violations.extend(
        rust_callee_calls(&text, callees)
            .into_iter()
            .map(|(line, callee)| format!("{}:{line} calls {callee}", path.display())),
    );
}

fn scan_rust_dir_for_callees(
    dir: &Path,
    callees: &[&str],
    violations: &mut Vec<String>,
    files_scanned: &mut usize,
) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read Rust architecture directory {}: {e}", dir.display()))
        .map(|entry| entry.unwrap_or_else(|e| panic!("read entry below {}: {e}", dir.display())))
        .collect();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            scan_rust_dir_for_callees(&path, callees, violations, files_scanned);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            scan_rust_file_for_callees(&path, callees, violations, files_scanned);
        }
    }
}

fn scan_frontend_dir_for_command(
    dir: &Path,
    command: &str,
    violations: &mut Vec<String>,
    files_scanned: &mut usize,
) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| {
            panic!(
                "read frontend architecture directory {}: {e}",
                dir.display()
            )
        })
        .map(|entry| entry.unwrap_or_else(|e| panic!("read entry below {}: {e}", dir.display())))
        .collect();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            scan_frontend_dir_for_command(&path, command, violations, files_scanned);
            continue;
        }
        if !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "vue")
        ) {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read frontend architecture input {}: {e}", path.display()));
        *files_scanned += 1;
        let code = strip_c_style_comments(&text, false);
        let literals = [
            format!("\"{command}\""),
            format!("'{command}'"),
            format!("`{command}`"),
        ];
        for (line_no, line) in code.lines().enumerate() {
            if literals.iter().any(|literal| line.contains(literal)) {
                violations.push(format!(
                    "{}:{} sends forbidden frontend command {command}",
                    path.display(),
                    line_no + 1
                ));
            }
        }
    }
}

/// Return real call expressions for the named Rust callees. Removing comments and literals first
/// prevents examples such as `// start_review(...)` or `"start_review(...)"` from tripping the
/// gate. Requiring a following call parenthesis (optionally after a turbofish) and rejecting a
/// preceding `fn` token distinguishes calls from declarations without relying on line formatting.
fn rust_callee_calls(source: &str, callees: &[&str]) -> Vec<(usize, String)> {
    let code = strip_c_style_comments(source, true);
    let bytes = code.as_bytes();
    let mut calls = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if !is_rust_ident_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_rust_ident_continue(bytes[cursor]) {
            cursor += 1;
        }
        let identifier = &code[start..cursor];
        if !callees.contains(&identifier)
            || previous_rust_identifier(&code, start).as_deref() == Some("fn")
            || !has_call_suffix(&code, cursor)
        {
            continue;
        }
        let line = bytes[..start].iter().filter(|byte| **byte == b'\n').count() + 1;
        calls.push((line, identifier.to_string()));
    }
    calls
}

fn is_rust_ident_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_rust_ident_continue(byte: u8) -> bool {
    is_rust_ident_start(byte) || byte.is_ascii_digit()
}

fn previous_rust_identifier(code: &str, before: usize) -> Option<String> {
    let bytes = code.as_bytes();
    let mut end = before;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_rust_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    (start < end).then(|| code[start..end].to_string())
}

fn has_call_suffix(code: &str, after_identifier: usize) -> bool {
    let bytes = code.as_bytes();
    let mut cursor = skip_ascii_whitespace(bytes, after_identifier);
    if bytes.get(cursor) == Some(&b'(') {
        return true;
    }
    if bytes.get(cursor..cursor + 3) != Some(b"::<") {
        return false;
    }
    cursor += 3;
    let mut depth = 1usize;
    while cursor < bytes.len() && depth > 0 {
        match bytes[cursor] {
            b'<' => depth += 1,
            b'>' => depth -= 1,
            _ => {}
        }
        cursor += 1;
    }
    depth == 0 && bytes.get(skip_ascii_whitespace(bytes, cursor)) == Some(&b'(')
}

fn skip_ascii_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

/// Strip C-style line/block comments. For Rust scanning, quoted and raw-string literals are also
/// blanked (`strip_literals=true`) because identifiers inside data are not callees. For frontend
/// command scanning literals remain intact, while comments are blanked. Newlines are preserved so
/// diagnostics retain source line numbers. Rust block comments may nest, so depth is tracked.
fn strip_c_style_comments(source: &str, strip_literals: bool) -> String {
    let bytes = source.as_bytes();
    let mut output = bytes.to_vec();
    let mut cursor = 0usize;
    let mut block_depth = 0usize;
    while cursor < bytes.len() {
        if block_depth > 0 {
            if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                blank_byte(&mut output, cursor);
                blank_byte(&mut output, cursor + 1);
                block_depth += 1;
                cursor += 2;
            } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                blank_byte(&mut output, cursor);
                blank_byte(&mut output, cursor + 1);
                block_depth -= 1;
                cursor += 2;
            } else {
                blank_byte(&mut output, cursor);
                cursor += 1;
            }
            continue;
        }

        if bytes.get(cursor..cursor + 2) == Some(b"//") {
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                blank_byte(&mut output, cursor);
                cursor += 1;
            }
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"/*") {
            blank_byte(&mut output, cursor);
            blank_byte(&mut output, cursor + 1);
            block_depth = 1;
            cursor += 2;
            continue;
        }

        if let Some((quote, raw_hashes)) = literal_start(bytes, cursor, strip_literals) {
            let end = if let Some(hashes) = raw_hashes {
                raw_literal_end(bytes, quote + 1, hashes)
            } else {
                quoted_literal_end(bytes, quote + 1, bytes[quote])
            };
            if strip_literals {
                for at in cursor..end {
                    blank_byte(&mut output, at);
                }
            }
            cursor = end;
            continue;
        }
        cursor += 1;
    }
    String::from_utf8(output).expect("blanking source bytes preserves UTF-8")
}

/// Locate a normal/raw literal beginning at `cursor`. Frontend mode preserves all literals so the
/// exact command string remains visible; Rust mode blanks them before callee parsing.
fn literal_start(
    bytes: &[u8],
    cursor: usize,
    strip_literals: bool,
) -> Option<(usize, Option<usize>)> {
    if !strip_literals {
        return matches!(bytes[cursor], b'"' | b'\'' | b'`').then_some((cursor, None));
    }
    if bytes[cursor] == b'"' {
        return Some((cursor, None));
    }
    if bytes[cursor] != b'r' {
        return None;
    }
    let mut quote = cursor + 1;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
    }
    (bytes.get(quote) == Some(&b'"')).then_some((quote, Some(quote - cursor - 1)))
}

fn quoted_literal_end(bytes: &[u8], mut cursor: usize, quote: u8) -> usize {
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(bytes.len());
        } else if bytes[cursor] == quote {
            return cursor + 1;
        } else {
            cursor += 1;
        }
    }
    bytes.len()
}

fn raw_literal_end(bytes: &[u8], mut cursor: usize, hashes: usize) -> usize {
    while cursor < bytes.len() {
        if bytes[cursor] == b'"'
            && bytes
                .get(cursor + 1..cursor + 1 + hashes)
                .is_some_and(|tail| tail.iter().all(|byte| *byte == b'#'))
        {
            return cursor + 1 + hashes;
        }
        cursor += 1;
    }
    bytes.len()
}

fn blank_byte(output: &mut [u8], at: usize) {
    if output.get(at) != Some(&b'\n') {
        output[at] = b' ';
    }
}

/// Recursively scan `dir` (a slice owned by `owner`) for `crate::<sibling>::` value references.
fn scan_dir(dir: &Path, owner: &str, violations: &mut Vec<String>, files_scanned: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return; // a slice without its own subdir (none today) is simply not scanned.
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, owner, violations, files_scanned);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        *files_scanned += 1;
        let mut grouped_use: Option<(usize, String)> = None;
        for (i, line) in text.lines().enumerate() {
            // Strip any trailing `//` comment (and skip whole comment lines) before scanning, so a
            // cross-slice mention in a doc/inline comment is NOT a false positive — only real code
            // references count. A `//` inside a string can truncate early, but that only drops a
            // would-be match in a comment, never hides a real code-position `crate::<slice>::`.
            let code = line.split("//").next().unwrap_or(line);

            if let Some((start_line, statement)) = grouped_use.as_mut() {
                statement.push('\n');
                statement.push_str(code);
                if code.contains(';') {
                    scan_grouped_use(&path, *start_line, owner, statement, violations);
                    grouped_use = None;
                }
                continue;
            }

            if code.contains("use crate::{") && !code.contains(';') {
                grouped_use = Some((i + 1, code.to_owned()));
                continue;
            }

            for target in SLICES {
                if target == owner {
                    continue;
                }
                let needle = format!("crate::{target}::");
                let direct_use = format!("use crate::{target}");
                if code.trim_start().starts_with(&direct_use) {
                    if target == CONSUMABLE_SLICE
                        && code.trim_start().starts_with("use crate::config::service")
                    {
                        continue;
                    }
                    let rest = &code.trim_start()[direct_use.len()..];
                    if rest.starts_with(';')
                        || rest.starts_with(',')
                        || rest.starts_with(' ')
                        || rest.starts_with("::{")
                    {
                        violations.push(format!(
                            "{}:{} imports sibling slice crate::{} — wire it through the \
                             composition root instead",
                            path.display(),
                            i + 1,
                            target
                        ));
                        continue;
                    }
                }
                if let Some(group) = crate_group(code) {
                    if group.split(',').map(str::trim).any(|part| {
                        if target == CONSUMABLE_SLICE
                            && part.starts_with(&format!(
                                "{CONSUMABLE_SLICE}::{CONSUMABLE_ALLOWED_SUBMODULE}"
                            ))
                        {
                            return false;
                        }
                        part == target || part.starts_with(&format!("{target}::"))
                    }) {
                        violations.push(format!(
                            "{}:{} imports sibling slice crate::{} via grouped use — wire it \
                             through the composition root instead",
                            path.display(),
                            i + 1,
                            target
                        ));
                        continue;
                    }
                }
                // `config` is the one cross-slice consumable, but ONLY through its public
                // `service` seam (AB#1182 F1): a reach into another config submodule
                // (`crate::config::model`, …) is the same Medium boundary violation as referencing
                // a sibling slice. Check EACH occurrence on the line — its submodule token — so a
                // `service` reference on the same line can't mask a `model` reach-in.
                if target == CONSUMABLE_SLICE {
                    for (at, _) in code.match_indices(&needle) {
                        let submodule = code[at + needle.len()..]
                            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or("");
                        if submodule == CONSUMABLE_ALLOWED_SUBMODULE {
                            continue;
                        }
                        violations.push(format!(
                            "{}:{} reaches into config internals crate::config::{submodule} — \
                             cross-slice config access must go through crate::config::service",
                            path.display(),
                            i + 1,
                        ));
                    }
                    continue;
                }
                if code.contains(&needle) {
                    violations.push(format!(
                        "{}:{} references sibling slice crate::{}::",
                        path.display(),
                        i + 1,
                        target
                    ));
                }
                let app_state_field = format!("state.{target}");
                if code.contains(&app_state_field) {
                    violations.push(format!(
                        "{}:{} reaches sibling slice AppState field `{}` — expose an opaque \
                         composition-root hook instead",
                        path.display(),
                        i + 1,
                        app_state_field
                    ));
                }
            }
        }
        if let Some((start_line, statement)) = grouped_use {
            scan_grouped_use(&path, start_line, owner, &statement, violations);
        }
    }
}

fn scan_dir_for_needle(
    dir: &Path,
    needle: &str,
    violations: &mut Vec<String>,
    files_scanned: &mut usize,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_for_needle(&path, needle, violations, files_scanned);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        *files_scanned += 1;
        for (i, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if code.contains(needle) {
                violations.push(format!("{}:{} references {needle}", path.display(), i + 1));
            }
        }
    }
}

fn scan_grouped_use(
    path: &Path,
    start_line: usize,
    owner: &str,
    code: &str,
    violations: &mut Vec<String>,
) {
    let Some(group) = crate_group(code) else {
        return;
    };
    for target in SLICES {
        if target == owner {
            continue;
        }
        if group.split(',').map(str::trim).any(|part| {
            if target == CONSUMABLE_SLICE
                && part.starts_with(&format!(
                    "{CONSUMABLE_SLICE}::{CONSUMABLE_ALLOWED_SUBMODULE}"
                ))
            {
                return false;
            }
            part == target || part.starts_with(&format!("{target}::"))
        }) {
            violations.push(format!(
                "{}:{} imports sibling slice crate::{} via grouped use — wire it through the \
                 composition root instead",
                path.display(),
                start_line,
                target
            ));
        }
    }
}

fn crate_group(code: &str) -> Option<&str> {
    let group_start = code.find("use crate::{")? + "use crate::{".len();
    let group_end = code[group_start..].find('}')? + group_start;
    Some(&code[group_start..group_end])
}

#[test]
fn crate_group_handles_multiline_grouped_use() {
    let group =
        crate_group("use crate::{\n    config::service,\n    review::notify,\n    outbox,\n};")
            .expect("multiline grouped use should be parsed");
    assert!(group.contains("review::notify"));
    assert!(group.contains("outbox"));
}

/// Mutation proof for the callgraph carrier: declarations/docs/examples remain legal, while the
/// real current start callees and the ingress sink installer are recognized as executable calls.
#[test]
fn external_review_call_scanner_distinguishes_definitions_from_real_bypasses() {
    let safe_fixture = r##"
        pub async fn start_review<R: Runtime>(app: AppHandle<R>) {}
        pub async fn start_for_outbox<R: Runtime>(app: AppHandle<R>) {}
        pub fn set_sinks(&self) {}
        // crate::review::commands::start_review(app).await?;
        /* crate::review::commands::start_for_outbox(app).await?; */
        const EXAMPLE: &str = "start_review(app)";
        const RAW_EXAMPLE: &str = r#"start_for_outbox(app)"#;
    "##;
    assert!(
        rust_callee_calls(
            safe_fixture,
            &["start_review", "start_for_outbox", "set_sinks"]
        )
        .is_empty(),
        "definitions, comments, and literals are not executable bypasses"
    );

    let bypass_fixture = r#"
        crate::review::commands::start_review(app).await?;
        crate::review::commands::start_for_outbox::<MockRuntime>(app).await?;
        state.external_review.set_sinks(submit, get);
    "#;
    let calls = rust_callee_calls(
        bypass_fixture,
        &["start_review", "start_for_outbox", "set_sinks"],
    );
    assert_eq!(
        calls
            .iter()
            .map(|(_, callee)| callee.as_str())
            .collect::<Vec<_>>(),
        vec!["start_review", "start_for_outbox", "set_sinks"],
        "each load-bearing bypass mutation must be machine-detected"
    );
}

/// Fixture proof that both directory arms are recursive. A bypass hidden one level below
/// `remote/**` or `remoteConsole/**` must still produce a violation; comments with the same text do
/// not count as executable code.
#[test]
fn external_review_adapter_fixture_scan_is_recursive() {
    struct FixtureDir(std::path::PathBuf);
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock after epoch")
        .as_nanos();
    let fixture = FixtureDir(std::env::temp_dir().join(format!(
        "prmonitor-external-review-boundary-{}-{unique}",
        std::process::id()
    )));
    let rust_nested = fixture.0.join("remote/nested");
    let frontend_nested = fixture.0.join("remoteConsole/nested");
    fs::create_dir_all(&rust_nested).expect("create nested Rust fixture");
    fs::create_dir_all(&frontend_nested).expect("create nested frontend fixture");
    fs::write(
        rust_nested.join("bypass.rs"),
        "fn adapter() { crate::review::commands::start_review(app); }",
    )
    .expect("write Rust bypass fixture");
    fs::write(
        frontend_nested.join("bypass.ts"),
        "// transport.request(\"start_review\");\ntransport.request(\"start_review\");",
    )
    .expect("write frontend bypass fixture");

    let mut violations = Vec::new();
    let mut rust_files = 0usize;
    scan_rust_dir_for_callees(
        &fixture.0.join("remote"),
        &["start_review", "start_for_outbox"],
        &mut violations,
        &mut rust_files,
    );
    let mut frontend_files = 0usize;
    scan_frontend_dir_for_command(
        &fixture.0.join("remoteConsole"),
        "start_review",
        &mut violations,
        &mut frontend_files,
    );

    assert_eq!(rust_files, 1, "nested Rust fixture was visited");
    assert_eq!(frontend_files, 1, "nested frontend fixture was visited");
    assert_eq!(
        violations.len(),
        2,
        "the Rust call and uncommented frontend command are both rejected: {violations:?}"
    );
    assert!(violations.iter().any(|v| v.contains("calls start_review")));
    assert!(violations
        .iter()
        .any(|v| v.contains("sends forbidden frontend command start_review")));
}

#[test]
fn local_api_does_not_expose_duplicate_tauri_receipt_commands() {
    let source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/review/local_api.rs"))
            .expect("read local API source");
    assert!(!source.contains("pub fn request_review<"));
    assert!(!source.contains("pub fn review_receipt<"));
}
