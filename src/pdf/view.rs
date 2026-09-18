//! PDF 阅读器视图：瓦片级虚拟化核心（加载、滚动调度、按需瓦片渲染、滚动条）。
//! **零磁盘缓存**：瓦片与占位（缩略/中清）全部纯内存，关闭文档即释放。
//!
//! 子模块拆分（都在 `src/pdf/` 下）：
//! - [`super::layout`]：页列纯布局 + 瓦片网格几何（worker 与 view 共用同一套数学）
//!
//! 书签目录已统一移到侧边栏（[`crate::app::sidebar`] + 公共 [`crate::toc`]），
//! 本模块只保留书签数据（`outlines`）与 [`Self::scroll_to_page`] 跳页能力。
//!
//! 关键机制：
//! - **打开即显**：文件读取 + 结构解析 + 页尺寸测量都在后台线程做（便宜），
//!   完成后马上铺好全部页槽（滚动条正确）；**不**预先栅格化任何页。
//! - **瓦片按需栅格化（SumatraPDF 式分块）**：每帧根据滚动位置与可视区，把可见页
//!   切成 [TILE_SIZE × TILE_SIZE] 网格，只向后台派发「视口可见 + TILE_MARGIN 缓冲」
//!   的少数几片（每片 ~1MB）。内存 = 视口面积 × 4B，**不再随整页位图缩放爆炸**；
//!   瓦片按显示物理像素（[`super::layout::render_scale`]）1:1 渲染，高 DPI / Ctrl+滚轮
//!   放大始终锐利（根治旧版"一页 4× 位图 64MB + 密度封顶发糊"）。
//! - **占位层**：L0 缩略图 / L0.5 中清页在**快速拖动**期间铺为全页低清底图（瓦片
//!   逐片到达前"永远有内容"）；停止后瓦片立即按新视口补齐，替换占位。
//! - **回收**：滚离视口的瓦片按 [`TILE_CACHE_BYTES`] 字节预算 + 距离 LRU 淘汰，
//!   显式 `drop_image` 下线 GPU 纹理；缩放/换 DPI 后 `tile_scale` 比对惰性作废。
//! - **不闪烁**：页槽先是有尺寸占位（页号/加载中），瓦片到位后替换，布局稳定。
//!
//! ## 性能优化（相对旧整页版本）
//!
//! 1. **页列 DOM 虚拟化**：`render()` 每帧只构建「可见页 + MARGIN 缓冲」这几张页面卡片
//!    并以绝对定位铺在等高的“全文档高度”容器里（[`super::layout::compute_layout`]），
//!    不再把几百上千页全部 `div()` 出来布局——长文档滚动/换帧开销从 O(页数) 降到 O(可见页)。
//! 2. **瓦片级调度**：派发单位从"整页"降为"512px 瓦片"，视口填充粒度细、并发可更高
//!    （[`TILE_INFLIGHT`]），快速滚动时只会浪费少量小片，不再排队整页巨型任务。
//! 3. **在途请求节流**：瓦片/中清/缩略图各自额度封顶，单线程 pdfium 串行渲染吞吐不变，
//!    快速滚动时不会无限堆积过期请求。

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
    COVER_INFLIGHT, MAX_MID_INFLIGHT, MAX_THUMB_INFLIGHT, MID_CACHE_PAGES, MOVE_THRESHOLD_LOWER_RATIO, MOVE_THRESHOLD_PX, PAGE_PAD_X, PAGE_WIDTH, TEXT_CACHE_PAGES,
    TEXT_INFLIGHT, THUMB_CACHE_PAGES, TILE_CACHE_BYTES, TILE_INFLIGHT, TILE_MARGIN, TILE_SIZE,
    ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
};
use crate::pdf::layout::{
    advance_match_index, bitmap_display_scales, column_left, compute_layout, content_to_local,
    cover_affordable, cover_scale, display_height, hit_char, keep_window, local_to_pt,
    page_bitmap_size, page_segment_text, pt_to_local, render_scale, selection_spans_pt,
    visible_tile_range, Slot, TileSlot,
};
use crate::pdf::{
    render_image_from_bgra, DocId, PageText, PdfOutlineEntry, PdfSearchHit, PdfSearchResults,
    PdfWorker, RenderRequest, RenderResult, TileRequest, TileResult,
};
/// 是否开启视图侧计时日志（`GPUI_EDITOR_PDF_TIMING=1`，进程内缓存一次）。
fn timing_view() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("GPUI_EDITOR_PDF_TIMING").map_or(false, |v| v == "1" || v == "true")
    })
}

/// PDF 阅读器：自包含的瓦片级虚拟化视图。
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
    /// 每页渲染槽（含瓦片缓存 + 缩略/中清占位）。
    pub(crate) slots: Vec<Slot>,
    /// 在途 **L0 缩略图**请求的结果接收通道（独立于瓦片额度）。
    pub(crate) pending_thumb: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 在途 **L0.5 中清页**请求的结果接收通道（快速拖动占位）。
    pub(crate) pending_mid: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 中清页内存 LRU 顺序（最近使用在队尾）。纹理本体就在 `slots[i].mid`
    /// （O(1) 主线程查找），本队列只用于淘汰顺序。
    pub(crate) mid_lru: VecDeque<usize>,
    /// 在途**瓦片**请求：`(页, row, col)` → (请求时密度 scale, 结果通道)。
    /// `scale` 用于结果到达时的密度守卫：缩放/换 DPI 后旧密度瓦片直接丢弃。
    pub(crate) pending_tiles: HashMap<(usize, u32, u32), (f32, mpsc::Receiver<Option<TileResult>>)>,
    /// 在途**封面**（首屏整页锐化）请求：`页` → (密度 scale, 结果通道)。
    pub(crate) pending_cover: HashMap<usize, (f32, mpsc::Receiver<Option<RenderResult>>)>,
    /// 当前已驻留瓦片的估算字节（近似 RGBA 内存；淘汰时重算自愈漂移）。
    pub(crate) tile_bytes: u64,
    /// 瓦片自刷新泵是否在途：派发瓦片后若无用户输入就不会再有重绘，而瓦片结果要等
    /// 下一次 `ensure_window` 才收集拼图——此泵每 ~16ms 触发一次重绘直到在途瓦片清空。
    pub(crate) tile_pump_running: bool,
    /// 上一帧滚动偏移（预测滚动方向用：delta>0 正在往下滚 → 预取下方页）。
    pub(crate) last_scroll_y: f32,
    /// 滚动速度估计（px/帧，EMA)：> [`MOVE_THRESHOLD_PX`] 视为快速拖动 →
    /// 改派缩略/中清占位、暂停瓦片派发（避免小片洪流），停止后瓦片立即补齐。
    pub(crate) scroll_velocity: f32,
    /// 迟滞判定后的"快速拖动"标志（进入 v>阈值、退出 v<阈值×比率，中间保持）。
    pub(crate) prev_fast: bool,
    /// 竖向滚动句柄（页列外层 scroll 容器）。
    pub(crate) scroll: ScrollHandle,
    /// 是否仍在后台解析。
    pub(crate) loading: bool,
    /// 加载失败信息。
    pub(crate) load_error: Option<String>,
    /// 阅读画布背景（页面“纸面”之外底色；None 用主题推导）。
    pub(crate) bg: Option<Hsla>,
    /// 待释放的旧位图/旧瓦片（替换或回收的，需在下一帧 `drop_image`）。
    pub(crate) pending_drops: Vec<Arc<RenderImage>>,
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

    /// Ctrl+滚轮缩放：页列宽按倍率变化，锚定视口顶部内容点，重算布局。
    /// 瓦片不在此清空——回到可见页时 [`Slot::tile_scale`] 与新密度比对会惰性作废，
    /// 在途旧密度瓦片由 [`Self::collect_tile_results`] 的 scale 守卫丢弃。
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

    /// 应用缩放因子并维护其后的一致性：锚定滚动偏移、重算页列布局。
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
        // 重算布局：瓦片按新密度惰性重渲（见 `schedule_tiles` 的 tile_scale 比对）。
        let (tops, total) = compute_layout(&self.dims, new_w);
        self.page_tops = tops;
        self.total_height = total;
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

    /// 每帧：回收在途结果、估速、刷盘、按视口调度瓦片（常态）/占位（快速拖动）、
    /// 淘汰超预算瓦片。
    fn ensure_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            if MEM_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 30 == 0 {
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
        let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);
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
        // 5) L0.5 中清内存封顶：中清页（~1.5MB/张）数量硬顶。删除统一走 `pending_drops`
        //    （下一帧开头 `cx.drop_image` 显式下线 GPU 纹理，防显存/提交内存缓慢累积）。
        let mid_retained = self.slots.iter().filter(|s| s.mid.is_some()).count();
        if mid_retained > MID_CACHE_PAGES {
            // LRU 先与槽同步（删除即丢引用，保证预算与队列一致），再按 LRU 淘汰。
            self.mid_lru
                .retain(|&p| self.slots.get(p).map_or(false, |s| s.mid.is_some()));
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

        // 7) 瓦片在途自刷新：若无用户输入就没有下一次重绘，而瓦片结果要等下一次
        //    `ensure_window` 才收集——启动 ~16ms 泵直到在途瓦片清空（静置也能出图）。
        self.ensure_tile_pump(cx);
    }

    /// 瓦片在途期间的自刷新泵：仅当「有在途瓦片/封面」或「已加载但滚动容器尚未拿到真实
    /// 布局高度（首帧 bounds=0，此时视口为空 → 调度不到任何瓦片）」时启动，每 ~16ms 触发
    /// 一次窗口重绘（`refresh_windows`），直到工作清空后自动停止。避免"派发了瓦片但没有任何
    /// 事件触发重绘 → 首屏永远停留在占位/加载中"以及"首帧 bounds=0 → 永远调不到瓦片"。
    fn ensure_tile_pump(&mut self, cx: &mut Context<Self>) {
        if self.tile_pump_running || !self.pump_still_needed() {
            return;
        }
        self.tile_pump_running = true;
        if timing_view() {
            eprintln!("[{:.1}ms] [VIEW-PUMP] start pending_t={}", crate::pdf::timing_ms(), self.pending_tiles.len());
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let still = this
                    .update(cx, |v, cx| {
                        if timing_view() {
                            eprintln!("[{:.1}ms] [VIEW-PUMP] tick pending_t={}", crate::pdf::timing_ms(), v.pending_tiles.len());
                        }
                        // 关键：`refresh_windows()` 无条件要求全部窗口重绘（不依赖实体是否被
                        // 窗口 tracking，测试/真实 app 都可靠）。单用 `notify()` 在实体未被
                        // `tracked_entities` 收录时是空操作 → “worker 渲完了但从未重绘”。
                        // 16ms 泵节流，工作清空即停。
                        cx.refresh_windows();
                        v.pump_still_needed()
                    })
                    .unwrap_or(false);
                if !still {
                    break;
                }
            }
            let _ = this.update(cx, |v, _cx| v.tile_pump_running = false);
        })
        .detach();
    }

    /// 泵是否仍需运行：有在途瓦片/封面 → 重绘收图；已加载但滚动容器布局仍为零高
    /// （首帧 layout 落后于 render）→ 重绘以拿到真实视口、否则瓦片永远调度不出来。
    /// **额外关键分支：仍处快速模式（`prev_fast`）时持续出帧。** 速度 EMA 靠“帧”衰减，
    /// 而帧由输入驱动——滚轮骤停时最后几帧速度还在快速阈值上（`fast=true` 且无在途 →
    /// 泵自停 → 再没有帧 → `prev_fast` 卡死、`ensure_window` 不再跑 → 可见页停在低清占位，
    /// 直到用户下一次输入才换挡补瓦片（实测日志：1.7 秒无 `[VIEW-TICK]`，正是“随机页先
    /// 模糊、数秒后清晰”的根因）。本分支让泵一直驱动到退出快速态并完成清尾，无需输入。
    fn pump_still_needed(&self) -> bool {
        if self.prev_fast {
            return true;
        }
        if !self.pending_tiles.is_empty() || !self.pending_cover.is_empty() {
            return true;
        }
        if !self.loading && !self.dims.is_empty() && self.load_error.is_none() {
            let viewport_h = f32::from(self.scroll.bounds().size.height);
            if viewport_h <= 0.0 {
                return true;
            }
        }
        false
    }

    /// 渲染驻留内存水位（计时日志，`GPUI_EDITOR_PDF_TIMING=1` 时每 ~30 帧打一行）：
    /// 瓦片字节 + 封面（整页位图，按当前密度估算字节）+ 中清/缩略计数，便于实时看
    /// “内存到底是谁占的”（瓦片 vs 封面 vs 中清 vs pdfium 内部未计入部分）。
    fn log_memory_waterline(&self, scale_factor: f32) {
        let pw = self.page_width();
        let mids = self.slots.iter().filter(|s| s.mid.is_some()).count();
        let thumbs = self.slots.iter().filter(|s| s.thumb.is_some()).count();
        let mut covers = 0usize;
        let mut cover_bytes = 0u64;
        for (p, s) in self.slots.iter().enumerate() {
            if s.cover.is_some() {
                covers += 1;
                let cscale = cover_scale(self.dims[p], pw, scale_factor);
                let (w, h) = page_bitmap_size(self.dims[p], cscale);
                cover_bytes += (w as u64) * (h as u64) * 4;
            }
        }
        // 中清 ~2MB、缩略 ~0.37MB 为近似（不逐页精确，仅用于水位构成）
        let mid_bytes = mids as u64 * 2 * 1024 * 1024;
        let thumb_bytes = thumbs as u64 * 385 * 1024;
        let total = self.tile_bytes + cover_bytes + mid_bytes + thumb_bytes;
        eprintln!(
            "[{:.1}ms] [VIEW-MEM] tiles={:.1}MB covers={}({:.1}MB) mids={} thumbs={} est_total={:.1}MB",
            crate::pdf::timing_ms(),
            self.tile_bytes as f64 / 1048576.0,
            covers,
            cover_bytes as f64 / 1048576.0,
            mids,
            thumbs,
            total as f64 / 1048576.0,
        );
    }

    /// 常态瓦片调度：为可见页推算视口相交的瓦片网格区间，惰性作废旧密度瓦片，
    /// 按「离视口中心远近」排序后向 worker 派发（[`TILE_INFLIGHT`] 节流）。
    fn schedule_tiles(
        &mut self,
        window: &Window,
        start: usize,
        end: usize,
        page_width: f32,
        scale_factor: f32,
        scroll_y: f32,
    ) {
        let wrap_w = self.wrap_width(window, page_width);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let tile_size = TILE_SIZE;
        // 先量「想看的窗」[真正的可见页 ±1 邻居]（与封面同源）：瓦片与封面都只服务这一窗。
        // `start..=end` 来自 `keep_window`、含 MARGIN 页；滚得更远的页**不再预取瓦片/封面**
        // ——滚动越过的页即时回收，驻留内存不再随文档长度增长（MARGIN 页到眼前自然进窗
        // 再按需渲染，由封面/瓦片补上）。
        let mut vis_first: Option<usize> = None;
        let mut vis_last: Option<usize> = None;
        for page in start..=end {
            if self
                .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                .is_some()
            {
                vis_first.get_or_insert(page);
                vis_last = Some(page);
            }
        }
        let vis_first = vis_first.unwrap_or(start);
        let vis_last = vis_last.unwrap_or(end);
        let first = vis_first.saturating_sub(1);
        let last = (vis_last + 1).min(self.dims.len().saturating_sub(1));
        let center = (start + end) / 2;
        let mut need: Vec<(usize, u32, u32, u64)> = Vec::new();

        for page in first..=last {
            let dim = self.dims[page];
            let scale = render_scale(page_width, scale_factor, dim.0);
            let bmp = page_bitmap_size(dim, scale);
            let disp_h = display_height(dim, page_width);
            let rect = self.page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w);

            // 惰性失效：缩放/DPI 已变 → 清该页旧密度瓦片（在途旧密度结果由 scale 守卫丢弃）。
            {
                let slot = &mut self.slots[page];
                if (slot.tile_scale - scale).abs() > 1e-4 {
                    for (_, ts) in slot.tiles.drain() {
                        self.tile_bytes = self.tile_bytes.saturating_sub(ts.bytes());
                        self.pending_drops.push(ts.image);
                    }
                    slot.tile_scale = scale;
                }
            }

            let Some(rect) = rect else { continue };
            // 整页封面已在当前密度下覆盖整页 → 该页瓦片全部冗余，**不再派发**（省渲染与
            // 内存：封面即全页锐化底图，瓦片只服务于「无封面/封面被密度上限截断」的页）。
            let skip_covered = self.slots[page].cover.is_some()
                && (self.slots[page].cover_scale - scale).abs() <= 1e-4;
            if skip_covered {
                continue;
            }
            let Some((r0, c0, r1, c1)) =
                visible_tile_range(bmp, page_width, disp_h, rect, tile_size, TILE_MARGIN)
            else {
                continue;
            };
            let mid_row = (r0 + r1) / 2;
            for row in r0..=r1 {
                for col in c0..=c1 {
                    if self.slots[page].tiles.contains_key(&(row, col))
                        || self.pending_tiles.contains_key(&(page, row, col))
                    {
                        continue;
                    }
                    let d = (page as i64 - center as i64).unsigned_abs() as u64 * 1_000_000
                        + (row as i64 - mid_row as i64).unsigned_abs() as u64;
                    need.push((page, row, col, d));
                }
            }
        }

        // 瓦片（**先派**）：可见页瓦片先进 worker 队列 → 停稳瞬间可见条带每片 ~2-8ms
        // 逐片出图、首片 ~5ms 就清晰。日志佐证：快速澄清的页没有 `[PAGE-RENDER] scale=2.47`
        // 全页封面、纯靠瓦片填满；慢的页是「整页封面 20-40ms 堵在瓦片前面」才清晰。故把
        // 封面调度排到瓦片之后：可见页的封面/下折叠与邻居预取仍会整页锐化，只是不再阻塞
        // 首屏瓦片（封面候选集合、驻留与淘汰语义完全不变，风暴安全无回归）。
        if need.is_empty() {
            if timing_view() {
                eprintln!("[{:.1}ms] [VIEW-SCHED] need empty pending_t={}", crate::pdf::timing_ms(), self.pending_tiles.len());
            }
        } else {
            // 靠近视口的瓦片先渲染（正在看的那片先填）
            need.sort_by_key(|&(_, _, _, d)| d);
            let worker = self.worker.clone().unwrap();
            let budget = TILE_INFLIGHT.saturating_sub(self.pending_tiles.len());
            for (page, row, col, _) in need.into_iter().take(budget) {
                let scale = render_scale(page_width, scale_factor, self.dims[page].0);
                let rx = worker.request_tile(
                    self.doc_id,
                    TileRequest {
                        page,
                        scale,
                        tile_size,
                        row,
                        col,
                    },
                );
                self.pending_tiles.insert((page, row, col), (scale, rx));
            }
        }

        // 封面（**后派**）：**只服务想看的窗 [first..=last]**（真正可见页 ±1 邻居，见上方
        // 求值；非 MARGIN 全窗——否则候选页数超驻留上限 → 换页风暴）。中心页优先出队。
        let cover_worker = self.worker.clone().unwrap();
        // 封面按「离可视中心远近」派发：正在看的页封面先出队（预览/整页锐化优先级更高，
        // 但已在瓦片之后，不再与首屏瓦片抢 worker）。
        let center_vis = (vis_first + vis_last) / 2;
        let mut cover_order: Vec<usize> = (first..=last).collect();
        cover_order.sort_by_key(|&p| (p as i64 - center_vis as i64).unsigned_abs());
        for page in cover_order {
            let dim = self.dims[page];
            let cscale = cover_scale(dim, page_width, scale_factor);
            // 缩放过期（缩放/DPI 已变）→ 丢弃旧封面
            if let Some(slot) = self.slots.get_mut(page) {
                let stale = slot.cover.is_some() && (slot.cover_scale - cscale).abs() > 1e-4;
                if stale {
                    if let Some(old) = slot.cover.take() {
                        self.pending_drops.push(old);
                    }
                }
            }
            // 整页不超预算 且 无封面 且 未在途 → 派发
            if cover_affordable(dim, cscale)
                && self.slots[page].cover.is_none()
                && !self.pending_cover.contains_key(&page)
                && self.pending_cover.len() < COVER_INFLIGHT
            {
                if timing_view() {
                    eprintln!("[{:.1}ms] [VIEW-COVER-REQ] page={page}", crate::pdf::timing_ms());
                }
                let rx =
                    cover_worker.request_render(self.doc_id, RenderRequest { page, scale: cscale });
                self.pending_cover.insert(page, (cscale, rx));
            }
        }
        self.evict_covers(first, last);
    }

    /// 页 `page` 在当前视口内**可见**的卡片局部矩形 `(x0, y0, x1, y1)`（CSS px），
    /// 已夹在页内；完全不可见（滚出窗口/水平溢出）→ `None`。与 `hit_test` 用同一套
    /// 坐标链（窗口 → 滚动 → wrap → 去列左偏移），保证"圈中的瓦片 = 正在看的瓦片"。
    fn page_visible_rect(
        &self,
        page: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        wrap_w: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        let top = self.page_tops.get(page).copied()?;
        let dim = self.dims.get(page).copied()?;
        let disp_h = display_height(dim, page_width);
        let vy0 = (scroll_y - top).clamp(0.0, disp_h);
        let vy1 = (scroll_y + viewport_h - top).clamp(0.0, disp_h);
        if vy1 <= vy0 {
            return None;
        }
        let col_left = column_left(wrap_w, page_width);
        let off_x = f32::from(-self.scroll.offset().x);
        let viewport_w = f32::from(self.scroll.bounds().size.width);
        let vx0 = (off_x - col_left).clamp(0.0, page_width);
        let vx1 = (off_x - col_left + viewport_w).clamp(0.0, page_width);
        if vx1 <= vx0 {
            return None;
        }
        Some((vx0, vy0, vx1, vy1))
    }

    /// 快速拖动占位：为可见页派发 L0.5 中清（比缩略图清晰，可辨识标题/段落）、
    /// L0 缩略图（纯内存，无磁盘缓存）。停止后常态调度自动补齐瓦片、替换占位。
    fn schedule_placeholders(
        &mut self,
        window: &Window,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        scale_factor: f32,
    ) {
        let worker = self.worker.clone().unwrap();
        // 1) L0.5 中清：优先派发（比缩略图清晰），独立额度不挤占瓦片/缩略图
        if self.pending_mid.len() < MAX_MID_INFLIGHT {
            let mut budget = MAX_MID_INFLIGHT - self.pending_mid.len();
            for page in start..=end {
                if budget == 0 {
                    break;
                }
                if self.mid_cached(page).is_some() {
                    continue; // 主线程缓存命中：直接复用（含 LRU 触摸），不发 WorkerMsg::Mid
                }
                if self.slots[page].mid_loading {
                    continue;
                }
                self.slots[page].mid_loading = true;
                let rx = worker.request_mid(self.doc_id, page, scale_factor);
                self.pending_mid.insert(page, rx);
                budget -= 1;
            }
        }
        // 2) L0 缩略图：仅为「无中清/无在途中清」的可见页派发
        if self.pending_thumb.len() < MAX_THUMB_INFLIGHT {
            let mut tb = MAX_THUMB_INFLIGHT - self.pending_thumb.len();
            for page in start..=end {
                if tb == 0 {
                    break;
                }
                if self.slots[page].thumb.is_some()
                    || self.slots[page].thumb_loading
                    || self.slots[page].mid.is_some()
                    || self.slots[page].mid_loading
                {
                    continue;
                }
                self.slots[page].thumb_loading = true;
                let rx = worker.request_thumb(self.doc_id, page);
                self.pending_thumb.insert(page, rx);
                tb -= 1;
            }
        }
        // 3) 减速期可见条带瓦片预热：仍 fast（迟滞未退出）但速度已降到「进入阈值」之下 →
        //    用户即将停稳。此时把**真正可见页**缺失的瓦片提前派发（中心页优先、
        //    TILE_INFLIGHT 封顶）：停稳换挡瞬间可见条带多半已被瓦片填满 → “停稳即清晰”
        //    走快路径（小片 ~2-8ms 逐片出图）。
        //    ⚠️ 封面是“慢路径”的修正依据：日志佐证 **[PAGE-RENDER] scale=2.47 整页封面
        //    20-40ms 一张、窗内 2-3 张串行，且堵在瓦片前面**；而“停稳即清晰”的页都是纯瓦片
        //    填满、没有整页封面。故这里不再派封面（原来预热的封面恰恰制造了“模糊→换挡→
        //    重新清晰”的慢感）；封面改由停稳帧 `schedule_tiles` 在瓦片之后预取邻居/下折叠。
        //    ⚠️ 可见区间必须与 `schedule_tiles` 同源（`page_visible_rect` 非空页），不能用
        //    `keep_window` 的算术中心（含 MARGIN 缓冲页，会偏出真正可见页）。
        //    风暴安全：速度 > MOVE_THRESHOLD_PX 的长距飞掠不派（避免小片洪流）；同一片
        //    已在 slot / 在途则跳过；TILE_INFLIGHT 封顶。（本函数只在 `prev_fast` 分支被调用。）
        if self.scroll_velocity < MOVE_THRESHOLD_PX {
            let page_width = self.page_width();
            let wrap_w = self.wrap_width(window, page_width);
            let tile_size = TILE_SIZE;
            let mut need: Vec<(usize, u32, u32, u64)> = Vec::new();
            let mut vis_first: Option<usize> = None;
            let mut vis_last: Option<usize> = None;
            for page in start..=end {
                if self
                    .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                    .is_some()
                {
                    vis_first.get_or_insert(page);
                    vis_last = Some(page);
                }
            }
            if let (Some(vf), Some(vl)) = (vis_first, vis_last) {
                let center_vis = (vf + vl) / 2;
                for page in vf..=vl {
                    let dim = self.dims[page];
                    let scale = render_scale(page_width, scale_factor, dim.0);
                    // 同密度整页封面在位 → 该页瓦片冗余（与 schedule_tiles 同一判据），不预热
                    let slot = &self.slots[page];
                    if slot.cover.is_some() && (slot.cover_scale - scale).abs() <= 1e-4 {
                        continue;
                    }
                    let bmp = page_bitmap_size(dim, scale);
                    let disp_h = display_height(dim, page_width);
                    let Some(rect) = self
                        .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                    else {
                        continue;
                    };
                    let Some((r0, c0, r1, c1)) =
                        visible_tile_range(bmp, page_width, disp_h, rect, tile_size, TILE_MARGIN)
                    else {
                        continue;
                    };
                    let mid_row = (r0 + r1) / 2;
                    for row in r0..=r1 {
                        for col in c0..=c1 {
                            if self.slots[page].tiles.contains_key(&(row, col))
                                || self.pending_tiles.contains_key(&(page, row, col))
                            {
                                continue;
                            }
                            let d = (page as i64 - center_vis as i64).unsigned_abs() as u64
                                * 1_000_000
                                + (row as i64 - mid_row as i64).unsigned_abs() as u64;
                            need.push((page, row, col, d));
                        }
                    }
                }
                // 中心页/中心行优先，只补在途预算内
                need.sort_by_key(|&(_, _, _, d)| d);
                let budget = TILE_INFLIGHT.saturating_sub(self.pending_tiles.len());
                for (page, row, col, _) in need.into_iter().take(budget) {
                    if timing_view() {
                        eprintln!("[{:.1}ms] [VIEW-TILE-REQ] warm p{page} r{row}c{col}", crate::pdf::timing_ms());
                    }
                    let scale = render_scale(page_width, scale_factor, self.dims[page].0);
                    let rx = worker.request_tile(
                        self.doc_id,
                        TileRequest {
                            page,
                            scale,
                            tile_size,
                            row,
                            col,
                        },
                    );
                    self.pending_tiles.insert((page, row, col), (scale, rx));
                }
            }
        }
    }

    /// 回收已完成的在途瓦片结果：worker 回传 RGBA → 重建 `RenderImage` 装入对应页槽
    /// （密度守卫：请求时的 `scale` ≠ 当前 `scale` → 丢弃，缩放已变）。替换旧瓦片时
    /// 旧纹理走 `pending_drops` 显式下线。返回是否有新瓦片入槽（调用方决定重绘）。
    fn collect_tile_results(&mut self, scale_factor: f32) -> bool {
        let finished: Vec<((usize, u32, u32), Option<TileResult>)> = self
            .pending_tiles
            .iter_mut()
            .filter_map(|(k, (_s, rx))| match rx.try_recv() {
                Ok(res) => Some((*k, res)),
                Err(_) => None,
            })
            .collect();
        if finished.is_empty() {
            return false;
        }
        let page_width = self.page_width();
        let mut any = false;
        for (key, res) in finished {
            let req_scale = self
                .pending_tiles
                .remove(&key)
                .map(|(s, _)| s)
                .unwrap_or(0.0);
            let (page, row, col) = key;
            let Some(res) = res else { continue };
            if page >= self.dims.len() {
                continue;
            }
            let scale = render_scale(page_width, scale_factor, self.dims[page].0);
            // 结果一致性守卫：回传格号/页号必须与请求一致、页位图尺寸必须与当前密度
            // 同宗（否则丢弃，防错位拼片）。
            if res.page != page || res.row != row || res.col != col {
                continue;
            }
            let cur_bmp = page_bitmap_size(self.dims[page], scale);
            if res.full_w != cur_bmp.0 || res.full_h != cur_bmp.1 {
                continue;
            }
            if (req_scale - scale).abs() > 1e-4 {
                continue; // 缩放/DPI 已变 → 过期密度瓦片丢弃
            }
            let Ok(img) = render_image_from_bgra(res.bgra, res.w, res.h) else {
                continue;
            };
            if let Some(slot) = self.slots.get_mut(page) {
                // 该页此时已有同密度整页封面 → 瓦片冗余（调度侧已不派新的，此处兜底收到
                // 封面到位前/后仍可能在途的老瓦片）：直接回收，不占内存不占显存。
                if slot.cover.is_some() && (slot.cover_scale - scale).abs() <= 1e-4 {
                    self.pending_drops.push(img);
                    continue;
                }
                let ts = TileSlot {
                    image: img,
                    x0: res.x0,
                    y0: res.y0,
                    w: res.w,
                    h: res.h,
                };
                let est = ts.bytes();
                if let Some(old) = slot.tiles.insert((row, col), ts) {
                    self.tile_bytes = self.tile_bytes.saturating_sub(old.bytes());
                    self.pending_drops.push(old.image);
                }
                self.tile_bytes = self.tile_bytes.saturating_add(est);
                any = true;
            }
        }
        any
    }

    /// 瓦片字节预算淘汰（LRU by 距视口中心远近）。先重算精确占用（自愈记账漂移），
    /// 超预算时从最远瓦片开始移除，旧纹理走 `pending_drops` 显式下线 GPU 纹理。
    /// **每帧全局清理（同密度封面瓦片冗余 + 过期密度瓦片）**：`schedule_tiles` 只对窗内
    /// 页做密度失效，窗外页的旧密度瓦片会滞留到 32MB 预算满——这里对**所有页**统一清：
    /// - 同密度整页封面在位（`cover_scale≈当前密度`）→ 瓦片冗余，清（封面即全页锐化底图）；
    /// - `tile_scale` 与当前密度不符 → 瓦片已过期（回看也必重渲），清。
    /// 只留下「无封面、当前密度」的窗外页瓦片作滚动回看缓存，受 [`TILE_CACHE_BYTES`] 约束。
    fn evict_tiles(&mut self, start: usize, end: usize, scale_factor: f32) {
        let pw = self.page_width();
        {
            let mut drops: Vec<Arc<RenderImage>> = Vec::new();
            for (p, slot) in self.slots.iter_mut().enumerate() {
                let cur = render_scale(pw, scale_factor, self.dims[p].0);
                let redundant =
                    slot.cover.is_some() && (slot.cover_scale - cur).abs() <= 1e-4;
                let stale = (slot.tile_scale - cur).abs() > 1e-4;
                if redundant || stale {
                    for (_, ts) in slot.tiles.drain() {
                        self.tile_bytes = self.tile_bytes.saturating_sub(ts.bytes());
                        drops.push(ts.image);
                    }
                }
            }
            self.pending_drops.extend(drops);
        }
        if self.tile_bytes <= TILE_CACHE_BYTES {
            return;
        }
        // 重算精确占用：防 collect/失效路径的加减记账漂移
        self.tile_bytes = self
            .slots
            .iter()
            .map(|s| s.tiles.values().map(|t| t.bytes()).sum::<u64>())
            .sum();
        if self.tile_bytes <= TILE_CACHE_BYTES {
            return;
        }
        let center = (start + end) / 2;
        let mut candidates: Vec<(u64, usize, u32, u32)> = Vec::new();
        for (page, slot) in self.slots.iter().enumerate() {
            for &(row, col) in slot.tiles.keys() {
                let d = (page as i64 - center as i64).unsigned_abs() as u64 * 1_000_000
                    + row as u64;
                candidates.push((d, page, row, col));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0)); // 最远优先
        for (_, page, row, col) in candidates {
            if self.tile_bytes <= TILE_CACHE_BYTES {
                break;
            }
            if let Some(slot) = self.slots.get_mut(page) {
                if let Some(ts) = slot.tiles.remove(&(row, col)) {
                    self.tile_bytes = self.tile_bytes.saturating_sub(ts.bytes());
                    self.pending_drops.push(ts.image);
                }
            }
        }
    }

    /// 回收已完成的在途**封面**结果：worker 回传整页（屏幕密度）BGRA → 装入 `slot.cover`
    /// （首屏锐化，先于瓦片到达；密度守卫：过期即弃）。返回是否有新封面入槽。
    fn collect_cover_results(&mut self, scale_factor: f32) -> bool {
        let finished: Vec<(usize, Option<RenderResult>)> = self
            .pending_cover
            .iter_mut()
            .filter_map(|(i, (_, rx))| match rx.try_recv() {
                Ok(res) => Some((*i, res)),
                Err(_) => None,
            })
            .collect();
        let mut any = false;
        for (page, res) in finished {
            let req_scale = self
                .pending_cover
                .remove(&page)
                .map(|(s, _)| s)
                .unwrap_or(0.0);
            let Some(res) = res else {
                continue;
            };
            if page >= self.dims.len() {
                continue;
            }
            let cscale = cover_scale(self.dims[page], self.page_width(), scale_factor);
            if (req_scale - cscale).abs() > 1e-4 {
                continue; // 缩放/DPI 已变 → 过期封面丢弃
            }
            let (w, h) = (res.width, res.height);
            let Ok(img) = render_image_from_bgra(res.bgra, w, h) else {
                continue;
            };
            if let Some(s) = self.slots.get_mut(page) {
                if let Some(old) = s.cover.replace(img) {
                    self.pending_drops.push(old);
                }
                s.cover_scale = cscale;
                // 封面 = 全页同密度整页位图：该页的瓦片（~整页 12-15 片）与中清（~2MB 占位）
                // 全部冗余 → 立即回收内存/显存；渲染直接以封面为底图，无任何视觉回退。
                for (_, ts) in s.tiles.drain() {
                    self.tile_bytes = self.tile_bytes.saturating_sub(ts.bytes());
                    self.pending_drops.push(ts.image);
                }
                if let Some(old) = s.mid.take() {
                    self.mid_lru.retain(|&p| p != page);
                    self.pending_drops.push(old);
                }
                let _ = (w, h);
                any = true;
            }
        }
        any
    }

    /// 封面内存管理：**只保留当前想要的封面 `[wanted_first..=wanted_last]`（可见页 ±1
    /// 邻居），窗外封面逐帧回收**。窗口内 ≤ 可见页+2 页，内存有界且**绝不触发换页风暴**：
    /// 当前窗口的封面永不被淘汰 → 不会被反复重请求；窗外的旧封面（随滚动累积）下帧即清。
    fn evict_covers(&mut self, wanted_first: usize, wanted_last: usize) {
        let mut drops: Vec<Arc<RenderImage>> = Vec::new();
        for (page, slot) in self.slots.iter_mut().enumerate() {
            if slot.cover.is_some() && (page < wanted_first || page > wanted_last) {
                if let Some(old) = slot.cover.take() {
                    drops.push(old);
                }
            }
        }
        self.pending_drops.extend(drops);
    }

    /// 回收已完成的在途 **L0 缩略图**结果：worker 回传低清 BGRA → 装入 `slot.thumb`
    /// （纯内存占位，不落盘）。非阻塞：仅检查已就绪通道；返回是否有新缩略图入槽。
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
    /// （快速拖动占位底图）。非阻塞；返回是否有新中清入槽（调用方决定重绘）。
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
                            self.mid_lru.retain(|&p| p != i);
                            self.mid_lru.push_back(i);
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

    /// P2：O(1) 主线程中清缓存命中（`slots[i].mid` 即内存缓存本体）。命中触摸 `mid_lru`
    /// （最近使用在队尾）并**复用纹理**——调用方据此跳过 Worker 请求；未命中返回 `None`
    /// → 走 Worker 渲染路径。仅主线程调用、只做哈希/克隆，不阻塞。
    fn mid_cached(&mut self, i: usize) -> Option<Arc<RenderImage>> {
        let img = self.slots.get(i).and_then(|s| s.mid.clone());
        if img.is_some() {
            self.mid_lru.retain(|&p| p != i);
            self.mid_lru.push_back(i);
        }
        img
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
            // 预取可见窗口文本层（低优先级懒加载）：交互时命中测试/复制/高亮已就绪。
            // 快速拖动时**跳过**——正在飞掠的页文本提取纯属浪费；停止后恢复预取。
            if !self.prev_fast {
                self.prefetch_texts(start, end);
            }
            let tops = &self.page_tops;
            let total = self.total_height;
            // P2 选区：render 帧内稳定的快照（高亮渲染用）
            let selection = self.selection();
            let sel_color = selection_color(cx);
            // P3 检索：命中高亮色 + 是否正在检索（非空查询才叠加命中高亮）
            let hit_color = search_hit_color(cx);
            let active_color = search_active_color();
            let searching = !self.search_query.trim().is_empty();
            let scale_factor = window.scale_factor();

            let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(end - start + 1);
            for i in start..=end {
                let dim = self.dims[i];
                let disp_h = display_height(dim, page_width);
                let top = tops[i];
                // 瓦片按显示密度 1:1 渲染；这里算出位图→CSS 的分轴缩放，放置瓦片与
                // 文本高亮（`pt_to_local` 同源）逐像素对齐。
                let scale = render_scale(page_width, scale_factor, dim.0);
                let bmp = page_bitmap_size(dim, scale);
                let (sx, sy) = bitmap_display_scales(bmp, page_width, disp_h);

                // 收集该页在**当前密度**下已就绪的瓦片（Arc 克隆 + 卡片局部定位）。
                let slot = &self.slots[i];
                let tile_ok = (slot.tile_scale - scale).abs() <= 1e-4;
                let slot_thumb = slot.thumb.clone();
                let slot_mid = slot.mid.clone();
                // 首屏整页锐化封面（满显示密度；缩放过期时 schedule_tiles 已丢弃，顶层保险）。
                let slot_cover = if (slot.cover_scale - cover_scale(dim, page_width, scale_factor))
                    .abs()
                    <= 1e-4
                {
                    slot.cover.clone()
                } else {
                    None
                };
                let mut tiles: Vec<(f32, f32, f32, f32, Arc<RenderImage>)> = Vec::new();
                if tile_ok {
                    tiles.reserve(slot.tiles.len());
                    for (_, ts) in slot.tiles.iter() {
                        tiles.push((
                            ts.x0 as f32 * sx,
                            ts.y0 as f32 * sy,
                            ts.w as f32 * sx,
                            ts.h as f32 * sy,
                            ts.image.clone(),
                        ));
                    }
                }
                let mut overlays = selection_overlays_for_page(
                    i,
                    selection,
                    dim,
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
                        dim,
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
                // 的子元素提供以卡片左上角为原点的坐标系。
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

                // 三层合成：底图（封面 → 中清 → 缩略图，先瞬时锐化铺满首屏、占位兜底）
                // → 瓦片（逐片绝对定位，opaque 覆盖底图）→ 选区/检索高亮。
                let base = slot_cover
                    .clone()
                    .or_else(|| slot_mid.clone())
                    .or_else(|| slot_thumb.clone());
                let inner: gpui::AnyElement = if !tiles.is_empty() {
                    let mut c = div().size_full().relative();
                    if let Some(base) = base {
                        c = c.child(
                            img(ImageSource::Render(base.clone()))
                                .w(px(page_width))
                                .h(px(disp_h)),
                        );
                    }
                    for (x, y, w, h, tile_img) in tiles {
                        c = c.child(
                            div()
                                .absolute()
                                .left(px(x))
                                .top(px(y))
                                .w(px(w))
                                .h(px(h))
                                .child(img(ImageSource::Render(tile_img)).size_full()),
                        );
                    }
                    c.into_any_element()
                } else if let Some(base) = base {
                    // 瓦片尚未就绪：占位底图铺满（快速拖动/首帧过渡）
                    div()
                        .size_full()
                        .relative()
                        .child(
                            img(ImageSource::Render(base))
                                .w(px(page_width))
                                .h(px(disp_h)),
                        )
                        .into_any_element()
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
                        .into_any_element()
                };
                // 外挂定位祖先：给选区/检索高亮提供以卡片左上角为原点的坐标系。
                card = card.child(
                    div()
                        .size_full()
                        .relative()
                        .child(inner)
                        .children(overlays),
                );
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
