//! Composition-root-only wiring helpers.
//!
//! This module owns cross-slice fan-out that must stay behind [`crate::state::AppState`] seams.

use std::sync::Arc;

use tauri::Runtime;

use crate::state::AppState;

mod cli_probe;
mod review_lifecycle;

pub(super) use cli_probe::{probe_cli_tools, ActiveCliFingerprints};

pub(super) fn install_review_lifecycle_sink<R: Runtime>(
    state: &AppState,
    app: tauri::AppHandle<R>,
) {
    state.review_lifecycle.set_sink(Arc::new(move |event| {
        review_lifecycle::enqueue(&app, &crate::MessagingActionsImpl, event)
    }));
}
