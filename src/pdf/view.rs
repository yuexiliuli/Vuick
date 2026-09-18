//! PDF 阅读器视图：页级虚拟化核心（加载、滚动调度、页渲染、滚动条）。
//!
//! 子模块拆分（都在 `src/pdf/` 下）：
//! - [`super::cache`]：磁盘二级页缓存（紧凑 BGRA 落盘 + LRU 封顶）
//! - [`super::layout`]：页列纯布局/调度数学（尺寸、可见区间、渲染规划）
//!
//! 书签目录已统一移到侧边栏（[`crate::app::sidebar`] + 公共 [`crate::toc`]），
//! 本模块只保留书签数据（`outlines`）与 [`Self::scroll_to_page`] 跳页能力。
//!
//! 关键机制：
//! - **打开即显**：文件读取 + 结构解析 + 页尺寸测量都在后台线程做（便宜），
//!   完成后马上铺好全部页槽（滚动条正确）；**不**预先栅格化任何页。
//! - **按需栅格化**：每帧根据滚动位置算“可见页区间”（[`super::layout::keep_window`]，
//!   含 MARGIN 缓冲），只对这些页在后台栅格化；渲染按“离可见区中心最近优先”填充。
//! - **回收**：滚离视口的页释放位图 + GPU 纹理（`drop_image`），内存受限。
//! - **不闪烁**：页槽先是有尺寸占位（页号/加载中），位图到位后替换，布局稳定。
//!
//! ## 性能优化（相对旧版）
//!
//! 1. **页列 DOM 虚拟化**：`render()` 每帧只构建「可见页 + MARGIN 缓冲」这几张页面卡片
//!    并以绝对定位铺在等高的“全文档高度”容器里（[`super::layout::compute_layout`]），
//!    不再把几百上千页全部 `div()` 出来布局——长文档滚动/换帧开销从 O(页数) 降到 O(可见页)。
//! 2. **磁盘二级缓存 = 原始 BGRA**：见 [`super::cache`]。
//! 3. **在途请求节流** [`super::layout::MAX_INFLIGHT`]：同时排队给 pdfium 线程的请求
//!    不超过 N 个。单线程 pdfium 串行渲染吞吐不变，但快速滚动时不会无限堆积过期请求。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use gpui::{
    div, img, px, rgba, rgb, App, ClipboardItem, Context, FocusHandle, Hsla, ImageSource,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Point, Pixels, Render, RenderImage, ScrollHandle, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::config::pdf::{
    FADE_MS, LOOKAHEAD_PAGES, MAX_INFLIGHT, MAX_MID_INFLIGHT, MAX_THUMB_INFLIGHT,
    MID_CACHE_PAGES, MOVE_THRESHOLD_LOWER_RATIO, MOVE_THRESHOLD_PX, PAGE_PAD_X, PAGE_WIDTH,
    TEXT_CACHE_PAGES, TEXT_INFLIGHT, THUMB_CACHE_PAGES, WRITE_QUEUE_MAX, ZOOM_MAX, ZOOM_MIN,
    ZOOM_STEP,
};
use crate::pdf::cache::PageCache;
use crate::pdf::layout::{
    Slot, advance_match_index, column_left, compute_layout, content_to_local, display_height,
    hit_char, keep_window, local_to_pt, page_segment_text, plan_page_work, pt_to_local,
    render_scale, selection_spans_pt, trim_transient_writes,
};
use crate::pdf::{
    render_image_from_bgra, DocId, PageText, PdfOutlineEntry, PdfSearchHit, PdfSearchResults,
    PdfWorker, RenderRequest, RenderResult,
};

/// PDF 阅读器：自包含的页级虚拟化视图。
pub struct PdfViewer {
    /// 文件路径（含文件名，标题用）。
    pub(crate) path: PathBuf,
    /// 进程级 pdfium 工作线程句柄（加载完成后为 `Some`）。
    pub(crate) worker: Option<PdfWorker>,
    /// 本阅读器在该工作线程中打开的文档编号。
    pub(crate) doc_id: DocId,
    /// 每页 (宽,高)（PDF 点）。
    pub(crate) dims: Vec<(f32, f32)>,
    /// 每页 top 偏移（`compute_layout` 结果，渲染定位 + 可见区推算共用）。
    pub(crate) page_tops: Vec<f32>,
    /// 整列总高（含上下内边距）。
    pub(crate) total_height: f32,
    /// 每页渲染槽。
    pub(crate) slots: Vec<Slot>,
    /// 渲染代次（缩放/质量变化时递增使旧位图失效）。
    pub(crate) generation: u64,
    /// 在途渲染请求的结果接收通道（worker 线程回传）。
    pub(crate) pending: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 在途 **L0 缩略图**请求的结果接收通道（独立于全尺寸，不占 `MAX_INFLIGHT` 额度）。
    pub(crate) pending_thumb: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 在途 **L0.5 中清页**请求的结果接收通道（快速拖动期派发，独立于全尺寸/缩略图）。
    pub(crate) pending_mid: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 在途**磁盘读回 + 解码**请求的接收通道（主线程不读盘：命中缓存时后台读/解码，
    /// 结果下一帧应用；避免快速滚动时主线程同步读几个 MB 卡帧）。
    pub(crate) restoring: HashMap<usize, mpsc::Receiver<Option<(Vec<u8>, u32, u32)>>>,
    /// 上一帧滚动偏移（预测滚动方向用：delta>0 正在往下滚 → 预取下方页）。
    pub(crate) last_scroll_y: f32,
    /// 滚动速度估计（px/帧，EMA)：> [`MOVE_THRESHOLD_PX`] 视为快速拖动 →
    /// 全尺寸渲染降并发、跳过预取与文本预取，由缩略图层占位。
    pub(crate) scroll_velocity: f32,
    /// 迟滞判定后的"快速拖动"标志（进入 v>阈值、退出 v<阈值×比率，中间保持）：
    /// 所有降级门控统一以它为准，避免阈值边缘反复横跳。
    pub(crate) prev_fast: bool,
    /// 正在交叉淡入的页 → 全尺寸开始渐变的时间戳（透明度按 `now - t` 插值）。
    pub(crate) fades: HashMap<usize, std::time::Instant>,
    /// 淡入动画驱动任务的在途标记（每帧 notify 直到 `fades` 清空）。
    pub(crate) fade_animator_running: bool,
    /// 待落盘写队列 `(页, BGRA, w, h, 是否缩略图)`：编码/写盘在后台 executor **串行**
    /// （最多 1 在途）；快速拖动时超 [`WRITE_QUEUE_MAX`] 的旧条目直接丢弃——
    /// 避免 WebP 无损编码把后台 CPU 打满（长 PDF 拖滚动条时的 CPU 尖峰主因）。
    pub(crate) disk_queue: VecDeque<(usize, Vec<u8>, u32, u32, bool)>,
    /// 后台写盘任务是否在途（串行闸：最多同时 1 个编码）。
    pub(crate) disk_write_inflight: bool,
    /// 写盘完成唤醒通道（后台任务完成后回信，主线程据此放行下一个）。
    pub(crate) disk_write_wakeup: Option<mpsc::Receiver<()>>,
    /// 竖向滚动句柄（页列外层 scroll 容器）。
    pub(crate) scroll: ScrollHandle,
    /// 是否仍在后台解析。
    pub(crate) loading: bool,
    /// 加载失败信息。
    pub(crate) load_error: Option<String>,
    /// 阅读画布背景（页面“纸面”之外底色；None 用主题推导）。
    pub(crate) bg: Option<Hsla>,
    /// 待释放的旧位图（重渲染替换下来的，需在下一帧 `drop_image`）。
    pub(crate) pending_drops: Vec<Arc<RenderImage>>,
    /// 磁盘二级页缓存：内存淘汰后读盘即得，来回翻页不重新栅格化。
    pub(crate) cache: PageCache,
    /// 书签目录（标题 + 层级 + 目标页码）；无书签时为 `[]`。供侧边栏目录使用。
    pub(crate) outlines: Vec<PdfOutlineEntry>,
    /// 键盘焦点句柄（点选后聚焦，`Ctrl+C` 复制）。
    pub(crate) focus_handle: FocusHandle,
    /// 页文本层缓存（懒加载，LRU 上限 [`TEXT_CACHE_PAGES`]；选中/复制/命中测试用）。
    pub(crate) text_pages: HashMap<usize, PageText>,
    /// 文本层 LRU 顺序（最近使用在队尾）。
    pub(crate) text_lru: VecDeque<usize>,
    /// 在途文本请求（worker 回传通道）。
    pub(crate) pending_text: HashMap<usize, mpsc::Receiver<Option<PageText>>>,
    /// 选区锚点（鼠标按下点：(页号, 字符索引)）。松手后保留（作为选区一端）。
    pub(crate) sel_anchor: Option<(usize, usize)>,
    /// 选区活动端（拖拽中随鼠标更新）。
    pub(crate) sel_active: Option<(usize, usize)>,
    /// 是否正在拖拽选区（鼠标按下未松开）；松手置 false，但选区本身保留。
    pub(crate) selecting: bool,
    /// P3 检索：当前查询（由侧边栏搜索框驱动；trim 后用于检索）。
    pub(crate) search_query: String,
    /// 文档级检索命中（页升序）。
    pub(crate) search_matches: Vec<PdfSearchHit>,
    /// 检索是否因命中过多被截断。
    pub(crate) search_truncated: bool,
    /// 当前激活命中下标。
    pub(crate) search_active: usize,
    /// 在途检索结果接收通道（代次过期即弃）。
    pub(crate) search_pending: Option<mpsc::Receiver<Option<PdfSearchResults>>>,
    /// 在途检索对应的调度代次。
    pub(crate) search_pending_seq: u64,
    /// 检索调度代次（防抖：仅最新一次生效）。
    pub(crate) search_seq: u64,
    /// 页面缩放倍数（Ctrl+滚轮可调；页面显示宽度 = `PAGE_WIDTH * zoom`）。
    pub(crate) zoom: f32,
}

impl PdfViewer {
    /// 创建阅读器并异步加载 `path`（读文件 + 解析 + 量页在后台，UI 不阻塞）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let cache = PageCache::new(&path);
        let mut this = Self {
            path,
            worker: None,
            doc_id: 0,
            dims: Vec::new(),
            page_tops: Vec::new(),
            total_height: 0.0,
            slots: Vec::new(),
            generation: 0,
            pending: HashMap::new(),
            pending_thumb: HashMap::new(),
            pending_mid: HashMap::new(),
            restoring: HashMap::new(),
            last_scroll_y: 0.0,
            scroll_velocity: 0.0,
            prev_fast: false,
            fades: HashMap::new(),
            fade_animator_running: false,
            disk_queue: VecDeque::new(),
            disk_write_inflight: false,
            disk_write_wakeup: None,
            scroll: ScrollHandle::new(),
            loading: true,
            load_error: None,
            bg: None,
            pending_drops: Vec::new(),
            cache,
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
        };
        this.start_load(cx);
        this
    }

    /// 后台加载：读文件 + 经单例 pdfium 工作线程解析 + 量页 + 书签目录，完成后在主线程铺页槽。
    fn start_load(&mut self, cx: &mut Context<Self>) {
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
                        cx.notify();
                    }
                    Err(e) => {
                        this.loading = false;
                        this.load_error = Some(e);
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

    /// 页数（测试 / 状态栏使用；文档尚未加载完时为 0）。
    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// 是否已加载完成（测试用）。
    #[allow(dead_code)]
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.worker.is_some() && !self.dims.is_empty()
    }

    /// 设置阅读画布背景色。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 画布默认底色：浅色模式浅灰、深色模式深灰。
    fn default_canvas(cx: &App) -> Hsla {
        if gpui_component::Theme::global(cx).is_dark() {
            rgb(0x111827).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 页面“卡片”配色：纸面白 / 深色纸面，配边框。
    fn page_card_colors(cx: &App) -> (Hsla, Hsla) {
        if gpui_component::Theme::global(cx).is_dark() {
            (rgb(0x1f2937).into(), rgb(0x374151).into())
        } else {
            (gpui::white().into(), rgb(0xd1d5db).into())
        }
    }

    /// 释放所有已上传的 GPU 纹理（关闭标签页 / 丢弃视图时调用），关闭 worker 侧文档并清空磁盘缓存。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut drops = std::mem::take(&mut self.pending_drops);
        for slot in self.slots.iter_mut() {
            if let Some(arc) = slot.image.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.thumb.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.mid.take() {
                drops.push(arc);
            }
        }
        for arc in drops {
            cx.drop_image(arc, Some(window));
        }
        if let Some(worker) = &self.worker {
            if self.doc_id != 0 {
                worker.close(self.doc_id);
                self.doc_id = 0;
            }
        }
        self.cache.clear();
        // 文本层与选区随文档一起释放
        self.text_pages.clear();
        self.text_lru.clear();
        self.pending_text.clear();
        self.pending_thumb.clear();
        self.pending_mid.clear();
        self.restoring.clear();
        self.disk_queue.clear();
        self.disk_write_wakeup = None;
        self.disk_write_inflight = false;
        self.fades.clear();
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

    /// Ctrl+滚轮缩放：页列宽按倍率变化，锚定视口顶部内容点，重算布局并失效旧密度位图。
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 仅 Ctrl+滚轮缩放；普通滚轮不拦截（交给滚动容器）。
        if !event.modifiers.control {
            return;
        }
        let delta_y = match event.delta {
            gpui::ScrollDelta::Lines(v) => v.y,
            gpui::ScrollDelta::Pixels(v) => f32::from(v.y),
        };
        // 实测本平台：向上滚动 delta.y>0 → 放大；向下 <0 → 缩小（与编辑器字号缩放一致）
        let step = delta_y.clamp(-1.0, 1.0);
        self.adjust_zoom(1.0 + step * ZOOM_STEP, cx);
        cx.stop_propagation();
    }

    /// 应用缩放因子并维护其后的一致性：锚定滚动偏移、重算页列布局、递增渲染/缓存代次。
    fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if self.dims.is_empty() {
            return;
        }
        let old_w = self.page_width();
        let new_zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if (new_zoom - self.zoom).abs() < 1e-4 {
            return;
        }
        self.zoom = new_zoom;
        let new_w = self.page_width();
        // 锚定视口：页列布局随 page_width 线性缩放 → 同一文档点的滚动偏移同比缩放。
        let ratio = new_w / old_w;
        let sx = f32::from(-self.scroll.offset().x);
        let sy = f32::from(-self.scroll.offset().y);
        self.scroll
            .set_offset(gpui::point(px(-sx * ratio), px(-sy * ratio)));
        // 重算布局 + 递增代次：旧缩放位图失效，可见页按新密度重栅格化；
        // 磁盘缓存键随版本递增失效（旧密度位图不复用）。
        let (tops, total) = compute_layout(&self.dims, new_w);
        self.page_tops = tops;
        self.total_height = total;
        self.generation = self.generation.wrapping_add(1);
        self.cache.bump_version();
        cx.notify();
    }

    /// 页列 wrap 宽度 = 滚动容器实际可视宽。⚠️ 不用 `window.bounds()`（那是全窗口宽）：
    /// 侧边栏开启时可视区比窗口窄，若 wrap 按窗口宽居中，页列会相对可视区整体右偏。
    /// 首帧未布局时 `scroll.bounds()` 宽为 0，回退到窗口宽——只影响首帧，布局后即校正。
    fn wrap_width(&self, window: &Window, page_width: f32) -> f32 {
        let view = f32::from(self.scroll.bounds().size.width);
        let view = if view > 0.0 {
            view
        } else {
            window.bounds().size.width.as_f32()
        };
        view.max(page_width + 2.0 * PAGE_PAD_X)
    }

    /// 每帧：重算可见区间，回收远端页，派发可见页的后台栅格化。
    fn ensure_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for arc in std::mem::take(&mut self.pending_drops) {
            cx.drop_image(arc, Some(window));
        }
        if self.dims.is_empty() || self.worker.is_none() {
            return;
        }
        let generation = self.generation;

        // 1) 回收本帧已完成的在途渲染结果（主线程，BGRA 直包 + 异步落盘）
        self.collect_results(cx);
        // 1a) 回收在途缩略图结果 → 装入 slot.thumb（拖动/回翻的低清占位即时可见）
        if self.collect_thumb_results() {
            cx.notify();
        }
        // 1a2) 回收在途中清页结果 → 装入 slot.mid（快速拖动清晰度主力）
        if self.collect_mid_results() {
            cx.notify();
        }
        // 1b) 回收在途**磁盘恢复**结果：成功装入槽；失败/过期 → 这些页需真正栅格化
        let restore_failed = self.collect_restores(cx);
        // 1c) 回收在途文本层结果入 LRU 缓存（懒加载）：有新文本到达则重绘（选中高亮补全/可复制）
        if self.collect_text_results() {
            cx.notify();
        }
        // 1d) P3 检索：回收在途结果（代次过期即弃）；查询由侧边栏经 `set_search` 驱动
        if self.collect_search_results() {
            cx.notify();
        }

        let page_width = self.page_width();
        let scale_factor = window.scale_factor();
        let scroll_y = f32::from(-self.scroll.offset().y);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        // 滚动速度估计（EMA，px/帧）+ 迟滞判定：进入快速 = v>阈值，退出 = v<阈值×比率
        let dy = (scroll_y - self.last_scroll_y).abs();
        self.scroll_velocity = self.scroll_velocity * 0.7 + dy * 0.3;
        self.update_fast_flag();
        // 拖动中：取消淡入（保留缩略图占位）；停止后再继续
        if self.prev_fast && !self.fades.is_empty() {
            self.fades.clear();
        }
        // 0) 刷盘：串行写盘（拖动丢弃判据用 prev_fast）
        self.flush_disk_queue(cx);
        // 0b) 淡入动画驱动：有未完成的淡入且无驱动任务 → 启动
        self.ensure_fade_animator(cx);
        let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);

        // 规划：要渲染的页 + 要淘汰位图的页（保留策略见 plan_page_work）。
        // 直接以闭包逐页投影 slots 的状态，不再按总页数物化 Vec<bool>/Vec<u64>
        //（旧版每帧 3×O(总页数) 堆分配，几百上千页的 PDF 纯浪费）。
        let (to_render, to_evict_idx) = plan_page_work(
            self.slots.len(),
            start,
            end,
            generation,
            |i| self.slots[i].image.is_some(),
            |i| self.slots[i].loading,
            |i| self.slots[i].generation,
        );
        let mut to_evict: Vec<Arc<RenderImage>> = Vec::new();
        for i in to_evict_idx {
            if let Some(slot) = self.slots.get_mut(i) {
                if let Some(arc) = slot.image.take() {
                    slot.generation = 0;
                    to_evict.push(arc);
                }
            }
        }
        for arc in to_evict {
            cx.drop_image(arc, Some(window));
        }
        // L0 内存封顶：保留缩略图数超 THUMB_CACHE_PAGES 时，丢离视口中心最远的，
        // 拆除走 `drop_image`（显式下线 GPU 纹理工卡，否则仅丢 Arc 会累积显存提交）。
        let center = (start + end) / 2;
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
                if let Some(slot) = self.slots.get_mut(i) {
                    if let Some(arc) = slot.thumb.take() {
                        dropped.push(arc);
                    }
                }
            }
            for arc in dropped {
                cx.drop_image(arc, Some(window));
            }
        }
        // L0.5 内存封顶：中清页（~0.75MB/张）同样数量硬顶 + 显式 drop_image。
        let mid_retained = self.slots.iter().filter(|s| s.mid.is_some()).count();
        if mid_retained > MID_CACHE_PAGES {
            let mut md_candidates: Vec<(usize, u64)> = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.mid.is_some())
                .map(|(i, _)| (i, (i as i64 - center as i64).unsigned_abs()))
                .collect();
            md_candidates.sort_by_key(|&(i, d)| (d, i));
            let excess = mid_retained - MID_CACHE_PAGES;
            let mut dropped: Vec<Arc<RenderImage>> = Vec::new();
            for (i, _) in md_candidates.into_iter().take(excess) {
                if let Some(slot) = self.slots.get_mut(i) {
                    if let Some(arc) = slot.mid.take() {
                        dropped.push(arc);
                    }
                }
            }
            for arc in dropped {
                cx.drop_image(arc, Some(window));
            }
        }
        // 先尝试磁盘二级缓存：`contains`（无 IO 记账查询）预判是否存在——
        // 在 → 后台**异步**读盘+解码（`restoring`，结果下帧应用，主线程不读盘）；
        // 不在 → 需要真正栅格化。`collect_restores` 产出的失败页并入 need_render。
        let mut need_render: Vec<usize> = restore_failed;
        for &i in &to_render {
            if self.slots[i].image.is_some() {
                continue;
            }
            if self.restoring.contains_key(&i) {
                continue; // 已在异步恢复中，等结果，不重复发起
            }
            if self.cache.contains(i) {
                let cache_inner = self.cache.inner_arc();
                let (tx, rx) = mpsc::channel();
                cx.background_executor()
                    .spawn(async move {
                        let res = crate::pdf::cache::load_bgra_inner(&cache_inner, i);
                        let _ = tx.send(res);
                    })
                    .detach();
                self.restoring.insert(i, rx);
            } else {
                need_render.push(i);
            }
        }
        need_render.sort_unstable();
        need_render.dedup();
        // 4) 预测性预取：滚动方向前方窗口外的页提前入调度队列（低优先级——离可见区
        //    中心远 → 排序后自然排在可见页之后、预算耗尽是自然放弃；有盘缓存的页
        //    到时按需异步恢复即可，不在此预栅格化）。快速拖动时不预取——正在飞掠的
        //    页全尺寸渲染纯属浪费（缩略图层已占位）。
        let dir = (scroll_y - self.last_scroll_y).signum() as i32;
        self.last_scroll_y = scroll_y;
        if dir != 0 && !self.prev_fast {
            let last = self.dims.len().saturating_sub(1);
            let (lo, hi) = if dir > 0 {
                (
                    end.saturating_add(1),
                    end.saturating_add(LOOKAHEAD_PAGES).min(last),
                )
            } else {
                (
                    start.saturating_sub(LOOKAHEAD_PAGES),
                    start.saturating_sub(1),
                )
            };
            if lo <= hi {
                for p in lo..=hi {
                    if self.slots[p].image.is_some()
                        || self.slots[p].loading
                        || self.restoring.contains_key(&p)
                        || self.cache.contains(p)
                    {
                        continue;
                    }
                    need_render.push(p);
                }
                need_render.sort_unstable();
                need_render.dedup();
            }
        }
        // L0：磁盘缩略图兜底——缺全尺寸的可见页若盘上有缩略图，先铺低清占位
        //（几十 KB 小文件，主线程 ~1ms；否则等全尺寸/缩略图请求补齐）。
        for &i in &need_render {
            if self.slots[i].thumb.is_some() || self.slots[i].thumb_loading {
                continue;
            }
            if let Some(img) = self
                .cache
                .load_thumb(i)
                .and_then(|(bgra, w, h)| render_image_from_bgra(bgra, w, h).ok())
            {
                self.slots[i].thumb = Some(img);
            }
        }
        if need_render.is_empty() {
            return;
        }

        // 靠近可见区中心的页先渲染（先填正在看的那页）
        need_render.sort_by_key(|&i| (i as i64 - center as i64).unsigned_abs());

        let worker = self.worker.as_ref().unwrap();
        if self.prev_fast {
            // 快速拖动：**只派中清页**（~520px，清晰可辨识、比全清便宜 ~6×；不派全清——
            // 正在飞掠的页全清渲染纯属浪费，停止后由常态分支补齐）。中清页与缩略图
            // 互斥占位：无中清时缩略图充当更低一级闭环。
            if self.pending_mid.len() < MAX_MID_INFLIGHT {
                let mut mid_budget = MAX_MID_INFLIGHT - self.pending_mid.len();
                for &i in need_render.iter() {
                    if mid_budget == 0 {
                        break;
                    }
                    if self.slots[i].loading
                        || self.slots[i].mid.is_some()
                        || self.slots[i].mid_loading
                    {
                        continue;
                    }
                    self.slots[i].mid_loading = true;
                    let rx = worker.request_mid(self.doc_id, i);
                    self.pending_mid.insert(i, rx);
                    mid_budget -= 1;
                }
            }
        } else {
            // 常态：派发全尺寸渲染（MAX_INFLIGHT 节流）；已在途（loading）的页不重复。
            let inflight = self.pending.len();
            if inflight < MAX_INFLIGHT {
                let budget = MAX_INFLIGHT - inflight;
                for &i in need_render.iter().take(budget) {
                    if self.slots[i].loading {
                        continue;
                    }
                    self.slots[i].loading = true;
                    let scale = render_scale(page_width, scale_factor, self.dims[i].0);
                    let rx = worker.request_render(self.doc_id, RenderRequest { page: i, scale });
                    self.pending.insert(i, rx);
                }
            }
            // 缩略图兜底：仅为「无全尺寸、无中清」的可见页派发（中清是更高清占位，
            // 避免无效的 260px 请求；从中清淡入全清更平滑）。
            if self.pending_thumb.len() < MAX_THUMB_INFLIGHT {
                let mut thumb_budget = MAX_THUMB_INFLIGHT - self.pending_thumb.len();
                for &i in need_render.iter() {
                    if thumb_budget == 0 {
                        break;
                    }
                    if self.slots[i].thumb.is_some()
                        || self.slots[i].thumb_loading
                        || self.slots[i].loading
                        || self.slots[i].mid.is_some()
                        || self.slots[i].mid_loading
                    {
                        continue;
                    }
                    self.slots[i].thumb_loading = true;
                    let rx = worker.request_thumb(self.doc_id, i);
                    self.pending_thumb.insert(i, rx);
                    thumb_budget -= 1;
                }
            }
        }
    }

    /// 回收已完成的在途渲染结果：worker 回传 BGRA → 重建 `RenderImage` 装入内存槽
    /// （代次未变才装；被淘汰/失效的槽只清 loading 允许重投）→ **异步落盘**二级缓存。
    /// 非阻塞：仅检查已就绪通道，未完成的留到下一帧。
    fn collect_results(&mut self, cx: &mut Context<Self>) {
        let generation = self.generation;
        let finished: Vec<(usize, Option<RenderResult>)> = self
            .pending
            .iter_mut()
            .filter_map(|(i, rx)| match rx.try_recv() {
                Ok(res) => Some((*i, res)),
                Err(_) => None,
            })
            .collect();
        let mut ready: Vec<(usize, Arc<RenderImage>)> = Vec::new();
        for (i, res) in finished {
            self.pending.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    // 入队异步落盘（串行写盘见 flush_disk_queue；拖动时瞬态页被丢弃，
                    // 编码不打满后台 CPU）。拷贝是主线程 ~4.5MB memcpy（~百 µs，可接受）。
                    self.disk_queue
                        .push_back((i, res.bgra.clone(), w, h, false));
                    // 主线程快路径：BGRA 直接包裹（零像素循环），worker 已排好字节序
                    if let Ok(img) = render_image_from_bgra(res.bgra, w, h) {
                        ready.push((i, img));
                    }
                }
                None => {
                    if let Some(slot) = self.slots.get_mut(i) {
                        slot.loading = false; // 渲染失败，允许之后重试
                    }
                }
            }
        }
        let mut any = false;
        for (i, img) in ready {
            let mut faded = false;
            if let Some(s) = self.slots.get_mut(i) {
                s.loading = false;
                if generation == self.generation {
                    s.generation = generation;
                    faded = s.thumb.is_some() || s.mid.is_some();
                    if let Some(old) = s.image.replace(img) {
                        self.pending_drops.push(old);
                    }
                }
            }
            // 全尺寸刚就绪且已有占位（中清/缩略图） → 交叉淡入（消除清晰化跳变）
            if faded {
                self.start_fade(i);
            }
            any = true;
        }
        if any {
            cx.notify();
        }
    }

    /// 帧内刷盘：落盘 WebP 编码在后台 executor **串行**执行（最多 1 在途，经
    /// `disk_write_wakeup` 闸门放行下一个），防止逐页编码任务堆积打满后台 CPU。
    /// 快速拖动期间，超过 [`WRITE_QUEUE_MAX`] 的**最旧**条目直接丢弃（瞬态页不落盘/
    /// 不编码）；停止后队列自然排空，热页仍会归档到磁盘。
    fn flush_disk_queue(&mut self, cx: &mut Context<Self>) {
        // 1) 回收在途写盘完成标记 → 放行下一个
        if self.disk_write_inflight {
            if let Some(rx) = &self.disk_write_wakeup {
                if rx.try_recv().is_ok() {
                    self.disk_write_inflight = false;
                    self.disk_write_wakeup = None;
                }
            }
        }
        // 2) 拖动中：丢弃最旧的瞬态条目（保留最近的 WRITE_QUEUE_MAX 个）
        if self.prev_fast && self.disk_queue.len() > WRITE_QUEUE_MAX {
            let excess = trim_transient_writes(self.disk_queue.len(), WRITE_QUEUE_MAX, true);
            self.disk_queue.drain(0..excess);
        }
        if self.disk_write_inflight || self.disk_queue.is_empty() {
            return;
        }
        // 3) 取队首，后台串行编码 + 写盘
        let (idx, bgra, w, h, is_thumb) = self.disk_queue.pop_front().unwrap();
        self.disk_write_inflight = true;
        let (tx, rx) = mpsc::channel();
        self.disk_write_wakeup = Some(rx);
        let cache_inner = self.cache.inner_arc();
        cx.background_executor()
            .spawn(async move {
                if is_thumb {
                    crate::pdf::cache::save_bgra_thumb(cache_inner, idx, bgra, w, h);
                } else {
                    crate::pdf::cache::save_bgra(cache_inner, idx, bgra, w, h);
                }
                let _ = tx.send(());
            })
            .detach();
    }

    /// 迟滞比较：进入快速模式用 [`MOVE_THRESHOLD_PX`]，退出用其×
    /// [`MOVE_THRESHOLD_LOWER_RATIO`]（更小）；中间区间保持上一状态。
    /// 避免临界速度下 LOD 降级/恢复反复横跳造成调度抖动。
    fn update_fast_flag(&mut self) {
        let enter = MOVE_THRESHOLD_PX;
        let exit = MOVE_THRESHOLD_PX * MOVE_THRESHOLD_LOWER_RATIO;
        self.prev_fast = if self.prev_fast {
            self.scroll_velocity > exit
        } else {
            self.scroll_velocity > enter
        };
    }

    /// 页 `i` 的全尺寸位图新就绪且已有**中清或缩略图**占位 → 登记交叉淡入起点。
    /// 动画由 `ensure_fade_animator` 每帧驱动；渲染层在 `FADE_MS` 内把
    /// 全尺寸顶盖透明度 0→1，结束由 `prune_fades` 卸载占位图。
    fn start_fade(&mut self, i: usize) {
        let has_placeholder = self
            .slots
            .get(i)
            .map(|s| s.thumb.is_some() || s.mid.is_some())
            .unwrap_or(false);
        if has_placeholder && !self.prev_fast && !self.fades.contains_key(&i) {
            self.fades.insert(i, std::time::Instant::now());
        }
    }

    /// 若存在未完成的淡入且无驱动任务 → 启动 per-帧 notify 直到 `fades` 清空。
    fn ensure_fade_animator(&mut self, cx: &mut Context<Self>) {
        if self.fade_animator_running || self.fades.is_empty() {
            return;
        }
        self.fade_animator_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let still = this
                    .update(cx, |v, cx| {
                        let alive = v.prune_fades();
                        cx.notify();
                        alive
                    })
                    .unwrap_or(false);
                if !still {
                    break;
                }
            }
            let _ = this.update(cx, |v, _cx| {
                v.fade_animator_running = false;
            });
        })
        .detach();
    }

    /// 清理到期淡入：透明度到 1 后卸载中清/缩略图占位（走 `pending_drops` 显式下线
    /// GPU 纹理，防显存累积）；返回是否仍有在途淡入。
    fn prune_fades(&mut self) -> bool {
        if self.fades.is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        let done: Vec<usize> = self
            .fades
            .iter()
            .filter(|&(_, t)| now.duration_since(*t) >= std::time::Duration::from_millis(FADE_MS))
            .map(|(&p, _)| p)
            .collect();
        for p in done {
            self.fades.remove(&p);
            if let Some(s) = self.slots.get_mut(p) {
                if let Some(arc) = s.thumb.take() {
                    self.pending_drops.push(arc);
                }
                if let Some(arc) = s.mid.take() {
                    self.pending_drops.push(arc);
                }
            }
        }
        !self.fades.is_empty()
    }

    /// 回收已完成的**磁盘恢复**结果：成功 → BGRA 包裹成 `RenderImage` 装入槽
    /// （代次未变才装；被淘汰/失效的槽只清 loading 允许重投）；失败/过期
    /// → 把页号返回给调用方，纳入 `need_render` 真正栅格化。非阻塞。
    fn collect_restores(&mut self, cx: &mut Context<Self>) -> Vec<usize> {
        let generation = self.generation;
        let finished: Vec<(usize, Option<(Vec<u8>, u32, u32)>)> = self
            .restoring
            .iter_mut()
            .filter_map(|(i, rx)| match rx.try_recv() {
                Ok(res) => Some((*i, res)),
                Err(_) => None,
            })
            .collect();
        if finished.is_empty() {
            return Vec::new();
        }
        let mut failed = Vec::new();
        let mut applied = false;
        for (i, res) in finished {
            self.restoring.remove(&i);
            match res {
                Some((bgra, w, h)) => {
                    let mut faded = false;
                    let slot_ok = self
                        .slots
                        .get_mut(i)
                        .map(|s| {
                            s.loading = false;
                            if generation == self.generation {
                                if let Ok(img) = render_image_from_bgra(bgra, w, h) {
                                    s.generation = generation;
                                    faded = s.thumb.is_some() || s.mid.is_some();
                                    if let Some(old) = s.image.replace(img) {
                                        self.pending_drops.push(old);
                                    }
                                    return true;
                                }
                            }
                            false
                        })
                        .unwrap_or(false);
                    if slot_ok && faded {
                        self.start_fade(i); // 恢复完成 + 有缩略图 → 淡入
                    }
                    if slot_ok {
                        applied = true;
                    } else {
                        failed.push(i);
                    }
                }
                None => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.loading = false; // 缺失/损坏，允许栅格化重试
                    }
                    failed.push(i);
                }
            }
        }
        if applied {
            cx.notify();
        }
        failed
    }

    /// 回收已完成的在途 **L0 缩略图**结果：worker 回传低清 BGRA → 装入 `slot.thumb`
    /// （供拖动/回翻期间的低清占位，不依赖代次——缩放后低清占位仍可先用）→ 入队异步落盘。
    /// 非阻塞：仅检查已就绪通道；返回是否有新缩略图入槽（调用方决定重绘）。
    fn collect_thumb_results(&mut self) -> bool {
        let finished: Vec<(usize, Option<RenderResult>)> = self
            .pending_thumb
            .iter_mut()
            .filter_map(|(i, rx)| match rx.try_recv() {
                Ok(res) => Some((*i, res)),
                Err(_) => None,
            })
            .collect();
        let mut any = false;
        for (i, res) in finished {
            self.pending_thumb.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    // 入队异步落盘（与全尺寸同队列，串行节流）
                    self.disk_queue
                        .push_back((i, res.bgra.clone(), w, h, true));
                    if let Some(s) = self.slots.get_mut(i) {
                        s.thumb_loading = false;
                        if let Ok(img) = render_image_from_bgra(res.bgra, w, h) {
                            s.thumb = Some(img);
                        }
                        any = true;
                    }
                }
                None => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.thumb_loading = false; // 失败，允许之后重试
                    }
                }
            }
        }
        any
    }

    /// 回收已完成的在途 **L0.5 中清页**结果：worker 回传 ~520px BGRA → 装入 `slot.mid`
    /// （快速拖动清晰度主力；中清是缩略图的上位替代）。非阻塞；返回是否有新中清入槽。
    fn collect_mid_results(&mut self) -> bool {
        let finished: Vec<(usize, Option<RenderResult>)> = self
            .pending_mid
            .iter_mut()
            .filter_map(|(i, rx)| match rx.try_recv() {
                Ok(res) => Some((*i, res)),
                Err(_) => None,
            })
            .collect();
        let mut any = false;
        for (i, res) in finished {
            self.pending_mid.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    if let Some(s) = self.slots.get_mut(i) {
                        s.mid_loading = false;
                        if let Ok(img) = render_image_from_bgra(res.bgra, w, h) {
                            // 替换旧中清：旧纹理走 pending_drops 显式下线
                            if let Some(old) = s.mid.replace(img) {
                                self.pending_drops.push(old);
                            }
                        }
                        any = true;
                    }
                }
                None => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.mid_loading = false; // 失败，允许之后重试
                    }
                }
            }
        }
        any
    }

    /// 书签目录访问器（侧边栏目录使用）。
    pub(crate) fn outlines(&self) -> &[PdfOutlineEntry] {
        &self.outlines
    }

    // ---- P2 文本层：命中测试 / 选区 / 复制 / 懒加载缓存 ----

    /// 窗口坐标 → (页号, 字符索引)。点空白（列外/页间沟槽/无文本页）返回 `None`。
    ///
    /// 坐标链：窗口坐标 → 滚动容器局部 → 内容(wrap)坐标 → 列坐标（减 [`column_left`]）
    /// → `content_to_local` 定页 + 页内局部 → `local_to_pt` 逆映射回 PDF 点 → `hit_char`。
    fn hit_test(&self, window: &Window, pos: Point<Pixels>) -> Option<(usize, usize)> {
        if self.dims.is_empty() {
            return None;
        }
        let bounds = self.scroll.bounds();
        let mx = pos.x.as_f32() - bounds.left().as_f32();
        let my = pos.y.as_f32() - bounds.top().as_f32();
        let off = self.scroll.offset();
        let wx = mx - off.x.as_f32();
        let wy = my - off.y.as_f32();
        let page_width = self.page_width();
        let wrap_w = self.wrap_width(window, page_width);
        let cx = wx - column_left(wrap_w, page_width);
        let (page, lx, ly) = content_to_local(&self.dims, &self.page_tops, page_width, cx, wy)?;
        let text = self.text_pages.get(&page)?;
        let scale = render_scale(page_width, window.scale_factor(), self.dims[page].0);
        let (px_, py_) =
            local_to_pt(lx, ly, self.dims[page].0, self.dims[page].1, page_width, scale);
        hit_char(text, px_, py_).map(|ci| (page, ci))
    }

    /// 鼠标按下：聚焦（供 Ctrl+C）。命中 → 置锚点/活动端并进入拖拽；点空白 → 清空选区。
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        if event.button != MouseButton::Left {
            return;
        }
        match self.hit_test(window, event.position) {
            Some(pos) => {
                self.sel_anchor = Some(pos);
                self.sel_active = Some(pos);
                self.selecting = true;
            }
            None => {
                self.sel_anchor = None;
                self.sel_active = None;
                self.selecting = false;
            }
        }
        cx.notify();
    }

    /// 按住左键拖拽 → 活动端随鼠标更新（选区实时扩展/收缩）。
    fn on_mouse_move(&mut self, _: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let pos = window.mouse_position();
        if let Some(cur) = self.hit_test(window, pos) {
            if self.sel_active != Some(cur) {
                self.sel_active = Some(cur);
                cx.notify();
            }
        }
    }

    /// 松开：拖拽结束（`selecting` 复位），**选区保留**（anchor + active 都在，可 Ctrl+C）。
    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            cx.notify();
        }
    }

    /// 快捷键：`Ctrl/Cmd+C` 复制选区。（同 markdown 预览的按键处理。
    /// 全文检索已迁移到侧边栏搜索框，PDF 内不再自建搜索框。）
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.keystroke.modifiers;
        if (m.control || m.platform) && !m.alt && !m.shift && event.keystroke.key == "c" {
            self.copy_selection(window, cx);
        }
        cx.notify();
    }

    /// 把当前选区文本写入系统剪贴板。
    fn copy_selection(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// 归一化选区（锚点 ≤ 活动端，页号与字符索引均按此序）。
    fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.sel_anchor?;
        let b = self.sel_active?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// 选区文本：逐页取**已缓存**文本（缺页跳过，尽力而为），跨页以 `\n` 拼接。
    fn selected_text(&self) -> Option<String> {
        let ((ap, ac), (bp, bc)) = self.selection()?;
        let mut out = String::new();
        for page in ap..=bp {
            let Some(text) = self.text_pages.get(&page) else {
                continue;
            };
            let n = text.chars.len();
            if n == 0 {
                continue;
            }
            let lo = if page == ap { ac.min(n - 1) } else { 0 };
            let hi = if page == bp { bc.min(n - 1) } else { n - 1 };
            if lo > hi {
                continue;
            }
            let seg = page_segment_text(text, lo, hi);
            if seg.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&seg);
        }
        (!out.is_empty()).then_some(out)
    }

    /// 文本页入 LRU 缓存（超上限淘汰最久未用）。
    fn cache_text_page(&mut self, page: usize, text: PageText) {
        self.text_lru.retain(|&p| p != page);
        self.text_lru.push_back(page);
        self.text_pages.insert(page, text);
        while self.text_pages.len() > TEXT_CACHE_PAGES {
            if let Some(evict) = self.text_lru.pop_front() {
                self.text_pages.remove(&evict);
            } else {
                break;
            }
        }
    }

    /// 回收已完成的在途文本结果。返回是否有**新**文本入缓存（供调用方决定是否重绘）。
    fn collect_text_results(&mut self) -> bool {
        let finished: Vec<(usize, Option<PageText>)> = self
            .pending_text
            .iter_mut()
            .filter_map(|(p, rx)| match rx.try_recv() {
                Ok(res) => Some((*p, res)),
                Err(_) => None,
            })
            .collect();
        let mut any = false;
        for (page, res) in finished {
            self.pending_text.remove(&page);
            if let Some(text) = res {
                self.cache_text_page(page, text);
                any = true;
            }
        }
        any
    }

    /// 可见窗口文本预取（低优先级懒加载，封顶 [`TEXT_INFLIGHT`] 并发）：
    /// 文本按需缓存，交互前已就绪；worker 侧调度已保证 Render 优先于 Text，
    /// 预取不会拖慢滚动画帧。
    fn prefetch_texts(&mut self, start: usize, end: usize) {
        let Some(worker) = self.worker.clone() else {
            return;
        };
        if self.pending_text.len() >= TEXT_INFLIGHT {
            return;
        }
        let mut budget = TEXT_INFLIGHT - self.pending_text.len();
        let last = self.dims.len().saturating_sub(1);
        for page in start..=end.min(last) {
            if budget == 0 {
                break;
            }
            if self.text_pages.contains_key(&page) || self.pending_text.contains_key(&page) {
                continue;
            }
            let rx = worker.request_text(self.doc_id, page);
            self.pending_text.insert(page, rx);
            budget -= 1;
        }
    }

    // ---- P3 全文检索：由**侧边栏搜索框**驱动（PDF 内不自建搜索框）----
    //
    // 查询经 [`PdfViewer::set_search`] 注入 → 250ms 防抖 → worker 后台全页扫描 →
    // 结果经 [`PdfViewer::collect_search_results`] 收集进 `search_matches` → 阅读器
    // 渲染命中高亮，侧边栏展示计数与页列表。阅读器只负责检索执行、高亮与跳转。

    /// 注入新查询（侧边栏搜索框内容变化时调用）。空串立即清空命中；非空防抖后后台检索。
    pub(crate) fn set_search(&mut self, query: &str, cx: &mut Context<Self>) {
        let q = query.trim().to_string();
        if self.search_query == q {
            return;
        }
        self.search_query = q;
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_truncated = false;
            self.search_active = 0;
            self.search_pending = None;
            cx.notify();
            return;
        }
        self.schedule_search(cx);
    }

    /// 侧边栏读取检索状态：`(命中数, 是否截断, 当前激活命中序号)`。
    pub(crate) fn search_summary(&self) -> (usize, bool, usize) {
        (
            self.search_matches.len(),
            self.search_truncated,
            self.search_active,
        )
    }

    /// 命中上下文片段（侧边栏逐行展示）：`(页号, 前5字+目标+后5字)`，封顶 60 行。
    /// 片段在 worker 搜到时就地生成（`PdfSearchHit::snippet`），不依赖阅读器文本缓存。
    pub(crate) fn search_snippets(&self) -> Vec<(usize, String)> {
        self.search_matches
            .iter()
            .take(60)
            .map(|h| (h.page, h.snippet.clone()))
            .collect()
    }

    /// 跳到某页上**第一个命中**（侧边栏点"第 N 页"）。
    pub(crate) fn focus_match_on_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(pos) = self.search_matches.iter().position(|h| h.page == page) else {
            self.scroll_to_page(page);
            cx.notify();
            return;
        };
        self.search_active = pos;
        self.scroll_to_page(page);
        self.prefetch_texts(page, page);
        cx.notify();
    }

    /// 查询变化 → 防抖调度检索（防抖 250ms，只允许最新一次生效）。
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.search_seq = self.search_seq.wrapping_add(1);
        let seq = self.search_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |v, cx| {
                if v.search_seq == seq {
                    v.run_search(cx);
                }
            });
        })
        .detach();
    }

    /// 向 worker 发起文档级检索（后台低优先级）；结果通道暂存，帧循环里收集。
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let q = self.search_query.trim();
        if q.is_empty() {
            self.search_matches.clear();
            self.search_truncated = false;
            self.search_active = 0;
            self.search_pending = None;
            cx.notify();
            return;
        }
        let Some(worker) = self.worker.clone() else {
            return;
        };
        self.search_pending = None; // 丢弃未消费的旧结果
        let rx = worker.search(self.doc_id, q.to_string());
        self.search_pending = Some(rx);
        self.search_pending_seq = self.search_seq;
    }

    /// 帧循环：收集在途检索结果（代次过期即丢弃）。有**新**有效结果 → true（调用方 notify）。
    fn collect_search_results(&mut self) -> bool {
        let Some(rx) = &self.search_pending else {
            return false;
        };
        let res = match rx.try_recv() {
            Ok(res) => res,
            Err(_) => return false,
        };
        self.search_pending = None;
        let Some(results) = res else {
            return false;
        };
        if self.search_pending_seq != self.search_seq {
            return false; // 过期结果
        }
        self.search_matches = results.hits;
        self.search_truncated = results.truncated;
        self.search_active = if self.search_matches.is_empty() { 0 } else { 0 };
        true
    }

    /// 前进/后退一个命中（循环），滚动到命中页并预取其文本层（高亮立即可用）。
    /// 侧边栏「上一处/下一处」按钮驱动。
    pub(crate) fn move_match(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(next) =
            advance_match_index(self.search_matches.len(), self.search_active, delta)
        else {
            return;
        };
        self.search_active = next;
        let page = self.search_matches[next].page;
        self.scroll_to_page(page);
        self.prefetch_texts(page, page);
        cx.notify();
    }
}

/// 选区背景色（半透蓝；深浅两版，与 markdown 预览同一套色值，视觉统一）。
fn selection_color(cx: &App) -> Hsla {
    if gpui_component::Theme::global(cx).is_dark() {
        rgba(0x4d6bfe61).into()
    } else {
        rgba(0x4d6bfe42).into()
    }
}

/// 某页的选区高亮元素（**卡片局部**坐标，原点 = 卡片左上角，Y 向下）：
/// - 整页覆盖（选区中间的页）→ 单个全卡矩形；
/// - 边界页 → 把 `chars[lo..=hi]` 按行合并（[`selection_spans_pt`]）成矩形条；
/// - 页不在选区 / 文本尚未缓存 → 空。
fn selection_overlays_for_page(
    page: usize,
    selection: Option<((usize, usize), (usize, usize))>,
    dim: (f32, f32),
    display_w: f32,
    scale: f32,
    text: Option<&PageText>,
    color: Hsla,
) -> Vec<gpui::AnyElement> {
    let Some(((lo_p, lo_c), (hi_p, hi_c))) = selection else {
        return Vec::new();
    };
    if lo_p > page || page > hi_p {
        return Vec::new();
    }
    if lo_p < page && page < hi_p {
        return vec![div().absolute().inset_0().bg(color).into_any_element()];
    }
    let Some(text) = text else {
        return Vec::new();
    };
    let n = text.chars.len();
    if n == 0 {
        return Vec::new();
    }
    let from = if page == lo_p { lo_c } else { 0 };
    let to = if page == hi_p { hi_c } else { n - 1 };
    let (lo, hi) = (from.min(to).min(n - 1), from.max(to).min(n - 1));
    if lo > hi {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (l, b, r, t) in selection_spans_pt(text, lo, hi) {
        let (x0, y_top) = pt_to_local(l, t, dim.0, dim.1, display_w, scale); // (left, top → 小 y)
        let (x1, y_bottom) = pt_to_local(r, b, dim.0, dim.1, display_w, scale); // (right, bottom → 大 y)
        let (x, y, w, h) = (x0, y_top, (x1 - x0).max(1.0), (y_bottom - y_top).max(1.0));
        out.push(
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(w))
                .h(px(h))
                .bg(color)
                .into_any_element(),
        );
    }
    out
}

/// 检索命中（非激活）高亮色：半透蓝（与选区蓝区分度低一些）。
fn search_hit_color(cx: &App) -> Hsla {
    if gpui_component::Theme::global(cx).is_dark() {
        rgba(0x60a5fab0).into()
    } else {
        rgba(0x60a5fa70).into()
    }
}

/// 检索激活命中的高亮色：橙（更醒目）。
fn search_active_color() -> Hsla {
    rgba(0xf59e0bcc).into()
}

/// 某页的检索命中高亮元素（**卡片局部**坐标，原点 = 卡片左上角，Y 向下）。
/// 命中区间经 [`selection_spans_pt`] 按行合并；激活命中（`idx == active`）用橙，其余用蓝。
fn search_overlays_for_page(
    page: usize,
    matches: &[PdfSearchHit],
    active: usize,
    text: Option<&PageText>,
    dim: (f32, f32),
    display_w: f32,
    scale: f32,
    hit_color: Hsla,
    active_color: Hsla,
) -> Vec<gpui::AnyElement> {
    let Some(text) = text else {
        return Vec::new();
    };
    let n = text.chars.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (idx, hit) in matches.iter().enumerate() {
        if hit.page != page {
            continue;
        }
        let lo = hit.start.min(n);
        let hi = hit.end.saturating_sub(1).min(n - 1);
        if lo > hi {
            continue;
        }
        let color = if idx == active { active_color } else { hit_color };
        for (l, b, r, t) in selection_spans_pt(text, lo, hi) {
            let (x0, y_top) = pt_to_local(l, t, dim.0, dim.1, display_w, scale);
            let (x1, y_bottom) = pt_to_local(r, b, dim.0, dim.1, display_w, scale);
            out.push(
                div()
                    .absolute()
                    .left(px(x0))
                    .top(px(y_top))
                    .w(px((x1 - x0).max(1.0)))
                    .h(px((y_bottom - y_top).max(1.0)))
                    .bg(color)
                    .into_any_element(),
            );
        }
    }
    out
}

impl Render for PdfViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_window(window, cx);
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let (page_bg, page_border) = Self::page_card_colors(cx);
        let page_width = self.page_width();

        // 错误 / 加载中 / 已加载三种状态
        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            div()
                .h_full()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .text_color(page_border)
                .child(format!("无法打开 PDF：{err}"))
                .into_any_element()
        } else if self.loading || self.dims.is_empty() {
            div()
                .h_full()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .text_color(page_border)
                .child("正在解析 PDF…")
                .into_any_element()
        } else {
            // **页列 DOM 虚拟化**：scroll 容器里放一个「全文档总高」的定位容器，
            // 只把可见页 ± MARGIN 的卡片以绝对定位铺进去。整列高度 = total_height，
            // 所以滚动条范围从一开始就正确；远页（几千页时）不参与本帧 DOM/布局。
            let scroll_y = f32::from(-self.scroll.offset().y);
            let viewport_h = f32::from(self.scroll.bounds().size.height);
            let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);
            // 1d) 预取可见窗口文本层（低优先级懒加载）：交互时命中测试/复制/高亮已就绪。
            //   快速拖动时**跳过**——正在飞掠的页文本提取（FPDFText_GetCharBox 逐字符）
            //   纯属浪费、还会占 pdfium worker 的时间；停止后由 ensure_window 恢复预取。
            if !self.prev_fast {
                self.prefetch_texts(start, end);
            }
            let tops = &self.page_tops;
            let total = self.total_height;
            let slots = &self.slots;
            // P2 选区：render 帧内稳定的快照（高亮渲染用）
            let selection = self.selection();
            let sel_color = selection_color(cx);
            // P3 检索：命中高亮色 + 是否正在检索（非空查询才叠加命中高亮）
            let hit_color = search_hit_color(cx);
            let active_color = search_active_color();
            let searching = !self.search_query.trim().is_empty();
            let scale_factor = window.scale_factor();
            // L0→L1 交叉淡入：本帧时间点 + 淡入周期（秒）。透明度按 `now - 登记时刻`
            // 线性插值；拖动中淡入被取消（fades 清空），此处自然回退到二选一渲染。
            let now = std::time::Instant::now();
            let fade_secs = FADE_MS as f32 / 1000.0;

            let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(end - start + 1);
            for i in start..=end {
                let disp_h = display_height(self.dims[i], page_width);
                let top = tops[i];
                let slot_img = slots.get(i).and_then(|s| s.image.clone());
                let slot_thumb = slots.get(i).and_then(|s| s.thumb.clone());
                let slot_mid = slots.get(i).and_then(|s| s.mid.clone());
                let scale = render_scale(page_width, scale_factor, self.dims[i].0);
                let mut overlays = selection_overlays_for_page(
                    i,
                    selection,
                    self.dims[i],
                    page_width,
                    scale,
                    self.text_pages.get(&i),
                    sel_color,
                );
                if searching {
                    overlays.extend(search_overlays_for_page(
                        i,
                        &self.search_matches,
                        self.search_active,
                        self.text_pages.get(&i),
                        self.dims[i],
                        page_width,
                        scale,
                        hit_color,
                        active_color,
                    ));
                }
                // 卡片保持 **absolute 定位**（页列虚拟化的根基）。⚠️ 不可改成
                // `.relative()`：`absolute`/`relative` 同是 position 设置器、后调用者胜，
                // 一旦覆盖成 relative，页列绝对定位整体失效 → 页距被拉远、滚动跳页、
                // 滚动条量程错乱。选区高亮需要"定位祖先"做锚点 → 改在**卡片内部**包一层
                // `size_full().relative()` 容器：它不影响卡片自身盒定位，又给绝对定位
                // 的高亮子元素提供以卡片左上角为原点的坐标系。
                let mut card = div()
                    .id(SharedString::from(format!("pdf-page-{i}")))
                    .absolute()
                    .top(px(top))
                    .left(px(0.))
                    .flex_shrink_0()
                    .w(px(page_width))
                    .h(px(disp_h))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(page_border)
                    .overflow_hidden();
                let inner = {
                    // 淡入底图：优先中清（更清晰），无中清才用缩略图。
                    let base = slot_mid.clone().or_else(|| slot_thumb.clone());
                    // 交叉淡入透明度：仅在全尺寸与底图同存且正在淡入时 `Some`。
                    let fade_alpha = if slot_img.is_some() && base.is_some() {
                        self.fades.get(&i).map(|&t| {
                            let el = now.duration_since(t).as_secs_f32();
                            (el / fade_secs).clamp(0.0, 1.0)
                        })
                    } else {
                        None
                    };
                    if let (Some(full), Some(base), Some(a)) = (slot_img.clone(), base.clone(), fade_alpha)
                    {
                        // 淡入中：底图（中清/缩略图）为底、全尺寸顶盖透明度 a 渐显
                        div()
                            .size_full()
                            .relative()
                            .child(
                                img(ImageSource::Render(base))
                                    .w(px(page_width))
                                    .h(px(disp_h)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .opacity(a)
                                    .child(
                                        img(ImageSource::Render(full))
                                            .w(px(page_width))
                                            .h(px(disp_h)),
                                    ),
                            )
                    } else if let Some(page_img) = slot_img {
                        div()
                            .size_full()
                            .relative()
                            .child(
                                img(ImageSource::Render(page_img))
                                    .w(px(page_width))
                                    .h(px(disp_h)),
                            )
                    } else if let Some(mid) = slot_mid {
                        // L0.5 中清：快速拖动期/停止等待期的主力显示（比缩略图清晰）
                        div()
                            .size_full()
                            .relative()
                            .child(
                                img(ImageSource::Render(mid))
                                    .w(px(page_width))
                                    .h(px(disp_h)),
                            )
                    } else if let Some(thumb) = slot_thumb {
                        // L0 低清占位：全尺寸/中清未到时先铺缩略图（"永远有内容"）
                        div()
                            .size_full()
                            .relative()
                            .child(
                                img(ImageSource::Render(thumb))
                                    .w(px(page_width))
                                    .h(px(disp_h)),
                            )
                    } else {
                        div()
                            .size_full()
                            .relative()
                            .bg(page_bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(page_border)
                                    .child(format!("第 {} 页 加载中…", i + 1)),
                            )
                    }
                }
                .children(overlays);
                card = card.child(inner);
                cards.push(card.into_any_element());
            }

            let content = div()
                .id("pdf-pages")
                .debug_selector(|| "pdf-pages".into())
                .relative()
                .w(px(page_width))
                .h(px(total))
                .children(cards);

            // 横向+竖向滚动容器 + 定位页列（水平居中，宽 = 页宽，高 = 全文档总高）
            // wrap 不写死宽度：`w_full` 铺满滚动容器可视区 → 内容 justify_center 即
            // 真正居中（不再依赖 window.bounds / scroll.bounds，首帧与每次布局都正确，
            // 侧边栏/分屏等使可视区窄于窗口的形态也不会右偏）；
            // `min_w(页宽+边距)` 保证可视区更窄时 wrap 宽于视口 → 触发横向滚动条，
            // 可滚到全部页面不被左边裁剪。
            div()
                .id("pdf-scroll")
                .debug_selector(|| "pdf-scroll".into())
                .size_full()
                .overflow_y_scroll()
                .overflow_x_scroll()
                .track_scroll(&self.scroll)
                // P2 文本层交互：点选聚焦 → 拖拽选区 → Ctrl/Cmd+C 复制（坐标链见 hit_test）
                .track_focus(&self.focus_handle)
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                .on_mouse_move(cx.listener(Self::on_mouse_move))
                .on_key_down(cx.listener(Self::on_key_down))
                .child(
                    div()
                        .id("pdf-column-wrap")
                        .w_full()
                        .min_w(px(page_width + 2.0 * PAGE_PAD_X))
                        .flex()
                        .flex_row()
                        .justify_center()
                        // Ctrl+滚轮缩放：挂在 wrap（pdf-scroll 的子树）上，bubble 期先于
                        // pdf-scroll 的内置滚动处理 → Ctrl 时 stop_propagation 可阻止滚动；
                        // 普通滚轮不拦截，仍回落到滚动容器。覆盖整个可视区（含页边距）。
                        .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                        .child(content),
                )
                .into_any_element()
        };

        // 外层：PDF 主体铺满画布。目录已统一由侧边栏提供，此处不再有内部目录面板。
        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(canvas)
            .child(
                div()
                    .id("pdf-main")
                    .debug_selector(|| "pdf-main".into())
                    // 右：PDF 页面主体。flex 行里填满剩余宽度；min_w(0) 允许其压缩到内容
                    // 最小宽以下——否则自动最小尺寸=页面列宽(~880px)，滚动条叠层越出窗口
                    // 右侧（横向条的右半不可见）且 bounds/比例与窗口脱节。
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .relative()
                    .child(body)
                    .child(
                        div()
                            .id("pdf-scrollbar-overlay")
                            .debug_selector(|| "pdf-scrollbar-overlay".into())
                            .absolute()
                            .inset_0()
                            .child(
                                Scrollbar::vertical(&self.scroll)
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            )
                            // 横向滚动条：窗口过窄时 scroll 容器内容宽于视口，
                            // 横向溢出可滚；轴 Always 常驻可见。
                            .child(
                                Scrollbar::horizontal(&self.scroll)
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            )
    }
}
