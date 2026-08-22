"""生成音频的本地播放（尽力而为，播放失败不影响主流程）。

Windows 使用标准库 ``winsound`` 播放 WAV；macOS/Linux 回退到系统命令
（afplay / aplay / paplay）。默认异步（不阻塞调用线程）；``blocking=True``
时同步等待播放结束——CLI 等短生命周期进程必须用同步播放，否则进程退出会
终止 SND_ASYNC 播放，导致听不到声音。
"""

from __future__ import annotations

import logging
import shutil
import subprocess
import sys
from pathlib import Path

LOGGER = logging.getLogger(__name__)


def play_wav(path: str | Path, *, blocking: bool = False) -> bool:
    """播放 WAV 文件；返回是否成功发起播放。

    ``blocking=False``（默认）：异步发起后立即返回，不阻塞调用线程；适合
    Agent 工具等长生命周期进程。``blocking=True``：同步等待播放结束；适合
    CLI 等短生命周期进程——SND_ASYNC 播放会随进程退出被终止，导致无声。

    任何失败只记录日志并返回 False，绝不抛出异常。
    """
    audio_path = Path(path).expanduser().resolve()
    if not audio_path.is_file():
        LOGGER.warning("自动播放失败：音频文件不存在 %s", audio_path)
        return False
    try:
        if sys.platform == "win32":
            import winsound

            if blocking:
                # 不带 SND_ASYNC 即为同步播放：阻塞到播放结束，进程退出前能
                # 完整听到声音（winsound 没有 SND_SYNC 常量，同步是默认模式）。
                winsound.PlaySound(str(audio_path), winsound.SND_FILENAME)
            else:
                # SND_ASYNC：立即返回，不阻塞合成/Agent 线程。
                winsound.PlaySound(
                    str(audio_path), winsound.SND_FILENAME | winsound.SND_ASYNC
                )
            LOGGER.info("已自动播放 %s（%s）", audio_path, "同步" if blocking else "异步")
            return True
        for command in ("afplay", "aplay", "paplay"):
            executable = shutil.which(command)
            if executable:
                process = subprocess.Popen(  # noqa: S603 - 白名单系统播放器
                    [executable, str(audio_path)],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                if blocking:
                    process.wait()
                LOGGER.info("已自动播放 %s（%s）", audio_path, command)
                return True
        LOGGER.warning("未找到可用的音频播放器（afplay/aplay/paplay），跳过自动播放。")
        return False
    except Exception:  # noqa: BLE001 - 播放失败不阻断主流程
        LOGGER.warning("自动播放失败：%s", audio_path, exc_info=True)
        return False


__all__ = ["play_wav"]
