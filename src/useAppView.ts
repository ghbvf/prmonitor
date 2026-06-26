// Composition-root view state (#34): which top-level view is showing. Lives at the
// composition layer (a `src/` root file, NOT inside a slice) — App.vue and the slice
// views switch on it without prop-drilling. A module-level singleton `ref` (mirroring
// the review slice's useReviewStore pattern) is the lightest fit for one enum + nav
// actions; vue-router would add a dependency for three URL-less desktop views.
import { ref } from "vue";

export type AppView =
  | "monitor"
  | "settings"
  | "onboarding"
  | "inbox"
  | "outbox"
  | "terminal";

const currentView = ref<AppView>("monitor");

export function useAppView() {
  return {
    currentView,
    goMonitor: () => (currentView.value = "monitor"),
    goSettings: () => (currentView.value = "settings"),
    goOnboarding: () => (currentView.value = "onboarding"),
    goInbox: () => (currentView.value = "inbox"),
    goOutbox: () => (currentView.value = "outbox"),
    goTerminal: () => (currentView.value = "terminal"),
  };
}
