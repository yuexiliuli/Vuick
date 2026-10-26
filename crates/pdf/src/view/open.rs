//! PDF 阅读器子模块：文档打开 / 状态访问器 / 资源释放 / 每帧装配。
//!
//! - [`super::PdfViewer::new`] 与 [`super::PdfViewer::start_load`]：后台读文件 + 经单例
//!   pdfium 工作线程解析、量页、取书签目录，完成后在主线程铺页槽（打开即显，不预栅格化）；
//! - 状态只读访问器（页数 / 当前页 / 加载态 / 调度进度 / 画布背景 / 书签）供外壳
//!   （侧边栏、状态栏、`app`）消费；
//! - [`super::PdfViewer::release`]：关闭标签页 / 丢弃视图时显式 `drop_image` 下线 GPU 纹理，
//!   并关闭 worker 侧文档；
//! - [`super::PdfViewer::ensure_window`]：**每帧装配入口**——回收在途结果（瓦片/封面/
//!   缩略/中清/文本/检索）、估速、静态帧门控、按视口调度瓦片（常态）/占位（快速拖动）、
//!   淘汰超预算缓存。调度细节在各职责子模块（`tiles` / `search`）。

use super::*;

impl PdfViewer {
    /// 创建阅读器并异步加载 `path`（读文件 + 解析 + 量页在后台，UI 不阻塞）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            path,
            worker: None,
            doc_id: 0,
            dims: Vec::new(),
            page_tops: Vec::new(),
            total_height: 0.0,
            slots: Vec::new(),
            pending_thumb: HashMap::new(),
            pending_mid: HashMap::new(),
            mid_lru: VecDeque::new(),
            pending_tiles: HashMap::new(),
            pending_cover: HashMap::new(),
            tile_bytes: 0,
            tile_pump_running: false,
            last_scroll_y: 0.0,
            scroll_velocity: 0.0,
            prev_fast: false,
            scroll: ScrollHandle::new(),
            loading: true,
            load_error: None,
            bg: None,
            pending_drops: Vec::new(),
            outlines: Vec::new(),
            // 焦点句柄须由 Context 创建（gpui 内部注册），同 markdown 预览
            focus_handle: cx.focus_handle(),
            text_pages: HashMap::new(),
            text_lru: VecDeque::new(),
            pending_text: HashMap::new(),
            sel_anchor: None,
            sel_active: None,
            selecting: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_truncated: false,
            search_active: 0,
            search_pending: None,
            search_pending_seq: 0,
            search_seq: 0,
            zoom: 1.0,
            window_sig: None,
            window_settled: false,
            idle_gate_skips: 0,
        };
        this.start_load(cx);
        this
    }

    /// 后台加载：读文件 + 经单例 pdfium 工作线程解析 + 量页 + 书签目录，完成后在主线程铺页槽。
    pub(super) fn start_load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                    // 进程级单例工作线程：打开文档 + 量页 + 书签目录，返回句柄与 DocId
                    let (worker, doc_id, dims, outlines) =
                        PdfWorker::open_and_dims_full(bytes)
                            .map_err(|e| format!("无法启动 pdfium：{e}"))?;
                    Ok::<_, String>((worker, doc_id, dims, outlines))
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok((worker, doc_id, dims, outlines)) => {
                        this.worker = Some(worker);
                        this.doc_id = doc_id;
                        this.dims = dims;
                        this.outlines = outlines;
                        let (tops, total) = compute_layout(&this.dims, this.page_width());
                        this.page_tops = tops;
                        this.total_height = total;
                        this.slots = vec![Slot::default(); this.dims.len()];
                        this.loading = false;
                        if timing_view() {
                            eprintln!(
                                "[{:.1}ms] [VIEW-OPEN-DONE] pages={} tops0={:.1} total={:.1}",
                                crate::pdf::timing_ms(),
                                this.dims.len(),
                                this.page_tops.first().copied().unwrap_or(0.0),
                                this.total_height,
                            );
                        }
                        // 关键：`refresh_windows()` 无条件要求重绘（不依赖实体被窗口 tracking）。
                        // 仅 `notify()` 在实体未注册 window invalidator 时是空操作 → 首帧后
                        // 永远不会再跑 `ensure_window` → 瓦片/封面从未被调度（“CPU 低、不变化”）。
                        cx.refresh_windows();
                        cx.notify();
                    }
                    Err(e) => {
                        this.loading = false;
                        this.load_error = Some(e);
                        cx.refresh_windows();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 滚动到指定页（目录跳转）。页码越界时忽略。
    pub fn scroll_to_page(&mut self, page_index: usize) {
        if page_index >= self.dims.len() {
            return;
        }
        let top = self.page_tops.get(page_index).copied().unwrap_or(0.0);
        self.scroll.set_offset(gpui::point(px(0.), px(-top)));
    }

    /// 页数（文档尚未加载完时为 0）。P4 拆 crate 后根包 app::tests 亦消费，取消 `cfg(test)`。
    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// 当前第一张可见页（1 基）与总页数（状态栏滚动位置显示用）。
    /// 仅派生自既有滚动状态（`scroll` 偏移 + 页列布局 [`first_visible_page`]），不新增
    /// 状态；文档未加载完成（`dims` 为空）时返回 `None`。与 EPUB 页码口径一致：
    /// 「第一张可见页」如实反映视口，不含 `keep_window` 的 MARGIN 缓冲。
    pub fn current_page(&self) -> Option<(usize, usize)> {
        if self.dims.is_empty() {
            return None;
        }
        // 首帧 `scroll` 偏移为 0 → `first_visible_page` 起始页即第 1 页，布局后精确。
        let scroll_y = f32::from(-self.scroll.offset().y);
        let first = first_visible_page(&self.dims, &self.page_tops, self.page_width(), scroll_y);
        Some((first + 1, self.dims.len()))
    }

    /// 是否已加载完成。P4 拆 crate 后根包 app::tests 亦消费，取消 `cfg(test)`。
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.worker.is_some() && !self.dims.is_empty()
    }

    /// 无任何输入下「已调度渲染」进度快照（根包 app::tests 的 PDF 用例消费，验证
    /// 「负载完成 → notify → 窗口重绘」链条会主动发起瓦片/封面调度）：
    /// `(首槽已落瓦片数, 首槽封面是否已入, 在途瓦片请求数)`。
    /// P4 拆 crate 后 app::tests 不能直接读 `slots`/`pending_tiles`（pdf crate 私有字段），
    /// 经此只读访问器取数。
    pub fn tiles_schedule_progress(&self) -> (usize, bool, usize) {
        let s0 = self.slots.first();
        (
            s0.map(|s| s.tiles.len()).unwrap_or(0),
            s0.map(|s| s.cover.is_some()).unwrap_or(false),
            self.pending_tiles.len(),
        )
    }

    /// 设置阅读画布背景色。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 当前画布背景色（`set_background` 传入；None = 未设置 → render 回落 `default_canvas`）。
    pub fn background(&self) -> Option<Hsla> {
        self.bg
    }

    /// 画布默认底色：亮 #F5F5F5 / 暗 #202020（中性灰阶，与外壳 `window_bg` 同源；
    /// 应用启动后 `set_background` 会用 ChromeColors 覆盖，此处仅兜底）。
    pub(super) fn default_canvas(cx: &App) -> Hsla {
        let dark = ui::theme::Theme::global(cx).is_dark();
        ui::theme::ChromeColors::for_mode(dark).window_bg.into()
    }

    /// 页面“卡片”配色：纸面白 / 深色纸面，配中性边框（只动 chrome 边框，纸面不改）。
    pub(super) fn page_card_colors(cx: &App) -> (Hsla, Hsla) {
        if ui::theme::Theme::global(cx).is_dark() {
            (rgb(0x1f2937).into(), rgb(0x484848).into())
        } else {
            (gpui::white(), rgb(0xe0e0e0).into())
        }
    }

    /// 释放所有已上传的 GPU 纹理（关闭标签页 / 丢弃视图时调用），关闭 worker 侧文档。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut drops = std::mem::take(&mut self.pending_drops);
        for slot in self.slots.iter_mut() {
            if let Some(arc) = slot.cover.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.thumb.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.mid.take() {
                drops.push(arc);
            }
            for (_, ts) in slot.tiles.drain() {
                drops.push(ts.image);
            }
            slot.tile_bytes = 0;
        }
        for arc in drops {
            cx.drop_image(arc, Some(window));
        }
        if let Some(worker) = &self.worker
            && self.doc_id != 0 {
                worker.close(self.doc_id);
                self.doc_id = 0;
            }
        // 文本层与选区随文档一起释放
        self.text_pages.clear();
        self.text_lru.clear();
        self.pending_text.clear();
        self.pending_thumb.clear();
        self.pending_mid.clear();
        self.pending_tiles.clear();
        self.pending_cover.clear();
        self.tile_bytes = 0;
        self.tile_pump_running = false;
        self.mid_lru.clear();
        self.sel_anchor = None;
        self.sel_active = None;
        self.selecting = false;
        // 检索随文档一起释放
        self.search_query.clear();
        self.search_matches.clear();
        self.search_pending = None;
    }

    /// 当前页列显示宽度（随 Ctrl+滚轮缩放变化）。
    pub(crate) fn page_width(&self) -> f32 {
        PAGE_WIDTH * self.zoom
    }

    /// 书签目录访问器（侧边栏目录使用）。
    pub fn outlines(&self) -> &[PdfOutlineEntry] {
        &self.outlines
    }

    // ---- 每帧装配 / 调度驱动 ----

    /// 每帧：回收在途结果、估速、刷盘、按视口调度瓦片（常态）/占位（快速拖动）、
    /// 淘汰超预算瓦片。
    pub(super) fn ensure_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-TICK] loading={} dims={} pending_t={} pending_c={} fast={} bounds_h={:.0} off_y={:.0}",
                crate::pdf::timing_ms(),
                self.loading,
                self.dims.len(),
                self.pending_tiles.len(),
                self.pending_cover.len(),
                self.prev_fast,
                f32::from(self.scroll.bounds().size.height),
                f32::from(-self.scroll.offset().y),
            );
        }
        for arc in std::mem::take(&mut self.pending_drops) {
            cx.drop_image(arc, Some(window));
        }
        if self.dims.is_empty() || self.worker.is_none() {
            return;
        }
        let scale_factor = window.scale_factor();
        let page_width = self.page_width();
        let scroll_y = f32::from(-self.scroll.offset().y);

        // 内存水位巡检（仅计时模式）：每 ~30 帧打一行构成，供定位内存大头
        if timing_view() {
            static MEM_TICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if MEM_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed).is_multiple_of(30) {
                self.log_memory_waterline(scale_factor);
            }
        }

        // 1) 回收在途结果（占位 → 缓存；瓦片 → 拼图；文本/检索 → 各自缓存）——非阻塞
        if self.collect_thumb_results() {
            cx.notify();
        }
        if self.collect_mid_results() {
            cx.notify();
        }
        if self.collect_text_results() {
            cx.notify();
        }
        if self.collect_search_results() {
            cx.notify();
        }
        // 封面（首屏整页锐化）先于瓦片收集：一个 render 即铺满首屏，立即清晰
        if self.collect_cover_results(scale_factor) {
            cx.notify();
        }
        if self.collect_tile_results(scale_factor) {
            cx.notify();
        }

        // 2) 滚动速度估计（EMA，px/帧）+ 迟滞判定
        let dy = (scroll_y - self.last_scroll_y).abs();
        self.scroll_velocity = self.scroll_velocity * 0.7 + dy * 0.3;
        self.update_fast_flag();
        self.last_scroll_y = scroll_y;

        let viewport_h = f32::from(self.scroll.bounds().size.height);

        // 2.5) **静态帧门控（Sprint 5）**：滚动/缩放/DPI/视口/页数全未变 + 在途清空 +
        //      上帧已常态调度确认无待派（`window_settled`）→ 本帧 keep_window/内存封顶/
        //      schedule/evict 全部跳过。调度是异步投机——跳过只影响后续滚动时冷片延迟，
        //      恢复帧立即补调；首帧（sig=None）、加载/缩放/尺寸变化帧、任何「在途未决」
        //      帧、快速拖动帧都绝不门（防贴图迟滞）。跳过帧不采样泵（无在途 + 视口非空
        //      + 非快速 → `pump_still_needed` 本就为 false，早退不改变任何泵语义）。
        let sig = WindowSig::of(self, scroll_y, page_width, scale_factor, viewport_h);
        let idle_sig = self.window_sig.is_some_and(|s| s == sig);
        if idle_sig
            && self.window_settled
            && !self.prev_fast
            && viewport_h > 0.0
            && self.pending_tiles.is_empty()
            && self.pending_cover.is_empty()
            && self.pending_mid.is_empty()
            && self.pending_thumb.is_empty()
        {
            if timing_view() {
                eprintln!(
                    "[{:.1}ms] [VIEW-IDLE] 静置帧：跳过 keep_window/schedule/evict",
                    crate::pdf::timing_ms()
                );
            }
            self.idle_gate_skips += 1;
            // 不更新 `window_sig`/`window_settled`：保持同一静置签名与确认状态。
            return;
        }

        let (start, end) = keep_window(&self.dims, &self.page_tops, page_width, scroll_y, viewport_h);
        let center = (start + end) / 2;

        // 4) L0 缩略图内存封顶：保留数超 THUMB_CACHE_PAGES 时丢离视口中心最远的，
        //    拆除走 `drop_image`（显式下线 GPU 纹理工卡，否则仅丢 Arc 会累积显存提交）。
        let thumb_retained = self.slots.iter().filter(|s| s.thumb.is_some()).count();
        if thumb_retained > THUMB_CACHE_PAGES {
            let mut th_candidates: Vec<(usize, u64)> = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.thumb.is_some())
                .map(|(i, _)| (i, (i as i64 - center as i64).unsigned_abs()))
                .collect();
            th_candidates.sort_by_key(|&(i, d)| (d, i));
            let excess = thumb_retained - THUMB_CACHE_PAGES;
            let mut dropped: Vec<Arc<RenderImage>> = Vec::new();
            for (i, _) in th_candidates.into_iter().take(excess) {
                if let Some(slot) = self.slots.get_mut(i)
                    && let Some(arc) = slot.thumb.take() {
                        dropped.push(arc);
                    }
            }
            for arc in dropped {
                cx.drop_image(arc, Some(window));
            }
        }
        // 5) L0.5 中清内存封顶：中清页（~1.5MB/张）数量硬顶。删除统一走 `pending_drops`
        //    （下一帧开头 `cx.drop_image` 显式下线 GPU 纹理，防显存/提交内存缓慢累积）。
        let mid_retained = self.slots.iter().filter(|s| s.mid.is_some()).count();
        if mid_retained > MID_CACHE_PAGES {
            // LRU 先与槽同步（删除即丢引用，保证预算与队列一致），再按 LRU 淘汰。
            self.mid_lru
                .retain(|&p| self.slots.get(p).is_some_and(|s| s.mid.is_some()));
            let excess = mid_retained - MID_CACHE_PAGES;
            let mut evicted = 0usize;
            let mut dropped: Vec<Arc<RenderImage>> = Vec::new();
            // 第一遍：只淘汰可见窗之外的**最久未用**页（回翻场景保最近看过的）；
            // 第二遍：窗口内兜底（全部可见时仍能压回预算）。
            for pass in 0..2 {
                if evicted >= excess {
                    break;
                }
                let idxs: Vec<usize> = self
                    .mid_lru
                    .iter()
                    .copied()
                    .filter(|&p| pass == 1 || p < start || p > end)
                    .collect();
                for idx in idxs {
                    if evicted >= excess {
                        break;
                    }
                    let Some(slot) = self.slots.get_mut(idx) else {
                        continue;
                    };
                    if let Some(arc) = slot.mid.take() {
                        self.mid_lru.retain(|&p| p != idx);
                        dropped.push(arc);
                        evicted += 1;
                    }
                }
            }
            self.pending_drops.extend(dropped);
        }

        // 6) 调度：常态 → 瓦片（视口附近按需栅格化）；快速拖动 → 缩略/中清占位
        //    （瓦片停止后自动补齐）。之后统一做瓦片字节预算淘汰。
        if self.prev_fast {
            self.schedule_placeholders(window, start, end, scroll_y, viewport_h, scale_factor);
        } else {
            self.schedule_tiles(window, start, end, page_width, scale_factor, scroll_y);
        }
        self.evict_tiles(start, end, scale_factor);

        // 更新门控状态（本轮已真实跑过调度）：签名对齐当前帧；settled = 非快速态
        // 且调度后无在途瓦片/封面（即确认「已无待派的新 tile 请求」→ 下帧可门）。
        // 快速态（占位调度）或仍有在途 → settled=false，绝不允许静态帧跳过调度。
        self.window_sig = Some(sig);
        self.window_settled = !self.prev_fast
            && self.pending_tiles.is_empty()
            && self.pending_cover.is_empty();

        // 7) 瓦片在途自刷新：若无用户输入就没有下一次重绘，而瓦片结果要等下一次
        //    `ensure_window` 才收集——启动 ~16ms 泵直到在途瓦片清空（静置也能出图）。
        self.ensure_tile_pump(cx);
    }
}
