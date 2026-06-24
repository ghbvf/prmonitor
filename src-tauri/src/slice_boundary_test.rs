//! Rust slice-boundary enforcement test (Medium, per `.claude/rules/prmonitor/ai-robust.md`).
//!
//! The backend vertical slices (`config` / `pr` / `review` / `inbox` / `outbox`) must be
//! self-contained: a file in one slice must NOT make a runtime VALUE reference (`crate::<sibling>::`)
//! into a SIBLING slice. Cross-slice wiring is the composition root's job (`lib.rs` + the horizontal
//! modules `model` / `error` / `events` / `db` / `state` / `dispatch`, none of which are slices).
//! This is the Rust-side mirror of the frontend `src/slice-boundary.test.ts`; before it, the backend
//! boundary was a Soft convention with no machine guard (AB#1066 F1) — this test makes a sibling-slice
//! value reference a **Medium** machine-detected violation (a failing test), the carrier the charter
//! requires for the review→outbox decoupling.
//!
//! **`config` is allowlisted as a consumable.** `config::service` is the shared settings/project
//! provider that `pr` and `review` legitimately read (pre-existing architecture); a slice may
//! reference `crate::config::`. EVERY OTHER slice→sibling-slice reference is forbidden — e.g.
//! `review` must reach the outbox through the composition-root-injected sink, never `crate::outbox::`.

use std::fs;
use std::path::Path;

/// The backend vertical slices scanned for boundary violations.
const SLICES: [&str; 5] = ["config", "pr", "review", "inbox", "outbox"];

/// Slices a sibling MAY reference. `config` is the shared configuration provider consumed by
/// `pr`/`review`; widening this set is a deliberate architecture decision, not a silent allowance.
const ALLOWED_TARGETS: [&str; 1] = ["config"];

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
        for (i, line) in text.lines().enumerate() {
            // Strip any trailing `//` comment (and skip whole comment lines) before scanning, so a
            // cross-slice mention in a doc/inline comment is NOT a false positive — only real code
            // references count. A `//` inside a string can truncate early, but that only drops a
            // would-be match in a comment, never hides a real code-position `crate::<slice>::`.
            let code = line.split("//").next().unwrap_or(line);
            for target in SLICES {
                if target == owner || ALLOWED_TARGETS.contains(&target) {
                    continue;
                }
                if code.contains(&format!("crate::{target}::")) {
                    violations.push(format!(
                        "{}:{} references sibling slice crate::{}::",
                        path.display(),
                        i + 1,
                        target
                    ));
                }
            }
        }
    }
}
