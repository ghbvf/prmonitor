// Medium `assertNever`穷尽 carrier for `ListenerState` (AB#1225 PR1, ai-robust.md §载体决策).
// Lives in its own module so the vitest contract test can import it directly without
// mounting the Vue component (mirrors the src/types.ts pattern for eventTypeLabel /
// inboxStatusLabel).
//
// Adding a new value to LISTENER_STATES is a compile error here unless a matching
// `case` is added — the `default: assertNever(s)` makes the missing arm inexpressible
// at the type level (**Hard** for the label side; **Medium** for the overall dispatch
// since callers can still skip calling this fn, but CI tests below catch that).
//
// Labels are intentionally user-actionable and ticket-free (F7): the backend `message`
// field carries the technical detail; the label must stay clean for end users.
import { assertNever } from "../types";
import type { ListenerKind, ListenerState } from "./types";

export function listenerStateLabel(s: ListenerState): string {
  switch (s) {
    case "bound":
      return "已绑定";
    case "bound-no-auth":
      return "已绑定但未鉴权（token 未设置，请求将 401）";
    case "blocked-needs-1073":
      return "已阻止（绑定地址须为 127.0.0.1/localhost）";
    case "unsupported":
      return "暂不支持（远程访问功能开发中）";
    case "error":
      return "错误";
    default:
      return assertNever(s);
  }
}

// Display label for a listener kind (F15). Mirrors the `optionLabels` map in
// LISTENER_GROUPS (fields.ts) so the runtime status badge stays in sync with the
// editor's select labels. Exhaustive over `ListenerKind` via `assertNever` (same
// Medium穷尽 carrier pattern as listenerStateLabel).
export function listenerKindLabel(kind: ListenerKind): string {
  switch (kind) {
    case "local-api":
      return "本地 API (CLI)";
    case "remote-web":
      return "远程面板";
    case "event-ingress":
      return "事件入站";
    case "terminal":
      return "远程终端";
    default:
      return assertNever(kind);
  }
}
