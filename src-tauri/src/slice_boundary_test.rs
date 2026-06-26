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
const SLICES: [&str; 6] = ["config", "pr", "review", "inbox", "outbox", "terminal"];

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
                if target == owner {
                    continue;
                }
                let needle = format!("crate::{target}::");
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
            }
        }
    }
}
