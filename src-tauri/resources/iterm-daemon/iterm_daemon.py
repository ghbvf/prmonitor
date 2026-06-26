#!/usr/bin/env python3
"""prmonitor iTerm daemon (#1383).

A long-resident bridge between prmonitor (Rust) and the iTerm2 Python API, spoken as
newline-delimited JSON-RPC over stdio (one JSON object per line, no ``jsonrpc`` field —
the same MCP-style framing the codex app-server uses).

Contract (must stay in lock-step with ``src-tauri/src/terminal/protocol.rs``):

  request  : {"id": N, "method": "...", "params": {...}}
  response : {"id": N, "result": ...}  |  {"id": N, "error": {"code": C, "message": "..."}}
  notify   : {"method": "...", "params": {...}}            (no id; daemon -> client only)

Methods   : initialize, listSessions, createSession, sendText, subscribe, unsubscribe, resize
Notifies  : screenUpdate, sessionEnded, error

The iTerm connection is opened INSIDE ``initialize`` (never at import / startup) so a missing
``iterm2`` pip, iTerm not running, or an unauthorized Python API surfaces as a structured
JSON-RPC error with an actionable Chinese message — never a silent EOF. stdout carries ONLY
protocol frames; all human diagnostics go to stderr.
"""

import asyncio
import json
import sys
import traceback


def log(message: str) -> None:
    """Human diagnostics → stderr (stdout is reserved for protocol frames)."""
    print(message, file=sys.stderr, flush=True)


class DaemonError(Exception):
    """A handler failure that maps to a JSON-RPC error response (code + message)."""

    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class Daemon:
    def __init__(self) -> None:
        self.iterm2 = None
        self.connection = None
        self.app = None
        self.write_lock = None  # created inside run() (bound to the running loop)
        self.streamers = {}  # session_id -> asyncio.Task

    # ---- transport ----

    async def write_message(self, obj: dict) -> None:
        line = json.dumps(obj, ensure_ascii=False)
        async with self.write_lock:
            sys.stdout.write(line + "\n")
            sys.stdout.flush()

    async def send_result(self, request_id, result) -> None:
        await self.write_message({"id": request_id, "result": result})

    async def send_error(self, request_id, code: int, message: str) -> None:
        await self.write_message(
            {"id": request_id, "error": {"code": code, "message": message}}
        )

    async def send_notification(self, method: str, params: dict) -> None:
        await self.write_message({"method": method, "params": params})

    # ---- helpers ----

    def session_to_dict(self, session, window_id: str, tab_id: str, active_id) -> dict:
        title = ""
        try:
            title = session.name or ""
        except Exception:
            title = ""
        cols, rows = 0, 0
        try:
            grid = session.grid_size
            cols, rows = grid.width, grid.height
        except Exception:
            pass
        return {
            "sessionId": session.session_id,
            "windowId": window_id,
            "tabId": tab_id,
            "title": title,
            "isActive": active_id is not None and session.session_id == active_id,
            "rows": rows,
            "cols": cols,
        }

    def active_session_id(self):
        try:
            window = self.app.current_terminal_window
            if window is None:
                return None
            tab = window.current_tab
            if tab is None:
                return None
            session = tab.current_session
            return session.session_id if session is not None else None
        except Exception:
            return None

    def require_session(self, session_id: str):
        session = self.app.get_session_by_id(session_id)
        if session is None:
            raise DaemonError(-32012, f"未找到会话：{session_id}")
        return session

    # ---- request handlers ----

    async def handle_initialize(self, params: dict) -> dict:
        # Connect to iTerm HERE so every precondition failure is a structured error.
        try:
            import iterm2
        except ImportError as exc:
            raise DaemonError(
                -32001,
                "缺少 iterm2 Python 库：请在 iTerm 使用的 python3 上运行 `pip install iterm2`。",
            ) from exc
        try:
            self.iterm2 = iterm2
            self.connection = await iterm2.Connection.async_create()
            self.app = await iterm2.async_get_app(self.connection)
        except Exception as exc:
            raise DaemonError(
                -32002,
                "无法连接 iTerm Python API：请确认 iTerm 正在运行、已在「偏好设置 → General → "
                "Magic → Enable Python API」开启 API，并已在首次弹窗授权本脚本。"
                f"详情：{exc}",
            ) from exc
        version = ""
        try:
            version = str(getattr(iterm2, "__version__", "") or "")
        except Exception:
            version = ""
        return {"itermVersion": version}

    async def handle_list_sessions(self, params: dict) -> list:
        active_id = self.active_session_id()
        result = []
        for window in self.app.windows:
            for tab in window.tabs:
                for session in tab.sessions:
                    result.append(
                        self.session_to_dict(
                            session, window.window_id, tab.tab_id, active_id
                        )
                    )
        return result

    async def handle_create_session(self, params: dict) -> dict:
        iterm2 = self.iterm2
        profile = params.get("profile")
        window_id = params.get("windowId")
        if window_id:
            window = self.app.get_window_by_id(window_id)
            if window is None:
                raise DaemonError(-32010, f"未找到窗口：{window_id}")
            tab = await window.async_create_tab(profile=profile)
        else:
            window = await iterm2.Window.async_create(self.connection, profile=profile)
            if window is None:
                raise DaemonError(-32011, "创建 iTerm 窗口失败")
            tab = window.current_tab
        session = tab.current_session
        # A freshly created session is the active one.
        return self.session_to_dict(
            session, window.window_id, tab.tab_id, session.session_id
        )

    async def handle_send_text(self, params: dict) -> dict:
        session = self.require_session(params["sessionId"])
        await session.async_send_text(params.get("text", ""))
        return {}

    async def handle_resize(self, params: dict) -> dict:
        session = self.require_session(params["sessionId"])
        cols = int(params.get("cols", 0))
        rows = int(params.get("rows", 0))
        if cols <= 0 or rows <= 0:
            raise DaemonError(-32013, f"非法网格尺寸：cols={cols} rows={rows}")
        size = self.iterm2.Size(cols, rows)
        await session.async_set_grid_size(size)
        return {}

    async def handle_subscribe(self, params: dict) -> dict:
        session_id = params["sessionId"]
        session = self.require_session(session_id)
        # Idempotent: cancel any prior streamer before starting a fresh one.
        await self.stop_streamer(session_id)
        cols, rows = 0, 0
        try:
            grid = session.grid_size
            cols, rows = grid.width, grid.height
        except Exception:
            pass
        task = asyncio.ensure_future(self.stream_session(session_id))
        self.streamers[session_id] = task
        return {"cols": cols, "rows": rows}

    async def handle_unsubscribe(self, params: dict) -> dict:
        await self.stop_streamer(params["sessionId"])
        return {}

    # ---- screen streaming ----

    async def stream_session(self, session_id: str) -> None:
        session = self.app.get_session_by_id(session_id)
        if session is None:
            await self.send_notification(
                "sessionEnded", {"sessionId": session_id, "reason": "会话不存在"}
            )
            return
        try:
            async with session.get_screen_streamer() as streamer:
                while True:
                    contents = await streamer.async_get()
                    if contents is None:
                        break
                    await self.send_notification(
                        "screenUpdate", self.contents_to_update(session_id, contents)
                    )
        except asyncio.CancelledError:
            # Unsubscribe / shutdown cancelled us — no sessionEnded (the session lives on).
            self.streamers.pop(session_id, None)
            raise
        except Exception as exc:
            await self.send_notification(
                "error", {"sessionId": session_id, "message": f"屏幕流错误：{exc}"}
            )
        self.streamers.pop(session_id, None)
        await self.send_notification(
            "sessionEnded", {"sessionId": session_id, "reason": "stream ended"}
        )

    def contents_to_update(self, session_id: str, contents) -> dict:
        lines = []
        try:
            count = contents.number_of_lines
        except Exception:
            count = 0
        for i in range(count):
            try:
                lines.append(contents.line(i).string)
            except Exception:
                lines.append("")
        text = "\n".join(lines)

        cursor_row, cursor_col = None, None
        try:
            coord = contents.cursor_coord
            if coord is not None:
                cursor_col = coord.x
                cursor_row = coord.y
        except Exception:
            pass

        cols, rows = 0, count
        session = self.app.get_session_by_id(session_id)
        if session is not None:
            try:
                grid = session.grid_size
                cols, rows = grid.width, grid.height
            except Exception:
                pass

        update = {
            "sessionId": session_id,
            "cols": cols,
            "rows": rows,
            "contents": text,
        }
        if cursor_row is not None:
            update["cursorRow"] = cursor_row
        if cursor_col is not None:
            update["cursorCol"] = cursor_col
        return update

    async def stop_streamer(self, session_id: str) -> None:
        task = self.streamers.pop(session_id, None)
        if task is not None and not task.done():
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            except Exception:
                pass

    # ---- dispatch + loop ----

    HANDLERS = {
        "initialize": "handle_initialize",
        "listSessions": "handle_list_sessions",
        "createSession": "handle_create_session",
        "sendText": "handle_send_text",
        "subscribe": "handle_subscribe",
        "unsubscribe": "handle_unsubscribe",
        "resize": "handle_resize",
    }

    async def dispatch(self, request: dict) -> None:
        request_id = request.get("id")
        method = request.get("method")
        params = request.get("params") or {}

        if method != "initialize" and self.connection is None:
            await self.send_error(
                request_id, -32003, "daemon 尚未初始化（请先发送 initialize）"
            )
            return

        handler_name = self.HANDLERS.get(method)
        if handler_name is None:
            await self.send_error(request_id, -32601, f"未知方法：{method}")
            return

        handler = getattr(self, handler_name)
        try:
            result = await handler(params)
            await self.send_result(request_id, result)
        except DaemonError as exc:
            await self.send_error(request_id, exc.code, exc.message)
        except Exception as exc:
            log("handler 异常:\n" + traceback.format_exc())
            await self.send_error(request_id, -32000, f"{method} 执行失败：{exc}")

    async def run(self) -> None:
        self.write_lock = asyncio.Lock()
        loop = asyncio.get_running_loop()
        log("iTerm daemon 已启动，等待 initialize……")
        while True:
            # Read stdin on the default executor so the event loop keeps driving the
            # background screen-streamer tasks while we block on a line.
            line = await loop.run_in_executor(None, sys.stdin.readline)
            if line == "":  # EOF: the parent (prmonitor) closed our stdin.
                break
            line = line.strip()
            if not line:
                continue
            try:
                request = json.loads(line)
            except json.JSONDecodeError:
                # Length-truncated summary so an oversized malformed line can't flood the log;
                # don't rely on the Rust 512-byte stderr cap as the sole bound.
                log(f"无法解析请求行（已跳过）：{line[:80]!r}…")
                continue
            await self.dispatch(request)
        log("iTerm daemon 收到 EOF，退出。")


def main() -> None:
    daemon = Daemon()
    try:
        asyncio.run(daemon.run())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
