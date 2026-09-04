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

        // 回收可见区之外位图 + 决定需要渲染的页
        let mut to_render: Vec<usize> = Vec::new();
        let mut to_evict: Vec<Arc<RenderImage>> = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let in_window = i >= start && i <= end;
            if in_window {
                if !slot.loading && (slot.image.is_none() || slot.generation != generation) {
                    slot.loading = true;
                    to_render.push(i);
                }
            } else if let Some(arc) = slot.image.take() {
                slot.generation = 0;
                to_evict.push(arc);
            }
        }
        for arc in to_evict {
            cx.drop_image(arc, Some(window));
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
                    let in_window = {
                        let pw = this.page_width();
                        let sy = f32::from(-this.scroll.offset().y);
                        let vh = f32::from(this.scroll.bounds().size.height);
                        let (s, e) = keep_window(&this.dims, pw, sy, vh);
                        i >= s && i <= e
                    };
                    let gen_now = this.generation;
                    let mut retired = None;
                    if let Some(slot) = this.slots.get_mut(i) {
                        slot.loading = false;
                        if gen_now == generation && in_window {
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
            let mut column = div().id("pdf-pages").flex().flex_col().w(px(page_width));
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
