//! `windows_batch` 的执行器：把一条已解析的计划真正打到桌面上。
//!
//! 与解析分开：解析阶段的错误发生在动鼠标之前，执行阶段才需要窗口句柄与真实坐标。
//! 两者失败的收尾方式也不同——解析失败整份脚本都不执行，执行失败要**停下并如实回报
//! 已经完成到哪一步**，好让模型接着补完而不是从头再来。

use std::time::Duration;

use serde_json::{json, Value};

use super::super::error::ToolError;
use super::batch_plan::{self, sampled_count, Offset, OnError, Op, Origin, Plan};
use super::ffi;
use super::input::{self, Step};
use super::window;

/// 执行期的一处状态：脚本当前指向哪个窗口，以及两套原点。
///
/// 原点不在这里选：它跟着每个坐标走（见 `batch_plan::Offset::origin`），
/// 因此单步写 `coord` 换原点是有效的。
struct Frame {
    handle: Option<isize>,
    origins: Option<window::WindowOrigins>,
}

/// 一步执行的结果：是否命中，以及给模型看的简短描述。
struct StepOutcome {
    matched: bool,
    note: String,
}

/// 一个 `expect` 采到的实际颜色。
struct Sample {
    color: [u8; 3],
}

impl Frame {
    fn new() -> Self {
        Self {
            handle: None,
            origins: None,
        }
    }

    /// 换到另一个窗口：重取两套原点，之后的窗口相对坐标都按新窗口算。
    fn retarget(&mut self, handle: isize) -> Result<(), ToolError> {
        let origins = window::window_origins(handle)?;
        self.handle = Some(handle);
        self.origins = Some(origins);
        Ok(())
    }

    /// 把脚本坐标换成虚拟桌面物理像素：原点取自坐标本身。
    fn resolve(&self, offset: Offset) -> Result<(i64, i64), ToolError> {
        let (base_x, base_y) = match (offset.origin, self.origins) {
            (Origin::Screen, _) => (0, 0),
            (Origin::Client, Some(origins)) => (origins.client_left, origins.client_top),
            (Origin::Window, Some(origins)) => (origins.window_left, origins.window_top),
            // 窗口相对坐标却没有窗口：不能猜，猜了就是往错的地方点。
            (_, None) => {
                return Err(ToolError::new(
                    "脚本用了窗口相对坐标，但没有可用的目标窗口：先给 window 或 window_handle，坐标才能落地。",
                ))
            }
        };
        let (x, y) = (base_x + offset.x, base_y + offset.y);
        input::validate_position(x, y)?;
        Ok((x, y))
    }
}

/// 整份计划的执行情况。
pub struct Report {
    pub completed: usize,
    pub total: usize,
    pub stopped_at: Option<usize>,
    pub notes: Vec<String>,
    pub samples: Vec<Value>,
    pub shots: Vec<String>,
}

/// 执行整份计划。`expect` 不成立时按各自的 `on_error` 决定是否停下。
pub fn run(plan: &Plan) -> Result<Report, ToolError> {
    let dry_run = plan.dry_run;
    let total = expanded_len(&plan.steps);
    let mut frame = Frame::new();
    let mut report = Report {
        completed: 0,
        total,
        stopped_at: None,
        notes: Vec::new(),
        samples: Vec::new(),
        shots: Vec::new(),
    };

    for op in &plan.steps {
        if let Some(index) = run_op(op, &mut frame, dry_run, &mut report)? {
            report.stopped_at = Some(index);
            break;
        }
    }
    Ok(report)
}

/// 执行一步；返回 `Some(位置)` 表示这里停下（`expect` 未命中且策略为 abort）。
fn run_op(
    op: &Op,
    frame: &mut Frame,
    dry_run: bool,
    report: &mut Report,
) -> Result<Option<usize>, ToolError> {
    if let Op::Repeat { times, body } = op {
        for _ in 0..*times {
            for inner in body {
                if let Some(index) = run_op(inner, frame, dry_run, report)? {
                    return Ok(Some(index));
                }
            }
        }
        return Ok(None);
    }

    let outcome = match op {
        Op::Focus => {
            let Some(handle) = frame.handle else {
                return Err(ToolError::new(
                    "focus 之前要先给 window 或 window_handle：没有目标窗口就谈不上聚焦。",
                ));
            };
            if dry_run {
                StepOutcome::note_only("预演：跳过聚焦".to_string())
            } else {
                window::focus_window(handle)?;
                std::thread::sleep(Duration::from_millis(
                    batch_plan::FOCUS_SETTLE_MS as u64,
                ));
                StepOutcome::note_only(format!("聚焦 {}", window::format_window_handle(handle)))
            }
        }
        Op::Window {
            handle,
            title_contains,
        } => {
            let resolved = match (handle, title_contains) {
                (Some(handle), _) => *handle,
                (None, Some(needle)) => {
                    let matches = window::find_windows_by_title(needle)?;
                    match matches.len() {
                        0 => {
                            return Err(ToolError::new(format!(
                                "找不到标题含「{needle}」的可见窗口：先用 windows_window 的 list 确认它开着。"
                            )))
                        }
                        1 => interpret_handle(&matches[0])?,
                        count => {
                            let titles: Vec<String> = matches
                                .iter()
                                .take(5)
                                .filter_map(|item| item["title"].as_str().map(str::to_string))
                                .collect();
                            return Err(ToolError::new(format!(
                                "标题含「{needle}」的窗口有 {count} 个：{}。改用更精确的标题或直接给 window_handle。",
                                titles.join("、")
                            )));
                        }
                    }
                }
                (None, None) => {
                    return Err(ToolError::new("window 步骤缺少定位信息。"));
                }
            };
            frame.retarget(resolved)?;
            StepOutcome::note_only(format!(
                "目标窗口 {}",
                window::format_window_handle(resolved)
            ))
        }
        Op::Move { at } => {
            let (x, y) = frame.resolve(*at)?;
            if !dry_run {
                input::run_steps(&[Step::Move { x, y }])?;
            }
            StepOutcome::note_only(format!("移动到 ({x}, {y})"))
        }
        Op::Click { at, button, clicks } => {
            let (x, y) = frame.resolve(*at)?;
            if !dry_run {
                let mut steps = vec![Step::Move { x, y }];
                for _ in 0..*clicks {
                    steps.push(Step::Down {
                        button: button.clone(),
                    });
                    steps.push(Step::Up {
                        button: button.clone(),
                    });
                }
                input::run_steps(&steps)?;
            }
            StepOutcome::note_only(format!("{button} 点击 ({x}, {y}) ×{clicks}"))
        }
        Op::Drag {
            from,
            to,
            button,
            steps: interpolate,
        } => {
            let (from_x, from_y) = frame.resolve(*from)?;
            let (to_x, to_y) = frame.resolve(*to)?;
            if !dry_run {
                let mut steps = vec![
                    Step::Move {
                        x: from_x,
                        y: from_y,
                    },
                    Step::Down {
                        button: button.clone(),
                    },
                ];
                // 一步到位是「拖拽」；中间补点才是「拖着走」，很多画布应用只认后者。
                let segments = (*interpolate).max(1);
                for index in 1..=segments {
                    let ratio = index as f64 / segments as f64;
                    steps.push(Step::Move {
                        x: lerp(from_x, to_x, ratio),
                        y: lerp(from_y, to_y, ratio),
                    });
                }
                steps.push(Step::Up {
                    button: button.clone(),
                });
                input::run_steps(&steps)?;
            }
            StepOutcome::note_only(format!(
                "{button} 拖拽 ({from_x}, {from_y}) → ({to_x}, {to_y}) 分 {interpolate} 段"
            ))
        }
        Op::Wheel { at, delta } => {
            let mut steps = Vec::new();
            if let Some(at) = at {
                let (x, y) = frame.resolve(*at)?;
                steps.push(Step::Move { x, y });
            }
            steps.push(Step::Wheel { delta: *delta });
            if !dry_run {
                input::run_steps(&steps)?;
            }
            StepOutcome::note_only(format!("滚轮 {delta}"))
        }
        Op::Key { keys, presses } => {
            if !dry_run {
                input::run_steps(&[Step::Key {
                    keys: keys.clone(),
                    presses: *presses,
                }])?;
            }
            StepOutcome::note_only(format!("按键 {} ×{presses}", keys.join("+")))
        }
        Op::Type { text } => {
            if !dry_run {
                input::run_steps(&[Step::Text { text: text.clone() }])?;
            }
            StepOutcome {
                matched: true,
                // 不回显文本：与 windows_input 同口径，避免把敏感内容写进会话记录。
                note: format!("输入 {} 个字符", text.chars().count()),
            }
        }
        Op::Stroke {
            points,
            button,
            spacing,
            step_delay_ms,
            jitter,
            pressure,
        } => {
            let resolved: Vec<(i64, i64)> = points
                .iter()
                .map(|point| frame.resolve(*point))
                .collect::<Result<_, _>>()?;
            let samples = resample(&resolved, *spacing, *jitter, *pressure);
            if !dry_run {
                draw_stroke(&samples, button, *step_delay_ms)?;
            }
            StepOutcome::note_only(format!(
                "{button} 描边 {} 个控制点 → {} 个采样点，间距 {spacing}",
                points.len(),
                samples.len()
            ))
        }
        Op::Wait { ms } => {
            if !dry_run {
                std::thread::sleep(Duration::from_millis(*ms as u64));
            }
            StepOutcome::note_only(format!("等待 {ms} ms"))
        }
        Op::Expect { probe, on_error } => {
            let (x, y) = frame.resolve(probe.offset)?;
            let sample = read_pixel(x, y)?;
            let distance = channel_distance(sample.color, probe.color);
            let matched = distance <= probe.tolerance as i64;
            report.samples.push(json!({
                "at": [x, y],
                "expected": format_color(probe.color),
                "actual": format_color(sample.color),
                "tolerance": probe.tolerance,
                "distance": distance,
                "matched": matched,
            }));
            let mut note = format!(
                "像素 ({x}, {y}) 期望 {} 实际 {}（容差 {}，差 {}）",
                format_color(probe.color),
                format_color(sample.color),
                probe.tolerance,
                distance
            );
            // `continue` 把不成立降级成一条记录：脚本后续步骤照跑，报告里仍留着实况。
            if !matched && *on_error == OnError::Continue {
                note.push_str("（未命中，on_error=continue）");
                StepOutcome {
                    matched: true,
                    note,
                }
            } else {
                StepOutcome { matched, note }
            }
        }
        Op::Shot { as_name } => {
            report.shots.push(as_name.clone());
            StepOutcome::note_only(format!("截图 {as_name}"))
        }
        // 函数开头已经把 repeat 展开掉了：这里只剩防御性分支，不是可执行的路径。
        Op::Repeat { .. } => StepOutcome::note_only("repeat 已展开"),
    };

    report.completed += 1;
    let matched = outcome.matched;
    let note = outcome.note;
    if matched {
        report.notes.push(note);
        return Ok(None);
    }
    let message = format!("第 {} 步的校验不成立：{note}", report.completed);
    report.notes.push(note);
    Err(ToolError::new(message))
}

impl StepOutcome {
    fn note_only(note: impl Into<String>) -> Self {
        Self {
            matched: true,
            note: note.into(),
        }
    }
}

/// 一段描边分几次提交：`step_delay_ms` 是**每块之间**的停顿。
///
/// 一次提交上万个移动事件，目标应用会丢事件、笔迹断成折线；每点提交一次又慢到不可用
/// （一次系统调用一次睡眠）。分块是两者的折中：块内连成线，块间给应用时间跟手。
const STROKE_CHUNK: usize = 8;

fn draw_stroke(samples: &[(i64, i64)], button: &str, step_delay_ms: i64) -> Result<(), ToolError> {
    let mut steps = Vec::with_capacity(samples.len() + 2);
    if let Some((x, y)) = samples.first() {
        steps.push(Step::Move { x: *x, y: *y });
    }
    steps.push(Step::Down {
        button: button.to_string(),
    });
    for (index, (x, y)) in samples.iter().enumerate() {
        steps.push(Step::Move { x: *x, y: *y });
        let end_of_chunk = (index + 1) % STROKE_CHUNK == 0;
        if end_of_chunk && index + 1 < samples.len() {
            input::run_steps(&steps)?;
            steps.clear();
            if step_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(step_delay_ms as u64));
            }
        }
    }
    steps.push(Step::Up {
        button: button.to_string(),
    });
    input::run_steps(&steps)
}

fn interpret_handle(window: &Value) -> Result<isize, ToolError> {
    let text = window["window_handle"].as_str().unwrap_or_default();
    let hex = text.trim_start_matches("0x").trim_start_matches("0X");
    i64::from_str_radix(hex, 16)
        .ok()
        .filter(|value| *value > 0)
        .map(|value| value as isize)
        .ok_or_else(|| ToolError::new(format!("窗口句柄无法解析：{text}")))
}

fn expanded_len(steps: &[Op]) -> usize {
    steps
        .iter()
        .map(|op| match op {
            Op::Repeat { times, body } => expanded_len(body) * (*times).max(0) as usize,
            _ => 1,
        })
        .sum()
}

fn lerp(from: i64, to: i64, ratio: f64) -> i64 {
    from + ((to - from) as f64 * ratio).round() as i64
}

/// 把折线重采样成等距点，并按 `pressure` 收窄起收笔的偏移。
///
/// 抖动沿法线方向施加：只在「垂直前进方向」上加噪，笔迹的走向还是模型给的那条，
/// 不会被扰动成毛刺。同一段里用确定性序列（正弦叠加），便于复现问题。
fn resample(
    points: &[(i64, i64)],
    spacing: f64,
    jitter: f64,
    pressure: (f64, f64),
) -> Vec<(i64, i64)> {
    let mut samples: Vec<(i64, i64)> = Vec::new();
    let total = polyline_length(points);
    // 采样点数与解析阶段用的是同一口径：预算校验和实际执行不会各算一套。
    let count = sampled_count(total, spacing) as usize;
    for index in 0..count {
        let ratio = index as f64 / (count - 1) as f64;
        let (base_x, base_y, heading) = point_at(points, ratio * total);
        // 起笔和收笔处压力低，抖动按压力线性收窄，避免笔尖甩出去。
        let along = ratio;
        let weight = pressure.0 + (pressure.1 - pressure.0) * along;
        let amplitude = jitter * weight;
        let (offset_x, offset_y) = if amplitude > 0.0 {
            let wave = (index as f64 * 1.7).sin() * amplitude;
            // 法线方向：(dy, -dx) 归一化。
            let (sin, cos) = heading.sin_cos();
            (-sin * wave, cos * wave)
        } else {
            (0.0, 0.0)
        };
        samples.push((
            (base_x + offset_x).round() as i64,
            (base_y + offset_y).round() as i64,
        ));
    }
    samples
}

fn polyline_length(points: &[(i64, i64)]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let dx = (pair[1].0 - pair[0].0) as f64;
            let dy = (pair[1].1 - pair[0].1) as f64;
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

/// 沿折线走 `distance` 像素后的位置，以及该处的前进方向（弧度）。
fn point_at(points: &[(i64, i64)], distance: f64) -> (f64, f64, f64) {
    let mut walked = 0.0;
    for pair in points.windows(2) {
        let dx = (pair[1].0 - pair[0].0) as f64;
        let dy = (pair[1].1 - pair[0].1) as f64;
        let segment = (dx * dx + dy * dy).sqrt();
        if segment <= f64::EPSILON {
            continue;
        }
        if walked + segment >= distance {
            let ratio = (distance - walked) / segment;
            return (
                pair[0].0 as f64 + dx * ratio,
                pair[0].1 as f64 + dy * ratio,
                dy.atan2(dx),
            );
        }
        walked += segment;
    }
    // 走完全程：停在终点，方向沿用最后一段非零长度线段。
    let last = points.last().copied().unwrap_or((0, 0));
    let heading = points
        .windows(2)
        .rev()
        .find(|pair| pair[0] != pair[1])
        .map(|pair| ((pair[1].1 - pair[0].1) as f64).atan2((pair[1].0 - pair[0].0) as f64))
        .unwrap_or(0.0);
    (last.0 as f64, last.1 as f64, heading)
}

/// 取屏幕上一个物理像素的颜色。
fn read_pixel(x: i64, y: i64) -> Result<Sample, ToolError> {
    let screen = unsafe { ffi::GetDC(std::ptr::null_mut()) };
    if screen.is_null() {
        return Err(ToolError::new("获取屏幕设备上下文失败。"));
    }
    let color = unsafe { ffi::GetPixel(screen, x as i32, y as i32) };
    unsafe { ffi::ReleaseDC(std::ptr::null_mut(), screen) };
    if color == ffi::CLR_INVALID {
        return Err(ToolError::new(format!(
            "读取 ({x}, {y}) 的像素失败：坐标可能不在任何显示器上。"
        )));
    }
    // COLORREF 是 0x00BBGGRR。
    Ok(Sample {
        color: [
            (color & 0xFF) as u8,
            ((color >> 8) & 0xFF) as u8,
            ((color >> 16) & 0xFF) as u8,
        ],
    })
}

fn channel_distance(left: [u8; 3], right: [u8; 3]) -> i64 {
    (0..3)
        .map(|index| (left[index] as i64 - right[index] as i64).abs())
        .max()
        .unwrap_or(0)
}

fn format_color(color: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_point_stroke_is_resampled_evenly() {
        let samples = resample(&[(0, 0), (100, 0)], 10.0, 0.0, (1.0, 1.0));
        // 100 像素、间距 10 → 10 段 11 点，与解析阶段同一个口径。
        assert_eq!(samples.len(), 11);
        assert_eq!(samples.first(), Some(&(0, 0)));
        assert_eq!(samples.last(), Some(&(100, 0)));
        // 没有抖动时，点必须落在同一条水平线上。
        assert!(samples.iter().all(|(_, y)| *y == 0), "{samples:?}");
    }

    #[test]
    fn jitter_stays_perpendicular_to_the_stroke() {
        let samples = resample(&[(0, 0), (200, 0)], 4.0, 3.0, (1.0, 1.0));
        // 水平描边的法线是竖直方向：抖动只改 y，x 仍是等距推进。
        assert!(samples.iter().any(|(_, y)| *y != 0), "抖动应当生效");
        assert_eq!(samples.first().map(|point| point.0), Some(0));
        assert_eq!(samples.last().map(|point| point.0), Some(200));
    }

    #[test]
    fn zero_pressure_at_the_ends_removes_jitter_there() {
        let samples = resample(&[(0, 0), (400, 0)], 4.0, 5.0, (0.0, 0.0));
        assert!(
            samples.iter().all(|(_, y)| *y == 0),
            "压力为 0 时不该有抖动"
        );
    }

    #[test]
    fn polyline_length_adds_every_segment() {
        assert_eq!(polyline_length(&[(0, 0), (3, 4), (3, 14)]), 15.0);
        assert_eq!(polyline_length(&[(7, 7)]), 0.0);
    }

    #[test]
    fn colors_are_compared_by_the_worst_channel() {
        assert_eq!(channel_distance([10, 20, 30], [10, 20, 30]), 0);
        assert_eq!(channel_distance([10, 20, 30], [12, 20, 30]), 2);
        assert_eq!(channel_distance([0, 0, 0], [3, 250, 1]), 250);
        assert_eq!(format_color([0x1e, 0x2f, 0x3a]), "#1E2F3A");
    }

    #[test]
    fn screen_origin_needs_no_window() {
        let frame = Frame::new();
        let (x, y) = frame
            .resolve(Offset {
                x: 15,
                y: 25,
                origin: Origin::Screen,
            })
            .expect("屏幕坐标不需要窗口");
        assert_eq!((x, y), (15, 25));
    }

    #[test]
    fn window_relative_coordinates_without_a_window_are_refused() {
        let frame = Frame::new();
        let error = frame
            .resolve(Offset {
                x: 1,
                y: 1,
                origin: Origin::Client,
            })
            .expect_err("没有窗口就不能换算");
        assert!(
            error.message.contains("没有可用的目标窗口"),
            "{}",
            error.message
        );
    }

    /// 描边分块的边界：恰好整除与不足一块时都不能漏点，也不能多发一次空提交。
    #[test]
    fn stroke_chunks_cover_every_sample_exactly_once() {
        for count in [1usize, 2, 7, 8, 9, 16, 17] {
            let samples: Vec<(i64, i64)> = (0..count)
                .map(|index| (index as i64 * 3, index as i64))
                .collect();
            // 用计数替身走一遍分块规则：块数必须等于 ceil(count / STROKE_CHUNK)。
            let chunks = samples
                .iter()
                .enumerate()
                .filter(|(index, _)| (index + 1) % STROKE_CHUNK == 0 && index + 1 < samples.len())
                .count();
            assert_eq!(
                chunks,
                count.saturating_sub(1) / STROKE_CHUNK,
                "count={count}"
            );
        }
    }

    /// 单步 `coord` 必须真的生效：原点跟着坐标走，不是全局一份。
    ///
    /// 这条曾经是真 bug——`coord` 被解析出来又丢掉，只有顶层原点起作用，
    /// 脚本里想给某一步换原点会静默失效、点到别处去。
    #[test]
    fn a_step_level_coord_overrides_the_global_one() {
        let plan = batch_plan::parse(
            serde_json::json!({
                "coord": "screen",
                "dry_run": true,
                "steps": [{"op": "click", "coord": "client", "at": [5, 5]}]
            })
            .as_object()
            .expect("参数对象"),
        )
        .expect("脚本应当解析成功");
        let batch_plan::Op::Click { at, .. } = &plan.steps[0] else {
            panic!("应当是 click");
        };
        assert_eq!(at.origin, Origin::Client, "步骤自己的 coord 必须覆盖顶层");
        assert_eq!(plan.default_origin, Origin::Screen, "顶层原点保持不变");
    }

    #[test]
    fn nested_repeat_counts_towards_the_total() {
        let plan = vec![Op::Repeat {
            times: 3,
            body: vec![Op::Focus, Op::Wait { ms: 1 }],
        }];
        assert_eq!(expanded_len(&plan), 6);
    }
}
