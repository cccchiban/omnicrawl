//! `windows_batch` 的步骤表解析：把模型写的 JSON 步骤摊成一条可执行计划。
//!
//! 拆成独立模块的理由是**纯函数**：脚本解析不碰 Win32，因此越界坐标、未知 op、
//! 超限的重复次数都在真正动鼠标之前就能报错，测试也不必依赖真实桌面。
//!
//! 坐标在这里也不做换算——计划里保存的是「相对哪个原点、偏移多少」，换算发生在执行阶段
//! （窗口可能被用户挪动，解析时算一次会失效）。

use serde_json::{Map, Value};

use super::super::error::ToolError;
use super::input;

/// 一次脚本最多的步骤数（含 `repeat` 展开后的总数由 [`MAX_EXPANDED_STEPS`] 单独限制）。
pub const MAX_STEPS: usize = 200;
/// `repeat` 展开后的步骤上限：真正的防呆线，防止 `repeat.times` 写错把桌面锁死。
pub const MAX_EXPANDED_STEPS: usize = 20_000;
/// 一条 `stroke` 折线最多的控制点数。
pub const MAX_STROKE_POINTS: usize = 2_000;
/// 一条 `stroke` 重采样后最多的插值点数：一次 `SendInput` 数组的实际规模。
pub const MAX_STROKE_SAMPLES: usize = 20_000;
/// 一次脚本最长的等待总时长（毫秒）：脚本里可以等待，但不能变成挂死。
pub const MAX_TOTAL_WAIT_MS: i64 = 120_000;
/// `focus` 之后留给窗口切换前台的时间：执行侧真的睡这么久，预算里也要按此计费。
pub const FOCUS_SETTLE_MS: i64 = 80;

const OPS: [&str; 13] = [
    "click", "drag", "expect", "focus", "key", "move", "repeat", "shot", "stroke", "type", "wait",
    "wheel", "window",
];

/// 坐标原点：脚本里写的 `[x, y]` 相对什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// 虚拟桌面物理像素。
    Screen,
    /// 目标窗口的客户区左上角。
    Client,
    /// 目标窗口（含边框与标题栏）左上角。
    Window,
}

/// 一处待换算的坐标，连同它自己的原点。
///
/// 原点跟着坐标走（而不是只留在 `Frame` 上）：脚本里某一步可以单独写 `coord` 换原点，
/// 例如整体按客户区定位、个别 `expect` 按整窗定位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offset {
    pub x: i64,
    pub y: i64,
    pub origin: Origin,
}

/// 失败策略：一条 `expect` 不成立时是停下还是继续。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    Abort,
    Continue,
}

/// 一个像素采样点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelProbe {
    pub offset: Offset,
    pub color: [u8; 3],
    pub tolerance: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Focus,
    Move {
        at: Offset,
    },
    Click {
        at: Offset,
        button: String,
        clicks: i64,
    },
    Drag {
        from: Offset,
        to: Offset,
        button: String,
        /// 移动过程中的插值点数：0 表示一步到位。
        steps: i64,
    },
    Wheel {
        at: Option<Offset>,
        delta: i64,
    },
    Key {
        keys: Vec<String>,
        presses: i64,
    },
    Type {
        text: String,
    },
    Stroke {
        points: Vec<Offset>,
        button: String,
        /// 重采样后的点间距（像素）：越小越密、越慢。
        spacing: f64,
        /// 每采一点的停顿（毫秒）：给目标软件跟手的时间。
        step_delay_ms: i64,
        /// 垂直于前进方向的随机抖动幅度（像素）。
        jitter: f64,
        /// 起点压力与终点压力（0.0–1.0）：仅作记录，按键力度由目标软件自己决定。
        pressure: (f64, f64),
    },
    Wait {
        ms: i64,
    },
    Expect {
        probe: PixelProbe,
        on_error: OnError,
    },
    Shot {
        as_name: String,
    },
    Repeat {
        times: i64,
        body: Vec<Op>,
    },
    Window {
        handle: Option<isize>,
        title_contains: Option<String>,
    },
}

/// 解析后的脚本。
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// 顶层 `coord`：步骤没写自己的 `coord` 时沿用。
    pub default_origin: Origin,
    /// 顶层 `dry_run`。
    pub dry_run: bool,
    pub steps: Vec<Op>,
}

const TOP_KEYS: [&str; 6] = [
    "steps",
    "window",
    "window_handle",
    "coord",
    "dry_run",
    "on_error",
];

/// 解析整份脚本。参数错误一次性报出，绝不留下执行了一半的序列。
pub fn parse(arguments: &Map<String, Value>) -> Result<Plan, ToolError> {
    reject_unknown_keys(arguments, &TOP_KEYS, "")?;

    let default_origin = read_origin(arguments.get("coord"), Origin::Screen)?;
    let dry_run = read_bool(arguments.get("dry_run"), false, "dry_run")?;
    let default_on_error = read_on_error(arguments.get("on_error"), OnError::Abort)?;

    let Some(Value::Array(raw_steps)) = arguments.get("steps") else {
        return Err(ToolError::new(
            "steps 必须是步骤对象数组，例如 [{\"op\":\"focus\"},{\"op\":\"click\",\"at\":[100,200]}]。",
        ));
    };
    if raw_steps.is_empty() {
        return Err(ToolError::new("steps 不能为空。"));
    }

    // 顶层定位信息先摊成一条 `window` 预置步骤：脚本里不必重复写窗口名。
    let mut steps: Vec<Op> = Vec::new();
    let window = read_optional_text(arguments.get("window"), "window")?;
    let handle = read_optional_handle(arguments.get("window_handle"))?;
    if window.is_some() || handle.is_some() {
        if window.is_some() && handle.is_some() {
            return Err(ToolError::new(
                "window 与 window_handle 只能给一个：标题匹配与句柄是两种定位方式。",
            ));
        }
        steps.push(Op::Window {
            handle,
            title_contains: window,
        });
    }

    let mut budget = Budget {
        expanded: 0,
        wait_ms: 0,
        total_steps: 0,
    };
    for (index, raw) in raw_steps.iter().enumerate() {
        let Some(object) = raw.as_object() else {
            return Err(ToolError::new(format!("steps[{index}] 必须是步骤对象。")));
        };
        let op = parse_op(object, default_origin, default_on_error)
            .map_err(|error| ToolError::new(format!("steps[{index}] 失败：{}", error.message)))?;
        push_with_budget(&mut steps, op, &mut budget)
            .map_err(|error| ToolError::new(format!("steps[{index}] 失败：{}", error.message)))?;
    }

    Ok(Plan {
        default_origin,
        dry_run,
        steps,
    })
}

/// 脚本规模预算：步骤数、展开后的步骤数与累计等待时间各有一条上限。
struct Budget {
    expanded: usize,
    wait_ms: i64,
    total_steps: usize,
}

/// 一条 `stroke` 实际会发出的采样点数。
///
/// 采样数由几何决定，与 `step_delay_ms` 无关：间距为 0 时等待预算算不出任何东西，
/// 但 `draw_stroke` 照样要把这些点逐个发出去，所以展开预算必须按它计费。
fn stroke_samples(points: &[Offset], spacing: f64) -> i64 {
    sampled_count(polyline_length(points), spacing)
}

fn push_with_budget(steps: &mut Vec<Op>, op: Op, budget: &mut Budget) -> Result<(), ToolError> {
    budget.total_steps += 1;
    if budget.total_steps > MAX_STEPS {
        return Err(ToolError::new(format!(
            "步骤总数不能超过 {MAX_STEPS} 个（含 repeat 的成员）。"
        )));
    }
    budget.expanded += expanded_size(&op);
    if budget.expanded > MAX_EXPANDED_STEPS {
        return Err(ToolError::new(format!(
            "repeat 展开后的步骤总数不能超过 {MAX_EXPANDED_STEPS} 个。"
        )));
    }
    budget.wait_ms += declared_wait_ms(&op);
    if budget.wait_ms > MAX_TOTAL_WAIT_MS {
        return Err(ToolError::new(format!(
            "脚本累计等待不能超过 {MAX_TOTAL_WAIT_MS} 毫秒。"
        )));
    }
    steps.push(op);
    Ok(())
}

fn expanded_size(op: &Op) -> usize {
    match op {
        Op::Repeat { times, body } => {
            let inner: usize = body.iter().map(expanded_size).sum();
            inner.saturating_mul(*times as usize)
        }
        // 描边一步会发出与采样点数同样多的输入事件，按 1 计费就能被 repeat 放大成
        // 千万级事件，绕过 `MAX_EXPANDED_STEPS`。
        Op::Stroke { points, spacing, .. } => stroke_samples(points, *spacing).max(0) as usize,
        _ => 1,
    }
}

fn declared_wait_ms(op: &Op) -> i64 {
    match op {
        Op::Wait { ms } => *ms,
        Op::Repeat { times, body } => body
            .iter()
            .map(declared_wait_ms)
            .sum::<i64>()
            .saturating_mul(*times),
        Op::Stroke {
            points,
            spacing,
            step_delay_ms,
            ..
        } => stroke_samples(points, *spacing).saturating_mul(*step_delay_ms),
        // 执行侧的 focus 固定等一小会儿让前台切换完成，这段也要计入预算，
        // 否则 repeat 能把挂死时间藏在「等待为 0」的背后。
        Op::Focus => FOCUS_SETTLE_MS,
        _ => 0,
    }
}

/// 折线总长：`stroke` 的重采样密度按它分配。
///
/// 用 `saturating_sub` 而不是 `-`：坐标是模型输入，极端值相减在开启溢出检查的
/// 构建里会 panic，而这里只需要一个「足够大」的长度让采样数封顶。
pub fn polyline_length(points: &[Offset]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let dx = pair[1].x.saturating_sub(pair[0].x) as f64;
            let dy = pair[1].y.saturating_sub(pair[0].y) as f64;
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

/// 按间距算出的采样点数：至少两点（起点与终点），最多 [`MAX_STROKE_SAMPLES`]。
pub fn sampled_count(length: f64, spacing: f64) -> i64 {
    if spacing <= 0.0 || !length.is_finite() {
        return 2;
    }
    let count = (length / spacing).ceil();
    if !count.is_finite() || count >= MAX_STROKE_SAMPLES as f64 {
        return MAX_STROKE_SAMPLES as i64;
    }
    (count as i64 + 1).clamp(2, MAX_STROKE_SAMPLES as i64)
}

fn parse_op(
    object: &Map<String, Value>,
    default_origin: Origin,
    default_on_error: OnError,
) -> Result<Op, ToolError> {
    let Some(Value::String(raw_op)) = object.get("op") else {
        return Err(ToolError::new(
            "op 必须是字符串；可用值：click、drag、expect、focus、key、move、repeat、shot、stroke、type、wait、wheel、window。",
        ));
    };
    let op = raw_op.trim().to_lowercase();
    if !OPS.contains(&op.as_str()) {
        let mut sorted = OPS.to_vec();
        sorted.sort_unstable();
        return Err(ToolError::new(format!(
            "op 不支持：{raw_op}。可用值：{}。",
            sorted.join("、")
        )));
    }
    let origin = read_origin(object.get("coord"), default_origin)?;
    reject_unknown_keys(object, &op_keys(&op)?, &op)?;
    Ok(match op.as_str() {
        "focus" => Op::Focus,
        "window" => {
            let title = read_optional_text(object.get("window"), "window")?;
            let handle = read_optional_handle(object.get("window_handle"))?;
            if title.is_some() == handle.is_some() {
                return Err(ToolError::new(
                    "window 步骤要且只要给 window 或 window_handle 之一。",
                ));
            }
            Op::Window {
                handle,
                title_contains: title,
            }
        }
        "move" => Op::Move {
            at: read_offset(object, "at", origin, true)?.expect("必填 at"),
        },
        "click" => Op::Click {
            at: read_offset(object, "at", origin, true)?.expect("必填 at"),
            button: read_button(object)?,
            clicks: read_int(object.get("clicks"), 1, 1, 3, "clicks")?,
        },
        "drag" => {
            let from = read_offset(object, "from", origin, true)?.expect("必填 from");
            let to = read_offset(object, "to", origin, true)?.expect("必填 to");
            Op::Drag {
                from,
                to,
                button: read_button(object)?,
                steps: read_int(object.get("steps"), 1, 0, 240, "steps")?,
            }
        }
        "wheel" => Op::Wheel {
            at: read_offset(object, "at", origin, false)?,
            delta: read_int(object.get("delta"), -120, -12_000, 12_000, "delta")?,
        },
        "key" => {
            let keys = read_keys(object.get("keys"))?;
            let presses = read_int(object.get("presses"), 1, 1, 10, "presses")?;
            // 多键组合是按一次「按下全部、再倒序抬起」发的，重复次数没有意义；
            // 静默只按一次会让脚本作者以为按了多次。
            if keys.len() > 1 && presses > 1 {
                return Err(ToolError::new(
                    "多键组合（如 [\"ctrl\",\"s\"]）只能按一次，presses 必须为 1。",
                ));
            }
            Op::Key { keys, presses }
        }
        "type" => {
            let Some(Value::String(text)) = object.get("text") else {
                return Err(ToolError::new("type 的 text 必须是非空字符串。"));
            };
            if text.is_empty() {
                return Err(ToolError::new("type 的 text 必须是非空字符串。"));
            }
            if text.chars().count() > 4_096 {
                return Err(ToolError::new("type 的 text 不能超过 4096 个字符。"));
            }
            Op::Type { text: text.clone() }
        }
        "stroke" => read_stroke(object, origin)?,
        "wait" => Op::Wait {
            ms: read_int(object.get("ms"), 100, 0, 60_000, "ms")?,
        },
        "expect" => Op::Expect {
            probe: read_probe(object, origin)?,
            on_error: read_on_error(object.get("on_error"), default_on_error)?,
        },
        "shot" => Op::Shot {
            as_name: read_optional_text(object.get("as"), "as")?
                .unwrap_or_else(|| "shot".to_string()),
        },
        "repeat" => {
            let times = read_int(object.get("times"), 2, 1, 500, "times")?;
            let Some(Value::Array(body)) = object.get("steps") else {
                return Err(ToolError::new(
                    "repeat 的 steps 必须是非空步骤数组（重复体里的步骤照常写 op）。",
                ));
            };
            if body.is_empty() {
                return Err(ToolError::new("repeat 的 steps 不能为空。"));
            }
            let mut inner_steps = Vec::new();
            for (index, raw) in body.iter().enumerate() {
                let Some(inner) = raw.as_object() else {
                    return Err(ToolError::new(format!(
                        "repeat.steps[{index}] 必须是步骤对象。"
                    )));
                };
                if inner
                    .get("op")
                    .and_then(Value::as_str)
                    .map(|name| name.trim().to_lowercase())
                    .as_deref()
                    == Some("repeat")
                {
                    return Err(ToolError::new(
                        "repeat 不能再嵌套 repeat：把次数乘进外层的 times。",
                    ));
                }
                inner_steps.push(parse_op(inner, origin, default_on_error).map_err(|error| {
                    ToolError::new(format!("repeat.steps[{index}] 失败：{}", error.message))
                })?);
            }
            Op::Repeat {
                times,
                body: inner_steps,
            }
        }
        _ => unreachable!("OPS 与分支必须一一对应"),
    })
}

fn op_keys(op: &str) -> Result<&'static [&'static str], ToolError> {
    let keys: &'static [&'static str] = match op {
        "focus" => &["op", "coord"],
        "window" => &["op", "coord", "window", "window_handle"],
        "move" => &["op", "coord", "at"],
        "click" => &["op", "coord", "at", "button", "clicks"],
        "drag" => &["op", "coord", "from", "to", "button", "steps"],
        "wheel" => &["op", "coord", "at", "delta"],
        "key" => &["op", "keys", "presses"],
        "type" => &["op", "text"],
        "stroke" => &[
            "op",
            "coord",
            "points",
            "button",
            "spacing",
            "step_delay_ms",
            "jitter",
            "pressure",
        ],
        "wait" => &["op", "ms"],
        "expect" => &["op", "coord", "at", "color", "tolerance", "on_error"],
        "shot" => &["op", "as"],
        "repeat" => &["op", "times", "steps"],
        _ => return Err(ToolError::new(format!("op 不支持：{op}。"))),
    };
    Ok(keys)
}

fn read_stroke(object: &Map<String, Value>, origin: Origin) -> Result<Op, ToolError> {
    let Some(Value::Array(raw_points)) = object.get("points") else {
        return Err(ToolError::new(
            "stroke 的 points 必须是坐标数组，例如 [[10,10],[80,40],[160,20]]。",
        ));
    };
    if raw_points.len() < 2 {
        return Err(ToolError::new("stroke 至少需要两个点。"));
    }
    if raw_points.len() > MAX_STROKE_POINTS {
        return Err(ToolError::new(format!(
            "stroke 的控制点不能超过 {MAX_STROKE_POINTS} 个。"
        )));
    }
    let mut points = Vec::with_capacity(raw_points.len());
    for raw in raw_points {
        points.push(parse_offset(raw, origin)?);
    }
    let spacing = read_float(object.get("spacing"), 4.0, 0.5, 200.0, "spacing")?;
    let step_delay_ms = read_int(object.get("step_delay_ms"), 2, 0, 50, "step_delay_ms")?;
    let jitter = read_float(object.get("jitter"), 0.0, 0.0, 20.0, "jitter")?;
    let pressure = read_pressure(object.get("pressure"))?;
    Ok(Op::Stroke {
        points,
        button: read_button(object)?,
        spacing,
        step_delay_ms,
        jitter,
        pressure,
    })
}

fn read_probe(object: &Map<String, Value>, origin: Origin) -> Result<PixelProbe, ToolError> {
    let at = read_offset(object, "at", origin, true)?.expect("必填 at");
    let color = read_color(object.get("color"))?;
    let tolerance = read_int(object.get("tolerance"), 12, 0, 255, "tolerance")?;
    Ok(PixelProbe {
        offset: at,
        color,
        tolerance: tolerance as u8,
    })
}

fn read_color(value: Option<&Value>) -> Result<[u8; 3], ToolError> {
    let Some(Value::String(text)) = value else {
        return Err(ToolError::new(
            "expect 的 color 必须是 #rrggbb 形式的字符串。",
        ));
    };
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ToolError::new(format!(
            "color 格式不对：{text}。要写成 #rrggbb，例如 #1E1E1E。"
        )));
    }
    let channel = |index: usize| u8::from_str_radix(&hex[index..index + 2], 16).unwrap_or(0);
    Ok([channel(0), channel(2), channel(4)])
}

fn read_button(object: &Map<String, Value>) -> Result<String, ToolError> {
    let raw = match object.get("button") {
        None | Some(Value::Null) => return Ok("left".to_string()),
        Some(Value::String(text)) => text.clone(),
        Some(_) => return Err(ToolError::new("button 必须是字符串。")),
    };
    input::validate_button(&raw)
}

fn read_keys(value: Option<&Value>) -> Result<Vec<String>, ToolError> {
    let Some(Value::Array(raw_keys)) = value else {
        return Err(ToolError::new(
            "key 的 keys 必须是 1 到 5 个键名组成的数组，例如 [\"ctrl\",\"s\"]。",
        ));
    };
    if raw_keys.is_empty() || raw_keys.len() > 5 {
        return Err(ToolError::new(
            "key 的 keys 必须是 1 到 5 个键名组成的数组，例如 [\"ctrl\",\"s\"]。",
        ));
    }
    let mut keys = Vec::with_capacity(raw_keys.len());
    for raw in raw_keys {
        let Some(text) = raw.as_str() else {
            return Err(ToolError::new("keys 里的每一项都必须是字符串。"));
        };
        keys.push(input::validate_key(text)?);
    }
    let mut sorted: Vec<&String> = keys.iter().collect();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != keys.len() {
        return Err(ToolError::new("keys 不能包含重复键名。"));
    }
    Ok(keys)
}

fn read_offset(
    object: &Map<String, Value>,
    key: &str,
    origin: Origin,
    required: bool,
) -> Result<Option<Offset>, ToolError> {
    match object.get(key) {
        None | Some(Value::Null) => {
            if required {
                return Err(ToolError::new(format!(
                    "该 op 必须提供 {key}：[x, y] 或 {{\"x\":10,\"y\":20}}。"
                )));
            }
            Ok(None)
        }
        Some(value) => parse_offset(value, origin).map(Some),
    }
}

/// 坐标：`[x, y]` 或 `{"x":…,"y":…}`，两者都按整数物理像素读。
fn parse_offset(value: &Value, origin: Origin) -> Result<Offset, ToolError> {
    let pair = match value {
        Value::Array(items) if items.len() == 2 => {
            let x = read_number(&items[0], "坐标 x")?;
            let y = read_number(&items[1], "坐标 y")?;
            (x, y)
        }
        Value::Object(map) => {
            let x = read_number(map.get("x").unwrap_or(&Value::Null), "坐标 x")?;
            let y = read_number(map.get("y").unwrap_or(&Value::Null), "坐标 y")?;
            (x, y)
        }
        _ => return Err(ToolError::new("坐标要写成 [x, y] 或 {\"x\":10,\"y\":20}。")),
    };
    Ok(Offset {
        x: pair.0,
        y: pair.1,
        origin,
    })
}

fn read_number(value: &Value, label: &str) -> Result<i64, ToolError> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.round() as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
    .ok_or_else(|| ToolError::new(format!("{label} 必须是整数像素值。")))
}

fn read_origin(value: Option<&Value>, default: Origin) -> Result<Origin, ToolError> {
    let Some(value) = value.filter(|item| !item.is_null()) else {
        return Ok(default);
    };
    let Value::String(text) = value else {
        return Err(ToolError::new(
            "coord 必须是 screen、client 或 window 之一。",
        ));
    };
    match text.trim().to_lowercase().as_str() {
        "screen" => Ok(Origin::Screen),
        "client" | "window_client" => Ok(Origin::Client),
        "window" | "window_rect" => Ok(Origin::Window),
        other => Err(ToolError::new(format!(
            "coord 不支持：{other}。可用值：client、screen、window。"
        ))),
    }
}

fn read_on_error(value: Option<&Value>, default: OnError) -> Result<OnError, ToolError> {
    let Some(value) = value.filter(|item| !item.is_null()) else {
        return Ok(default);
    };
    let Value::String(text) = value else {
        return Err(ToolError::new("on_error 必须是 abort 或 continue。"));
    };
    match text.trim().to_lowercase().as_str() {
        "abort" | "stop" => Ok(OnError::Abort),
        "continue" | "ignore" => Ok(OnError::Continue),
        other => Err(ToolError::new(format!(
            "on_error 不支持：{other}。可用值：abort、continue。"
        ))),
    }
}

fn read_optional_text(value: Option<&Value>, key: &str) -> Result<Option<String>, ToolError> {
    let Some(value) = value.filter(|item| !item.is_null()) else {
        return Ok(None);
    };
    let Value::String(text) = value else {
        return Err(ToolError::new(format!("{key} 必须是字符串。")));
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

fn read_optional_handle(value: Option<&Value>) -> Result<Option<isize>, ToolError> {
    let Some(value) = value.filter(|item| !item.is_null()) else {
        return Ok(None);
    };
    let handle = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.trunc() as i64)),
        Value::String(text) => {
            let text = text.trim();
            if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16).ok()
            } else if text.chars().all(|ch| ch.is_ascii_digit()) {
                text.parse::<i64>().ok()
            } else if text.chars().all(|ch| ch.is_ascii_hexdigit()) {
                i64::from_str_radix(text, 16).ok()
            } else {
                None
            }
        }
        _ => None,
    };
    match handle {
        Some(handle) if handle > 0 => Ok(Some(handle as isize)),
        _ => Err(ToolError::new(
            "window_handle 必须是正整数或十六进制字符串（如 0x000000000003099C）。",
        )),
    }
}

fn read_pressure(value: Option<&Value>) -> Result<(f64, f64), ToolError> {
    let Some(value) = value.filter(|item| !item.is_null()) else {
        return Ok((1.0, 1.0));
    };
    let Value::Array(items) = value else {
        return Err(ToolError::new(
            "pressure 必须是 [起点压力, 终点压力]，各取 0.0–1.0。",
        ));
    };
    if items.len() != 2 {
        return Err(ToolError::new(
            "pressure 必须是 [起点压力, 终点压力]，各取 0.0–1.0。",
        ));
    }
    let read = |item: &Value| -> Result<f64, ToolError> {
        item.as_f64()
            .filter(|number| (0.0..=1.0).contains(number))
            .ok_or_else(|| ToolError::new("pressure 的每一项都必须是 0.0 到 1.0 的数字。"))
    };
    Ok((read(&items[0])?, read(&items[1])?))
}

fn read_int(
    value: Option<&Value>,
    default: i64,
    minimum: i64,
    maximum: i64,
    key: &str,
) -> Result<i64, ToolError> {
    let parsed = match value.filter(|item| !item.is_null()) {
        None => return Ok(default),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.round() as i64)),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        Some(_) => None,
    };
    let Some(parsed) = parsed else {
        return Err(ToolError::new(format!("{key} 必须是整数。")));
    };
    if parsed < minimum || parsed > maximum {
        return Err(ToolError::new(format!(
            "{key} 必须在 {minimum} 到 {maximum} 之间，当前为 {parsed}。"
        )));
    }
    Ok(parsed)
}

fn read_float(
    value: Option<&Value>,
    default: f64,
    minimum: f64,
    maximum: f64,
    key: &str,
) -> Result<f64, ToolError> {
    let parsed = match value.filter(|item| !item.is_null()) {
        None => return Ok(default),
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    let Some(parsed) = parsed else {
        return Err(ToolError::new(format!("{key} 必须是数字。")));
    };
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ToolError::new(format!(
            "{key} 必须在 {minimum} 到 {maximum} 之间。"
        )));
    }
    Ok(parsed)
}

fn read_bool(value: Option<&Value>, default: bool, key: &str) -> Result<bool, ToolError> {
    match value.filter(|item| !item.is_null()) {
        None => Ok(default),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(ToolError::new(format!("{key} 必须是布尔值。"))),
    }
}

fn reject_unknown_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    prefix: &str,
) -> Result<(), ToolError> {
    let mut unsupported: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    unsupported.sort_unstable();
    if unsupported.is_empty() {
        return Ok(());
    }
    let scope = if prefix.is_empty() {
        "顶层".to_string()
    } else {
        format!("{prefix} 步骤")
    };
    Err(ToolError::new(format!(
        "{scope}不支持参数：{}；可用参数：{}。",
        unsupported.join("、"),
        allowed.join("、")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(value: Value) -> Result<Plan, ToolError> {
        parse(value.as_object().expect("参数对象"))
    }

    #[test]
    fn a_script_parses_into_an_ordered_plan() {
        let parsed = plan(json!({
            "window": "画笔",
            "coord": "client",
            "steps": [
                {"op": "focus"},
                {"op": "click", "at": [10, 20]},
                {"op": "stroke", "points": [[0,0],[100,0]]},
                {"op": "wait", "ms": 200}
            ]
        }))
        .expect("脚本应当解析成功");
        assert_eq!(parsed.default_origin, Origin::Client);
        assert!(!parsed.dry_run);
        // 顶层 window 摊成第一条定位步骤，脚本里不必重复写窗口名。
        assert_eq!(
            parsed.steps.first(),
            Some(&Op::Window {
                handle: None,
                title_contains: Some("画笔".to_string()),
            })
        );
        assert_eq!(parsed.steps.len(), 5);
        assert!(matches!(parsed.steps[2], Op::Click { .. }));
        assert!(matches!(parsed.steps[3], Op::Stroke { .. }));
        assert_eq!(parsed.steps[4], Op::Wait { ms: 200 });
    }

    #[test]
    fn unknown_op_and_unknown_key_are_rejected_before_any_input() {
        let error = plan(json!({"steps": [{"op": "telekinesis"}]})).expect_err("未知 op");
        assert!(error.message.contains("op 不支持"), "{}", error.message);

        let error = plan(json!({"steps": [{"op": "click", "at": [1,2], "speed": 3}]}))
            .expect_err("未知参数");
        assert!(
            error.message.contains("click 步骤不支持参数"),
            "{}",
            error.message
        );
    }

    #[test]
    fn coordinates_must_be_complete_pairs() {
        let error = plan(json!({"steps": [{"op": "click"}]})).expect_err("缺 at");
        assert!(error.message.contains("必须提供 at"), "{}", error.message);

        let error = plan(json!({"steps": [{"op": "move", "at": [10]}]})).expect_err("缺一个");
        assert!(error.message.contains("坐标要写成"), "{}", error.message);
    }

    #[test]
    fn repeat_expansion_is_bounded() {
        let error = plan(json!({
            "steps": [{"op": "repeat", "times": 500, "steps": [
                {"op": "wait", "ms": 60000}
            ]}]
        }))
        .expect_err("累计等待应当超限");
        assert!(error.message.contains("累计等待"), "{}", error.message);

        let error = plan(json!({
            "steps": [{"op": "repeat", "times": 500, "steps": [
                {"op": "repeat", "times": 2, "steps": [{"op": "focus"}]}
            ]}]
        }))
        .expect_err("嵌套 repeat 应当被拒");
        assert!(error.message.contains("不能再嵌套"), "{}", error.message);
    }

    #[test]
    fn stroke_resampling_is_proportional_to_length() {
        let points = vec![
            Offset {
                x: 0,
                y: 0,
                origin: Origin::Screen,
            },
            Offset {
                x: 100,
                y: 0,
                origin: Origin::Screen,
            },
            Offset {
                x: 100,
                y: 100,
                origin: Origin::Screen,
            },
        ];
        assert_eq!(polyline_length(&points), 200.0);
        // 间距 4 像素 → 200 / 4 + 1 = 51 个采样点。
        assert_eq!(sampled_count(polyline_length(&points), 4.0), 51);
        // 零间距退化为「只连起止点」，不制造除零。
        assert_eq!(sampled_count(200.0, 0.0), 2);
    }

    /// 描边一步的输入事件数由几何决定，`step_delay_ms=0` 时等待预算恒为 0，
    /// 因此必须由展开预算挡住：否则 repeat 能把千万级移动发到桌面。
    #[test]
    fn stroke_samples_count_towards_the_expansion_budget() {
        let error = plan(json!({
            "steps": [{"op": "repeat", "times": 500, "steps": [
                {"op": "stroke", "points": [[0,0],[1000,0]], "spacing": 0.5,
                 "step_delay_ms": 0}
            ]}]
        }))
        .expect_err("描边采样数应当计入展开预算");
        assert!(
            error.message.contains("展开后的步骤总数"),
            "{}",
            error.message
        );
    }

    /// 极端坐标不能让解析阶段 panic：`i64` 相减在开启溢出检查时会炸。
    #[test]
    fn extreme_coordinates_do_not_overflow() {
        let parsed = plan(json!({
            "steps": [{"op": "stroke", "points": [
                ["9223372036854775807", 0], ["-9223372036854775808", 0]
            ], "spacing": 0.5}]
        }))
        .expect("极端坐标应当被采样上限兜住，而不是 panic");
        let Op::Stroke { points, spacing, .. } = &parsed.steps[0] else {
            panic!("应当是 stroke");
        };
        assert_eq!(sampled_count(polyline_length(points), *spacing), 20_000);
    }

    /// 多键组合是按一次「按下全部、再倒序抬起」发的，`presses > 1` 没有意义。
    #[test]
    fn multi_key_chords_reject_repeated_presses() {
        let error = plan(json!({
            "steps": [{"op": "key", "keys": ["ctrl", "s"], "presses": 3}]
        }))
        .expect_err("多键组合不该接受重复次数");
        assert!(error.message.contains("只能按一次"), "{}", error.message);

        // 单键重复仍然有效。
        plan(json!({"steps": [{"op": "key", "keys": ["delete"], "presses": 3}]}))
            .expect("单键可以重复");
    }

    /// `focus` 执行时会固定等一小会儿，这段等待同样受总时长预算约束。
    #[test]
    fn focus_settle_time_counts_towards_the_wait_budget() {
        // repeat 单次上限 500：四条各 500 次的 focus 合起来是 160 秒，越过 120 秒预算。
        let focus = json!({"op": "repeat", "times": 500, "steps": [{"op": "focus"}]});
        let error = plan(json!({
            "steps": [focus.clone(), focus.clone(), focus.clone(), focus]
        }))
        .expect_err("大量重复的 focus 应当被累计等待挡住");
        assert!(error.message.contains("累计等待"), "{}", error.message);
    }

    #[test]
    fn colors_are_read_as_rrggbb() {
        let parsed = plan(json!({
            "steps": [{"op": "expect", "at": [1,2], "color": "#1E2F3A"}]
        }))
        .expect("脚本应当解析成功");
        let Op::Expect { probe, .. } = parsed.steps[0] else {
            panic!("应当是 expect");
        };
        assert_eq!(probe.color, [0x1E, 0x2F, 0x3A]);

        let error = plan(json!({"steps": [{"op": "expect", "at": [1,2], "color": "red"}]}))
            .expect_err("颜色格式不对");
        assert!(error.message.contains("#rrggbb"), "{}", error.message);
    }

    #[test]
    fn key_names_are_validated_at_parse_time() {
        let error =
            plan(json!({"steps": [{"op": "key", "keys": ["hyperkey"]}]})).expect_err("未知键名");
        assert!(error.message.contains("不支持的键名"), "{}", error.message);

        let error = plan(json!({"steps": [{"op": "key", "keys": ["ctrl", "ctrl"]}]}))
            .expect_err("重复键名");
        assert!(error.message.contains("重复键名"), "{}", error.message);
    }
}
