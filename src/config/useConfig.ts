// Config slice store/composable. PR2 expands this (save + validation + pinia).
import { ref } from "vue";
import { getConfig } from "../api";
import type { AppConfig } from "../types";

export function useConfig() {
  const config = ref<AppConfig | null>(null);

  async function load() {
    config.value = await getConfig();
  }

  return { config, load };
}
