//! 生成音频的本地播放（尽力而为，播放失败不影响主流程）。
//!
//! `omnicrawl/tts/player.py` 的等价实现：Windows 用 `winmm` 的 `PlaySoundW` 播放
//! WAV；macOS/Linux 回退到系统命令（afplay / aplay / paplay）。默认异步（不阻塞
//! 调用线程）；`blocking = true` 时同步等待播放结束——CLI 等短生命周期进程必须用
//! 同步播放，否则进程退出会终止播放，导致听不到声音。

use std::path::Path;

/// 播放 WAV 文件；返回是否成功发起播放。
///
/// `blocking = false`（默认）：异步发起后立即返回，不阻塞调用线程；适合长生命
/// 周期进程。`blocking = true`：同步等待播放结束；适合 CLI 等短生命周期进程。
///
/// 任何失败只返回 `false`，绝不 panic。
pub fn play_wav(path: &Path, blocking: bool) -> bool {
    let audio_path = crate::paths::resolve_lenient(path);
    if !audio_path.is_file() {
        eprintln!("自动播放失败：音频文件不存在 {}", audio_path.display());
        return false;
    }
    match play(&audio_path, blocking) {
        Ok(()) => {
            eprintln!(
                "已自动播放 {}（{}）",
                audio_path.display(),
                if blocking { "同步" } else { "异步" }
            );
            true
        }
        Err(error) => {
            eprintln!("自动播放失败（{}）：{error}", audio_path.display());
            false
        }
    }
}

#[cfg(windows)]
fn play(path: &Path, blocking: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    const SND_FILENAME: u32 = 0x0002_0000;
    const SND_ASYNC: u32 = 0x0001;

    #[link(name = "winmm")]
    extern "system" {
        fn PlaySoundW(sound: *const u16, module: *mut core::ffi::c_void, flags: u32) -> i32;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // 不带 SND_ASYNC 即为同步播放：阻塞到播放结束，进程退出前能完整听到声音。
    let flags = SND_FILENAME | if blocking { 0 } else { SND_ASYNC };
    let played = unsafe { PlaySoundW(wide.as_ptr(), std::ptr::null_mut(), flags) };
    if played == 0 {
        return Err("PlaySoundW 未能播放该文件。".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn play(path: &Path, blocking: bool) -> Result<(), String> {
    for command in ["afplay", "aplay", "paplay"] {
        let Some(executable) = which(command) else {
            continue;
        };
        let mut child = std::process::Command::new(executable)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        if blocking {
            let _ = child.wait();
        }
        return Ok(());
    }
    Err("未找到可用的音频播放器（afplay/aplay/paplay），跳过自动播放。".to_string())
}

#[cfg(not(windows))]
fn which(command: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|directory| directory.join(command))
        .find(|candidate| candidate.is_file())
}
