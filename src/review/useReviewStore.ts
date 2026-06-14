// Review slice store. PR6 fills this from the streamed ReviewEvent channel.
import { ref } from "vue";
import type { ReviewEvent } from "../types";

export function useReviewStore() {
  const events = ref<ReviewEvent[]>([]);
  return { events };
}
