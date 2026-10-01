//! 工具卡里的图片预览：把工具返回的图片解码成半块字形缩略图。
//!
//! 图片来自宿主工具执行体的**视觉附件**（`read_image` 与 `windows_screenshot` 各回一张），
//! 因此预览与「模型是否在看图」无关：没开原生视觉、也没配视觉代理时，附件照样进这里。
//!
//! 三处刻意的取舍：
//!
//! 1. **只走半块字形通路**。sixel / kitty / iTerm2 都要先可靠地问终端要图形能力与像素尺寸，
//!    而 `ratatui-image` 的探测要在 stdin 上阻塞读——Windows Terminal 走的 ConPTY 未必把应答
//!    交回子进程，探测线程超时后仍会继续占着 stdin，那是主输入循环的命脉。半块字形用普通字符
//!    格与前后景色画图，不需要任何探测，任何终端都能出图。
//! 2. **解码在后台线程**。源图解码是唯一的重活（大图动辄上百毫秒），留在主线程会把界面卡住。
//! 3. **预览数量有上限**，按注册顺序淘汰最旧的：附件是 Base64 原文，逐句留在内存里会一路涨。
//!
//! 缩略图的单元格尺寸不自己算，直接从建好的协议上读（[`SlicedProtocol::size`]）：渲染层要用同一组
//! 行列数铺占位行，尺寸与库内部的取整规则不能有两份实现，否则占位行与图片会对不齐。

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};

use base64::Engine;
use image::DynamicImage;
use omnicrawl_controllers::types::ToolImageAttachment;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FilterType, Resize};

/// 缩略图的行数上限：宽度自适应，高度收在这个行数以内（用户选定的默认呈现）。
pub const MAX_PREVIEW_ROWS: u16 = 16;

/// 解码后先缩到的最长边：半块缩略图只用到终端级分辨率，源图不必整张留在内存里。
const MAX_SOURCE_EDGE: u32 = 640;

/// 同时保留的预览数量上限（超出后淘汰最旧的一张）。
const MAX_PREVIEWS: usize = 8;

/// 一次工具调用最多铺几张图（`read_image` 与截图都只回一张，这里只是兜底）。
const MAX_IMAGES_PER_CALL: usize = 4;

/// 带图片预览的工具：卡片正文换成缩略图（工具名归一化后比对）。
///
/// 只有 `read_image`：它的文本载荷是一行 `{"path":…}` 紧凑 JSON，对用户没有展示价值，
/// 正文位置正好让给图片。其余会回视觉附件的工具（`windows_screenshot`）仍按普通卡片渲染，
/// 需要时加进这张表即可（正文替换、占位分块与渲染都是同一条链路）。
pub const PREVIEW_TOOLS: &[&str] = &["read_image"];

/// 该工具名是否走图片预览正文。
pub fn has_image_preview(tool_name: &str) -> bool {
    let operation = tool_name.rsplit('.').next().unwrap_or(tool_name);
    PREVIEW_TOOLS.contains(&operation)
}

/// 按 `call_id` 索引的图片预览存储。
///
/// 渲染路径只拿得到 `&AppState`，因此实例装在 `RefCell` 里（与 `ConversationCache` 同一思路）。
pub struct ImagePreviews {
    /// 半块字形的协议工厂；[`Picker::halfblocks`] 不做终端探测，构造是纯本地的。
    picker: Picker,
    /// 已完成解码的预览，按注册顺序排列（队首最旧，超限时先淘汰）。
    entries: VecDeque<Preview>,
    /// 已注册过附件的调用：图片还没解码完时也据此铺占位行。
    ///
    /// 没有它就会出现「卡片先按空块排版、图片到了再撑开」的跳变；已解码但**失败**的图会从
    /// 集合里去掉，块随之消失。
    known: HashSet<String>,
    sender: Sender<Decoded>,
    receiver: Receiver<Decoded>,
    /// 还在后台解码的图片数（事件循环据此按活动帧率醒来取结果）。
    pending: usize,
}

/// 一张已解码的预览图，以及它按某个可用宽度建好的缩略图。
struct Preview {
    /// 拥有这张图的工具调用；渲染层按 `call_id` 找到它。
    call_id: String,
    /// 解码并缩小后的源图：终端宽度变化时用它重建缩略图。
    source: DynamicImage,
    /// `protocol` 对应的可用宽度；变了就要重建（软折行结果跟着宽度走）。
    width: Option<u16>,
    /// 建好的缩略图；构建失败时为 `None`（该图不铺占位行，不留空块）。
    protocol: Option<SlicedProtocol>,
    /// 缩略图占用的单元格尺寸；只在 `protocol` 建好时才有值。
    cells: Option<(u16, u16)>,
}

/// 后台解码线程的产物。
struct Decoded {
    call_id: String,
    image: Option<DynamicImage>,
}

impl Default for ImagePreviews {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagePreviews {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            picker: Picker::halfblocks(),
            entries: VecDeque::new(),
            known: HashSet::new(),
            sender,
            receiver,
            pending: 0,
        }
    }

    /// 把一次工具调用的视觉附件交给后台线程解码。
    ///
    /// 同一 `call_id` 重复注册（重试、重放）时先丢掉旧的那张，避免同一张卡挂着两份。
    pub fn register(&mut self, call_id: &str, images: &[ToolImageAttachment]) {
        if call_id.is_empty() || images.is_empty() {
            return;
        }
        self.forget(call_id);
        self.known.insert(call_id.to_string());
        for image in images.iter().take(MAX_IMAGES_PER_CALL) {
            let payload = image.data_base64.clone();
            let sender = self.sender.clone();
            let call_id = call_id.to_string();
            self.pending += 1;
            // 解码放后台：大图（手机照片级别）在主线程上要几百毫秒。
            std::thread::spawn(move || {
                // 解码器遇到畸形图片理论上可能 panic：那条结果无论如何都要回，
                // 否则 `pending` 永远减不到 0，事件循环会一直按活动帧率空转。
                let image = std::panic::catch_unwind(|| decode(&payload)).unwrap_or(None);
                // 接收端已消失（界面退出）时忽略：这条结果本来也没人要了。
                let _ = sender.send(Decoded { call_id, image });
            });
        }
    }

    /// 收下后台解码的结果；返回内容真的变了的调用（显示行要跟着重算）。
    ///
    /// 失败的解码也算变化：那张卡要从「正在准备图片」换回原来的载荷正文，漏掉就会**永久**
    /// 停在提示行上（显示行是缓存的，不会再自己算一遍）。
    pub fn drain(&mut self) -> Vec<String> {
        let mut changed: Vec<String> = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(decoded) => {
                    self.pending = self.pending.saturating_sub(1);
                    let call_id = decoded.call_id;
                    let Some(image) = decoded.image else {
                        // 解不出来的图不占位置：把调用从「有预览」名单里摘掉。
                        if self.known.remove(&call_id) {
                            changed.push(call_id);
                        }
                        continue;
                    };
                    self.forget(&call_id);
                    // 淘汰最旧的一张时要连带把它从名单里摘掉，否则那张卡会永远铺着块。
                    let mut evicted = Vec::new();
                    while self.entries.len() >= MAX_PREVIEWS {
                        if let Some(dropped) = self.entries.pop_front() {
                            self.known.remove(&dropped.call_id);
                            evicted.push(dropped.call_id);
                        }
                    }
                    self.entries.push_back(Preview {
                        call_id: call_id.clone(),
                        source: image,
                        width: None,
                        protocol: None,
                        cells: None,
                    });
                    changed.push(call_id);
                    changed.extend(evicted);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = 0;
                    break;
                }
            }
        }
        changed
    }

    /// 是否还有图片在后台解码（事件循环据此缩短等待）。
    pub fn is_pending(&self) -> bool {
        self.pending > 0
    }

    /// 把缩略图补到当前可用宽度；返回是否有条目被重建（渲染前调用，编码只在这里发生）。
    ///
    /// 宽度不变时整条跳过：滚动、鼠标移动这些每帧都会走到的路径因此不重复编码。
    pub fn ensure_protocols(&mut self, available_width: u16) -> bool {
        let mut rebuilt = false;
        for entry in self.entries.iter_mut() {
            if entry.width == Some(available_width) {
                continue;
            }
            entry.width = Some(available_width);
            // 目标尺寸就是「可用宽度 × 行数上限」，由库按 `Fit` 收进这个框并回报实际尺寸。
            let target = Size::new(available_width.max(1), MAX_PREVIEW_ROWS);
            let protocol = SlicedProtocol::new_with_resize(
                &self.picker,
                entry.source.clone(),
                target,
                Resize::Fit(Some(FilterType::Triangle)),
            )
            .ok();
            entry.cells = protocol.as_ref().map(|protocol| {
                let size = protocol.size();
                (size.width, size.height)
            });
            entry.protocol = protocol;
            rebuilt = true;
        }
        rebuilt
    }

    /// 缩略图在该可用宽度下占用的单元格尺寸；还没建好时为 `None`。
    ///
    /// 这也是渲染层铺占位行的依据：返回 `None` 时卡片不铺块，不会留下空白洞。
    pub fn block_size(&self, call_id: &str, available_width: u16) -> Option<(u16, u16)> {
        let entry = self.entry(call_id)?;
        if entry.width != Some(available_width) {
            return None;
        }
        entry.cells
    }

    /// 该调用是否会出示图片（附件已注册）；解码还没完成时也算。
    ///
    /// 渲染层据此决定把话卡正文换成图片区——提早铺块，图片到达时不会把下面的行挤开。
    pub fn shows_image(&self, call_id: &str) -> bool {
        self.known.contains(call_id)
    }

    /// 已建好的缩略图；渲染层据此把图片画进占位行。
    pub fn protocol(&self, call_id: &str) -> Option<&SlicedProtocol> {
        self.entry(call_id)?.protocol.as_ref()
    }

    /// 丢掉全部预览（会话被清空 / 重放时用）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.known.clear();
    }

    fn entry(&self, call_id: &str) -> Option<&Preview> {
        self.entries.iter().find(|entry| entry.call_id == call_id)
    }

    fn forget(&mut self, call_id: &str) {
        self.entries.retain(|entry| entry.call_id != call_id);
    }
}

/// 后台线程里的解码：Base64 → 按内容认格式解码 → 缩到 [`MAX_SOURCE_EDGE`]。
fn decode(payload: &str) -> Option<DynamicImage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    if image.width() > MAX_SOURCE_EDGE || image.height() > MAX_SOURCE_EDGE {
        return Some(image.thumbnail(MAX_SOURCE_EDGE, MAX_SOURCE_EDGE));
    }
    Some(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 红点 PNG（`rust/tools` 侧的对照数据集用的是同一类最小样本）。
    const PNG_PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    fn attachment(base64: &str) -> ToolImageAttachment {
        ToolImageAttachment {
            media_type: "image/png".to_string(),
            data_base64: base64.to_string(),
            filename: "a.png".to_string(),
            detail: "auto".to_string(),
        }
    }

    /// 等后台解码结果到达；超时说明解码线程没能回结果。
    fn wait_for_previews(previews: &mut ImagePreviews) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !previews.drain().is_empty() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn decode_accepts_a_real_png_and_rejects_garbage() {
        let image = decode(PNG_PIXEL).expect("合法 PNG 应当解码成功");
        assert_eq!((image.width(), image.height()), (1, 1));
        assert!(decode("不是图片").is_none());
        assert!(decode("").is_none());
    }

    #[test]
    fn large_sources_are_bounded_before_encoding() {
        let source: DynamicImage =
            image::ImageBuffer::from_pixel(2000, 1000, image::Rgba([1u8, 2, 3, 255])).into();
        assert_eq!((source.width(), source.height()), (2000, 1000));
        assert_eq!((source.thumbnail(MAX_SOURCE_EDGE, MAX_SOURCE_EDGE).width()), MAX_SOURCE_EDGE);
    }

    #[test]
    fn a_registered_image_becomes_a_block_at_the_available_width() {
        let mut previews = ImagePreviews::new();
        assert_eq!(previews.block_size("call-1", 80), None, "解码前没有占位块");
        previews.register("call-1", &[attachment(PNG_PIXEL)]);
        assert!(previews.is_pending());
        assert!(wait_for_previews(&mut previews), "后台解码应当有结果");

        // 编码只在预处理时发生：宽度还没确认之前仍然没有块。
        assert_eq!(previews.block_size("call-1", 80), None);
        assert!(previews.ensure_protocols(80));
        let (width, height) = previews.block_size("call-1", 80).expect("块尺寸");
        assert_eq!((width, height), (1, 1), "1x1 的源图不该被放大");
        assert!(previews.protocol("call-1").is_some());
        // 宽度没变时不重复编码。
        assert!(!previews.ensure_protocols(80));
    }

    #[test]
    fn the_row_cap_bounds_a_tall_image_and_keeps_it_proportional() {
        // 100x1000 的自然尺寸是 10 列 × 50 行（半块字形按 1:2 的字体格算）；
        // 只受行数上限约束时收到 16 行，宽度跟着比例缩。
        let source: DynamicImage =
            image::ImageBuffer::from_pixel(100, 1000, image::Rgba([9u8, 9, 9, 255])).into();
        let mut previews = ImagePreviews::new();
        previews.entries.push_back(Preview {
            call_id: "tall".to_string(),
            source,
            width: None,
            protocol: None,
            cells: None,
        });
        previews.ensure_protocols(200);
        let (width, height) = previews.block_size("tall", 200).expect("块尺寸");
        assert_eq!(height, MAX_PREVIEW_ROWS, "高度收到上限");
        assert!(width < 200, "宽度跟着比例缩：{width}");
        assert!(width >= 2, "宽度不该缩成一条线：{width}");
    }

    #[test]
    fn a_wide_image_is_capped_by_the_available_width() {
        // 4000x200 的自然尺寸是 400 列 × 10 行：宽度满了之后高度跟着降下来。
        let source: DynamicImage =
            image::ImageBuffer::from_pixel(4000, 200, image::Rgba([3u8, 3, 3, 255])).into();
        let mut previews = ImagePreviews::new();
        previews.entries.push_back(Preview {
            call_id: "wide".to_string(),
            source,
            width: None,
            protocol: None,
            cells: None,
        });
        previews.ensure_protocols(40);
        let (width, height) = previews.block_size("wide", 40).expect("块尺寸");
        assert_eq!(width, 40, "宽度收到可用宽度");
        assert!(height <= MAX_PREVIEW_ROWS, "高度不超过上限：{height}");
    }

    #[test]
    fn a_small_image_is_not_blown_up_to_fill_the_width() {
        let source: DynamicImage =
            image::ImageBuffer::from_pixel(10, 10, image::Rgba([7u8, 7, 7, 255])).into();
        let mut previews = ImagePreviews::new();
        previews.entries.push_back(Preview {
            call_id: "small".to_string(),
            source,
            width: None,
            protocol: None,
            cells: None,
        });
        previews.ensure_protocols(120);
        let (width, height) = previews.block_size("small", 120).expect("块尺寸");
        assert_eq!((width, height), (1, 1), "放大只会糊掉像素");
    }

    #[test]
    fn clearing_drops_every_preview() {
        let mut previews = ImagePreviews::new();
        previews.register("call-1", &[attachment(PNG_PIXEL)]);
        assert!(wait_for_previews(&mut previews));
        previews.ensure_protocols(80);
        assert!(previews.protocol("call-1").is_some());
        previews.clear();
        assert!(previews.protocol("call-1").is_none());
        assert_eq!(previews.block_size("call-1", 80), None);
    }

    #[test]
    fn registering_the_same_call_twice_keeps_one_preview() {
        let mut previews = ImagePreviews::new();
        previews.register("call-1", &[attachment(PNG_PIXEL)]);
        assert!(wait_for_previews(&mut previews));
        previews.register("call-1", &[attachment(PNG_PIXEL)]);
        assert!(wait_for_previews(&mut previews));
        previews.ensure_protocols(80);
        assert_eq!(previews.entries.len(), 1);
    }

    #[test]
    fn blank_registrations_are_ignored() {
        let mut previews = ImagePreviews::new();
        previews.register("", &[attachment(PNG_PIXEL)]);
        previews.register("call-1", &[]);
        assert!(!previews.is_pending());
        assert!(previews.drain().is_empty());
        assert!(previews.entries.is_empty());
    }

    #[test]
    fn a_failed_decode_reports_the_call_so_the_card_can_fall_back() {
        let mut previews = ImagePreviews::new();
        previews.register("bad", &[attachment("bm90IGFuIGltYWdl")]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut reported: Vec<String> = Vec::new();
        while std::time::Instant::now() < deadline {
            reported.extend(previews.drain());
            if !previews.is_pending() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            reported,
            vec!["bad".to_string()],
            "失败的结果也要上报，否则卡片会停在「正在准备图片」"
        );
    }

    #[test]
    fn failed_decodes_do_not_leave_a_block_behind() {
        let mut previews = ImagePreviews::new();
        previews.register("bad", &[attachment("bm90IGFuIGltYWdl")]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while previews.is_pending() && std::time::Instant::now() < deadline {
            previews.drain();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!previews.is_pending(), "解码线程应当报完结果");
        assert!(previews.entries.is_empty(), "解不出来的图不留占位");
    }
}
