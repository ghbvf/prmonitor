//! JSON-RPC plumbing: a dedicated reader task that demuxes responses (`id` →
//! oneshot) from notifications (broadcast to the session manager), plus a serial
//! writer. This is what the existing `router.py` lacks — it discards
//! notifications; we forward them so the UI can stream. PR5.
