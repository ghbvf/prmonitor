// HttpTransport: the browser adapter (AB#1375). request→POST `<baseUrl>/invoke/<command>`,
// subscribe→SSE `<baseUrl>/events?topic=<event>`, openExternal→window.open. This is the
// frontend foundation for AB#1063; the matching backend HTTP+SSE server lands THERE, so
// there is no end-to-end backend to talk to yet (this adapter is unit-tested against
// mocked fetch / EventSource). WebSocket is intentionally omitted: every backend event is
// one-way server→client, which SSE covers — a bidirectional channel is deferred to AB#1063.
//
// AB#1063 backend contract this adapter assumes (documented so it is not violated silently):
//  • void commands MAY return 204 / an empty body (handled below), otherwise a JSON body.
//  • the SSE `/events` stream emits JSON `data:` frames, one payload per message.
import type { Transport, UnlistenFn } from "./index";

export function createHttpTransport(baseUrl: string): Transport {
  return {
    async request<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      // encodeURIComponent the command (symmetry with subscribe's `topic`); guards against
      // a non-URL-safe command name silently malforming the path.
      const res = await fetch(`${baseUrl}/invoke/${encodeURIComponent(command)}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        // JSON.stringify drops `undefined` keys — same arg shape Tauri's invoke sees.
        body: JSON.stringify(args ?? {}),
      });
      if (!res.ok) {
        throw new Error(`request "${command}" failed: ${res.status} ${res.statusText}`);
      }
      // void commands return 204 / an empty body — calling res.json() on those throws, so
      // read text first and parse only when there is a body (parity with invoke<void>).
      const text = await res.text();
      return (text ? JSON.parse(text) : undefined) as T;
    },
    subscribe<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
      // One SSE stream per subscription; the backend routes by the `topic` query param.
      const source = new EventSource(`${baseUrl}/events?topic=${encodeURIComponent(event)}`);
      source.onmessage = (e) => {
        // ONLY a JSON-parse failure is a "malformed frame": parse inside the try, then call the
        // handler OUTSIDE it. A handler that throws (e.g. an `assertNever` fail-fast guard) must
        // NOT be swallowed/misattributed as a bad frame — let it surface so that defense stays
        // visible (an uncaught throw out of the native callback is logged by the browser, and one
        // bad dispatch does not break later SSE messages).
        let payload: T;
        try {
          payload = JSON.parse(e.data) as T;
        } catch (err) {
          console.error(`SSE "${event}": dropped malformed frame`, err);
          return;
        }
        handler(payload);
      };
      // Resolve the subscription only once the SSE stream is OPEN — parity with Tauri `listen()`,
      // whose promise resolves when the listener is REGISTERED. Consumers `await subscribe()` THEN
      // read a snapshot / enable the start button (e.g. `usePrStore.init`, `useReviewStore.init`),
      // so resolving before the server accepted the stream would let a follow-up request beat the
      // subscription and drop the events fired in between (#27 F3). An initial (pre-open) connect
      // error rejects + closes (the caller's await fails instead of proceeding on a dead stream);
      // after open, EventSource auto-reconnects, so a later error is only surfaced, not fatal.
      // TODO(AB#1063): reconnect/backoff policy + propagate post-open errors to the caller.
      return new Promise<UnlistenFn>((resolve, reject) => {
        let opened = false;
        source.onopen = () => {
          opened = true;
          resolve(() => source.close());
        };
        source.onerror = (e) => {
          if (opened) {
            console.error(`SSE "${event}": stream error`, e);
            return;
          }
          source.close();
          reject(new Error(`SSE "${event}": failed to open stream`));
        };
      });
    },
    openExternal(url: string): Promise<void> {
      // noreferrer (not only noopener): also strips the Referer header on the outbound nav.
      window.open(url, "_blank", "noopener,noreferrer");
      return Promise.resolve();
    },
  };
}
