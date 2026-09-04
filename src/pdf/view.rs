//! PDF 阅读器视图：自包含、页级虚拟化（参照 gpui-pdf / zorite `gpui-pdf` 的
//! `PdfView`，裁剪掉缩放/搜索/目录等与本项目无关的能力）。
//!
//! 关键机制：
//! - **打开即显**：文件读取 + 结构解析 + 页尺寸测量都在后台线程做（便宜），
//!   完成后马上铺好全部页槽（滚动条正确）；**不**预先栅格化任何页。
//! - **按需栅格化**：每帧根据滚动位置算“可见页区间”（[`keep_window`]，含 MARGIN
//!   缓冲），只对这些页在后台栅格化；渲染按“离可见区中心最近优先”填充。
//! - **回收**：滚离视口的页释放位图 + GPU 纹理（`drop_image`），内存受限。
//! - **不闪烁**：页槽先是有尺寸占位（页号/加载中），位图到位后替换，布局稳定。

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, img, px, rgb, App, Context, Hsla, ImageSource, InteractiveElement, IntoElement,
    ParentElement, Render, RenderImage, ScrollHandle, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::pdf::{page_dims, parse_bytes, render_page, Document};

/// 页面统一显示基准宽（px，视口内按比例缩放）。
const PAGE_WIDTH: f32 = 840.0;
/// 页与页之间的垂直间距（px）。
const PAGE_GAP: f32 = 14.0;
/// 页列顶部/底部内边距（px）。
const PAGE_PAD_Y: f32 = 16.0;
/// 页面左右内边距（px）。
const PAGE_PAD_X: f32 = 20.0;
/// 最小渲染倍率：更小则软化不放大。
const MAX_RENDER_SCALE: f32 = 3.0;
/// 可见页上下各多留存的已渲染页数（滚动时已就绪，不抖动）。
const MARGIN: usize = 3;
/// 最多同时保留的已渲染位图页数（内存上限）。超出后把**离当前视口最远**的页回收；
/// 只要总量未超限，「往回翻」命中的都是已渲染位图，不会重新加载。
/// 按 840px 宽 A4（≈4MB/页）估算，70 页 ≈ 280MB。
const CACHE_MAX_PAGES: usize = 70;

/// 页槽：某页的渲染状态。
#[derive(Default, Clone)]
struct Slot {
    /// 已栅格化位图（`None` = 从未渲染或已回收）。
    image: Option<Arc<RenderImage>>,
    /// 是否正在后台栅格化（避免重复派发）。
    loading: bool,
    /// 该位图对应的渲染代次（缩放/质量变化时使旧位图失效）。
    generation: u64,
}

/// 某页在页列里的显示高度（保持宽高比）。
fn display_height((w, h): (f32, f32), page_width: f32) -> f32 {
    if w > 0.0 {
        page_width * (h / w)
    } else {
        page_width
    }
}

/// 可见页区间（含缓冲）：与页列布局镜像，纯函数便于测试。
///
/// `scroll_y` 为内容已向下滚动的像素数（≥0）；`viewport_h` 为可见区高度。
fn keep_window(
    dims: &[(f32, f32)],
    page_width: f32,
    scroll_y: f32,
    viewport_h: f32,
) -> (usize, usize) {
    if dims.is_empty() {
        return (0, 0);
    }
    let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
    let top = scroll_y.max(0.0);
    let bottom = top + vh;

    let mut y = PAGE_PAD_Y;
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    for (i, dim) in dims.iter().enumerate() {
        let page_top = y;
        let page_bottom = y + display_height(*dim, page_width);
        if page_bottom > top && page_top < bottom {
            first.get_or_insert(i);
            last = i;
        }
        y = page_bottom + PAGE_GAP;
    }
    let first = first.unwrap_or(0);
    let start = first.saturating_sub(MARGIN);
    let end = (last + MARGIN).min(dims.len() - 1);
    (start, end)
}

/// 渲染倍率：以在屏宽度、显示 DPI 推导，封顶避免超大位图。
fn render_scale(page_width: f32, scale_factor: f32, page_pt_width: f32) -> f32 {
    if page_pt_width > 0.0 {
        (page_width * scale_factor / page_pt_width).clamp(0.5, MAX_RENDER_SCALE)
    } else {
        1.5
    }
}

/// 每帧的页工作规划（纯函数，便于测试）：根据每页当前是否“已渲染/正在加载/代次”，
/// 决定 (需要渲染的页, 需要淘汰位图的页)。
///
/// - **渲染**：可见区内既**未渲染**又**未在加载中**（或位图代次过期）的页。
///   正在加载的页绝不再派发任务，避免每个可见帧都重复栅格化同一页。
/// - **淘汰（延后回收 / 距离 LRU）**：仅当保留位图数超过 [`CACHE_MAX_PAGES`] 时，
///   才把**离当前视口中心最远**的页依次回收直到回到预算内。预算内任一页——无论多靠前
///   多靠后——都保留，因此「往下翻很多页再倒回去」命中缓存，不重新加载。
fn plan_page_work(
    rendered: &[bool],
    loading: &[bool],
    start: usize,
    end: usize,
    generation: u64,
    rendered_generation: &[u64],
) -> (Vec<usize>, Vec<usize>) {
    let dims_len = rendered.len();
    let mut to_render: Vec<usize> = Vec::new();
    for i in start..=end.min(dims_len.saturating_sub(1)) {
        let needs_render =
            !rendered[i] || rendered_generation.get(i).copied() != Some(generation);
        if needs_render && !loading[i] {
            to_render.push(i);
        }
    }

    let retained = rendered.iter().filter(|r| **r).count();
    if retained <= CACHE_MAX_PAGES {
        return (to_render, Vec::new());
    }
    // 预算已满：离中心最远优先淘汰（等距取索引小者，保证确定性）
    let center = (start + end) / 2;
    let mut candidates: Vec<usize> = rendered
        .iter()
        .enumerate()
        .filter(|(_, r)| **r)
        .map(|(i, _)| i)
        .collect();
    candidates.sort_by_key(|&i| (std::cmp::Reverse((i as i64 - center as i64).abs()), i));
    let mut to_evict: Vec<usize> = Vec::new();
    for i in candidates {
        if to_evict.len() >= retained - CACHE_MAX_PAGES {
            break;
        }
        to_evict.push(i);
    }
    to_evict.sort_unstable();
    (to_render, to_evict)
}

/// PDF 阅读器：自包含的页级虚拟化视图。
pub struct PdfViewer {
    /// 文件路径（含文件名，标题用）。
    path: PathBuf,
    /// 已解析文档（加载完成后为 `Some`）。
    pdf: Option<Arc<Document>>,
    /// 每页 (宽,高)（PDF 点）。
    dims: Vec<(f32, f32)>,
    /// 每页渲染槽。
    slots: Vec<Slot>,
    /// 渲染代次（缩放/质量变化时递增使旧位图失效）。
    generation: u64,
    /// 竖向滚动句柄（页列外层 scroll 容器）。
    scroll: ScrollHandle,
    /// 是否仍在后台解析。
    loading: bool,
    /// 加载失败信息。
    load_error: Option<String>,
    /// 阅读画布背景（页面“纸面”之外底色；None 用主题推导）。
    bg: Option<Hsla>,
    /// 待释放的旧位图（重渲染替换下来的，需在下一帧 `drop_image`）。
    pending_drops: Vec<Arc<RenderImage>>,
}

impl PdfViewer {
    /// 创建阅读器并异步加载 `path`（读文件 + 解析 + 量页在后台，UI 不阻塞）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            path,
            pdf: None,
            dims: Vec::new(),
            slots: Vec::new(),
            generation: 0,
            scroll: ScrollHandle::new(),
            loading: true,
            load_error: None,
            bg: None,
            pending_drops: Vec::new(),
        };
        this.start_load(cx);
        this
    }

    /// 后台加载：读文件 + 解析 + 量页，完成后在主线程铺页槽。
    fn start_load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                    let doc = parse_bytes(bytes).map_err(|e| e.to_string())?;
                    let dims = page_dims(&doc);
                    Ok::<_, String>((doc, dims))
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok((doc, dims)) => {
                        this.pdf = Some(doc);
                        this.dims = dims;
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

    /// 页数（测试 / 状态栏使用；文档尚未加载完时为 0）。
    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// 是否已加载完成（测试用）。
    #[allow(dead_code)]
    pub fn is_loaded(&self) -> bool {
        self.pdf.is_some()
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

    /// 释放所有已上传的 GPU 纹理（关闭标签页 / 丢弃视图时调用）。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut drops = std::mem::take(&mut self.pending_drops);
        drops.extend(self.slots.iter_mut().filter_map(|s| s.image.take()));
        for arc in drops {
            cx.drop_image(arc, Some(window));
        }
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
        if self.dims.is_empty() || self.pdf.is_none() {
            return;
        }
        let page_width = self.page_width();
        let scale_factor = window.scale_factor();
        let scroll_y = f32::from(-self.scroll.offset().y);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);
        let generation = self.generation;

        // 规划：要渲染的页 + 要淘汰位图的页（保留策略见 plan_page_work）
        let rendered: Vec<bool> = self.slots.iter().map(|s| s.image.is_some()).collect();
        let loading: Vec<bool> = self.slots.iter().map(|s| s.loading).collect();
        let rendered_generation: Vec<u64> = self.slots.iter().map(|s| s.generation).collect();
        let (mut to_render, to_evict_idx) =
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
        for i in &to_render {
            self.slots[*i].loading = true;
        }
        if to_render.is_empty() {
            return;
        }

        // 靠近可见区中心的页先渲染（先填正在看的那页）
        let center = (start + end) / 2;
        to_render.sort_by_key(|&i| (i as i64 - center as i64).unsigned_abs());

        let pdf = self.pdf.clone().unwrap();
        for i in to_render {
            let pdf = pdf.clone();
            let scale = render_scale(page_width, scale_factor, self.dims[i].0);
            cx.spawn(async move |weak, cx| {
                let page = cx
                    .background_executor()
                    .spawn(async move { render_page(&pdf, i, scale).ok() })
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    let gen_now = this.generation;
                    let mut retired = None;
                    if let Some(slot) = this.slots.get_mut(i) {
                        slot.loading = false;
                        // 始终存入结果（预算淘汰在每帧 plan_page_work 里做）；
                        // 只有代次过期才丢弃，避免替换还在显示的旧图。
                        if gen_now == generation {
                            if let Some(img) = page {
                                retired = slot.image.replace(img);
                                slot.generation = generation;
                            }
                        }
                    }
                    if let Some(old) = retired {
                        this.pending_drops.push(old);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

impl Render for PdfViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_window(window, cx);
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let (page_bg, page_border) = Self::page_card_colors(cx);
        let page_width = self.page_width();
        let dims = self.dims.clone();
        let slots = &self.slots;

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
            // 页列：每页一个固定宽高（宽高比不变）的页槽卡片，位图到位即替换占位。
            // 页间距必须与 keep_window/plan_page_work 的虚拟布局一致（PAGE_GAP），
            // 否则按滚动位置推算的“可见/保留窗口”会随页数漂移，导致可见页被误判淘汰。
            let mut column =
                div().id("pdf-pages").flex().flex_col().gap(px(PAGE_GAP)).w(px(page_width));
            for (i, dim) in dims.iter().enumerate() {
                let disp_h = display_height(*dim, page_width);
                let slot_img = slots.get(i).and_then(|s| s.image.clone());
                let card = div()
                    .id(gpui::SharedString::from(format!("pdf-page-{i}")))
                    .flex_shrink_0()
                    .w(px(page_width))
                    .h(px(disp_h))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(page_border)
                    .overflow_hidden();
                let card = match slot_img {
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
                column = column.child(card);
            }
            // 竖向滚动容器 + 页列（水平居中，宽 = 页宽，高自适应溢出）
            div()
                .id("pdf-scroll")
                .debug_selector(|| "pdf-scroll".into())
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(
                    div()
                        .id("pdf-column-wrap")
                        .w_full()
                        .flex()
                        .flex_row()
                        .justify_center()
                        .px(px(PAGE_PAD_X))
                        .py(px(PAGE_PAD_Y))
                        .child(column),
                )
                .into_any_element()
        };

        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .bg(canvas)
            .child(body)
            .child(
                div()
                    .id("pdf-vscroll-overlay")
                    .debug_selector(|| "pdf-vscroll-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::vertical(&self.scroll)
                            .mode(ScrollbarMode::Scrolling)
                            .viewport_from_layout(),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a4() -> (f32, f32) {
        (595.0, 842.0) // A4 竖版（pt）
    }

    #[test]
    fn keep_window_covers_only_visible_plus_margin() {
        // 30 页 A4，页宽 840 → 每页高约 1189；视口 600px 高，滚到第 10 页附近
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let scroll_y = 10.0 * page_step(840.0);
        let (s, e) = keep_window(&dims, 840.0, scroll_y, 600.0);
        // 第 10 页应可见，区间含 MARGIN 缓冲且不越界
        assert!(s <= 10 && 10 <= e, "第 10 页应在可见区间 [{s},{e}]");
        assert!(e - s + 1 >= 2 * MARGIN + 1, "区间宽度应至少为可见页 + 2*MARGIN");
        assert!(e <= 29, "末页不越界");
    }

    #[test]
    fn keep_window_empty_dims_returns_zero() {
        assert_eq!(keep_window(&[], 840.0, 0.0, 600.0), (0, 0));
    }

    #[test]
    fn plan_renders_only_unrendered_visible_pages() {
        // 10 页，第 0 页已渲染且代次一致；视口 [0,2] → 只应渲染 1、2
        let rendered = [true, false, false, false, false, false, false, false, false, false];
        let loading = [false; 10];
        let gens = [0u64; 10];
        let (render, evict) = plan_page_work(&rendered, &loading, 0, 2, 0, &gens);
        assert_eq!(render, vec![1, 2], "仅未渲染的可见页应列入渲染");
        assert!(evict.is_empty(), "无越界已渲染页不应淘汰");
    }

    #[test]
    fn plan_skips_pages_already_loading() {
        // 可见页 1 正在加载 → 不应重复派发渲染任务（CPU 占用暴增的元凶）
        let rendered = [true, false, false, false, false, false, false, false, false, false];
        let mut loading = [false; 10];
        loading[1] = true;
        let gens = [0u64; 10];
        let (render, _) = plan_page_work(&rendered, &loading, 0, 2, 0, &gens);
        assert!(
            !render.contains(&1),
            "正在加载的页 1 不应重复派发（杜绝每帧重栅格化）"
        );
        assert!(render.contains(&2), "其余未渲染可见页仍应渲染");
    }

    #[test]
    fn plan_keeps_rendered_pages_within_budget() {
        // 200 页：仅第 10 页渲染，其余未渲染。预算 70，远未满 → 第 10 页必须保留。
        let mut rendered = vec![false; 200];
        let mut gens = vec![0u64; 200];
        let loading = vec![false; 200];
        rendered[10] = true;
        gens[10] = 0;
        let (_r, evict) = plan_page_work(&rendered, &loading, 50, 55, 0, &gens);
        assert!(evict.is_empty(), "预算未满不应淘汰任何页（即使远在 40 页之外）");
    }

    #[test]
    fn plan_evicts_farthest_first_when_over_budget() {
        // 200 页都渲染（超预算 70），视口在 [100,105]。
        // 应淘汰离中心最远的页（最前/最后），而可见页绝不能被淘汰。
        let n = 200usize;
        let rendered = vec![true; n];
        let gens = vec![0u64; n];
        let loading = vec![false; n];
        let (r, evict) = plan_page_work(&rendered, &loading, 100, 105, 0, &gens);
        assert!(r.is_empty(), "全部已渲染，无需再渲染");
        // 回收到预算内
        assert_eq!(n - evict.len(), CACHE_MAX_PAGES, "保留数应等于预算");
        assert!(evict.contains(&0) && evict.contains(&199), "最远两端应先被淘汰");
        for i in 100..=105 {
            assert!(!evict.contains(&i), "可见页 {i} 绝不能被淘汰");
        }
    }

    #[test]
    fn scrolling_down_and_back_does_not_reload_rendered_pages() {
        // 模拟用户「往下翻很多页再倒回去」：全程用 plan_page_work 推进状态机，
        // 已渲染页只要未超预算、未被 LRU 淘汰，就绝不会进入 to_render（即不再“加载”）。
        let n = 400usize;
        let mut rendered = vec![false; n];
        let mut gens = vec![0u64; n];
        let loading = vec![false; n];
        let mut render_calls = 0usize;

        // 逐窗口滚动：从第 0 页往下翻 100 次（每次窗口推进 2 页），再倒回翻 100 次
        for step in -100i64..=100 {
            let base = (50 + step * 2).clamp(0, (n as i64) - 6) as usize;
            let (start, end) = (base, (base + 5).min(n - 1));
            let (to_render, to_evict) = plan_page_work(&rendered, &loading, start, end, 0, &gens);
            // 记录本轮“需要重新加载”的页（应只含“从未渲染过”的页）
            for i in &to_render {
                assert!(
                    !rendered[*i],
                    "第 {i} 页已渲染却仍被要求渲染（往回翻又加载了！）step={step}"
                );
                rendered[*i] = true;
                gens[*i] = 0;
                render_calls += 1;
            }
            for i in &to_evict {
                rendered[*i] = false;
            }
            let _ = render_calls;
        }
    }

    #[test]
    fn keep_window_before_first_paint_assumes_viewport() {
        // viewport_h ≤ 1 时回退默认高度，首帧也应覆盖从首页开始的若干页
        let dims: Vec<(f32, f32)> = vec![a4(); 5];
        let (s, e) = keep_window(&dims, 840.0, 0.0, 0.0);
        assert!(s == 0, "首帧应从首页开始，实际 s={s}");
        assert!(e <= 4, "不越界，实际 e={e}");
    }

    #[test]
    fn display_height_preserves_aspect() {
        let h = display_height(a4(), 840.0);
        assert!((h - 840.0 * 842.0 / 595.0).abs() < 0.01);
    }

    #[test]
    fn render_scale_capped_and_sane() {
        // 页宽 595pt，在屏 840px @ 1x DPI → ~1.41x，夹在 [0.5, MAX_RENDER_SCALE]
        let s = render_scale(840.0, 1.0, 595.0);
        assert!(s > 0.5 && s <= MAX_RENDER_SCALE);
        // 超大 DPI（Retina 2x）也要封顶
        let s2 = render_scale(840.0, 2.0, 100.0);
        assert!(s2 <= MAX_RENDER_SCALE);
    }

    /// 与 `keep_window` 同款布局的一页（高 + 间距）步长（测试辅助）。
    fn page_step(page_width: f32) -> f32 {
        display_height(a4(), page_width) + PAGE_GAP
    }
}
