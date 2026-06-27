// Config slice wire types are generated from Rust (`src-tauri/src/config/model.rs` and
// `src-tauri/src/remote/status.rs`). This facade intentionally does not carry legacy
// Remote Access listener/tunnel aliases: `remoteAccess` is the single wire source.
export * from "./types.generated";
