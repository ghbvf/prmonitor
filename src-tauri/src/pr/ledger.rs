//! Dispatch de-duplication ledger + dispatch-events log.
//!
//! Implemented in PR3/PR4: persists `{number}@{headSha}:{kind}` keys and
//! per-dispatch events so a restart never re-dispatches the same review and
//! cooldown can be enforced (mirrors `router.py`).
