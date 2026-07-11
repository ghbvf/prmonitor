/// <reference types="vite/client" />

declare module "*.vue" {
  import type { DefineComponent } from "vue";
  const component: DefineComponent<{}, {}, any>;
  export default component;
}

interface Window {
  __PRMONITOR_REMOTE_BASE_PATH__?: string;
  __PRMONITOR_API_BASE_PATH__?: string;
}
