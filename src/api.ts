// The only module that imports `@tauri-apps/api`. Slices import the raw
// `invoke` / `listen` primitives from here and wrap them in their own api.ts.
export { invoke } from "@tauri-apps/api/core";
export { listen } from "@tauri-apps/api/event";
