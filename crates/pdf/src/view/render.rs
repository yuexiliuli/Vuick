//! PDF 阅读器子模块：上屏渲染（原 `view.rs` 拆出）。
//!
//! 覆盖 [`gpui::Render`] 全链路：状态路由（错误 / 加载中 / 已加载）、外层画布与滚动条、
//! **页列 DOM 虚拟化**（[`super::PdfViewer::render_pdf_pages`] → 只铺「可见页 ± MARGIN」
//! 的绝对定位卡片 [`super::PdfViewer::build_visible_page_cards`]，长文档换帧 O(可见页)）、
//! 三层合成（底图封面/中清/缩略 → 瓦片 → 选区/检索高亮）。选区与检索命中高亮的纯几何
//! 叠加函数（`selection_overlays_for_page` / `search_overlays_for_page`）亦在本模块。

use super::*;

/// 选区背景色（半透蓝；深浅两版，与 markdown 预览同一套色值，视觉统一）。
fn selection_color(cx: &App) -> Hsla {
    if ui::theme::Theme::global(cx).is_dark() {
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
    if ui::theme::Theme::global(cx).is_dark() {
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
#[allow(clippy::too_many_arguments)] // 检索叠加纯函数：几何/颜色参数互为正交
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

impl PdfViewer {
    /// 把「可视窗口内 [start, end] 的页」逐个铺成**绝对定位**的页卡片列表
    /// （页列虚拟化的核心，被 [`Render::render`] 的已加载分支调用）。
    ///
    /// 每页：按显示密度 1:1 取瓦片，算出位图→CSS 的分轴缩放（`pt_to_local` 同源，
    /// 瓦片与文本高亮逐像素对齐）；三层合成——底图（封面→中清→缩略图，先瞬时锐化
    /// 铺满首屏、占位兜底）→ 瓦片（逐片绝对定位、opaque 覆盖底图）→ 选区/检索高亮。
    /// 卡片自身保持 `absolute` 定位（页列虚拟化的根基，⚠️ 不可改 `.relative()`）。
    #[allow(clippy::too_many_arguments)] // 页卡片上下文互为正交
    fn build_visible_page_cards(
        &self,
        start: usize,
        end: usize,
        page_width: f32,
        scale_factor: f32,
        selection: Option<((usize, usize), (usize, usize))>,
        sel_color: Hsla,
        searching: bool,
        hit_color: Hsla,
        active_color: Hsla,
        page_bg: Hsla,
        page_border: Hsla,
    ) -> Vec<gpui::AnyElement> {
        let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(end - start + 1);
        for (i, &top) in self.page_tops.iter().enumerate().skip(start).take(end - start + 1) {
            let dim = self.dims[i];
            let disp_h = display_height(dim, page_width);
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
                for ts in slot.tiles.values() {
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
        cards
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
            Self::pdf_placeholder_body(&format!("无法打开 PDF：{err}"), page_border)
        } else if self.loading || self.dims.is_empty() {
            Self::pdf_placeholder_body("正在解析 PDF…", page_border)
        } else {
            self.render_pdf_pages(window, cx, page_width, page_bg, page_border)
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

impl PdfViewer {
    /// 错误 / 加载中的居中占位文本（两分支同构，仅文案不同）。
    fn pdf_placeholder_body(text: &str, page_border: Hsla) -> gpui::AnyElement {
        div()
            .h_full()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(page_border)
            .child(text.to_string())
            .into_any_element()
    }

    /// 已加载正文：**页列 DOM 虚拟化**——scroll 容器里放一个「全文档总高」的定位容器，
    /// 只把可见页 ± MARGIN 的卡片以绝对定位铺进去（`build_visible_page_cards`）。
    /// 整列高度 = total_height，滚动条范围从一开始就正确；远页不参与本帧 DOM/布局。
    fn render_pdf_pages(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        page_width: f32,
        page_bg: Hsla,
        page_border: Hsla,
    ) -> gpui::AnyElement {
        let scroll_y = f32::from(-self.scroll.offset().y);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let (start, end) = keep_window(&self.dims, &self.page_tops, page_width, scroll_y, viewport_h);
        // 预取可见窗口文本层（低优先级懒加载）：交互时命中测试/复制/高亮已就绪。
        // 快速拖动时**跳过**——正在飞掠的页文本提取纯属浪费；停止后恢复预取。
        if !self.prev_fast {
            self.prefetch_texts(start, end);
        }
        let total = self.total_height;
        // P2 选区：render 帧内稳定的快照（高亮渲染用）
        let selection = self.selection();
        let sel_color = selection_color(cx);
        // P3 检索：命中高亮色 + 是否正在检索（非空查询才叠加命中高亮）
        let hit_color = search_hit_color(cx);
        let active_color = search_active_color();
        let searching = !self.search_query.trim().is_empty();
        let scale_factor = window.scale_factor();

        let cards = self.build_visible_page_cards(
            start,
            end,
            page_width,
            scale_factor,
            selection,
            sel_color,
            searching,
            hit_color,
            active_color,
            page_bg,
            page_border,
        );

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
    }
}
