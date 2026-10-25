//! Canvas 2D 软件光栅化：让 `getImageData` / `toDataURL` 产出真实像素。
//!
//! 为什么自己写光栅化而不是「返回恒定哈希」：探测器比较的不只是「同一页面两次取值相同」，
//! 还会绘制已知图形再核对像素分布（渐变方向、文字覆盖区域、`fillRect` 覆盖后是否透明）。
//! 恒定位图能过一致性校验，过不了这类语义校验。
//!
//! 边界：
//! - 只做 `getImageData`/`toDataURL` 需要的子集：`fillRect`/`clearRect`/`fill`/`stroke` 的
//!   矩形与路径、纯色与线性/径向渐变填充、`fillText`（-300..900 内的常规字重）。
//! - 文字用系统 TTF 走 `ttf-parser` 取轮廓、自己抹抗锯齿。不同机器上的字体度量一致，
//!   因此同一段文字在同一系统上产出同一像素；换系统字体版本才会变。
//! - 不做 `globalCompositeOperation` 里 blur/阴影这类需要卷积的模式。

use std::collections::HashMap;
use std::f64::consts::TAU;

/// 一张离屏画布：预乘前的 RGBA8，行优先。
#[derive(Debug, Clone)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Self {
        let length = (width as usize) * (height as usize) * 4;
        Self {
            width,
            height,
            data: vec![0; length],
        }
    }

    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(((y as usize) * (self.width as usize) + x as usize) * 4)
    }

    /// 源覆盖式混合：一次只画一个像素。`alpha` 是额外覆盖率（抗锯齿用）。
    pub fn blend(&mut self, x: u32, y: u32, color: [u8; 4], alpha: f64) {
        let Some(index) = self.index(x, y) else {
            return;
        };
        let source_alpha = (color[3] as f64 / 255.0) * alpha.clamp(0.0, 1.0);
        if source_alpha <= 0.0 {
            return;
        }
        let destination_alpha = self.data[index + 3] as f64 / 255.0;
        // source-over：输出 alpha = Sa + Da*(1-Sa)
        let out_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
        if out_alpha <= 0.0 {
            return;
        }
        for channel in 0..3 {
            let source = color[channel] as f64;
            let destination = self.data[index + channel] as f64;
            let mixed = (source * source_alpha
                + destination * destination_alpha * (1.0 - source_alpha))
                / out_alpha;
            self.data[index + channel] = mixed.round().clamp(0.0, 255.0) as u8;
        }
        self.data[index + 3] = (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// 轴对齐矩形填充，覆盖率按像素中心判断（与浏览器的非抗锯齿 `fillRect` 接近）。
    pub fn fill_rect(&mut self, x: f64, y: f64, width: f64, height: f64, color: [u8; 4]) {
        let left = x.min(x + width);
        let right = x.max(x + width);
        let top = y.min(y + height);
        let bottom = y.max(y + height);
        let start_x = left.floor().max(0.0) as u32;
        let start_y = top.floor().max(0.0) as u32;
        let end_x = (right.ceil().max(0.0) as u32).min(self.width);
        let end_y = (bottom.ceil().max(0.0) as u32).min(self.height);
        for py in start_y..end_y {
            for px in start_x..end_x {
                let coverage = axis_aligned_coverage(
                    px as f64,
                    py as f64,
                    left,
                    top,
                    right,
                    bottom,
                );
                if coverage > 0.0 {
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    /// 用扫描线覆盖率填充任意多边形（`fill()` 的落点）。
    ///
    /// 每个像素取 4×4 子采样求覆盖比例：足以让斜边与曲线不出现明显锯齿，
    /// 又不必实现完整的抗锯齿几何管线。
    pub fn fill_polygon(&mut self, points: &[(f64, f64)], color: [u8; 4]) {
        if points.len() < 3 {
            return;
        }
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        for (x, y) in points {
            min_y = min_y.min(*y);
            max_y = max_y.max(*y);
            min_x = min_x.min(*x);
            max_x = max_x.max(*x);
        }
        if !min_y.is_finite() || !max_y.is_finite() {
            return;
        }
        let start_y = min_y.floor().max(0.0) as u32;
        let end_y = (max_y.ceil().max(0.0) as u32).min(self.height);
        let start_x = min_x.floor().max(0.0) as u32;
        let end_x = (max_x.ceil().max(0.0) as u32).min(self.width);
        const SAMPLES: u32 = 4;
        let step = 1.0 / SAMPLES as f64;
        for py in start_y..end_y {
            for px in start_x..end_x {
                let mut hits = 0u32;
                for sub_y in 0..SAMPLES {
                    for sub_x in 0..SAMPLES {
                        let sample_x = px as f64 + (sub_x as f64 + 0.5) * step;
                        let sample_y = py as f64 + (sub_y as f64 + 0.5) * step;
                        if point_in_polygon(points, sample_x, sample_y) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    let coverage = hits as f64 / (SAMPLES * SAMPLES) as f64;
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    /// 描边：沿折线铺圆头笔帽的线段，等价于 `lineWidth` 宽度的 `stroke()`。
    pub fn stroke_polygon(&mut self, points: &[(f64, f64)], color: [u8; 4], line_width: f64, closed: bool) {
        let half = (line_width.max(1.0)) / 2.0;
        let segments = if closed {
            points.len()
        } else {
            points.len().saturating_sub(1)
        };
        for index in 0..segments {
            let (x0, y0) = points[index];
            let (x1, y1) = points[(index + 1) % points.len()];
            self.fill_capsule(x0, y0, x1, y1, color, half);
        }
        if !closed && line_width > 2.0 {
            // 圆头笔帽：端点再补一个圆，避免粗线拐角缺角。
            for (x, y) in points {
                self.fill_circle(*x, *y, color, half);
            }
        }
    }

    fn fill_capsule(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, color: [u8; 4], half: f64) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let length_squared = dx * dx + dy * dy;
        let min_x = (x0.min(x1) - half).floor().max(0.0) as u32;
        let max_x = ((x0.max(x1) + half).ceil().max(0.0) as u32).min(self.width);
        let min_y = (y0.min(y1) - half).floor().max(0.0) as u32;
        let max_y = ((y0.max(y1) + half).ceil().max(0.0) as u32).min(self.height);
        for py in min_y..max_y {
            for px in min_x..max_x {
                let center_x = px as f64 + 0.5;
                let center_y = py as f64 + 0.5;
                let projection = if length_squared <= f64::EPSILON {
                    0.0
                } else {
                    (((center_x - x0) * dx + (center_y - y0) * dy) / length_squared).clamp(0.0, 1.0)
                };
                let nearest_x = x0 + projection * dx;
                let nearest_y = y0 + projection * dy;
                let distance = ((center_x - nearest_x).powi(2) + (center_y - nearest_y).powi(2)).sqrt();
                // 边缘一像素内做线性过渡，否则斜线会是硬锯齿。
                let coverage = (half + 0.5 - distance).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    pub fn fill_circle(&mut self, center_x: f64, center_y: f64, color: [u8; 4], radius: f64) {
        let min_x = (center_x - radius).floor().max(0.0) as u32;
        let max_x = ((center_x + radius).ceil().max(0.0) as u32).min(self.width);
        let min_y = (center_y - radius).floor().max(0.0) as u32;
        let max_y = ((center_y + radius).ceil().max(0.0) as u32).min(self.height);
        for py in min_y..max_y {
            for px in min_x..max_x {
                let distance = ((px as f64 + 0.5 - center_x).powi(2)
                    + (py as f64 + 0.5 - center_y).powi(2))
                .sqrt();
                let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    /// 线性渐变：沿 `(x0,y0)→(x1,y1)` 取色，超出端点按端点色截断（与 Canvas 规范一致）。
    pub fn fill_rect_gradient(
        &mut self,
        left: f64,
        top: f64,
        width: f64,
        height: f64,
        gradient: &Gradient,
    ) {
        let right = left + width;
        let bottom = top + height;
        let start_x = left.floor().max(0.0) as u32;
        let start_y = top.floor().max(0.0) as u32;
        let end_x = (right.ceil().max(0.0) as u32).min(self.width);
        let end_y = (bottom.ceil().max(0.0) as u32).min(self.height);
        for py in start_y..end_y {
            for px in start_x..end_x {
                let coverage = axis_aligned_coverage(
                    px as f64,
                    py as f64,
                    left,
                    top,
                    right,
                    bottom,
                );
                if coverage <= 0.0 {
                    continue;
                }
                let color = gradient.color_at(px as f64 + 0.5, py as f64 + 0.5);
                self.blend(px, py, color, coverage);
            }
        }
    }

    /// 文字栅格化：逐字形取轮廓、按非零环绕规则求覆盖。
    pub fn fill_text(
        &mut self,
        text: &str,
        x: f64,
        y: f64,
        size: f64,
        color: [u8; 4],
        font: Option<&FontFace>,
    ) {
        let Some(font) = font else {
            return;
        };
        let mut pen_x = x;
        for character in text.chars() {
            let Some(glyph_id) = font.glyph_for(character) else {
                pen_x += size * 0.5;
                continue;
            };
            let outline = font.outline(glyph_id, size);
            let advance = font.advance_width(glyph_id, size);
            // 基线在 y：轮廓坐标是「向上为正」，落到画布要翻转。
            let shifted: Vec<Path> = outline
                .into_iter()
                .map(|path| Path {
                    points: path
                        .points
                        .into_iter()
                        .map(|(px, py)| (pen_x + px, y - py))
                        .collect(),
                    closed: path.closed,
                })
                .collect();
            for path in &shifted {
                if path.points.len() >= 3 {
                    self.fill_polygon(&path.points, color);
                }
            }
            pen_x += advance;
        }
    }

    /// 编码成 PNG。`data:image/png;base64,...` 里的后半段就是它的 base64。
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut buffer = Vec::new();
        let mut encoder = png::Encoder::new(&mut buffer, self.width.max(1), self.height.max(1));
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&self.data)
            .map_err(|error| error.to_string())?;
        writer.finish().map_err(|error| error.to_string())?;
        Ok(buffer)
    }
}

fn axis_aligned_coverage(
    pixel_x: f64,
    pixel_y: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
) -> f64 {
    let overlap_x = (right.min(pixel_x + 1.0) - left.max(pixel_x)).clamp(0.0, 1.0);
    let overlap_y = (bottom.min(pixel_y + 1.0) - top.max(pixel_y)).clamp(0.0, 1.0);
    overlap_x * overlap_y
}

fn point_in_polygon(points: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    let count = points.len();
    let mut previous = count - 1;
    for current in 0..count {
        let (current_x, current_y) = points[current];
        let (previous_x, previous_y) = points[previous];
        let crosses = (current_y > y) != (previous_y > y);
        if crosses {
            let denominator = previous_y - current_y;
            if denominator.abs() > f64::EPSILON {
                let intersection_x =
                    current_x + (y - current_y) * (previous_x - current_x) / denominator;
                if x < intersection_x {
                    inside = !inside;
                }
            }
        }
        previous = current;
    }
    inside
}

/// 一条闭合/开放路径（`moveTo`/`lineTo`/`arc` 的累积结果）。
#[derive(Debug, Clone, Default)]
pub struct Path {
    pub points: Vec<(f64, f64)>,
    pub closed: bool,
}

/// 已注册的渐变：目标位置与色标列表。
#[derive(Debug, Clone)]
pub struct Gradient {
    pub kind: GradientKind,
    /// `[offset, r, g, b, a]`，按 offset 升序。
    pub stops: Vec<(f64, [u8; 4])>,
}

#[derive(Debug, Clone)]
pub enum GradientKind {
    Linear {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    },
    Radial {
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    },
}

impl Gradient {
    pub fn color_at(&self, x: f64, y: f64) -> [u8; 4] {
        let t = match &self.kind {
            GradientKind::Linear { x0, y0, x1, y1 } => {
                let dx = x1 - x0;
                let dy = y1 - y0;
                let length_squared = dx * dx + dy * dy;
                if length_squared <= f64::EPSILON {
                    0.0
                } else {
                    (((x - x0) * dx + (y - y0) * dy) / length_squared).clamp(0.0, 1.0)
                }
            }
            GradientKind::Radial {
                x0,
                y0,
                r0,
                x1,
                y1,
                r1,
            } => {
                // 简化：按到内圆外心连线的投影比例取色，足以让径向渐变有方向性。
                let dx = x1 - x0;
                let dy = y1 - y0;
                let length_squared = dx * dx + dy * dy;
                if length_squared <= f64::EPSILON {
                    ((x - x0).hypot(y - y0) - r0) / (r1 - r0).max(1e-6)
                } else {
                    let projection =
                        (((x - x0) * dx + (y - y0) * dy) / length_squared).clamp(0.0, 1.0);
                    0.0 + projection
                }
            }
        };
        self.sample(t)
    }

    fn sample(&self, t: f64) -> [u8; 4] {
        if self.stops.is_empty() {
            return [0, 0, 0, 0];
        }
        let t = t.clamp(0.0, 1.0);
        let mut previous = &self.stops[0];
        for stop in &self.stops {
            if t <= stop.0 {
                if stop.0 <= previous.0 {
                    return stop.1;
                }
                let ratio = (t - previous.0) / (stop.0 - previous.0);
                return [
                    lerp_channel(previous.1[0], stop.1[0], ratio),
                    lerp_channel(previous.1[1], stop.1[1], ratio),
                    lerp_channel(previous.1[2], stop.1[2], ratio),
                    lerp_channel(previous.1[3], stop.1[3], ratio),
                ];
            }
            previous = stop;
        }
        previous.1
    }
}

fn lerp_channel(start: u8, end: u8, ratio: f64) -> u8 {
    (start as f64 + (end as f64 - start as f64) * ratio)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// 一个可用的字体文件：解析结果按文件路径缓存，重复创建画布不再重读磁盘。
#[derive(Debug)]
pub struct FontFace {
    file: Vec<u8>,
    face_index: u32,
}

impl FontFace {
    pub fn load(path: &str, face_index: u32) -> Option<Self> {
        let file = std::fs::read(path).ok()?;
        let parsed = ttf_parser::Face::parse(&file, face_index).ok()?;
        // 只接受能取到基本度量的字体文件。
        parsed.units_per_em();
        Some(Self { file, face_index })
    }

    fn face(&self) -> Option<ttf_parser::Face<'_>> {
        ttf_parser::Face::parse(&self.file, self.face_index).ok()
    }

    fn glyph_for(&self, character: char) -> Option<ttf_parser::GlyphId> {
        let face = self.face()?;
        face.glyph_index(character)
    }

    fn advance_width(&self, glyph: ttf_parser::GlyphId, size: f64) -> f64 {
        let Some(face) = self.face() else {
            return size * 0.5;
        };
        let units = face.units_per_em() as f64;
        face.glyph_hor_advance(glyph).unwrap_or(units as u16 / 2) as f64 * size / units
    }

    /// 一段文字的排版宽度：按字形的 `advance` 累加，与 `fillText` 的推进一致。
    pub fn measure(&self, text: &str, size: f64) -> f64 {
        let Some(face) = self.face() else {
            return size * text.chars().count() as f64 * 0.5;
        };
        let units = face.units_per_em() as f64;
        let mut total = 0.0;
        for character in text.chars() {
            match face.glyph_index(character) {
                Some(glyph) => {
                    total += face.glyph_hor_advance(glyph).unwrap_or(units as u16 / 2) as f64
                }
                // 缺字时浏览器会显示 .notdef 或回退字体：按半宽估算，避免宽度为 0。
                None => total += units * 0.5,
            }
        }
        total * size / units
    }

    fn outline(&self, glyph: ttf_parser::GlyphId, size: f64) -> Vec<Path> {
        let Some(face) = self.face() else {
            return Vec::new();
        };
        let units = face.units_per_em() as f64;
        let scale = if units > 0.0 { size / units } else { 1.0 };
        let mut builder = OutlineBuilder::default();
        if face.outline_glyph(glyph, &mut builder).is_none() {
            // 空白字形（如空格）：没有轮廓，正常。
            return Vec::new();
        }
        builder
            .paths
            .into_iter()
            .map(|path| Path {
                points: path
                    .points
                    .into_iter()
                    .map(|(px, py)| (px * scale, py * scale))
                    .collect(),
                closed: true,
            })
            .collect()
    }
}

/// 把二次贝塞尔曲线离散成折线：真实字形轮廓几乎全是 `quad_to`。
#[derive(Default)]
struct OutlineBuilder {
    paths: Vec<Path>,
    current: Path,
    cursor: (f64, f64),
}

impl OutlineBuilder {
    fn flush(&mut self) {
        if self.current.points.len() >= 3 {
            self.paths.push(std::mem::take(&mut self.current));
        } else {
            self.current = Path::default();
        }
    }

    fn quad_to(&mut self, control: (f64, f64), end: (f64, f64)) {
        const STEPS: usize = 8;
        let start = self.cursor;
        for step in 1..=STEPS {
            let t = step as f64 / STEPS as f64;
            let one_minus = 1.0 - t;
            let x = one_minus * one_minus * start.0
                + 2.0 * one_minus * t * control.0
                + t * t * end.0;
            let y = one_minus * one_minus * start.1
                + 2.0 * one_minus * t * control.1
                + t * t * end.1;
            self.current.points.push((x, y));
        }
        self.cursor = end;
    }

    fn cubic_to(&mut self, control1: (f64, f64), control2: (f64, f64), end: (f64, f64)) {
        const STEPS: usize = 12;
        let start = self.cursor;
        for step in 1..=STEPS {
            let t = step as f64 / STEPS as f64;
            let one_minus = 1.0 - t;
            let x = one_minus.powi(3) * start.0
                + 3.0 * one_minus.powi(2) * t * control1.0
                + 3.0 * one_minus * t * t * control2.0
                + t.powi(3) * end.0;
            let y = one_minus.powi(3) * start.1
                + 3.0 * one_minus.powi(2) * t * control1.1
                + 3.0 * one_minus * t * t * control2.1
                + t.powi(3) * end.1;
            self.current.points.push((x, y));
        }
        self.cursor = end;
    }
}

impl ttf_parser::OutlineBuilder for OutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.flush();
        self.current = Path {
            points: vec![(x as f64, y as f64)],
            closed: false,
        };
        self.cursor = (x as f64, y as f64);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.current.points.push((x as f64, y as f64));
        self.cursor = (x as f64, y as f64);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.quad_to((x1 as f64, y1 as f64), (x as f64, y as f64));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.cubic_to(
            (x1 as f64, y1 as f64),
            (x2 as f64, y2 as f64),
            (x as f64, y as f64),
        );
    }

    fn close(&mut self) {
        self.flush();
    }
}

/// 把 `arc` 的角度区间离散成折线并追加到当前路径。
pub fn push_arc(points: &mut Vec<(f64, f64)>, center_x: f64, center_y: f64, radius: f64, start: f64, end: f64) {
    let span = end - start;
    let steps = ((span.abs() / TAU) * 64.0).ceil().max(2.0) as usize;
    for step in 0..=steps {
        let angle = start + span * (step as f64 / steps as f64);
        points.push((center_x + radius * angle.cos(), center_y + radius * angle.sin()));
    }
}

/// 字体路径缓存：键是「族名 + 字重」，避免每个字形都重新找文件。
pub fn font_cache() -> &'static std::sync::Mutex<HashMap<String, Option<std::sync::Arc<FontFace>>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<std::sync::Arc<FontFace>>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 从 CSS `font` 简写/族名解析出可用的字体文件。
///
/// 只在 Windows 上按注册表外的固定路径找系统字体：真机 Chrome 用的也是这些文件，
/// 因此同一段文字在两处渲染出的字形一致。
pub fn resolve_font(family: &str, bold: bool) -> Option<std::sync::Arc<FontFace>> {
    let key = format!("{family}|{bold}");
    if let Some(cached) = font_cache().lock().ok().and_then(|cache| cache.get(&key).cloned()) {
        return cached;
    }
    let candidates = font_candidates(family, bold);
    let mut resolved = None;
    for (path, face_index) in candidates {
        if let Some(face) = FontFace::load(path, face_index) {
            resolved = Some(std::sync::Arc::new(face));
            break;
        }
    }
    if let Ok(mut cache) = font_cache().lock() {
        cache.insert(key, resolved.clone());
    }
    resolved
}

fn font_candidates(family: &str, bold: bool) -> Vec<(&'static str, u32)> {
    // 常见族名映射到 Windows 自带字体文件；未识别的族按 sans-serif 处理。
    let lower = family.to_ascii_lowercase();
    let mono = lower.contains("monospace")
        || lower.contains("consolas")
        || lower.contains("courier")
        || lower.contains("menlo")
        || lower.contains("monaco");
    let serif = lower.contains("serif")
        && !lower.contains("sans-serif")
        || lower.contains("times")
        || lower.contains("song")
        || lower.contains("simsun");
    let cjk = lower.contains("yahei")
        || lower.contains("simsun")
        || lower.contains("simhei")
        || lower.contains("dengxian")
        || lower.contains("kaiti")
        || lower.contains("fangsong");

    if cjk {
        // .ttc 是字体集合：微软雅黑在 0 号面。
        return if bold {
            vec![
                ("C:/Windows/Fonts/msyhbd.ttc", 0),
                ("C:/Windows/Fonts/msyh.ttc", 0),
                ("C:/Windows/Fonts/simhei.ttf", 0),
            ]
        } else {
            vec![
                ("C:/Windows/Fonts/msyh.ttc", 0),
                ("C:/Windows/Fonts/simsun.ttc", 0),
                ("C:/Windows/Fonts/simhei.ttf", 0),
            ]
        };
    }
    if mono {
        return if bold {
            vec![
                ("C:/Windows/Fonts/consolab.ttf", 0),
                ("C:/Windows/Fonts/consola.ttf", 0),
            ]
        } else {
            vec![("C:/Windows/Fonts/consola.ttf", 0)]
        };
    }
    if serif {
        return if bold {
            vec![
                ("C:/Windows/Fonts/timesbd.ttf", 0),
                ("C:/Windows/Fonts/times.ttf", 0),
                ("C:/Windows/Fonts/arial.ttf", 0),
            ]
        } else {
            vec![
                ("C:/Windows/Fonts/times.ttf", 0),
                ("C:/Windows/Fonts/arial.ttf", 0),
            ]
        };
    }
    // sans-serif 兜底链：Arial 覆盖拉丁，缺字时退回雅黑取 CJK 字形。
    if bold {
        vec![
            ("C:/Windows/Fonts/arialbd.ttf", 0),
            ("C:/Windows/Fonts/msyhbd.ttc", 0),
        ]
    } else {
        vec![
            ("C:/Windows/Fonts/arial.ttf", 0),
            ("C:/Windows/Fonts/segoeui.ttf", 0),
            ("C:/Windows/Fonts/msyh.ttc", 0),
        ]
    }
}

/// 解析 CSS 简写 `font`（如 `16px Arial`、`bold 20px "Microsoft YaHei"`）。
///
/// 返回值是 `(字号, 是否粗体, 族名)`；解析不出字号时按 10px（Canvas 默认值）。
pub fn parse_font_shorthand(text: &str) -> (f64, bool, String) {
    let lower = text.to_ascii_lowercase();
    let bold = lower.contains("bold") || lower.contains("700") || lower.contains("800")
        || lower.contains("900");
    // 字号是第一个带 px/pt/em 单位的数值。
    let mut size = 10.0;
    let mut size_end = 0usize;
    for (index, token) in text.split_whitespace().enumerate() {
        let token_lower = token.to_ascii_lowercase();
        let digits: String = token_lower
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '.')
            .collect();
        if let Ok(value) = digits.parse::<f64>() {
            if token_lower.contains("px") {
                size = value;
            } else if token_lower.contains("pt") {
                // 1pt = 4/3 px，与浏览器一致。
                size = value * 4.0 / 3.0;
            } else if token_lower.contains("em") || token_lower.contains("rem") {
                size = value * 16.0;
            } else if token_lower.contains('%') {
                size = value * 0.16;
            } else {
                continue;
            }
            // 族名在字号之后；按 token 序号定位，避免被 `bold` 等关键字干扰。
            size_end = index;
            break;
        }
    }
    let family: String = text
        .split_whitespace()
        .skip(size_end + 1)
        .collect::<Vec<_>>()
        .join(" ");
    let family = family
        .trim()
        .trim_matches(|character| character == '"' || character == '\'')
        .split(',')
        .next()
        .unwrap_or("sans-serif")
        .trim()
        .trim_matches(|character| character == '"' || character == '\'')
        .to_string();
    let family = if family.is_empty() {
        "sans-serif".to_string()
    } else {
        family
    };
    (size.max(1.0), bold, family)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_reflects_pixels() {
        let mut red = Surface::new(8, 8);
        red.fill_rect(0.0, 0.0, 8.0, 8.0, [255, 0, 0, 255]);
        let empty = Surface::new(8, 8);
        let a = red.to_png().expect("red png");
        let b = empty.to_png().expect("empty png");
        println!("red={} empty={}", a.len(), b.len());
        assert_ne!(a, b, "不同像素必须编码出不同 PNG");
    }
}
