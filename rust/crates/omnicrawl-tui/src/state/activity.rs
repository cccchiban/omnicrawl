//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
        // ---- 三块刷新：版本号与缓存失效 -----------------------------------------------

        /// 当前的三块刷新版本号。
        pub fn refresh_versions(&self) -> RefreshVersions {
            self.refresh
        }

        /// 输入块置脏（键盘鼠标输入、清单/队列/面板/状态行变化）。
        pub fn touch_composer(&mut self) {
            self.refresh.bump_composer();
        }

        /// 写运行状态行（画在输入区方框的上边框上）。
        ///
        /// 状态行属于输入块，写入时必须置脏；`status` 字段直接赋值仍可用，但会漏掉重画。
        pub fn set_status(&mut self, status: Option<String>) {
            self.status = status;
            self.touch_composer();
        }

        /// 内核的「进行中」提示让位（`Status` / `RetryStatus` / `ProtocolWait` 写入的那条）。
        ///
        /// 这类提示说的是「此刻在等什么」（正在重试第 N 次、等待协议、网关降级…），回合一旦
        /// 重新往前走就不再成立，状态行该回到运行态的默认文案。不清理它会一直停在那句话上，
        /// 直到回合结束（对映 Python 每片增量都重设「正在思考 / 正在回复」）。
        ///
        /// 只在回合进行中让位：慢命令（`/review` 等）在空闲时把状态行写成「正在评审」，
        /// 不能被子代理的流式增量顶掉。
        pub(super) fn settle_transient_status(&mut self) {
            if self.turn.is_running() && self.status.is_some() {
                self.set_status(None);
            }
        }

        /// 底部块置脏（轮播换页与解密扫描）。
        pub fn touch_bottom(&mut self) {
            self.refresh.bump_bottom();
        }

        /// 会话块整块失效：记录被清空/重放、展开态或思考显示变化时用。
        ///
        /// 下次渲染会从第一条记录起重建显示行（缓存的水位归零）。
        pub fn touch_conversation(&mut self) {
            self.refresh.bump_conversation();
            self.conversation_cache.get_mut().mark_all_dirty();
        }

        /// 会话区需要重画，但**显示行缓存仍然有效**（滚动、拖选高亮这类只影响「怎么画」）。
        pub fn touch_conversation_view(&mut self) {
            self.refresh.bump_conversation();
        }

        /// 清空会话视图（`Ctrl+L` 一类命令）：记录、图片预览与计划清单一起丢掉。
        ///
        /// 预览必须同步清掉：记录没了以后没有任｛Desensitized:684｝引用那些锚点，留着只占内存。
        /// 计划清单同理：它画在输入框上方、属于这一屏的会话现场，视图空了还留着上一段的步骤
        /// 就是自相矛盾（`/new` 之后仍显示旧计划即此）。
        pub fn clear_conversation(&mut self) {
            self.records.clear();
            self.image_previews.get_mut().clear();
            self.todos.clear();
            self.touch_conversation();
            self.touch_composer();
        }

        /// 会话区尾部追加了一条记录：显示行只需补算新记录，但界面要重画。
        pub(super) fn touch_appended(&mut self) {
            self.refresh.bump_conversation();
        }

        /// 第 `index` 条记录的内容变了：从它开始重算显示行（增量），并标记会话块要重画。
        pub(super) fn touch_record(&mut self, index: usize) {
            self.refresh.bump_conversation();
            self.conversation_cache.get_mut().mark_record_dirty(index);
        }

        // ---- 思考段平滑显现 ---------------------------------------------------------

        /// 推进一帧思考段显现；返回 `true` 表示还没铺完（事件循环据此继续按帧重画）。
        pub fn tick_reasoning_reveal(&mut self) -> bool {
            // 记录被清空/回滚后游标可能指向已经不存在的记录：那种情况下直接退出显现态，
            // 否则「还没铺完」会永远为真，事件循环就按活动帧率空转。
            let stale = self
                .reasoning_reveal
                .borrow()
                .index
                .is_some_and(|index| index >= self.records.len());
            if stale {
                self.reasoning_reveal.get_mut().reset();
                return false;
            }
            let before = {
                let reveal = self.reasoning_reveal.borrow();
                (reveal.index, reveal.revealed)
            };
            let more = self.reasoning_reveal.get_mut().advance();
            let after = {
                let reveal = self.reasoning_reveal.borrow();
                (reveal.index, reveal.revealed)
            };
            // 显现量变了就是这一条记录的内容变了：必须让它重算显示行，只重画是不够的。
            if before != after {
                if let Some(index) = after.0 {
                    self.touch_record(index);
                }
            }
            more
        }

        /// 略过逐帧显现、立刻铺完全部思考（收口、展开/收起、回放与回合结束时用）。
        ///
        /// 只关掉「逐帧揭示」，**保留**当前下标：这段思考如果还在流式到达，后续增量仍会认成
        /// 同一条，按全文追加。清掉下标会让下一片被当成首片、从头重铺（展开/折叠后整段重放）。
        pub fn settle_reasoning_reveal(&mut self) {
            self.reasoning_reveal.get_mut().settle();
        }

        /// 该记录本次渲染应显示的字符数（只有正在显现的那条会被截断）。
        pub fn reasoning_visible_chars(&self, index: usize, total: usize) -> usize {
            self.reasoning_reveal.borrow().visible_chars(index, total)
        }

        /// 是否有思考段正在逐帧铺开。
        pub fn is_reasoning_revealing(&self) -> bool {
            let reveal = self.reasoning_reveal.borrow();
            reveal.tracking && reveal.index.is_some() && reveal.revealed < reveal.target
        }

        /// 每帧的活动计时：回合在跑时状态行 spinner（每 80ms 换帧）与工具卡耗时在变。
        ///
        /// 空闲时什么都不做，三块版本号不变，事件循环因此可以整帧跳过绘制。
        pub fn tick_activity(&mut self) {
            if !self.turn.is_running() {
                return;
            }
            self.touch_composer();
            self.touch_running_tools();
        }

        /// 是否存在需要按帧推进的活动（运行中的工具卡 / 回合状态行）。
        ///
        /// 事件循环用它决定等待上限：没有活动就不必按活动帧率唤醒。子代理的耗时由
        /// 独立窗口自己推进，主会话这边没有需要刷新的东西。
        pub fn has_running_activity(&self) -> bool {
            self.turn.is_running()
                || self.records.iter().any(
                    |record| matches!(record, Record::Tool(card) if card.status == ToolStatus::Running),
                )
        }

        /// 下一个动画帧的到期时刻距现在还有多久；没有动画在播时返回 `None`。
        ///
        /// 欢迎 Logo 与轮播解密扫描都按固定帧长换帧：事件循环据此精确睡到下一帧，
        /// 既不会早醒空转，也不会晚到掉帧。
        pub fn next_animation_frame(&self, now: Instant) -> Option<Duration> {
            let mut due: Option<Duration> = None;
            if let Some(delay) = self.logo.next_frame_delay(now) {
                due = Some(delay);
            }
            // 只在动画真的还在播时才算帧：轮播落定后 `carousel_anim_at` 仍留着上一次的时间戳，
            // 不看 `is_animating()` 会把每个空闲帧都当成「下一帧马上到」而忙等。
            if self.carousel.is_animating() {
                if let Some(last) = self.carousel_anim_at {
                    let remaining = frame_delay(now, last, CAROUSEL_ANIMATION_FRAME_SECONDS);
                    due = Some(due.map_or(remaining, |known| known.min(remaining)));
                }
            }
            due
        }

        /// 轮播数据源快照。<`reasoning_effort`> 不在界面状态里（它属于模型配置），
        /// 由宿主在 tick 时传入。
        pub(super) fn carousel_source(&self, reasoning_effort: &str) -> CarouselSource {
            CarouselSource {
                workspace: self.project.clone(),
                input_tokens: self.telemetry.input_tokens as i64,
                output_tokens: self.telemetry.output_tokens as i64,
                cached_input_tokens: self.telemetry.cached_input_tokens as i64,
                // 总量未知时传 0：对映层与 Python 一样按 max(1) 兜底，不编造分母。
                context_limit: self.telemetry.context_window.unwrap_or(0) as i64,
                tokens_per_second: self.telemetry.rate.value().unwrap_or(0.0),
                model: self.model.clone(),
                reasoning_effort: reasoning_effort.to_string(),
                approval_mode: self.approval.label().to_string(),
                mcp_enabled_count: self.mcp_servers as i64,
                pending_count: self.pending_inputs.len() as i64,
            }
        }

        /// 推进底部单行轮播：停留到点换页，动画中按帧距推进解密扫描。
        ///
        /// 定时器由装配层持有（对映 Python 把停留/帧定时器交给 Textual 的做法），
        /// 这里只吃「现在几点」并更新 [`AppState::carousel_text`]。
        ///
        /// 文本没变时不写回、不标脏：空闲时底部块因此保持干净，事件循环可以整帧跳过绘制。
        pub fn refresh_carousel(&mut self, now: Instant, reasoning_effort: &str) {
            let source = self.carousel_source(reasoning_effort);
            if self.carousel.is_animating() {
                let due = self
                    .carousel_anim_at
                    .map(|last| {
                        now.duration_since(last).as_secs_f64() >= CAROUSEL_ANIMATION_FRAME_SECONDS
                    })
                    .unwrap_or(true);
                if !due {
                    return;
                }
                self.carousel_anim_at = Some(now);
                match self.carousel.animation_tick(
                    &source,
                    &self.carousel_lines,
                    &mut self.carousel_rand,
                ) {
                    CarouselTick::Frame(text) => self.set_carousel_text(text),
                    CarouselTick::Settled(text) => {
                        self.set_carousel_text(text);
                        // 动画收口后重新开始本页的停留计时。
                        self.carousel_since = now;
                    }
                }
                return;
            }
            if !self.carousel_ready {
                self.carousel_ready = true;
                self.carousel_since = now;
            } else if now.duration_since(self.carousel_since).as_secs_f64()
                >= self.carousel.page_duration()
            {
                let next = self.carousel.next_page();
                self.carousel.switch_to(
                    next,
                    true,
                    &source,
                    &self.carousel_lines,
                    &mut self.carousel_rand,
                );
                self.carousel_anim_at = None;
                self.carousel_since = now;
                // 动画帧由下一帧推进，本帧仍是旧页文本（与 Python 切换当帧的可见状态一致）。
                return;
            }
            // 稳态：每帧按最新遥测重建当前页（对映 Python 的 `_carousel_refresh`，
            // 否则 token 计数要在下一次换页才会追上）。
            let next =
                self.carousel
                    .display_text(&source, &self.carousel_lines, &mut self.carousel_rand);
            self.set_carousel_text(next);
        }

        /// 写回轮播文本：内容真的变了才置脏底部块。
        pub(super) fn set_carousel_text(&mut self, text: StyledText) {
            if self.carousel_text == text {
                return;
            }
            self.carousel_text = text;
            self.touch_bottom();
        }

        /// 当前轮播页（测试与调试用）。
        pub fn carousel_page(&self) -> CarouselPage {
            self.carousel.page()
        }

        /// 追加一条系统消息；不改动回合状态。
        pub fn notice(&mut self, message: String) {
            self.records.push(Record::Notice(message));
            self.touch_appended();
        }

        /// 让第 `index` 条思考记录从当前长度起继续逐帧铺开 `appended` 个字符。
        pub(super) fn start_reasoning_reveal(&mut self, index: usize, appended: usize) {
            let reveal = self.reasoning_reveal.get_mut();
            if reveal.index != Some(index) {
                reveal.start(index);
            }
            reveal.extend(index, appended);
            self.touch_record(index);
        }

        /// 是否在消息流里显示思考段（`ui.show_thinking`）。
        ///
        /// 这是个纯界面开关，但会改变会话区的显示行（思考段整段出现/消失），
        /// 因此必须走 setter 让缓存整块失效。
        pub fn set_show_thinking(&mut self, enabled: bool) {
            if self.show_thinking == enabled {
                return;
            }
            self.show_thinking = enabled;
            // 开关变化后整块重排：思考段不再逐帧铺开，避免显示到一半的半句停在屏幕上。
            self.settle_reasoning_reveal();
            self.touch_conversation();
        }

        pub fn scroll_by(&mut self, delta: isize) {
            if delta < 0 {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(delta.unsigned_abs());
            } else {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(delta as usize);
            }
            // 滚动只换窗口、不动显示行：缓存不用重算，但画面要重画。
            self.touch_conversation_view();
        }

        pub fn scroll_to_bottom(&mut self) {
            self.scroll_from_bottom = 0;
            self.touch_conversation_view();
        }

        /// 在输入框上方那一行显示一条瞬时提示（拖选复制等）。
        pub fn show_notice_line(&mut self, text: impl Into<String>, now: Instant) {
            self.notice_line = Some((text.into(), now));
            self.touch_composer();
        }

        /// 当前该显示的提示行文本；超过存活时间返回 `None`。
        pub fn notice_line_text(&self, now: Instant) -> Option<&str> {
            let (text, at) = self.notice_line.as_ref()?;
            if now.saturating_duration_since(*at) > NOTICE_LINE_LINGER {
                return None;
            }
            Some(text.as_str())
        }

        pub fn tick_notice_line(&mut self, now: Instant) {
            if self.notice_line.is_some() && self.notice_line_text(now).is_none() {
                self.notice_line = None;
                // 提示行淡出后输入区组高会少一行：布局要重算。
                self.touch_composer();
            }
        }

}
