use std::path::Path;

fn main() {
    ensure_web_dist_placeholder();
    tauri_build::build()
}

// #1504: `remote::terminal_http` embeds `../dist-web` via rust-embed, whose compile-time `#[folder]`
// requires the directory to exist. On a fresh checkout / bare `cargo test` no web build has run yet,
// so write a minimal placeholder `index.html`. CI/release runs `pnpm build:web` BEFORE cargo to
// overwrite this with the real SPA (release embeds those bytes; debug reads `dist-web/` from disk).
// `dist-web` stays gitignored — no committed build artifacts.
fn ensure_web_dist_placeholder() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist-web");
    println!("cargo:rerun-if-changed={}", dist.display());
    // Per-file rerun: a dir-level `rerun-if-changed` only fires on add/remove, not on edits to existing
    // files, so an incremental release build would keep stale embedded bytes after `pnpm build:web`
    // rewrote an asset in place. Emit each file so content edits re-trigger the rust-embed embed.
    emit_rerun_for_files(&dist);
    let index = dist.join("index.html");
    if index.exists() {
        return;
    }
    // Hard-fail (not a soft warning): if we cannot create the placeholder, the rust-embed `#[folder]`
    // proc-macro will fail next with a far less obvious error, so surface the real cause here.
    std::fs::create_dir_all(&dist).unwrap_or_else(|e| {
        panic!(
            "#1504: failed to create dist-web placeholder dir {}: {e}",
            dist.display()
        )
    });
    let placeholder = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>prmonitor</title></head><body>\
<p>Web bundle not built. Run <code>pnpm build:web</code>.</p></body></html>\n";
    std::fs::write(&index, placeholder).unwrap_or_else(|e| {
        panic!(
            "#1504: failed to write dist-web placeholder {}: {e}",
            index.display()
        )
    });
}

fn emit_rerun_for_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            emit_rerun_for_files(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
