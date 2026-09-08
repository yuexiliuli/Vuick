//! PDF 阅读器视图：页级虚拟化核心（加载、滚动调度、页渲染、滚动条）。
//!
//! 子模块拆分（都在 `src/pdf/` 下）：
//! - [`super::cache`]：磁盘二级页缓存（紧凑 BGRA 落盘 + LRU 封顶）
//! - [`super::layout`]：页列纯布局/调度数学（尺寸、可见区间、渲染规划）
//! - [`super::toc`]：书签目录面板（折叠、悬浮、拖拽调宽）与其交互
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
    MouseButton, ParentElement, Render, RenderImage, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::pdf::cache::PageCache;
use crate::pdf::layout::{
    Slot, compute_layout, display_height, keep_window, plan_page_work, render_scale, MAX_INFLIGHT,
    PAGE_PAD_X, PAGE_WIDTH,
};
use crate::pdf::toc::TOC_WIDTH_DEFAULT;
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
    /// 书签目录（标题 + 层级 + 目标页码）；无书签时为 `[]`。
    pub(crate) outlines: Vec<PdfOutlineEntry>,
    /// 已折叠的目录父项索引（`HashSet<条目下标>`），默认全部展开。
    /// 只有「有下级」的条目（下标 i 满足 level[i] < level[i+1]）才可能被折叠；
    /// 折叠后其所有后代（直到回到同层或更浅的祖先层）都不再显示。
    pub(crate) toc_collapsed: std::collections::HashSet<usize>,
    /// 目录面板当前宽度（px，可拖拽右缘调整）。
    pub(crate) toc_width: f32,
    /// 目录右缘拖拽状态：(按下的起始 x, 起始面板宽)。`None` = 未在拖拽。
    pub(crate) toc_drag: Option<(f32, f32)>,
    /// 目录右缘拖拽手柄是否被鼠标悬浮（用于悬浮/拖拽时高亮手势线）。
    pub(crate) toc_resize_hovered: bool,
    /// 目录面板滚动句柄（跟踪纵向滚动容器，供滚动条组件显示）。
    pub(crate) toc_scroll: ScrollHandle,
    /// 右侧目录抽屉是否展开。
    pub(crate) toc_open: bool,
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
            toc_collapsed: std::collections::HashSet::new(),
            toc_width: TOC_WIDTH_DEFAULT,
            toc_drag: None,
            toc_resize_hovered: false,
            toc_scroll: ScrollHandle::new(),
            toc_open: false,
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
                        this.toc_collapsed.clear(); // 新文档默认全部展开
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

    /// 目录相关子模块所需的只读访问器。
    pub(crate) fn outlines(&self) -> &[PdfOutlineEntry] {
        &self.outlines
    }
    pub(crate) fn toc_collapsed(&self) -> &std::collections::HashSet<usize> {
        &self.toc_collapsed
    }
    pub(crate) fn toc_collapsed_mut(&mut self) -> &mut std::collections::HashSet<usize> {
        &mut self.toc_collapsed
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

            // 竖向+横向滚动容器 + 定位页列（水平居中，宽 = 页宽，高 = 全文档总高）
            // wrap 宽度 = max(视口宽, 页宽+边距)：窗口宽时内容居中；窗口过窄（如目录
            // 展开挤压）时 wrap 宽于视口 → 触发横向滚动条，可滚到全部页面不被裁剪。
            let view_w = f32::from(self.scroll.bounds().size.width);
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

        // 外层：flex row——左 TOC 面板（展开时挤压右侧 PDF）+ 右 PDF 主体
        // 目录按钮作为 absolute 覆盖层挂在最外层 pdf-viewer（relative），
        // 不随 PDF 滚动、也不被面板挤压，始终悬浮在右上角可见。
        let toc_panel: Option<gpui::AnyElement> = self.render_toc_panel(cx);

        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(canvas)
            // 目录拖拽：鼠标移动/松开事件冒泡到容器统一处理（与 app 分割线同款）
            .on_mouse_move(cx.listener(
                |this: &mut Self, ev: &gpui::MouseMoveEvent, _window, cx| {
                    this.update_toc_resize(ev.position.x.as_f32(), cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _: &gpui::MouseUpEvent, _window, cx| {
                    this.end_toc_resize(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _: &gpui::MouseUpEvent, _window, cx| {
                    this.end_toc_resize(cx);
                }),
            )
            .children(toc_panel) // 左：目录面板（宽度参与布局，展开时挤压右侧）
            .child(
                div()
                    .id("pdf-main")
                    .debug_selector(|| "pdf-main".into())
                    // 右：PDF 页面主体。flex 行里填满剩余宽度；min_w(0) 允许其压缩到内容
                    // 最小宽以下——否则自动最小尺寸=页面列宽(~880px)，目录展开时不被挤压，
                    // 滚动条叠层越出窗口右侧（横向条的右半不可见）且 bounds/比例与窗口脱节。
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
                            // 横向滚动条：参照 markdown 预览——窗口过窄（如目录展开挤压）时
                            // scroll 容器内容宽于视口，横向溢出可滚；轴 Always 常驻可见。
                            .child(
                                Scrollbar::horizontal(&self.scroll)
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            )
            .children(self.render_toc_button(cx)) // 悬浮按钮：absolute 覆盖在最外层
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext, size};

    struct PdfHost {
        viewer: Entity<PdfViewer>,
    }
    impl Render for PdfHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.viewer.clone()
        }
    }

    fn setup_host(
        cx: &mut TestAppContext,
        size: gpui::Size<gpui::Pixels>,
    ) -> (Entity<PdfHost>, &mut VisualTestContext) {
        let path = Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/N107内外饰配置表NEW.pdf"),
        );
        setup_host_with(cx, size, path)
    }

    /// 通用宿主：`path` 为 `None`/不可读时 viewer 加载失败，但**保留 seed 的目录**
    /// （不会被真实书签覆盖），适合校验目录树交互。
    fn setup_host_with(
        cx: &mut TestAppContext,
        size: gpui::Size<gpui::Pixels>,
        path: Option<std::path::PathBuf>,
    ) -> (Entity<PdfHost>, &mut VisualTestContext) {
        cx.update(gpui_component::init);
        let path = path.unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/__missing__.pdf")
        });
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let viewer = cx.new(|cx| {
                let mut v = PdfViewer::new(path, cx);
                // 测试不依赖真实书签解析：直接布出 2 条目录并展开面板
                v.seed_outlines_for_test();
                v.force_toc_open_for_test();
                v
            });
            PdfHost { viewer }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size);
        // 泵驱动：后台解析的 async 任务需要跑完（真实 PdfWorker 加载），
        // 多送几帧让滚动容器以最终窗口尺寸完成布局。
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        (host, cx)
    }

    /// 回归：目录展开 + 窗口收窄时，右侧 PDF 主体必须被挤压进窗口内，
    /// 滚动条叠层（`pdf-scrollbar-overlay`）不得越出窗口右边界；同时 `pdf-scroll`
    /// 视口宽应按剩余可用宽度计算（否则横向滚动条的滑块几何与窗口宽度脱节）。
    #[gpui::test]
    fn toc_open_narrow_window_keeps_scrollbar_inside(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let (host, cx) = setup_host(cx, size(px(980.), px(720.)));
        // 等 PdfViewer 真正加载完成（真实 pdfium 解析），在已铺页状态下校验布局。
        for _ in 0..300 {
            cx.run_until_parked();
            let loaded = cx.update(|_, cx| host.read(cx).viewer.read(cx).is_loaded());
            if loaded {
                break;
            }
        }

        // 目录面板展开时占 280px，主体视口宽应 = 980 - 280 = 700。
        let check = |cx: &mut VisualTestContext, expect_w: f32| {
            let main = cx.debug_bounds("pdf-main").expect("pdf-main 应存在");
            let overlay = cx
                .debug_bounds("pdf-scrollbar-overlay")
                .expect("滚动条叠层应存在");
            let scroll = cx.debug_bounds("pdf-scroll").expect("pdf-scroll 应存在");
            assert!(
                (main.size.width.as_f32() - expect_w).abs() < 2.0,
                "pdf-main 宽应约为 {expect_w}（视口 - 目录 280），实际={main:?}"
            );
            assert!(
                (overlay.size.width.as_f32() - expect_w).abs() < 2.0,
                "滚动条叠层应铺满 pdf-main（约 {expect_w}），不得越出窗口，实际={overlay:?}"
            );
            assert!(
                (scroll.size.width.as_f32() - expect_w).abs() < 2.0,
                "pdf-scroll 视口宽应等于剩余可用宽（约 {expect_w}），实际={scroll:?}"
            );
            let origin = overlay.origin;
            assert!(
                origin.x.as_f32() >= -0.5 && origin.y.as_f32() >= -0.5,
                "滚动条叠层不应越出窗口左上角，actual origin={origin:?}"
            );
        };
        check(cx, 700.0);

        // 窗口收窄到 620：主体宽应 ≈ 620 - 280 = 340（此前 bug：flex 项被自动
        // 最小宽=页面列宽阻止收缩，主体仍约 880px 且滚动条右半越出窗口）。
        cx.simulate_resize(size(px(620.), px(720.)));
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        check(cx, 340.0);
    }

    /// 回归：点击折叠箭头后，父项子代应被隐藏（从面板 DOM 消失），再次点击展开恢复。
    ///
    /// （只校验折叠状态机 + 可见性，避免对真实 pdfium 的强依赖：用 seed 的 2 层目录。）
    #[gpui::test]
    fn toc_collapse_hides_children_and_expands_back(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        // 用空路径 → 加载失败，seed 的 2 层书签不被真实书签覆盖，便于精确校验树交互
        let (host, cx) = setup_host_with(cx, size(px(980.), px(720.)), None);

        // seed_outlines_for_test: idx0=L0「测试章节」、idx1=L1「测试小节」
        // 初始全展开：两条目都应渲染
        let item1 = cx.debug_bounds("pdf-toc-item-0-0").expect("父项应渲染");
        let child = cx.debug_bounds("pdf-toc-item-1-1").expect("子项初始应可见");
        assert!(child.size.height > px(0.), "子项默认可见");
        let parent_y0 = item1.origin.y;

        // 折叠父项(0)：子项(1)应进入隐藏集合且不再渲染；父行仍在
        cx.update(|_, cx| {
            let viewer = host.read(cx).viewer.clone();
            viewer.update(cx, |v, cx| v.toggle_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let collapsed_now = cx.update(|_, cx| {
            host.read(cx).viewer.read(cx).toc_collapsed_for_test().contains(&0)
        });
        assert!(collapsed_now, "父项 0 应进入折叠集合");
        assert!(
            cx.debug_bounds("pdf-toc-item-1-1").is_none(),
            "子项折叠后不应再渲染"
        );
        assert!(cx.debug_bounds("pdf-toc-item-0-0").is_some(), "父项仍应渲染");

        // 再点击一次 → 展开，子项恢复
        cx.update(|_, cx| {
            let viewer = host.read(cx).viewer.clone();
            viewer.update(cx, |v, cx| v.toggle_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let collapsed_now = cx.update(|_, cx| {
            host.read(cx).viewer.read(cx).toc_collapsed_for_test().contains(&0)
        });
        assert!(!collapsed_now, "再次点击应展开（折叠集合移除）");
        let child = cx.debug_bounds("pdf-toc-item-1-1").expect("展开后子项应恢复");
        assert!(child.size.height > px(0.), "展开后子项应可见");
        assert!(
            (parent_y0.as_f32() - child.origin.y.as_f32()).abs() > 1.0,
            "子项应与父项不同行（在面板内垂直排列）"
        );
    }

    /// 冒烟：目录项 hover 不应 panic；指示条元素存在且位于对应行内（左侧指示条渲染）。
    #[gpui::test]
    fn toc_item_hover_renders_indicator(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let (_host, cx) = setup_host_with(cx, size(px(980.), px(720.)), None);

        let row = cx.debug_bounds("pdf-toc-item-0-0").expect("父项应渲染");
        let ind = cx.debug_bounds("pdf-toc-ind-0").expect("指示条应渲染");
        assert!(
            ind.origin.x.as_f32() >= row.origin.x.as_f32() - 1.0
                && ind.origin.x.as_f32() <= row.origin.x.as_f32() + row.size.width.as_f32(),
            "指示条应在行水平范围内，ind={ind:?} row={row:?}"
        );

        // 把鼠标移到父项上：仅验证不 panic、布局稳定（纯样式细化，无运行时断言）。
        let center = gpui::point(
            row.origin.x + row.size.width / 2.,
            row.origin.y + row.size.height / 2.,
        );
        cx.simulate_mouse_move(center, None, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        assert!(cx.debug_bounds("pdf-toc-item-0-0").is_some(), "hover 后行仍渲染");
    }

    /// 回归：目录右缘可拖拽改变面板宽；宽度受限在 [MIN, MAX]；松开后宽度保持。
    #[gpui::test]
    fn toc_resize_drag_changes_width(cx: &mut TestAppContext) {
        use crate::pdf::toc::{TOC_WIDTH_DEFAULT as DEF, TOC_WIDTH_MAX as MAX};
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let (host, cx) = setup_host_with(cx, size(px(980.), px(720.)), None);

        let panel0 = cx.debug_bounds("pdf-toc-panel").expect("目录面板应存在");
        let w0 = panel0.size.width.as_f32();
        assert!((w0 - DEF).abs() < 2.0, "默认宽应 280，实际 {w0}");

        // 取右缘中点（命中区 6px 内）作为拖拽起点
        let handle = cx.debug_bounds("pdf-toc-resize").expect("拖拽手柄应存在");
        let start = gpui::point(
            handle.origin.x + handle.size.width / 2.,
            panel0.origin.y + panel0.size.height / 2.,
        );
        // 按下 → 向右拖 60px → 松开
        cx.simulate_mouse_move(start, None, gpui::Modifiers::none());
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            gpui::point(start.x + px(60.), start.y),
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.simulate_mouse_up(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }

        let panel1 = cx.debug_bounds("pdf-toc-panel").expect("目录面板应存在");
        let w1 = panel1.size.width.as_f32();
        assert!(
            (w1 - (w0 + 60.0)).abs() < 3.0,
            "右拖 60px 后宽度应约 +60，w0={w0} w1={w1}"
        );
        assert!(
            (w1 - cx
                .update(|_, cx| host.read(cx).viewer.read(cx).toc_width)
                .abs()) < 0.01,
            "拖拽后内部保存宽度应一致"
        );

        // 拖出上限：向右拖到接近窗口右缘 → 宽度应封顶 == MAX
        let handle = cx.debug_bounds("pdf-toc-resize").expect("手柄应存在");
        let start = gpui::point(
            handle.origin.x + handle.size.width / 2.,
            panel1.origin.y + panel1.size.height / 2.,
        );
        cx.simulate_mouse_move(start, None, gpui::Modifiers::none());
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            gpui::point(start.x + px(630.), start.y),
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.simulate_mouse_up(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let w_max = cx
            .debug_bounds("pdf-toc-panel")
            .expect("目录面板应存在")
            .size
            .width
            .as_f32();
        assert!(
            (w_max - MAX).abs() < 1.0,
            "超大拖动应封顶在 MAX，实际 {w_max}"
        );
    }
}
