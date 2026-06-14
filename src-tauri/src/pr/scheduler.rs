//! Scheduled-pull loop + manual-pull trigger.
//!
//! Implemented in PR4: a `tokio::time::interval` driving discovery, plus a
//! manual wake (the "立即拉取" button) and live period changes.
