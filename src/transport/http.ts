// HttpTransport: the browser adapter. request→POST `<baseUrl>/invoke/<command>`,
// subscribe→fetch-based SSE `<baseUrl>/events?topic=<event>`, openExternal→window.open.
// Fetch is used instead of native EventSource so remote browser clients can send
// Authorization headers. WebSocket is intentionally omitted: every backend event is
// one-way server→client, which SSE covers.
//
// AB#1063 backend contract this adapter assumes (documented so it is not violated silently):
//  • void commands MAY return 204 / an empty body (handled below), otherwise a JSON body.
//  • the SSE `/events` stream emits JSON `data:` frames, one payload per message.
import type { RequestOptions, SubscribeOptions, Transport, UnlistenFn } from "./index";

export const REMOTE_BEARER_TOKEN_KEY = "prmonitor.remoteBearerToken";

interface HttpTransportOptions {
  onAuthRejected?: () => boolean | Promise<boolean>;
}

class HttpStatusError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

class SseHandlerError extends Error {
  constructor(readonly cause: unknown) {
    super("SSE handler failed");
  }
}

function remoteBearerToken(): string {
  try {
    return globalThis.sessionStorage?.getItem(REMOTE_BEARER_TOKEN_KEY)?.trim() ?? "";
  } catch {
    return "";
  }
}

function authHeaders(extra?: Record<string, string>): Record<string, string> {
  const token = remoteBearerToken();
  return token.length > 0
    ? { ...extra, authorization: `Bearer ${token}` }
    : { ...(extra ?? {}) };
}

async function responseError(res: Response, context: string): Promise<HttpStatusError> {
  let message = "";
  try {
    const text = await res.text();
    if (text.trim().length > 0) {
      try {
        message = (JSON.parse(text) as { message?: string }).message ?? text;
      } catch {
        message = text;
      }
    }
  } catch {
    // Fall through to the status fallback below.
  }
  return new HttpStatusError(
    `${context}: ${message || `${res.status} ${res.statusText}`}`,
    res.status,
  );
}

function dispatchSseFrame<T>(frame: string, event: string, handler: (payload: T) => void): void {
  const data = frame
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice("data:".length).trimStart())
    .join("\n");
  if (data.length === 0) return;
  let payload: T;
  try {
    payload = JSON.parse(data) as T;
  } catch (err) {
    console.error(`SSE "${event}": dropped malformed frame`, err);
    return;
  }
  try {
    handler(payload);
  } catch (err) {
    throw new SseHandlerError(err);
  }
}

async function pumpSse<T>(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  event: string,
  handler: (payload: T) => void,
): Promise<void> {
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    for (;;) {
      const sep = buffer.search(/\r?\n\r?\n/);
      if (sep === -1) break;
      const frame = buffer.slice(0, sep);
      buffer = buffer.slice(buffer[sep] === "\r" ? sep + 4 : sep + 2);
      dispatchSseFrame(frame, event, handler);
    }
  }
  buffer += decoder.decode();
  if (buffer.trim().length > 0) dispatchSseFrame(buffer, event, handler);
}

export function createHttpTransport(baseUrl: string, options: HttpTransportOptions = {}): Transport {
  async function retryAuth(err: HttpStatusError): Promise<boolean> {
    return (err.status === 401 || err.status === 403) && !!(await options.onAuthRejected?.());
  }

  return {
    async request<T>(
      command: string,
      args?: Record<string, unknown>,
      requestOptions: RequestOptions = {},
    ): Promise<T> {
      // encodeURIComponent the command (symmetry with subscribe's `topic`); guards against
      // a non-URL-safe command name silently malforming the path.
      for (let attempt = 0; attempt < 2; attempt += 1) {
        const res = await fetch(`${baseUrl}/invoke/${encodeURIComponent(command)}`, {
          method: "POST",
          headers: authHeaders({ "content-type": "application/json" }),
          // JSON.stringify drops `undefined` keys — same arg shape Tauri's invoke sees.
          body: JSON.stringify(args ?? {}),
          keepalive: requestOptions.keepalive,
        });
        if (!res.ok) {
          const err = await responseError(res, `request "${command}" failed`);
          if (attempt === 0 && (await retryAuth(err))) continue;
          throw err;
        }
        // void commands return 204 / an empty body — calling res.json() on those throws, so
        // read text first and parse only when there is a body (parity with invoke<void>).
        const text = await res.text();
        return (text ? JSON.parse(text) : undefined) as T;
      }
      throw new Error(`request "${command}" failed`);
    },
    async subscribe<T>(
      event: string,
      handler: (payload: T) => void,
      subscribeOptions: SubscribeOptions = {},
    ): Promise<UnlistenFn> {
      const abort = new AbortController();
      let res: Response;
      for (let attempt = 0; ; attempt += 1) {
        res = await fetch(`${baseUrl}/events?topic=${encodeURIComponent(event)}`, {
          method: "GET",
          headers: authHeaders({ accept: "text/event-stream" }),
          signal: abort.signal,
        });
        if (res.ok) break;
        const err = await responseError(res, `SSE "${event}": failed to open stream`);
        if (attempt === 0 && (await retryAuth(err))) continue;
        abort.abort();
        throw err;
      }
      const reader = res.body?.getReader();
      if (!reader) {
        abort.abort();
        throw new Error(`SSE "${event}": response has no body`);
      }
      const pump = pumpSse(reader, event, handler).then(
        () => {
          if (!abort.signal.aborted) subscribeOptions.onClosed?.(`SSE "${event}": stream closed`);
        },
        (err) => {
          if (abort.signal.aborted) return;
          if (err instanceof SseHandlerError) {
            setTimeout(() => { throw err.cause; }, 0);
            return;
          }
          subscribeOptions.onClosed?.(
            err instanceof Error ? err.message : `SSE "${event}": stream error`,
          );
        },
      );
      await Promise.race([Promise.resolve(), pump]);
      return () => {
        abort.abort();
        void reader.cancel();
      };
    },
    openExternal(url: string): Promise<void> {
      // noreferrer (not only noopener): also strips the Referer header on the outbound nav.
      window.open(url, "_blank", "noopener,noreferrer");
      return Promise.resolve();
    },
  };
}
