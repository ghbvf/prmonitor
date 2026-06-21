// Slice-private formatter for a review session's start time. `createdAtEpoch` is
// wall-clock EPOCH SECONDS (mirrors `SessionInfo.created_at_epoch`), so scale to ms
// for `Date`. A non-positive stamp is the durable fallback the backend writes via
// `now_epoch().unwrap_or(0)` / a legacy row predating the column — render nothing
// rather than a bogus 1970 date. `createdAtEpoch` is typed `number` (signed) on the
// wire, so guard `< 0` too even though the Rust source is `u64`.
export function formatStartedAt(epoch: number): string {
  if (!epoch || epoch < 0) return "";
  return new Date(epoch * 1000).toLocaleString();
}
