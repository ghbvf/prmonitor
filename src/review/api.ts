// Review slice → backend adapter. Wraps the codex availability command.
import { invoke } from "../api";
import type { CodexStatus } from "./types";

export function getCodexStatus(): Promise<CodexStatus> {
  return invoke<CodexStatus>("get_codex_status");
}
