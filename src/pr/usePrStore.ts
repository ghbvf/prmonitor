// PR slice store. PR3/PR4 populate this from `fetch_prs_now` + `prs:updated`.
import { ref } from "vue";
import type { PullRequestView } from "../types";

export function usePrStore() {
  const prs = ref<PullRequestView[]>([]);
  return { prs };
}
