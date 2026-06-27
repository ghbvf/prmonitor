//! Dispatch helper functions used by rule/outbox producers.

use crate::model::Candidate;

pub(crate) fn review_action_dedupe_key(cand: &Candidate) -> String {
    crate::pr::ledger::dispatch_key(cand.number, &cand.head_sha, &cand.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(number: u64, kind: &str) -> Candidate {
        Candidate {
            number,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    #[test]
    fn review_action_dedupe_key_follows_candidate_kind() {
        let review = cand(1, "review");
        assert_eq!(review_action_dedupe_key(&review), "1@sha:review");

        let check = cand(2, "check");
        assert_eq!(review_action_dedupe_key(&check), "2@sha:check");
    }

    #[test]
    fn app_code_uses_no_gh_write_subcommands() {
        let forbidden: Vec<String> = vec![
            format!("pr ed{}", "it"),
            format!("pr com{}", "ment"),
            format!("pr rev{}", "iew"),
            format!("issue com{}", "ment"),
            format!("issue ed{}", "it"),
            format!("--add-l{}", "abel"),
            format!("--remove-l{}", "abel"),
        ];
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

        let mut offenders = Vec::new();
        for path in rs_files_under(std::path::Path::new(src_root)) {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            for pat in &forbidden {
                if src.contains(pat.as_str()) {
                    offenders.push(format!(
                        "{} contains gh write pattern {:?}",
                        path.display(),
                        pat
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "app-side gh WRITE detected (only the pr-review skill may write to GitHub):\n{}",
            offenders.join("\n")
        );
    }

    fn rs_files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                out.extend(rs_files_under(&path));
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(path);
            }
        }
        out
    }
}
