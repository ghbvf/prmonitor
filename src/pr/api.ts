// PR slice → backend adapter. Wraps the PR-discovery commands.
import { invoke } from "../api";
import type { PullRequestView } from "../types";
import type { GhStatus } from "./types";

export function fetchPrsNow(): Promise<PullRequestView[]> {
  return invoke<PullRequestView[]>("fetch_prs_now");
}

export function ghStatus(): Promise<GhStatus> {
  return invoke<GhStatus>("gh_status");
}
