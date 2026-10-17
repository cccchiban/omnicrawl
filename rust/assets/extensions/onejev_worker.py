"""OneJev 本地决策 worker：在命名管道（Windows）/ Unix socket 上提供 System One 判定。

为什么不用 `qev serve`：官方那个服务经 uvicorn/FastAPI 暴露 HTTP，本机调用方因此要为
一次纯内存判定多付一层 TCP + ASGI 中转。这个 worker 直接持有 `qev` 的引擎对象，
请求从管道进来、答案从管道出去，中间只有一次 JSON 编解码。

协议（NDJSON，一行一个 JSON 对象）：

    请求   {"model": "...", "state": {...}, "questions": {...}}
    成功   {"model": "...", "answers": {...}, "usage": {...}, "qev": {...}}
    失败   {"error": "可读原因"}

响应形状故意与 HTTP 端点一致，Rust 侧读答案的代码因此不必区分两种传输。

服务是**本机共用**的常驻进程，由 OmniCrawl 宿主以脱离进程的方式拉起（见
`omnicrawl-onejev` 的 `server` 模块）：加载权重、创建端点、打出就绪标记，之后一直服务；
停止只能显式做（宿主按 PID 回收整棵进程树）。
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import socket
import sys
import threading
import time
from pathlib import Path

# 就绪标记：宿主按它认定「服务已经能接请求」（写进服务日志）。
READY_MARKER = "[onejev] 服务就绪"
# 单次连接的等待上限（秒）：客户端总是连上就发，静默超时说明对端已经不在了。
CONNECTION_TIMEOUT_SECONDS = 60.0
# 管道缓冲区：一次判定请求的 state 上限是 64k 字符级别，留一倍余量。
PIPE_BUFFER_BYTES = 128 * 1024

log = logging.getLogger("onejev.worker")


class Decider:
    """引擎 + 判定；串行化——一次前向独占前缀缓存，并发只会互相踩。"""

    def __init__(self, engine, model_name: str) -> None:
        self.engine = engine
        self.model_name = model_name
        self._lock = threading.Lock()

    def answer(self, body: dict) -> dict:
        from qev.schema import SystemOneRequest

        request = SystemOneRequest.model_validate(body)
        with self._lock:
            response, meta = self.engine.decide(request)
        response.model = self.model_name
        payload = response.model_dump()
        payload["qev"] = meta
        return payload


def _handle_connection(decider: Decider, read_line, write_line, close) -> None:
    """一次连接 = 一次判定：读一行请求，回一行响应（成功或错误），然后收线。"""
    try:
        line = read_line()
        if not line:
            return
        payload = decider.answer(json.loads(line))
    except Exception as error:  # 请求不合法 / 引擎报错：回一行可读原因，不猜答案
        log.warning("判定失败：%s", error)
        message = str(error) or error.__class__.__name__
        try:
            write_line(json.dumps({"error": message}, ensure_ascii=False))
        except Exception:
            pass
    else:
        write_line(json.dumps(payload, ensure_ascii=False))
    finally:
        close()


# --------------------------------------------------------------------------
# Windows：命名管道
# --------------------------------------------------------------------------


def windows_pipe_io(endpoint: str, decider: Decider) -> None:
    import ctypes
    import ctypes.wintypes as wintypes
    import msvcrt

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    pipe_access_duplex = 0x00000003
    pipe_type_byte = 0x00000000
    pipe_readmode_byte = 0x00000000
    pipe_wait = 0x00000000
    pipe_unlimited_instances = 255
    invalid_handle = wintypes.HANDLE(-1).value
    error_pipe_connected = 535

    kernel32.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel32.CreateNamedPipeW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
    ]
    kernel32.ConnectNamedPipe.restype = wintypes.BOOL
    kernel32.ConnectNamedPipe.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    kernel32.PeekNamedPipe.restype = wintypes.BOOL
    kernel32.PeekNamedPipe.argtypes = [
        wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(wintypes.DWORD),
    ]
    kernel32.CloseHandle.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]

    def create_instance() -> int:
        return kernel32.CreateNamedPipeW(
            endpoint,
            pipe_access_duplex,
            pipe_type_byte | pipe_readmode_byte | pipe_wait,
            pipe_unlimited_instances,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            None,
        )

    def serve_instance(handle: int) -> None:
        fd = msvcrt.open_osfhandle(handle, os.O_RDWR | os.O_BINARY)
        stream = os.fdopen(fd, "r+b", buffering=0)

        def read_line() -> str:
            buffer = bytearray()
            deadline = time.monotonic() + CONNECTION_TIMEOUT_SECONDS
            while b"\n" not in buffer:
                available = wintypes.DWORD(0)
                kernel32.PeekNamedPipe(
                    wintypes.HANDLE(handle), None, 0, None, ctypes.byref(available), None
                )
                if available.value:
                    chunk = stream.read(available.value)
                    if not chunk:
                        break
                    buffer += chunk
                    continue
                if time.monotonic() > deadline:
                    break
                time.sleep(0.005)
            return bytes(buffer).split(b"\n", 1)[0].decode("utf-8", "replace")

        def write_line(text: str) -> None:
            stream.write(text.encode("utf-8") + b"\n")

        # 关闭 fd 即关闭 handle（`open_osfhandle` 把它交给了 fd）。
        _handle_connection(decider, read_line, write_line, stream.close)

    # 实例先建出来再报就绪：宿主看到标记时端点必须已经能被连上。
    first = create_instance()
    if first == invalid_handle:
        raise SystemExit(f"创建命名管道失败：{ctypes.get_last_error()}（{endpoint}）")
    print(f"{READY_MARKER} endpoint={endpoint}", flush=True)

    pending = first
    while True:
        handle = pending
        if not kernel32.ConnectNamedPipe(wintypes.HANDLE(handle), None):
            error = ctypes.get_last_error()
            # ERROR_PIPE_CONNECTED 说明客户端已经连上了这个实例，照旧交给服务线程。
            if error != error_pipe_connected:
                kernel32.CloseHandle(wintypes.HANDLE(handle))
                pending = create_instance()
                continue
        # 一个实例只服务一个连接；下一个实例提前建好，客户端不会撞到「管道不存在」。
        pending = create_instance()
        threading.Thread(target=serve_instance, args=(handle,), daemon=True).start()


# --------------------------------------------------------------------------
# Unix：Unix domain socket（没有 TCP 端口，也没有 HTTP 中转）
# --------------------------------------------------------------------------


def unix_socket_io(endpoint: str, decider: Decider) -> None:
    path = Path(endpoint)
    if path.exists():
        path.unlink()
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(endpoint)
    server.listen(64)
    os.chmod(endpoint, 0o600)
    print(f"{READY_MARKER} endpoint={endpoint}", flush=True)

    def serve_connection(connection: socket.socket) -> None:
        stream = connection.makefile("rwb", buffering=0)

        def read_line() -> str:
            connection.settimeout(CONNECTION_TIMEOUT_SECONDS)
            line = stream.readline()
            return line.decode("utf-8", "replace").rstrip("\n") if line else ""

        def write_line(text: str) -> None:
            stream.write(text.encode("utf-8") + b"\n")

        def close() -> None:
            try:
                stream.close()
            finally:
                connection.close()

        _handle_connection(decider, read_line, write_line, close)

    while True:
        connection, _ = server.accept()
        threading.Thread(target=serve_connection, args=(connection,), daemon=True).start()


# --------------------------------------------------------------------------
# 引擎装配（与 `qev serve` 同口径，但不启 HTTP 服务）
# --------------------------------------------------------------------------


def build_engine(args: argparse.Namespace):
    from qev.calibrate import Calibration
    from qev.mm_engine import local_model_dir

    model_path = local_model_dir(args.model)
    local = Path(model_path) / "calibration.json"
    calib_path = args.calibration or (str(local) if local.exists() else None)
    calibration = Calibration.load(calib_path) if calib_path else None
    if calib_path:
        log.info("calibration: %s", calib_path)

    config = json.loads((Path(model_path) / "config.json").read_text())
    if args.multimodal or config.get("vision_config") is not None:
        from qev.mm_engine import MMDecisionEngine

        return MMDecisionEngine(
            model_path,
            device=args.device,
            dtype=args.dtype,
            calibration=calibration,
            fork_mode=args.fork_mode,
            head_dtype=args.head_dtype,
            max_branch_tokens=args.max_branch_tokens,
            max_request_tokens=args.max_request_tokens,
            gpu_preprocess=not args.no_gpu_preprocess,
            cuda_graphs=not args.no_cuda_graphs,
        )
    from qev.engine import DecisionEngine

    return DecisionEngine(
        model_path,
        device=args.device,
        dtype=args.dtype,
        calibration=calibration,
        fork_mode=args.fork_mode,
        head_dtype=args.head_dtype,
        max_branch_tokens=args.max_branch_tokens,
        max_request_tokens=args.max_request_tokens,
    )


def main() -> None:
    parser = argparse.ArgumentParser(prog="onejev_worker")
    parser.add_argument("--model", required=True, help="本地权重目录（或 Hub id）")
    parser.add_argument("--name", default=None, help="对外的模型名")
    parser.add_argument("--endpoint", required=True, help="命名管道名或 Unix socket 路径")
    parser.add_argument("--device", default="cuda")
    parser.add_argument("--dtype", default="bfloat16")
    parser.add_argument("--head-dtype", default="float32")
    parser.add_argument("--fork-mode", default="auto")
    parser.add_argument("--multimodal", action="store_true")
    parser.add_argument("--no-cuda-graphs", action="store_true")
    parser.add_argument("--no-gpu-preprocess", action="store_true")
    parser.add_argument("--calibration", default=None)
    parser.add_argument("--max-branch-tokens", type=int, default=32768)
    parser.add_argument("--max-request-tokens", type=int, default=65536)
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(levelname)s %(message)s")

    engine = build_engine(args)
    decider = Decider(engine, args.name or Path(args.model).name)

    if sys.platform == "win32":
        windows_pipe_io(args.endpoint, decider)
    else:
        unix_socket_io(args.endpoint, decider)


if __name__ == "__main__":
    main()
