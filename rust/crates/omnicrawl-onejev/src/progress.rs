//! 自部署长任务的进度快照：后台线程写、界面每帧读。
//!
//! 环境安装、权重下载与服务启动都是分钟级动作，界面必须能回答「现在在做什么、走到哪了」。
//! 进度有三种粒度，都用同一份快照承载：
//!
//! * [`ProgressUnit::Bytes`]：权重下载，`done` / `total` 是字节，界面画实心进度条；
//! * [`ProgressUnit::Steps`]：环境安装（建 venv → 装 torch → 装 qev），按步数计数；
//! * [`ProgressUnit::None`]：只有阶段文案（服务健康检查），界面画循环滑动的滑块。
//!
//! 句柄只共享一份快照（`Arc<Mutex<..>>`），不另开事件通道：界面每帧取一次即可，
//! 丢掉中间帧不影响观感，也不会让后台线程因界面慢而阻塞。

use std::sync::{Arc, Mutex};

/// 进度的计量单位（决定界面怎么念这两个数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgressUnit {
    /// 没有量的进度，只有阶段文案。
    #[default]
    None,
    /// 字节。
    Bytes,
    /// 步骤（第几步 / 共几步）。
    Steps,
}

/// 后台任务进度的一帧快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    /// 当前阶段的文案（直接进界面状态行）。
    pub stage: String,
    /// 已完成的量；`unit` 为 [`ProgressUnit::None`] 时无意义。
    pub done: u64,
    /// 总量；为 0 表示这一步没有可量化的进度。
    pub total: u64,
    pub unit: ProgressUnit,
}

impl ProgressSnapshot {
    /// 已完成比例；总量未知时为 `None`（界面据此改画滑动滑块）。
    pub fn ratio(&self) -> Option<f64> {
        (self.total > 0).then(|| (self.done as f64 / self.total as f64).clamp(0.0, 1.0))
    }

    /// 界面右侧的量化文案（如 `1.2 GB / 2.2 GB`、`2/3`）；无从量化时为空。
    pub fn detail(&self) -> String {
        match self.unit {
            ProgressUnit::Bytes if self.total > 0 => {
                format!("{} / {}", human_bytes(self.done), human_bytes(self.total))
            }
            ProgressUnit::Steps if self.total > 0 => format!("{}/{}", self.done, self.total),
            _ => String::new(),
        }
    }
}

/// 字节数的可读写法（保留一位小数，单位按 1024 进位）。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index + 1 < UNITS.len() {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[index])
    }
}

/// 后台任务与界面之间的进度句柄（克隆共享同一份快照）。
#[derive(Debug, Clone, Default)]
pub struct ProgressHandle {
    inner: Arc<Mutex<ProgressSnapshot>>,
}

impl ProgressHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 进入一个只有阶段文案的步骤（字节进度清零）。
    pub fn stage(&self, stage: impl Into<String>) {
        self.write(stage.into(), 0, 0, ProgressUnit::None);
    }

    /// 进入第 `index` 步（共 `count` 步）：安装类的确定进度。
    pub fn step(&self, index: u64, count: u64, stage: impl Into<String>) {
        self.write(stage.into(), index.min(count), count, ProgressUnit::Steps);
    }

    /// 读取当前快照；锁中毒时给出空快照而不是中断界面。
    pub fn snapshot(&self) -> ProgressSnapshot {
        self.inner
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 给 [`crate::env::ensure_environment`] 用的回调（阶段 + 字节）。
    pub fn sink(&self) -> impl FnMut(&str, u64, u64) + 'static {
        let handle = self.clone();
        move |stage: &str, done: u64, total: u64| {
            handle.write(stage.to_string(), done, total, ProgressUnit::Bytes)
        }
    }

    /// 给 [`crate::download::download_model`] 用的回调（只有字节，没有阶段串）。
    ///
    /// 阶段文案沿用调用方最近一次写入的（下载流程自己会先报「正在下载」）。
    pub fn bytes_sink(&self) -> impl FnMut(u64, u64) + 'static {
        let handle = self.clone();
        move |done: u64, total: u64| {
            let stage = handle
                .snapshot()
                .stage;
            handle.write(stage, done, total, ProgressUnit::Bytes)
        }
    }

    fn write(&self, stage: String, done: u64, total: u64, unit: ProgressUnit) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = ProgressSnapshot {
                stage,
                done,
                total,
                unit,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_needs_a_known_total() {
        let snapshot = ProgressSnapshot {
            stage: "下载".to_string(),
            done: 3,
            total: 0,
            unit: ProgressUnit::Bytes,
        };
        assert_eq!(snapshot.ratio(), None, "总量未知时不给比例");
        let snapshot = ProgressSnapshot {
            done: 3,
            total: 4,
            ..ProgressSnapshot::default()
        };
        assert_eq!(snapshot.ratio(), Some(0.75));
        let snapshot = ProgressSnapshot {
            done: 9,
            total: 4,
            ..ProgressSnapshot::default()
        };
        assert_eq!(snapshot.ratio(), Some(1.0), "越界比例要夹住");
    }

    #[test]
    fn detail_follows_the_unit() {
        let bytes = ProgressSnapshot {
            done: 1_073_741_824,
            total: 2_234_579_789,
            unit: ProgressUnit::Bytes,
            ..ProgressSnapshot::default()
        };
        assert_eq!(bytes.detail(), "1.0 GB / 2.1 GB");
        let steps = ProgressSnapshot {
            done: 2,
            total: 3,
            unit: ProgressUnit::Steps,
            ..ProgressSnapshot::default()
        };
        assert_eq!(steps.detail(), "2/3");
        let bare = ProgressSnapshot {
            done: 5,
            total: 5,
            unit: ProgressUnit::None,
            ..ProgressSnapshot::default()
        };
        assert_eq!(bare.detail(), "", "没有量化单位就不给数字");
        assert!((bytes.ratio().unwrap() - 0.4805).abs() < 0.001);
    }

    #[test]
    fn human_bytes_keeps_small_values_exact() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2_048), "2.0 KB");
        assert_eq!(human_bytes(54_735_927_480), "51.0 GB");
    }

    #[test]
    fn sink_writes_through_to_every_clone() {
        let handle = ProgressHandle::new();
        let observer = handle.clone();
        let mut sink = handle.sink();
        sink("下载 0.8B", 128, 1024);
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.stage, "下载 0.8B");
        assert_eq!(snapshot.done, 128);
        assert_eq!(snapshot.total, 1024);
        assert_eq!(snapshot.unit, ProgressUnit::Bytes);
    }

    #[test]
    fn stage_and_step_reset_byte_progress() {
        let handle = ProgressHandle::new();
        let mut sink = handle.sink();
        sink("下载", 512, 1024);
        handle.stage("正在启动本地决策服务…");
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.stage, "正在启动本地决策服务…");
        assert_eq!(snapshot.done, 0, "阶段切换不能留着上一步的字节数");
        assert_eq!(snapshot.total, 0);
        assert_eq!(snapshot.ratio(), None, "无量化阶段要退化成滑块");
        handle.step(2, 3, "安装 qev…");
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.detail(), "2/3");
        assert_eq!(snapshot.ratio(), Some(2.0 / 3.0));
    }
}
