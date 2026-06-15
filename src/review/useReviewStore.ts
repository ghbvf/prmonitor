// Review slice store. PR6 fills `events` from the streamed ReviewEvent channel;
// `codex` tracks the codex CLI availability surfaced in the StatusBar.
//
// Module-level singleton state so every `useReviewStore()` caller shares one
// instance (mirrors the cross-component sharing Pinia gives the pr store): the
// ReviewPanel hydrates `codex` on mount and the App-shell StatusBar reads the
// same ref. A fresh ref per call would silo those two consumers.
import { ref } from "vue";
import type { ReviewEvent } from "../types";
import { getCodexStatus } from "./api";
import type { CodexStatus } from "./types";

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to
// a stringified form for any non-conforming throw.
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

const events = ref<ReviewEvent[]>([]);
const codex = ref<CodexStatus | null>(null);

// Hydrate codex availability. Tolerates a rejected command by surfacing an
// unavailable status rather than throwing.
async function refreshCodexStatus() {
  try {
    codex.value = await getCodexStatus();
  } catch (err) {
    const message = toMessage(err);
    console.error("codex 状态获取失败", err);
    codex.value = { available: false, message: message || "codex 状态获取失败" };
  }
}

export function useReviewStore() {
  return { events, codex, refreshCodexStatus };
}
