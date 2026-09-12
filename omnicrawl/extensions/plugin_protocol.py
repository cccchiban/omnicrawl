"""Python 侧 Plugin Worker 协议客户端。

stdin/stdout 使用 NDJSON 承载 JSON-RPC 2.0。stdout 只能是协议消息；
插件日志与 console 输出应走 stderr，Host 只做收集，不当作协议输入。
"""

from __future__ import annotations

import json
import os
import queue
import shutil
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Callable, Mapping

from .plugin_models import (
    DEFAULT_MAX_MESSAGE_BYTES,
    HOOK_API_VERSION,
    OMNICRAWL_VERSION,
    PluginProtocolError,
    parse_hook_result,
)


NODE_RUNNER_FILENAME = "node_runner.mjs"


def node_runner_path() -> Path:
    return Path(__file__).resolve().parent / NODE_RUNNER_FILENAME


def resolve_node_executable() -> str:
    path = shutil.which("node")
    if not path:
        raise PluginProtocolError("未找到 Node.js，请安装 Node.js 20+ 并确保 node 在 PATH 中。")
    return path


def build_worker_env(base: Mapping[str, str] | None = None) -> dict[str, str]:
    """构造 Worker 环境变量白名单，避免把 API Key / Token 等敏感变量传给插件。"""

    source = dict(base or os.environ)
    blocked_keys = {
        "OPENAI_API_KEY",
        "API_KEY",
        "ANTHROPIC_API_KEY",
        "DEEPSEEK_API_KEY",
        "AI_CONFIG_FILE",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_ACCESS_KEY_ID",
        "AZURE_OPENAI_API_KEY",
        "GITHUB_TOKEN",
        "NPM_TOKEN",
        "NODE_AUTH_TOKEN",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    }
    blocked_substrings = ("API_KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "COOKIE")
    env: dict[str, str] = {}
    for key, value in source.items():
        upper = key.upper()
        if key in blocked_keys or upper in blocked_keys:
            continue
        if any(part in upper for part in blocked_substrings):
            continue
        env[key] = value
    # 明确提供最小运行信息，不包含凭据。
    env["OMNICRAWL_PLUGIN_WORKER"] = "1"
    env["OMNICRAWL_VERSION"] = OMNICRAWL_VERSION
    env["OMNICRAWL_HOOK_API_VERSION"] = HOOK_API_VERSION
    return env


class PluginWorkerClient:
    """单个插件的常驻 Node Worker 客户端。"""

    def __init__(
        self,
        *,
        plugin_root: Path,
        plugin_name: str,
        timeout_ms: int = 2000,
        max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES,
        node_executable: str | None = None,
        runner_path: Path | None = None,
        env: Mapping[str, str] | None = None,
        on_stderr: Callable[[str], None] | None = None,
        on_host_request: Callable[[str, Mapping[str, Any]], Mapping[str, Any]] | None = None,
    ) -> None:
        self.plugin_root = Path(plugin_root).resolve()
        self.plugin_name = plugin_name
        self.timeout_ms = timeout_ms
        self.max_message_bytes = max_message_bytes
        self.node_executable = node_executable or resolve_node_executable()
        self.runner_path = Path(runner_path or node_runner_path()).resolve()
        self._env = build_worker_env(env)
        self._on_stderr = on_stderr
        # Worker → Host 请求回调（如 custom.emit）；返回 result 对象，抛异常则回 error。
        self._on_host_request = on_host_request
        self._process: subprocess.Popen[str] | None = None
        self._next_id = 1
        self._lock = threading.RLock()
        self._pending: dict[int, queue.Queue[dict[str, Any] | BaseException]] = {}
        self._reader_thread: threading.Thread | None = None
        self._stderr_thread: threading.Thread | None = None
        self._closed = False
        self._stdout_noise = 0

    @property
    def alive(self) -> bool:
        process = self._process
        return process is not None and process.poll() is None and not self._closed

    def start(self) -> None:
        if self.alive:
            return
        if not self.runner_path.is_file():
            raise PluginProtocolError(f"缺少 Node runner：{self.runner_path}")
        if not self.plugin_root.is_dir():
            raise PluginProtocolError(f"插件根目录不存在：{self.plugin_root}")

        creationflags = 0
        if os.name == "nt" and hasattr(subprocess, "CREATE_NO_WINDOW"):
            creationflags = subprocess.CREATE_NO_WINDOW

        command = [
            self.node_executable,
            str(self.runner_path),
            "--plugin-root",
            str(self.plugin_root),
        ]
        try:
            self._process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
                env=self._env,
                cwd=str(self.plugin_root),
                creationflags=creationflags,
            )
        except OSError as exc:
            raise PluginProtocolError(f"启动插件 Worker 失败（{self.plugin_name}）：{exc}") from exc

        self._closed = False
        self._reader_thread = threading.Thread(
            target=self._read_stdout_loop,
            name=f"plugin-stdout-{self.plugin_name}",
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=self._read_stderr_loop,
            name=f"plugin-stderr-{self.plugin_name}",
            daemon=True,
        )
        self._reader_thread.start()
        self._stderr_thread.start()

    def initialize(self, params: Mapping[str, Any], *, timeout_ms: int | None = None) -> dict[str, Any]:
        result = self.request("initialize", dict(params), timeout_ms=timeout_ms)
        if not isinstance(result, dict):
            raise PluginProtocolError(f"initialize 返回值必须是对象：{self.plugin_name}")
        return result

    def invoke_handler(
        self,
        *,
        handler_id: str,
        event: Mapping[str, Any],
        timeout_ms: int,
    ) -> dict[str, Any]:
        result = self.request(
            "hook.invoke",
            {"handlerId": handler_id, "event": dict(event)},
            timeout_ms=timeout_ms,
        )
        if result is None:
            return {"action": "continue"}
        if not isinstance(result, dict):
            raise PluginProtocolError(f"hook.invoke 返回值必须是对象：{self.plugin_name}/{handler_id}")
        return result

    def cancel(self, request_id: int) -> None:
        try:
            self.notify("hook.cancel", {"requestId": request_id})
        except PluginProtocolError:
            # 取消失败时由上层超时终止进程。
            pass

    def ping(self, *, timeout_ms: int = 1000) -> dict[str, Any]:
        result = self.request("ping", {}, timeout_ms=timeout_ms)
        return result if isinstance(result, dict) else {}

    def shutdown(self, *, timeout_ms: int = 2000) -> None:
        if not self.alive:
            self.close(force=True)
            return
        try:
            self.request("shutdown", {}, timeout_ms=timeout_ms)
        except PluginProtocolError:
            pass
        self.close(force=False)

    def close(self, *, force: bool = False) -> None:
        self._closed = True
        process = self._process
        self._process = None
        if process is None:
            self._fail_all_pending(PluginProtocolError(f"插件 Worker 已关闭：{self.plugin_name}"))
            return
        try:
            if process.stdin:
                process.stdin.close()
        except OSError:
            pass
        if process.poll() is None:
            if force:
                process.kill()
            else:
                process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
        for stream_name in ("stdout", "stderr"):
            stream = getattr(process, stream_name, None)
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass
        self._fail_all_pending(PluginProtocolError(f"插件 Worker 已关闭：{self.plugin_name}"))

    def request(
        self,
        method: str,
        params: Mapping[str, Any] | None = None,
        *,
        timeout_ms: int | None = None,
    ) -> Any:
        with self._lock:
            if not self.alive:
                self.start()
            request_id = self._next_id
            self._next_id += 1
            wait_queue: queue.Queue[dict[str, Any] | BaseException] = queue.Queue(maxsize=1)
            self._pending[request_id] = wait_queue
            message = {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": dict(params or {}),
            }
            self._send(message)

        deadline = time.monotonic() + max((timeout_ms or self.timeout_ms), 50) / 1000.0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self.cancel(request_id)
                # 宽限期后再强制回收。
                time.sleep(0.05)
                if request_id in self._pending:
                    self.close(force=True)
                raise PluginProtocolError(
                    f"插件调用超时：{self.plugin_name} {method} > {timeout_ms or self.timeout_ms}ms"
                )
            try:
                payload = wait_queue.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                if not self.alive:
                    raise PluginProtocolError(f"插件 Worker 已退出：{self.plugin_name}")
                continue
            with self._lock:
                self._pending.pop(request_id, None)
            if isinstance(payload, BaseException):
                raise payload
            if "error" in payload:
                error = payload.get("error")
                if isinstance(error, Mapping):
                    message_text = str(error.get("message") or error)
                else:
                    message_text = str(error)
                raise PluginProtocolError(f"插件调用失败：{self.plugin_name} {method}：{message_text}")
            return payload.get("result")

    def notify(self, method: str, params: Mapping[str, Any] | None = None) -> None:
        with self._lock:
            if not self.alive:
                self.start()
            self._send(
                {
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": dict(params or {}),
                }
            )

    def _send(self, message: Mapping[str, Any]) -> None:
        process = self._process
        if process is None or process.stdin is None:
            raise PluginProtocolError(f"插件 Worker stdin 不可用：{self.plugin_name}")
        line = json.dumps(message, ensure_ascii=False, separators=(",", ":"))
        encoded = line.encode("utf-8")
        if len(encoded) > self.max_message_bytes:
            raise PluginProtocolError(
                f"协议消息超过上限 {self.max_message_bytes} 字节：{self.plugin_name}"
            )
        try:
            process.stdin.write(line + "\n")
            process.stdin.flush()
        except OSError as exc:
            raise PluginProtocolError(f"写入插件 Worker 失败：{self.plugin_name}：{exc}") from exc

    def _read_stdout_loop(self) -> None:
        process = self._process
        if process is None or process.stdout is None:
            return
        try:
            for raw_line in process.stdout:
                if self._closed:
                    break
                line = raw_line.strip()
                if not line:
                    continue
                if len(line.encode("utf-8")) > self.max_message_bytes:
                    self._fail_all_pending(
                        PluginProtocolError(f"插件 stdout 消息过大：{self.plugin_name}")
                    )
                    self.close(force=True)
                    return
                try:
                    payload = json.loads(line)
                except json.JSONDecodeError:
                    self._stdout_noise += 1
                    self._fail_all_pending(
                        PluginProtocolError(f"插件 stdout 非 JSON 协议消息：{self.plugin_name}")
                    )
                    self.close(force=True)
                    return
                if not isinstance(payload, dict):
                    self._fail_all_pending(
                        PluginProtocolError(f"插件协议消息必须是对象：{self.plugin_name}")
                    )
                    self.close(force=True)
                    return
                self._dispatch_incoming(payload)
        except Exception as exc:  # noqa: BLE001 - 读线程需把异常回传给等待方
            self._fail_all_pending(PluginProtocolError(f"读取插件 stdout 失败：{exc}"))
        finally:
            if not self._closed:
                self._fail_all_pending(PluginProtocolError(f"插件 Worker stdout 已关闭：{self.plugin_name}"))

    def _read_stderr_loop(self) -> None:
        process = self._process
        if process is None or process.stderr is None:
            return
        try:
            for raw_line in process.stderr:
                text = raw_line.rstrip("\n")
                if not text:
                    continue
                if self._on_stderr is not None:
                    try:
                        self._on_stderr(text)
                    except Exception:  # noqa: BLE001 - stderr 回调失败不中断 Worker 输出读取
                        pass
        except Exception:
            return

    def _dispatch_incoming(self, payload: dict[str, Any]) -> None:
        # Worker → Host 的请求（如 custom.emit）。
        if "method" in payload and "id" in payload and "result" not in payload and "error" not in payload:
            method = str(payload.get("method", ""))
            request_id = payload.get("id")
            if not isinstance(request_id, int):
                return
            params = payload.get("params") or {}
            if not isinstance(params, Mapping):
                params = {}
            if self._on_host_request is None:
                self._send_response(
                    request_id,
                    error={"code": -32601, "message": f"Host method not handled: {method}"},
                )
                return
            try:
                result = self._on_host_request(method, params)
                self._send_response(request_id, result=dict(result or {"ok": True}))
            except Exception as exc:  # noqa: BLE001 - 回传给 Worker
                self._send_response(
                    request_id,
                    error={"code": -32000, "message": str(exc)},
                )
            return

        if "id" not in payload:
            return
        request_id = payload.get("id")
        if not isinstance(request_id, int):
            self._fail_all_pending(PluginProtocolError(f"未知 request id：{self.plugin_name}"))
            self.close(force=True)
            return
        with self._lock:
            wait_queue = self._pending.get(request_id)
        if wait_queue is None:
            # 重复响应或未知 ID：协议失败。
            self._fail_all_pending(
                PluginProtocolError(f"收到未知/重复 request id={request_id}：{self.plugin_name}")
            )
            self.close(force=True)
            return
        wait_queue.put(payload)

    def _send_response(
        self,
        request_id: int,
        *,
        result: Any = None,
        error: Mapping[str, Any] | None = None,
    ) -> None:
        message: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id}
        if error is not None:
            message["error"] = dict(error)
        else:
            message["result"] = result
        try:
            self._send(message)
        except PluginProtocolError:
            pass

    def _fail_all_pending(self, error: BaseException) -> None:
        with self._lock:
            pending = list(self._pending.items())
            self._pending.clear()
        for _request_id, wait_queue in pending:
            try:
                wait_queue.put_nowait(error)
            except queue.Full:
                pass


def invoke_and_parse(
    client: PluginWorkerClient,
    *,
    handler_key: str,
    handler_id: str,
    event: Mapping[str, Any],
    timeout_ms: int,
) -> Any:
    """调用 Handler 并解析为 HookResult 原始 dict，供 manager 再校验。"""

    started = time.perf_counter()
    raw = client.invoke_handler(handler_id=handler_id, event=event, timeout_ms=timeout_ms)
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    return parse_hook_result(raw, handler_key=handler_key, elapsed_ms=elapsed_ms)
