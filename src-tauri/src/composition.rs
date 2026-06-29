//! Composition-root-only wiring helpers.
//!
//! This module owns cross-slice fan-out that must stay behind [`crate::state::AppState`] seams.

use std::sync::Arc;

use tauri::Runtime;

use crate::state::AppState;

mod review_lifecycle;

pub(super) fn install_review_lifecycle_sink<R: Runtime>(
    state: &AppState,
    app: tauri::AppHandle<R>,
) {
    state.review_lifecycle.set_sink(Arc::new(move |event| {
        review_lifecycle::enqueue(&app, &crate::MessagingActionsImpl, event)
    }));
}
