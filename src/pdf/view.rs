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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use gpui::{
    div, img, px, rgb, App, Context, Hsla, ImageSource, InteractiveElement, IntoElement,
    ParentElement, Render, RenderImage, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::config::pdf::{MAX_INFLIGHT, PAGE_PAD_X, PAGE_WIDTH};
use crate::pdf::cache::PageCache;
use crate::pdf::layout::{
    Slot, compute_layout, display_height, keep_window, plan_page_work, render_scale,
};
use crate::pdf::{
    render_image_from_bgra, DocId, PdfOutlineEntry, PdfWorker, RenderRequest, RenderResult,
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
            scroll: ScrollHandle::new(),
            loading: true,
            load_error: None,
            bg: None,
            pending_drops: Vec::new(),
            cache,
            outlines: Vec::new(),
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
        drops.extend(self.slots.iter_mut().filter_map(|s| s.image.take()));
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
    }

    /// 当前页列宽度。
    fn page_width(&self) -> f32 {
        PAGE_WIDTH
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

        let page_width = self.page_width();
        let scale_factor = window.scale_factor();
        let scroll_y = f32::from(-self.scroll.offset().y);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);

        // 规划：要渲染的页 + 要淘汰位图的页（保留策略见 plan_page_work）
        let rendered: Vec<bool> = self.slots.iter().map(|s| s.image.is_some()).collect();
        let loading: Vec<bool> = self.slots.iter().map(|s| s.loading).collect();
        let rendered_generation: Vec<u64> = self.slots.iter().map(|s| s.generation).collect();
        let (to_render, to_evict_idx) =
            plan_page_work(&rendered, &loading, start, end, generation, &rendered_generation);
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
        // 先尝试磁盘二级缓存：命中则直接读盘重建位图（毫秒级），不派发栅格化任务；
        // 未命中才需要真正栅格化。
        let mut need_render: Vec<usize> = Vec::new();
        for &i in &to_render {
            let cached = self
                .cache
                .load_bgra(i)
                .and_then(|(bgra, w, h)| render_image_from_bgra(bgra, w, h).ok());
            match cached {
                Some(img) => {
                    self.slots[i].loading = false;
                    self.slots[i].image = Some(img);
                    self.slots[i].generation = generation;
                }
                None => need_render.push(i),
            }
        }
        if need_render.is_empty() {
            return;
        }

        // 靠近可见区中心的页先渲染（先填正在看的那页）
        let center = (start + end) / 2;
        need_render.sort_by_key(|&i| (i as i64 - center as i64).unsigned_abs());

        // 2) 派发渲染请求给进程级 pdfium 工作线程；已在途（loading）的页绝不重复派发；
        //    同时用 MAX_INFLIGHT 节流，防止快速滚动时请求无限堆积。
        let inflight = self.pending.len();
        let budget = MAX_INFLIGHT.saturating_sub(inflight);
        if budget == 0 {
            return;
        }
        let worker = self.worker.as_ref().unwrap();
        for i in need_render.into_iter().take(budget) {
            if self.slots[i].loading {
                continue;
            }
            self.slots[i].loading = true;
            let scale = render_scale(page_width, scale_factor, self.dims[i].0);
            let rx = worker.request_render(self.doc_id, RenderRequest { page: i, scale });
            self.pending.insert(i, rx);
        }
    }

    /// 回收已完成的在途渲染结果：worker 回传 BGRA → 重建 `RenderImage` 装入内存槽
    /// （代次未变才装；被淘汰/失效的槽只清 loading 允许重投）→ **异步落盘**二级缓存。
    /// 非阻塞：仅检查已就绪通道，未完成的留到下一帧。
    fn collect_results(&mut self, cx: &mut Context<Self>) {
        use crate::pdf::cache::save_bgra;
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
                    // 后台异步落盘紧凑 BGRA（写盘/LRU 淘汰不在主线程、不在 pdfium worker）
                    let cache_inner = self.cache.inner_arc();
                    let bgra = res.bgra.clone();
                    cx.background_executor()
                        .spawn(async move { save_bgra(cache_inner, i, bgra, w, h) })
                        .detach();
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
            if let Some(s) = self.slots.get_mut(i) {
                s.loading = false;
                if generation == self.generation {
                    s.generation = generation;
                    if let Some(old) = s.image.replace(img) {
                        self.pending_drops.push(old);
                    }
                }
            }
            any = true;
        }
        if any {
            cx.notify();
        }
    }

    /// 书签目录访问器（侧边栏目录使用）。
    pub(crate) fn outlines(&self) -> &[PdfOutlineEntry] {
        &self.outlines
    }
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
            let tops = &self.page_tops;
            let total = self.total_height;
            let slots = &self.slots;

            let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(end - start + 1);
            for i in start..=end {
                let disp_h = display_height(self.dims[i], page_width);
                let top = tops[i];
                let slot_img = slots.get(i).and_then(|s| s.image.clone());
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
                card = match slot_img {
                    Some(page_img) => card.child(
                        img(ImageSource::Render(page_img))
                            .w(px(page_width))
                            .h(px(disp_h)),
                    ),
                    None => card
                        .bg(page_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(page_border)
                                .child(format!("第 {} 页 加载中…", i + 1)),
                        ),
                };
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
            // wrap 宽度 = max(视口宽, 页宽+边距)：窗口宽时内容居中；窗口过窄时
            // wrap 宽于视口 → 触发横向滚动条，可滚到全部页面不被裁剪。
            let view_w = f32::from(window.bounds().size.width);
            let wrap_w = view_w.max(page_width + 2.0 * PAGE_PAD_X);
            div()
                .id("pdf-scroll")
                .debug_selector(|| "pdf-scroll".into())
                .size_full()
                .overflow_y_scroll()
                .overflow_x_scroll()
                .track_scroll(&self.scroll)
                .child(
                    div()
                        .id("pdf-column-wrap")
                        .w(px(wrap_w))
                        .flex()
                        .flex_row()
                        .justify_center()
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
                                    .mode(ScrollbarMode::Scrolling)
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
