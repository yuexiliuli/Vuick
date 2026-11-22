//! PDF 阅读器子模块：上屏渲染（原 `view.rs` 拆出）。
//!
//! 覆盖 [`gpui::Render`] 全链路：状态路由（错误 / 加载中 / 已加载）、外层画布与滚动条、
//! **页列 DOM 虚拟化**（[`super::PdfViewer::render_pdf_pages`] → 只铺「可见页 ± MARGIN」
//! 的绝对定位卡片 [`super::PdfViewer::build_visible_page_cards`]，长文档换帧 O(可见页)）、
//! 三层合成（底图封面/中清/缩略 → 瓦片 → 选区/检索高亮）。选区与检索命中高亮的纯几何
//! 叠加函数（`selection_overlays_for_page` / `search_overlays_for_page`）亦在本模块。

// 页渲染内核逐参数传参 → 放行
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use super::*;
use gpui::StyledImage as _;
use gpui::prelude::FluentBuilder as _;

/// 选区背景色（半透蓝；深浅两版，与 markdown 预览同一套色值，视觉统一）。
/// 已收编：读 `ui::theme::ColorScheme::doc_selection_bg`（明暗两值 = 本函数收编前原值）。
fn selection_color(cx: &App) -> Hsla {
    ui::theme::Theme::global(cx).colors.doc_selection_bg.into()
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
/// 已收编：`ui::content_colors::PDF_SEARCH_HIT_BLUE_{DARK,LIGHT}`（与编辑器查找弱黄
/// `editor_find_match_bg` 是不同色相/语义，勿混用）。
fn search_hit_color(cx: &App) -> Hsla {
    if ui::theme::Theme::global(cx).is_dark() {
        ui::content_colors::PDF_SEARCH_HIT_BLUE_DARK
    } else {
        ui::content_colors::PDF_SEARCH_HIT_BLUE_LIGHT
    }
}

/// 检索激活命中的高亮色：橙（更醒目）。
/// 已收编：`ui::content_colors::PDF_SEARCH_ACTIVE_AMBER`（与 WYSIWYG 告警橙同 `0xf59e0b`
/// 色相、alpha 档不同，见该 token 注释）。
fn search_active_color() -> Hsla {
    ui::content_colors::PDF_SEARCH_ACTIVE_AMBER
}

// ---- 待保存注释叠加层（P1：内存叠加渲染，零重载零闪烁；save() 才落盘）----

/// 待存高亮底色：半透明黄（叠在纸面之上、不遮正文；与写盘默认注释色同源系）。
/// 已收编：`ui::content_colors::PDF_ANNOT_HIGHLIGHT_YELLOW`。
fn annot_highlight_color() -> Hsla {
    ui::content_colors::PDF_ANNOT_HIGHLIGHT_YELLOW
}

/// 待存删除线色：红（不透明细条）。
/// 已收编：`ui::content_colors::PDF_ANNOT_STRIKE_RED`（非 `colors.error` 具体值，勿强行映射）。
fn annot_strike_color() -> Hsla {
    ui::content_colors::PDF_ANNOT_STRIKE_RED
}

/// 待存下划线色：蓝（不透明细条）。
/// 已收编：`ui::content_colors::PDF_ANNOT_UNDERLINE_BLUE`。
fn annot_underline_color() -> Hsla {
    ui::content_colors::PDF_ANNOT_UNDERLINE_BLUE
}

/// 灰框（点击命中的待存注释包围盒）边框色：中性灰——与选中高亮蓝（`selection_color`）、
/// 检索命中蓝/橙、折叠/点击高亮等所有既有叠加色**均可区分**（仓库既有「点击 vs 折叠
/// 高亮颜色必须不同」先例同精神）。
/// 已收编：`ui::content_colors::PDF_ANNOT_FRAME_GRAY`（与 `scrollbar_thumb` `0x6060604a`
/// 非同一具体值，勿强行映射）。
fn annot_focus_frame_color() -> Hsla {
    ui::content_colors::PDF_ANNOT_FRAME_GRAY
}

/// 图片选中描框色：**青绿系** rgb(0x2E_CC_8F)（暗处/亮处两版同色阶，透明度区分）。
/// 选择依据：与既有叠加色全部**明显可分**——选区蓝（`0x4d6bfe`）、检索蓝/橙、注释
/// 灰框、折叠/点击高亮均非青绿（对立色相），且对深色图片底也醒目（亮青描框）。
/// 已收编：`ui::content_colors::PDF_IMAGE_SELECT_GREEN_{DARK,LIGHT}`。
fn image_select_frame_color(cx: &App) -> Hsla {
    if ui::theme::Theme::global(cx).is_dark() {
        ui::content_colors::PDF_IMAGE_SELECT_GREEN_DARK
    } else {
        ui::content_colors::PDF_IMAGE_SELECT_GREEN_LIGHT
    }
}

/// 选中图片的描框叠加层（**卡片局部**坐标）：对 `(page, obj_index)` 命中的图片盒
/// 只画 1px 描边（**不盖任何底色**——图片内容颜色不得被选中高亮改变），盒外扩 2px
/// （同灰框 pad，视觉包住）。页图片盒未缓存 / 该页无选中图 → `None`。颜色由调用方经
/// [`image_select_frame_color`] 传入（与选区/检索叠加同款「渲染帧内取主题色」约定）。
#[allow(clippy::too_many_arguments)] // 几何/颜色参数互为正交，与 selection/search 叠加同构
fn image_select_frame_overlay_for_page(
    page: usize,
    selected: Option<(usize, usize)>,
    images: Option<&Vec<PdfImageBox>>,
    dim: (f32, f32),
    display_w: f32,
    scale: f32,
    color: Hsla,
) -> Option<gpui::AnyElement> {
    let (sel_page, sel_obj) = selected?;
    if sel_page != page {
        return None;
    }
    let b = images?.iter().find(|b| b.obj_index == sel_obj)?;
    // 外框略外扩（视觉包住图片本身），PDF 点 → 卡片局部（同灰框 pad=2.0）。
    let pad = 2.0;
    let (x0, y_top) = pt_to_local(b.x0 - pad, b.y1 + pad, dim.0, dim.1, display_w, scale);
    let (x1, y_bottom) = pt_to_local(b.x1 + pad, b.y0 - pad, dim.0, dim.1, display_w, scale);
    Some(
        div()
            .absolute()
            .left(px(x0))
            .top(px(y_top))
            .w(px((x1 - x0).max(1.0)))
            .h(px((y_bottom - y_top).max(1.0)))
            .border_1()
            .border_color(color)
            .into_any_element(),
    )
}

/// 某页的**待存注释**叠加层元素（卡片局部坐标，原点 = 卡片左上，Y 向下）：
/// - 高亮（Highlight）= 半透明黄整块填充 quad；
/// - 删除线（Strikeout）= 穿过 quad 垂直中心的细红条；
/// - 下划线（Underline）= 贴 quad 底缘的细蓝条。
///
/// 每条注释可多 quad（跨行选区）。quads 为 PDF 点（y up），经 `pt_to_local` 转卡片局部，
/// 与选区高亮/瓦片逐像素对齐（同一套几何管道）。
fn pending_annot_overlays_for_page(
    page: usize,
    pending: &[PendingAnnot],
    dim: (f32, f32),
    display_w: f32,
    scale: f32,
) -> Vec<gpui::AnyElement> {
    let mut out = Vec::new();
    let strike_col = annot_strike_color();
    let under_col = annot_underline_color();
    for annot in pending.iter().filter(|a| a.page == page) {
        if annot.kind == AnnotKind::Highlight {
            for q in &annot.quads_pt {
                let (x0, y_top) = pt_to_local(q.x0, q.y1, dim.0, dim.1, display_w, scale);
                let (x1, y_bottom) = pt_to_local(q.x1, q.y0, dim.0, dim.1, display_w, scale);
                out.push(
                    div()
                        .absolute()
                        .left(px(x0))
                        .top(px(y_top))
                        .w(px((x1 - x0).max(1.0)))
                        .h(px((y_bottom - y_top).max(1.0)))
                        .bg(annot_highlight_color())
                        .into_any_element(),
                );
            }
        } else {
            // 删除线/下划线：细条厚度随显示缩放（~1.5pt），视宽随 quad 宽
            let bar_h = (scale * 1.5).clamp(1.0, 3.0);
            for q in &annot.quads_pt {
                let (x0, y_top) = pt_to_local(q.x0, q.y1, dim.0, dim.1, display_w, scale);
                let (x1, y_bottom) = pt_to_local(q.x1, q.y0, dim.0, dim.1, display_w, scale);
                let w = (x1 - x0).max(1.0);
                let h = (y_bottom - y_top).max(1.0);
                // 删除线 = 垂直中心细条（pt y 中点 → local）；下划线 = 贴底缘
                let y_bar = if annot.kind == AnnotKind::Strikeout {
                    let mid_pt = (q.y0 + q.y1) * 0.5;
                    let (_, y_mid) = pt_to_local(0.0, mid_pt, dim.0, dim.1, display_w, scale);
                    y_mid - bar_h * 0.5
                } else {
                    y_top + h - bar_h
                };
                let color = if annot.kind == AnnotKind::Strikeout {
                    strike_col
                } else {
                    under_col
                };
                out.push(
                    div()
                        .absolute()
                        .left(px(x0))
                        .top(px(y_bar))
                        .w(px(w))
                        .h(px(bar_h))
                        .bg(color)
                        .into_any_element(),
                );
            }
        }
    }
    out
}

/// 某页的「灰框」焦注释包围盒（卡片局部坐标）：点击命中的那条注释的全部 quads 取
/// 外接矩形，画灰色边框框住文字区域（点非注释文字则无框）。`None` = 本页无焦注释。
fn annot_focus_frame_overlay_for_page(
    page: usize,
    focused: Option<u64>,
    pending: &[PendingAnnot],
    dim: (f32, f32),
    display_w: f32,
    scale: f32,
) -> Option<gpui::AnyElement> {
    let annot = pending
        .iter()
        .find(|a| a.page == page && Some(a.id) == focused)?;
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for q in &annot.quads_pt {
        min_x = min_x.min(q.x0.min(q.x1));
        max_x = max_x.max(q.x0.max(q.x1));
        min_y = min_y.min(q.y0.min(q.y1));
        max_y = max_y.max(q.y0.max(q.y1));
    }
    if !(min_x.is_finite()) {
        return None;
    }
    // 外框略外扩（视觉包住高亮条），PDF 点 → 卡片局部
    let pad = 2.0;
    let (x0, y_top) = pt_to_local(min_x - pad, max_y + pad, dim.0, dim.1, display_w, scale);
    let (x1, y_bottom) = pt_to_local(max_x + pad, min_y - pad, dim.0, dim.1, display_w, scale);
    // 已收编：待存注释聚焦填充底 `ui::content_colors::PDF_ANNOT_FOCUS_FILL_GRAY`（rgba(0x6b6b6b14)
    // = 灰框同色相的低透明填充，与边框 `PDF_ANNOT_FRAME_GRAY` 配套）。
    Some(
        div()
            .absolute()
            .left(px(x0))
            .top(px(y_top))
            .w(px((x1 - x0).max(1.0)))
            .h(px((y_bottom - y_top).max(1.0)))
            .border_1()
            .border_color(annot_focus_frame_color())
            .bg(ui::content_colors::PDF_ANNOT_FOCUS_FILL_GRAY)
            .into_any_element(),
    )
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
        let color = if idx == active {
            active_color
        } else {
            hit_color
        };
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

/// 三层合成中「新密度瓦片层是否允许上屏」的纯判定（S 系列双密度混叠根治，R1/R2/R3）。
///
/// 用户反馈放大缩小 PDF「似乎有东西盖在 PDF 上」、代码区缩放中「块状抖动」：settle 收口期
/// 新密度瓦片**逐块异步到位**、盖在「旧档封面被 CSS 拉伸」的底图上 → 同一页出现
/// `锐瓦片区 vs 糊底区` 的**扁平密度差**块状混叠。本判定是「同帧不混叠」不变式的唯一
/// 门控点，三条件任一满足才允许把瓦片层叠上屏：
/// - **`cover_now`（当前密度整页封面）在位** → 允许：底图与瓦片同密度，无密度差可混叠
///   （cap 邻域 `cover_serves_scale` 语义已把 4.0 封面视作等效当前密度，沿袭既有判定）；
/// - **全无底图**（`base == false`）→ 允许：首开/高速平移切入的**从未显示过**的新页没有
///   任何旧/低密度底可混 → 保持既有逐片流式出片（R3，不得误伤）；
/// - **瓦片整组就位**（`group_complete`，可见矩形 + `TILE_MARGIN` 每格都在槽，见
///   [`crate::pdf::layout::tile_range_all_in_slot`]）→ 允许：新密度内容对「已有旧底」的页
///   **整页一次换**（旧状 → 整页 sharp 一次跳变，R2 原子换）。
///
/// 三者全不满足（底图为旧/低密度 `prev_cover`/`prefetch`/`mid`/`thumb`，瓦片只部分到达）→
/// **禁止**绘制任何部分瓦片：整页统一显示旧底拉伸（**宁糊不混**），块状混叠从根上杜绝。
pub(crate) fn tiles_layer_allowed(cover_now: bool, base: bool, group_complete: bool) -> bool {
    cover_now || !base || group_complete
}

/// 页卡底图选择（缩放期/方向翻位防闪的纯逻辑，见 [`PdfViewer::build_visible_page_cards`]）：
/// 返回本帧应铺满整页的位图，优先级 **当前密度封面（锐利）→ 过渡底图
/// （[`Slot::prev_cover`]：旧密度封面缩放过渡 CSS 拉伸兜底，或 F2 方向翻位退位封面桥）
/// → 空闲预取走廊封面（[`Slot::prefetch_cover`]：0.5 密度、高速平移切入的即时底图）
/// → 中清 → 缩略**；全空 → `None`（调用方显示「第 N 页 加载中…」——仅首开/极速拖动这类
/// 真的没有任何位图的页才出现）。缩放瞬间 `cover_now` 为 `None`，但只要过渡图还在，底图
/// 就是它而非占位——「宁可瞬时模糊也不闪白」。抽成纯函数便于单测钉死该优先级。
pub(super) fn page_base_image(
    cover_now: Option<&Arc<RenderImage>>,
    cover_stale: Option<&Arc<RenderImage>>,
    prefetch: Option<&Arc<RenderImage>>,
    mid: Option<&Arc<RenderImage>>,
    thumb: Option<&Arc<RenderImage>>,
) -> Option<Arc<RenderImage>> {
    cover_now
        .or(cover_stale)
        .or(prefetch)
        .or(mid)
        .or(thumb)
        .cloned()
}

impl PdfViewer {
    /// 把「可视窗口内 [start, end] 的页」逐个铺成**绝对定位**的页卡片列表
    /// （页列虚拟化的核心，被 [`Render::render`] 的已加载分支调用）。
    ///
    /// 每页：按显示密度 1:1 取瓦片，算出位图→CSS 的分轴缩放（`pt_to_local` 同源，
    /// 瓦片与文本高亮逐像素对齐）；三层合成——底图（封面→中清→缩略图，先瞬时锐化
    /// 铺满首屏、占位兜底）→ 瓦片（逐片绝对定位、opaque 覆盖底图）→ 选区/检索高亮。
    /// 卡片自身保持 `absolute` 定位（页列虚拟化的根基，⚠️ 不可改 `.relative()`）。
    ///
    /// `paged` 为分页页排快照时（非空）：卡片定位/尺寸改由 `cell_rect(page) × zoom`
    /// 提供（双页=每行并排 2 页，行内 contain-fit）；位图密度/瓦片调度的**真源 =
    /// `page_card_display_size` 的卡宽/卡高**（B 线密度已落地：分页 = cell 宽 × zoom 渲染，
    /// 消除按连续 `page_width` 过采样 ~2.3× 线性 / ~5× 像素面积；见本函数下方密度块）。
    #[allow(clippy::too_many_arguments)] // 页卡片上下文互为正交
    fn build_visible_page_cards(
        &self,
        start: usize,
        end: usize,
        page_width: f32,
        scale_factor: f32,
        viewport_h: f32,
        wrap_w: f32,
        selection: Option<((usize, usize), (usize, usize))>,
        sel_color: Hsla,
        searching: bool,
        hit_color: Hsla,
        active_color: Hsla,
        img_color: Hsla,
        page_bg: Hsla,
        page_border: Hsla,
        paged: Option<&PagedLayout>,
    ) -> Vec<gpui::AnyElement> {
        let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(end - start + 1);
        // 观测探针：整次页列渲染前清零，pass 内逐页累加（本帧挂入的瓦片 img 元素总数，
        // 多页窗 = 各页之和；渲染未跑/无页 = 0）。
        self.last_frame_tiles_drawn.set(0);
        // #1 页卡设备像素网格对齐（去缩放换档「跳一下再归位」pop 本体）：页卡 top 原本是
        // 布局算出的 f32 分数值 → 旧底图（prev_cover，旧密度）被拉伸到分数设备位的卡片，
        // 与新栅格（ceil 整数位图像素）的 PDF→设备映射差 ≤1 设备 px/轴，换档瞬间内容
        // pop ±1px 然后稳。这里把**绘制用** top 先对齐到设备像素网格（`scroll_y` 参与：
        // 容器平移的分数设备位也被吸收；`snap_to_device` 幂等）——旧拉伸与新栅格共享
        // 同一放置网格，映射差归零（或 ≤0.5 设备 px）。只对齐放置矩形，不动 scroll、
        // 不动布局账（`page_tops`/命中链不变，测试读数不受影响）。
        let scroll_y = f32::from(-self.scroll.offset().y);
        for i in start..=end {
            let dim = self.dims[i];
            // 分页页排：卡位/尺寸由唯一显示矩形源 `paged_display_rect` 提供（水平按视口重算
            // 居中、垂直 r.y×zoom）；连续页列沿用 page_tops 定位、卡宽 = page_width。
            let (card_left, card_top_base, card_w, card_h) = if paged.is_some() {
                // 分页页拍：唯一显示矩形源 `paged_display_rect`（水平按视口重算居中——修复
                // zoom<1 时把居中坐标 `r.x` 也乘 zoom 导致的越缩越左偏；垂直沿用 r.y×zoom）。
                match self.paged_display_rect(i) {
                    Some(rect) => (rect.x, rect.y, rect.w, rect.h),
                    None => (
                        0.0,
                        self.page_tops[i],
                        page_width,
                        display_height(dim, page_width),
                    ),
                }
            } else {
                // 连续页列：page_tops 定位、卡宽 = page_width、卡高 = display_height。
                (
                    0.0,
                    self.page_tops[i],
                    page_width,
                    display_height(dim, page_width),
                )
            };
            let top = crate::pdf::layout::snap_to_device(card_top_base + scroll_y, scale_factor)
                - scroll_y;
            // 显示密度参考高 = 页卡显示高（分页 = paged_display_rect.h、连续 = display_height），
            // 与调度侧 `visible_tile_range_axis` 对账同口径；**B 线密度真源**：瓦片/封面密度
            // 改按卡宽（分页 = cell 宽 × zoom）渲染，消除分页按连续 page_width 过采样
            // ~2.3×（线性）/ ~5×（像素面积，本窗口）。
            let disp_h = card_h;
            // 瓦片按显示密度 1:1 渲染；这里算出位图→CSS 的分轴缩放，放置瓦片与
            // 文本高亮（`pt_to_local` 同源）逐像素对齐。分页卡按自身宽高（`card_w/h`）
            // 缩放——连续时 card_w==page_width、card_h==disp_h，行为不变。
            let scale = render_scale(card_w, scale_factor, dim.0);
            let bmp = page_bitmap_size(dim, scale);
            let (sx, sy) = bitmap_display_scales(bmp, card_w, card_h);

            // 收集该页在**当前密度**下已就绪的瓦片（Arc 克隆 + 卡片局部定位）。
            let slot = &self.slots[i];
            let tile_ok = (slot.tile_scale - scale).abs() <= 1e-4;
            let slot_thumb = slot.thumb.clone();
            let slot_mid = slot.mid.clone();
            // 首屏整页锐化封面（满显示密度；缩放过期时 schedule_tiles 已移入 prev_cover）。
            let slot_cover =
                if (slot.cover_scale - cover_scale(dim, card_w, scale_factor)).abs() <= 1e-4 {
                    slot.cover.clone()
                } else {
                    None
                };
            // 缩放过渡期兜底：旧密度封面（schedule_tiles 从 cover 移入）在新密度封面到位前
            // 按新页宽 CSS 拉伸铺满，防「Ctrl+滚轮缩放期间每档闪一次『加载中』」空白。
            let slot_prev_cover = slot.prev_cover.clone();
            // **P1 ③（邻居降档封面入视防闪白）**：邻居页封面降到 ~2.0 档后，入视首帧若恰逢
            // VeryFast（纯平移不跑 schedule 迁移 stale→prev_cover），该 2.0 封面仍在 slot.cover、
            // 密度 ≠ 当前 cscale → `slot_cover` 缺席 → 无底图 → 闪「加载中」。把「密度不符但在位
            // 的低档封面」也纳入过渡底图链（与 prev_cover 同角色、CSS 拉伸铺满）；schedule 随后
            // 迁移 + 满密重派，或满密封面到位即替换——过渡期屏显恒有底图。
            let slot_cover_stale = if slot_cover.is_some() || slot.cover_scale.abs() <= 1e-4 {
                None
            } else {
                slot.cover.clone()
            };
            // F4 空闲预取走廊封面（0.5 密度整页）：高速平移切入的新页在满密度封面/瓦片
            // 就位前的即时底图（瓦片未到、封面在途时 CSS 拉伸铺满，优于「加载中」白块）。
            // **S2 修正（评审 BLOCK①）**：仅当**当前 cscale 密度** cover 在位才隐藏 preview；
            // stale cover（`cover_scale≠当前 cscale`，缩放中途旧档）在位时 preview 仍作底图，
            // 屏显从「旧封面复利拉伸」降为「当前档 0.5 密度」——与 `collect_prefetch_cover_results`
            // 的「仅当前密度 cover 才弃」对称，共同让原地连续多档缩放的可见页能吃到 preview。
            let slot_prefetch = slot.prefetch_cover.clone().filter(|_| {
                slot.cover.is_none()
                    || (slot.cover_scale - cover_scale(dim, card_w, scale_factor)).abs() > 1e-4
            });
            // 底图选择提前（本帧「同帧不混叠」门控的输入之一）：当前密度封面 → 旧密度封面
            // 拉伸兜底（prev_cover 平移过渡桥 / P1 ③ 低档在位封面）→ 空闲预取走廊封面 → 中清
            // → 缩略图。缩放期不闪白（宁可瞬时模糊也要有底图）；全空（首开/极速拖动）才落
            // 「加载中」占位。
            let cover_stale = slot_prev_cover.as_ref().or(slot_cover_stale.as_ref());
            let base = page_base_image(
                slot_cover.as_ref(),
                cover_stale,
                slot_prefetch.as_ref(),
                slot_mid.as_ref(),
                slot_thumb.as_ref(),
            );
            let mut tiles: Vec<(f32, f32, f32, f32, Arc<RenderImage>)> = Vec::new();
            let mut group_complete = false;
            if tile_ok && !slot.tiles.is_empty() {
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
                // 新密度瓦片「整组就位」判定（R2 原子换）：与瓦片调度同源几何
                // （`page_visible_rect` + `visible_tile_range`，含 TILE_MARGIN：调度侧只派
                // 可见矩形 + margin 的格子 → 组 = 该集合，不是整页 rows×cols——高倍/大页的
                // 页高远超视口，整页组永远不会到齐，会永久停在糊底）。与
                // `retire_high_scale_covers` 的「可见区铺满」对账同一套数学。
                group_complete = self
                    .page_visible_rect(i, scroll_y, viewport_h, page_width, wrap_w)
                    .and_then(|rect| {
                        crate::pdf::layout::visible_tile_range_axis(
                            bmp,
                            card_w,
                            disp_h,
                            rect,
                            TILE_SIZE,
                            TILE_MARGIN,
                            config::pdf::TILE_MARGIN_COL,
                        )
                    })
                    .is_some_and(|(r0, c0, r1, c1)| {
                        crate::pdf::layout::tile_range_all_in_slot(&slot.tiles, r0, c0, r1, c1)
                    });
            }
            // **S 系列双密度混叠根治门控**：底图链为非当前密度（`cover_now` 缺席 →
            // prev_cover/prefetch/mid/thumb 其一在供）且瓦片只**部分**到达 → 本帧不绘制任何
            // 新密度瓦片，整页统一显示旧底（宁糊不混）；新密度对「已有旧底」的页整组一次换。
            // 仅「当前密度封面在位」或「全无底图（全新页，无旧底可混 → 保持流式）」例外。
            let render_tiles = tile_ok
                && !tiles.is_empty()
                && tiles_layer_allowed(slot_cover.is_some(), base.is_some(), group_complete);
            if render_tiles {
                // 观测探针：本帧实际挂入卡片 DOM 的瓦片 img 元素数（S-series 原子换回归断言，
                // 区分「旧底图 img」与「新瓦片 img」：修复前部分瓦片即 element 落树）。
                // pass 级累加（见函数首清零），多页窗 = 各页之和。
                self.last_frame_tiles_drawn
                    .set(self.last_frame_tiles_drawn.get() + tiles.len());
            }
            let mut overlays = selection_overlays_for_page(
                i,
                selection,
                dim,
                card_w, // 显示宽 = 卡宽（分页 = cell_rect.w×zoom；连续 = page_width，不变）
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
                    card_w,
                    scale,
                    hit_color,
                    active_color,
                ));
            }
            // 待保存注释叠加层（P1：内存叠加渲染，零重载零闪烁；坐标与选区同源）
            overlays.extend(pending_annot_overlays_for_page(
                i,
                &self.pending_annots,
                dim,
                card_w,
                scale,
            ));
            // 灰框（点击命中的待存注释包围盒）
            if let Some(frame) = annot_focus_frame_overlay_for_page(
                i,
                self.focused_annot,
                &self.pending_annots,
                dim,
                card_w,
                scale,
            ) {
                overlays.push(frame);
            }
            // 图片选中描框（左键/右键命中图片 → 青绿色描边；页图片盒未缓存则该帧暂缺）
            if let Some(frame) = image_select_frame_overlay_for_page(
                i,
                self.selected_image,
                self.image_pages.get(&i),
                dim,
                card_w,
                scale,
                img_color,
            ) {
                overlays.push(frame);
            }
            // 卡片保持 **absolute 定位**（页列虚拟化的根基）。⚠️ 不可改成
            // `.relative()`：`absolute`/`relative` 同是 position 设置器、后调用者胜，
            // 一旦覆盖成 relative，页列绝对定位整体失效 → 页距被拉远、滚动跳页、
            // 滚动条量程错乱。选区高亮需要"定位祖先"做锚点 → 改在**卡片内部**包一层
            // `size_full().relative()` 容器：它不影响卡片自身盒定位，又给绝对定位
            // 的子元素提供以卡片左上角为原点的坐标系。
            // T4 GPU 重着色通道：门控开启 + 设了重着色时，显示位图统一携带重着色参数
            // （`img().recolor(...)` → GPU 片元在绘制时着色；worker 已直传未着色原图）。
            let gpu_params = if self.gpu_recolor {
                if let Some(rc) = self.recolor {
                    // 真机诊断 3/4：首个带 recolor 的 img 挂参打一次（门控开启时；latch）。
                    crate::recolor::diag_view_recolor(rc.bg, rc.ink);
                    Some(crate::recolor::to_image_recolor(rc))
                } else {
                    None
                }
            } else {
                None
            };
            let mut card = div()
                .id(SharedString::from(format!("pdf-page-{i}")))
                .debug_selector(move || format!("pdf-page-{i}"))
                .absolute()
                .top(px(top))
                .left(px(card_left))
                .flex_shrink_0()
                .w(px(card_w))
                .h(px(card_h))
                .rounded(px(4.))
                .border_1()
                .border_color(page_border)
                .overflow_hidden();

            // 三层合成：底图（当前密度封面 → 旧密度封面拉伸兜底 → 空闲预取走廊封面 →
            // 中清 → 缩略图，缩放期不闪白：宁可瞬时模糊也要有底图）→ 瓦片（逐片绝对定位，
            // opaque 覆盖底图）→ 选区/检索高亮。全空（首开/极速拖动）才落「加载中」占位。
            // 瓦片分支受上方 `render_tiles` 门控：旧底 + 部分瓦片 → 走「底图铺满」分支，
            // 本帧不含任何瓦片元素（宁糊不混）。
            let inner: gpui::AnyElement = if render_tiles {
                let mut c = div().size_full().relative();
                if let Some(base) = base {
                    c = c.child(
                        img(ImageSource::Render(base.clone()))
                            .w(px(card_w))
                            .h(px(card_h))
                            .recolor(gpu_params),
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
                            .child(
                                img(ImageSource::Render(tile_img))
                                    .size_full()
                                    .recolor(gpu_params),
                            ),
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
                            .w(px(card_w))
                            .h(px(card_h))
                            .recolor(gpu_params),
                    )
                    .into_any_element()
            } else {
                // F4 观测：此处是「第 N 页 加载中…」唯一来源（底图链 cover→prev_cover→
                // 廊封面→mid→thumb 全空）。自增计数器供回归断言「常态主路径不再让可见页
                // 长时间落加载态」（仅剩 worker 未回的第一帧可短暂命中）。
                self.loading_placeholder_hits
                    .set(self.loading_placeholder_hits.get() + 1);
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
            card = card.child(div().size_full().relative().child(inner).children(overlays));
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

        // 错误 / 加载中 / 已加载三种状态（已加载再按 grid_mode / page_mode 三路分派：
        // 网格视图 vs 分页页排 vs 单列连续阅读）
        let body: gpui::AnyElement = if let Some(err) = &self.state.load_error {
            Self::pdf_placeholder_body(&format!("无法打开 PDF：{err}"), page_border)
        } else if self.state.loading || self.dims.is_empty() {
            Self::pdf_placeholder_body("正在解析 PDF…", page_border)
        } else if self.grid_mode {
            self.render_pdf_grid(window, cx)
        } else if self.page_mode != PageMode::Continuous {
            self.render_pdf_pages_paged(window, cx, page_width, page_bg, page_border)
        } else {
            self.render_pdf_pages(window, cx, page_width, page_bg, page_border)
        };
        // Phase B1：选中弹卡（独立 deferred 浮层，随滚动/重选/点外/Esc 关闭）。
        let sel_overlay = self.sel_card_element(window, cx);

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
                            // wave5：网格顶栏贴网格滚动容器顶端——overlay 顶偏工具栏高，滚动条
                            // 轨道与滚动容器可视区对齐（否则条长多出工具栏高一段，观感错位）。
                            .when(self.grid_mode, |this| this.top(px(GRID_TOOLBAR_H)))
                            // 滚动条绑定**当前活动滚动句柄**：单列跑 `scroll`、网格跑独立
                            // `grid_scroll`（两者状态互不干扰；网格无横向溢出故无横向条）。
                            .child(
                                Scrollbar::vertical(if self.grid_mode {
                                    &self.grid_scroll
                                } else {
                                    &self.scroll
                                })
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                            )
                            // 横向滚动条：仅单列（窗口过窄/放大超宽时 scroll 容器内容宽于
                            // 视口，横向溢出可滚；网格卡长按视口排布无横向溢出）。
                            .when(!self.grid_mode, |this| {
                                this.child(
                                    Scrollbar::horizontal(&self.scroll)
                                        .mode(ScrollbarMode::Always)
                                        .viewport_from_layout(),
                                )
                            }),
                    ),
            )
            .when_some(sel_overlay, |this, o| this.child(o))
    }
}

impl PdfViewer {
    /// Phase B1：选中文字后的操作菜单浮层（有选中弹卡时返回 `Some`）。沿用
    /// `note_card` / `image_edit_card` 的 `deferred(anchored(Window))` 约定（逃逸滚动
    /// 容器裁剪/命中）：锚定在**松开鼠标处**（窗口坐标，同右键菜单「点在哪出在哪」），
    /// 越界 `snap_to_window_with_margin` 夹回窗口；点卡片外（`on_mouse_down_out`）关闭。
    /// 菜单形式（一行一项，同右键 `PopupMenu`）—— 复制 / 高亮 / 删除线 / 下划线。
    fn sel_card_element(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let card_state = self.sel_card?;
        use gpui::{
            Anchor, AnchoredPositionMode, Edges, InteractiveElement, IntoElement, ParentElement,
        };
        let colors = ui::theme::Theme::global(cx).colors;
        let ed = cx.entity();
        let colors_fg: gpui::Hsla = colors.fg.into();
        let dark = ui::theme::Theme::global(cx).is_dark();
        let popover_bg: gpui::Hsla = colors.popover_bg.into();

        // 每一行菜单项：hover 底色 + 点击回调（捕获各自的注释类型 / 关闭动作）。
        let copy_item = Self::sel_card_item(
            "pdf-sel-card-copy",
            "复制",
            colors_fg,
            popover_bg,
            cx,
            |this: &mut PdfViewer, _: &gpui::ClickEvent, window, cx| {
                this.copy_selection(window, cx);
                // 复制不清选区（可反复复制）；仅收卡。
                this.close_sel_card(cx);
            },
        );
        let hl_item = Self::sel_card_item(
            "pdf-sel-card-highlight",
            "高亮",
            colors_fg,
            popover_bg,
            cx,
            |this: &mut PdfViewer, _: &gpui::ClickEvent, _w, cx| {
                this.annotate_selection(AnnotKind::Highlight, cx);
            },
        );
        let st_item = Self::sel_card_item(
            "pdf-sel-card-strike",
            "删除线",
            colors_fg,
            popover_bg,
            cx,
            |this: &mut PdfViewer, _: &gpui::ClickEvent, _w, cx| {
                this.annotate_selection(AnnotKind::Strikeout, cx);
            },
        );
        let ul_item = Self::sel_card_item(
            "pdf-sel-card-underline",
            "下划线",
            colors_fg,
            popover_bg,
            cx,
            |this: &mut PdfViewer, _: &gpui::ClickEvent, _w, cx| {
                this.annotate_selection(AnnotKind::Underline, cx);
            },
        );

        // 选区弹卡壳：走 ui::glass_card——Solid(popover_bg) 底、PopoverDouble 双影逐值 =
        // 原黑 hsla 0.18@12 + 0.12@4、Pg(4,2) = 原 flex_col+p4+gap_y2；id/debug_selector/
        // on_mouse_down_out/occlude 宿主交互与 min_w120 补挂留在原位。
        let card = ui::glass_card(
            &colors,
            dark,
            ui::GlassCardBg::Solid(popover_bg),
            ui::CardShellShadow::PopoverDouble,
            ui::CardShellRadius::Lg,
            ui::CardShellPad::Pg(px(4.), px(2.)),
            None,
        )
        .id("pdf-sel-card")
        .debug_selector(|| "pdf-sel-card".into())
        // 点卡片外任意处 → 关闭（浮层内点击不外发，不误关；K15 事件互斥）。
        .on_mouse_down_out(move |_: &gpui::MouseDownEvent, _w, cx| {
            let ed = ed.clone();
            ed.update(cx, |e, cx| e.close_sel_card(cx));
        })
        .occlude()
        .min_w(px(120.))
        .child(copy_item)
        .child(hl_item)
        .child(st_item)
        .child(ul_item);

        Some(
            gpui::deferred(
                gpui::anchored()
                    .position_mode(AnchoredPositionMode::Window)
                    .anchor(Anchor::TopLeft)
                    .position(card_state.pos)
                    .snap_to_window_with_margin(Edges::all(px(8.)))
                    .child(card),
            )
            .with_priority(100)
            .into_any_element(),
        )
    }
}

impl PdfViewer {
    /// 选中菜单里的一行操作项（id/debug_selector 供测试定位）。
    #[allow(clippy::too_many_arguments)] // 四色 + 回调，与 toc_item_frame 同构
    fn sel_card_item<F>(
        id: &'static str,
        label: &'static str,
        colors_fg: gpui::Hsla,
        popover_bg: gpui::Hsla,
        cx: &mut Context<PdfViewer>,
        handler: F,
    ) -> gpui::AnyElement
    where
        F: Fn(&mut PdfViewer, &gpui::ClickEvent, &mut Window, &mut Context<PdfViewer>) + 'static,
    {
        let id2 = id;
        div()
            .id(gpui::ElementId::from(id))
            .debug_selector(move || id2.into())
            .w_full()
            .h(px(28.))
            .px_2()
            .rounded_sm()
            .flex()
            .items_center()
            .cursor(gpui::CursorStyle::PointingHand)
            .text_size(px(12.5))
            .text_color(colors_fg)
            .hover(|style| style.bg(popover_bg.opacity(0.5)))
            .active(|style| style.bg(popover_bg.opacity(0.8)))
            .on_click(cx.listener(handler))
            .child(label)
            .into_any_element()
    }
}

impl PdfViewer {
    /// 错误 / 加载中的居中占位文本（两分支同构，仅文案不同）。
    /// DOM 与旧手写版本逐字一致（size_full flex 居中 + 14px + `page_border` 色），
    /// 改由 [`ui::centered_placeholder`] 承载（Pattern 1.1 收敛）。
    fn pdf_placeholder_body(text: &str, page_border: Hsla) -> gpui::AnyElement {
        ui::centered_placeholder(text)
            .color(page_border)
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
        let (start, end) = keep_window(
            &self.dims,
            &self.page_tops,
            page_width,
            scroll_y,
            viewport_h,
        );
        // 预取可见窗口文本层（低优先级懒加载）：交互时命中测试/复制/高亮已就绪。
        // 快速拖动时**跳过**——正在飞掠的页文本提取纯属浪费；停止后恢复预取。
        if !self.prev_fast {
            self.prefetch_texts(start, end);
            // 图片盒同理（点击/右键命中图片用）：与文本层同节奏懒加载预取。
            self.prefetch_images(start, end);
        }
        let total = self.total_height;
        // P2 选区：render 帧内稳定的快照（高亮渲染用）
        let selection = self.selection();
        let sel_color = selection_color(cx);
        // P3 检索：命中高亮色 + 是否正在检索（非空查询才叠加命中高亮）
        let hit_color = search_hit_color(cx);
        let active_color = search_active_color();
        let searching = !self.search_query.trim().is_empty();
        // 图片选中描框色（青绿系，与选区蓝/检索/灰框均可区分）
        let img_color = image_select_frame_color(cx);
        let scale_factor = window.scale_factor();

        let cards = self.build_visible_page_cards(
            start,
            end,
            page_width,
            scale_factor,
            viewport_h,
            self.wrap_width(window, page_width),
            selection,
            sel_color,
            searching,
            hit_color,
            active_color,
            img_color,
            page_bg,
            page_border,
            None, // 连续页列：不用分页布局定位
        );

        // 非溢出档鼠标缩放水平偏移：行 justify_center 居中的是「外边距盒」→ 在内容上加
        // 不对称外边距 `ml=2·max(off,0)` / `mr=2·max(−off,0)` 即把内容列平移 `off`
        // （演算 K177：`内容左 = (wrap−页宽−ml−mr)/2 + ml = (wrap−页宽)/2 + off`，与命中链
        // `column_left + x_zoom_offset` 逐值同源；taffy 0.13 `used_space` 含 margin，
        // justify_center 按外边距盒居中）。off=0 时的 ml=mr=0 → 与旧 justify_center 逐字节一致。
        let x_off = self.x_zoom_offset;
        let ml = (2.0 * x_off).max(0.0);
        let mr = (2.0 * -x_off).max(0.0);
        let content = div()
            .id("pdf-pages")
            .debug_selector(|| "pdf-pages".into())
            .relative()
            .w(px(page_width))
            .h(px(total))
            .ml(px(ml))
            .mr(px(mr))
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

    /// 已加载正文的**分页页排**（`!grid_mode && page_mode != Continuous`）：与
    /// [`Self::render_pdf_pages`] 同构的滚动容器，但卡片按 [`PagedLayout::cell_rect`]×zoom
    /// 绝对定位——双页 = 每行并排 2 页（页高 ≤ 视口），行按页高堆叠。
    /// 布局由 `ensure_window` 每帧以 zoom=1（fit）口径算入 [`Self::paged_layout_cell`]，
    /// 本函数读上一帧布局并统一乘 `zoom` 显示；可见页区间经
    /// [`crate::pdf::layout::paged_render_window`]（负偏移，换算回布局坐标 ÷zoom）→
    /// 视口相交的**全可见行**（行中自由滚动可同时看到上下两行）± 1 行缓冲的连续页下标区间。
    /// **Facing = 连续自由滚动 + 2-up**：滚动语义与连续一致（不再逐行步进/吸附、不再
    /// 「绝不露下一行」）。连续阅读专属的 `page_tops`/`total_height` 在分页模式下**不再用于
    /// 定位**（存续，仅调度侧 keep_window 兜底派图）。`pdf-pages` 容器宽 = `spread_max × zoom`
    /// ——行内居中由 `cell_rect` 的 `left_pad` 内建（不再 `justify_center`，防双重居中）；
    /// spread > 视口时横向溢出自然出现。
    fn render_pdf_pages_paged(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        page_width: f32,
        page_bg: Hsla,
        page_border: Hsla,
    ) -> gpui::AnyElement {
        // 无布局快照（连续模式/尚未装配）→ 安全走连续兜底渲染。
        let Some(layout) = self.paged_layout_cell.borrow().clone() else {
            return self.render_pdf_pages(window, cx, page_width, page_bg, page_border);
        };
        // PagedLayout 以 zoom=1（fit）口径：显示几何统一乘 zoom（均匀缩放）。
        let zoom = self.zoom.max(0.01);
        let total = layout.total_height * zoom;
        let spread = (layout.spread_max * zoom).max(1.0);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        // 可见行换算回布局坐标：内容显示 = 布局 × zoom → 布局滚动偏移 = 显示偏移 / zoom
        // （first_visible_row 用「负偏移」，视口顶内容 y = −scroll_y）。
        let offset = f32::from(-self.scroll.offset().y).max(0.0);
        let scroll_y_fit = -(offset / zoom);
        let vh_fit = viewport_h / zoom;

        let mut cards: Vec<gpui::AnyElement> = Vec::new();
        if layout.rows > 0 {
            // 连续自由滚动语义：视口相交的**全可见行**（行中位压到的下一行也算）± 1 行缓冲
            // 全部物化。行中停位可同时看到上下两行——旧的「只渲首行 r0、绝不露下一行」观感
            // 被本条取代（bug 1 由全可见行窗消除）。机械 / 底图缓冲由 A 线 ±PAGED_WINDOW_ROWS
            // 行调度兜底，此处 RENDER_BUF_ROWS=1 仅保「显示行都有卡」。
            const RENDER_BUF_ROWS: usize = 1;
            if let Some((start, end)) = crate::pdf::layout::paged_render_window(
                &layout,
                scroll_y_fit,
                vh_fit,
                self.dims.len(),
                RENDER_BUF_ROWS,
            )
                && start <= end {
                    // 预取可见页文本层/图片盒（与连续阅读同节奏懒加载）。
                    if !self.prev_fast {
                        self.prefetch_texts(start, end);
                        self.prefetch_images(start, end);
                    }
                    // P2 选区 / P3 检索 / 图片描框：与连续阅读同口径（本切片命中/视觉
                    // 对齐随 S5b 再精修到 cell_rect_zoom；位图密度暂用既有 page_width）。
                    let selection = self.selection();
                    let sel_color = selection_color(cx);
                    let hit_color = search_hit_color(cx);
                    let active_color = search_active_color();
                    let searching = !self.search_query.trim().is_empty();
                    let img_color = image_select_frame_color(cx);
                    let scale_factor = window.scale_factor();
                    cards = self.build_visible_page_cards(
                        start,
                        end,
                        page_width,
                        scale_factor,
                        viewport_h,
                        self.wrap_width(window, page_width),
                        selection,
                        sel_color,
                        searching,
                        hit_color,
                        active_color,
                        img_color,
                        page_bg,
                        page_border,
                        Some(&layout),
                    );
                }
        }

        let content = div()
            .id("pdf-pages")
            .debug_selector(|| "pdf-pages".into())
            .relative()
            .w(px(spread))
            .h(px(total))
            .children(cards);

        // 与连续阅读同构的滚动容器（分页复用阅读 scroll；滚动语义同连续自由滚动）。
        div()
            .id("pdf-scroll")
            .debug_selector(|| "pdf-scroll".into())
            .size_full()
            .overflow_y_scroll()
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            // 文本层交互钩子同连续（P2 命中在 S5b 改分页几何，先保持挂载语义不破坏）。
            .track_focus(&self.focus_handle)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                div()
                    .id("pdf-column-wrap")
                    .w_full()
                    .min_w(px(spread))
                    // 行内居中已由 cell_rect 的 left_pad 内建 → 不 justify_center（防双重居中）；
                    // spread 超视口时 wrap 宽于视口 → 横向溢出自然出现。
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(content),
            )
            .into_any_element()
    }

    /// 已加载正文的**网格视图**（`grid_mode` 时）：顶部一条 wave5 工具栏（全选/清空/退出 +
    /// 删除·拆分·提取·合并计数按钮），其下是独立 `grid_scroll` 滚动容器（`flex_1` 占满
    /// 剩余高；工具栏为 out-of-flow 固定头，卡区视口从工具栏底开始），滚动容器里放一个
    /// 「全文档网格总高」的定位容器，只把 **第一可见行 ±1 行缓冲** 内的页卡以绝对定位
    /// 铺进 DOM（`grid_visible_range`），远行不物化（行带虚拟化）。卡宽 = `grid_card_width()`
    /// （derived from zoom）；Ctrl+滚轮缩放 → 卡宽/列数变化（`on_scroll_wheel` 挂在内容元素，
    /// 见下）。每卡 = 页缩略图（`grid_thumbs`，经 `request_grid` 异步拉取；未就绪显示占位
    /// 底色 + 页码）＋ 页码徽标 + wave5 选中视觉（checkbox 勾选 / Ctrl 切换 / Shift 范围选，
    /// 见 `build_grid_cards`）；普通单击卡 → 切换该页选中态（**留在网格**）、双击 →
    /// 退出网格 + `scroll_to_page(该页)`（wave3 跳页语义迁到双击）。滚动偏移、缩略图槽
    /// 均独立于单列状态，切模式互不干扰。
    fn render_pdf_grid(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let (page_bg, page_border) = Self::page_card_colors(cx);
        let (vw, vh) = self.grid_viewport(window);
        // 记录本帧视口宽：无 window 句柄的 `set_grid_zoom` 用它作列数钳的 bounds 回退
        // （grid scroll 未布局时），保证 wheel/programmatic/render 三处钳界同源。
        self.last_grid_viewport_w = vw;
        // R4：网格缩放**列数钳归一化** —— `grid_zoom` 若超出当前视口宽的 [z_min, z_max]
        // （2/9 列钳，见 `grid_zoom_limits_for_cols`）就归回界内，**走 `set_grid_zoom`**
        // 路径以触发同比重锚 / 跨桶桥 / 管派刷新，不直接写字段。进入网格（首帧）与视口宽
        // 变化（拉宽/拉窄）都在这里收敛（wheel/programmatic 已按同范围钳，此处只管外部
        // 状态变化——如先前较窄视口遗留的越界 grid_zoom；resize 后 1-2 帧内 settle）。
        let (z_min, z_max) = grid_zoom_limits_for_cols(
            GRID_CARD_WIDTH,
            GRID_GAP,
            GRID_PADDING,
            vw,
            GRID_MAX_COLS,
            GRID_MIN_COLS,
        );
        if self.grid_zoom < z_min - 1e-4 || self.grid_zoom > z_max + 1e-4 {
            self.set_grid_zoom(self.grid_zoom.clamp(z_min, z_max), cx);
        }
        let layout = grid_layout(
            &self.dims,
            self.grid_card_width(),
            GRID_GAP,
            GRID_PADDING,
            vw,
        );
        // wave6：**仅拖拽态**把布局快照抄入拖拽命中槽——未拖拽时消费者为空（`grid_drag_move`
        // 只在拖拽中读上一帧布局算插入位），避免每帧全量 `clone` 一份 GridLayout；
        // 进入拖拽后下一帧自会重新写入（on_drag 建立拖拽态 → 本帧/下帧写快照）。
        if self.grid_drag.is_some() {
            *self.grid_layout_cell.borrow_mut() = Some(layout.clone());
        }
        let offset = f32::from(-self.grid_scroll.offset().y);
        // 行带虚拟化：仅物化「可视行 ±1 行」的页卡，其余不建 DOM（对齐页列虚拟化口径）。
        // 观测探针（headless 断言虚拟化的确定性证据）：记录本帧实际物化区间与卡数。
        let mut cards: Vec<gpui::AnyElement> = match grid_visible_range(&layout, offset, vh) {
            Some((start, end)) => {
                self.grid_last_range.set((start, end));
                self.grid_last_drawn.set(end - start + 1);
                self.build_grid_cards(cx, &layout, start, end, page_bg, page_border)
            }
            None => {
                self.grid_last_range.set((0, 0));
                self.grid_last_drawn.set(0);
                Vec::new()
            }
        };
        // wave6：拖拽中 → 在网格内容容器内叠加 ghost（半透明卡随指针）+ 插入指示线。
        if self.grid_drag.is_some() {
            cards.extend(self.grid_reorder_overlays(cx, &layout));
        }
        let content = div()
            .id("pdf-grid")
            .debug_selector(|| "pdf-grid".into())
            .relative()
            .w(px(vw))
            .h(px(layout.total_height))
            .bg(canvas)
            // wave6 拖拽移动：`pdf-grid`（内容容器）的 laid-out bounds 含滚动平移 →
            // 指针窗口坐标 − bounds 原点即内容坐标（无需额外滚动换算）。
            .on_drag_move(
                cx.listener(|this, ev: &DragMoveEvent<GridDragData>, _window, _cx| {
                    this.grid_drag_move(ev);
                }),
            )
            // Ctrl+滚轮缩放：挂在**网格内容元素**上（`pdf-grid-scroll` 的子元素），bubble 期
            // 先于滚动容器的内置滚动处理 → Ctrl 时 `on_scroll_wheel` 内 stop_propagation 阻止
            // 滚动、按 zoom 改卡宽（列数变化 + 缩略图按新分桶重取）；普通滚轮不拦截，仍交
            // 滚动容器垂直滚动。与单列 `pdf-column-wrap` 的挂载姿势一致。
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .children(cards);
        div()
            .id("pdf-grid-root")
            .debug_selector(|| "pdf-grid-root".into())
            .size_full()
            .flex()
            .flex_col()
            .items_stretch()
            // wave6：网格根聚焦本视图焦点句柄 → 拖拽中 Esc 可达 `on_key_down` 取消拖拽。
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            // wave6 松手收尾：网格根任一松手（卡/内容/padding/工具条）都结算拖拽或单击；
            // 网格外松手（窗口外 / 根之外）→ 放弃（`on_mouse_up_out`）。
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.grid_drag_release(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.grid_drag_release_outside(cx);
                }),
            )
            // wave5 工具栏：固定头在滚动容器之上（flex 行内先渲染），贴顶不高过卡区。
            .child(self.render_grid_toolbar(cx))
            .child(
                div()
                    .id("pdf-grid-scroll")
                    .debug_selector(|| "pdf-grid-scroll".into())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.grid_scroll)
                    .child(content),
            )
            .into_any_element()
    }

    /// wave6 网格拖拽覆盖层（拖拽中每帧构建，装进网格内容容器、绝对定位=内容坐标）：
    /// 半透明 ghost（随 `GridDrag::pointer` 居中）+ 插入指示线（[`Self::grid_insert_line_rect`]，
    /// `tab_reorder_line` 纯白语义色——与选项卡拖拽插入线同源颜色）。无拖拽组 → 空。
    fn grid_reorder_overlays(
        &self,
        cx: &Context<Self>,
        layout: &GridLayout,
    ) -> Vec<gpui::AnyElement> {
        let Some(drag) = self.grid_drag.as_ref() else {
            return Vec::new();
        };
        if drag.dragging.is_empty() {
            return Vec::new();
        }
        let theme = ui::theme::Theme::global(cx);
        let colors = theme.colors;
        let chrome = theme.chrome;
        let accent: gpui::Hsla = colors.accent.into();
        let line_color: gpui::Hsla = chrome.tab_reorder_line.into();
        // ghost 尺寸取被拖组首卡的实际卡矩形（无卡矩形时退 cell_w × 120 兜底）。
        let (w, h) = layout
            .cell_rect(drag.dragging[0])
            .map(|(_, _, w, h)| (w, h))
            .unwrap_or((layout.cell_w, 120.0));
        let mut out = Vec::with_capacity(2);
        out.push(
            div()
                .id("pdf-grid-ghost")
                .debug_selector(|| "pdf-grid-ghost".into())
                .absolute()
                .left(px((drag.pointer.x - w / 2.0).max(0.0)))
                .top(px((drag.pointer.y - h / 2.0).max(0.0)))
                .w(px(w))
                .h(px(h))
                .rounded(px(6.))
                .border_1()
                .border_color(accent)
                .bg(accent.opacity(0.22))
                .into_any_element(),
        );
        out.push(
            drag.insert
                .and_then(|p| Self::grid_insert_line_rect(layout, p))
                .map(|(x, y, lw, lh)| {
                    div()
                        .id("pdf-grid-reorder-line")
                        .debug_selector(|| "pdf-grid-reorder-line".into())
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(lw))
                        .h(px(lh))
                        .rounded_full()
                        .bg(line_color)
                        .into_any_element()
                })
                .unwrap_or_else(|| div().size_0().into_any_element()),
        );
        out
    }

    /// 插入指示线矩形（网格内容坐标）：线性位 `insert`（0..=页数）→ 目标格间隙的竖条。
    /// `insert < n` → 目标卡（原序 `insert`）**左缘**（col_gap 中线）；`insert == n` → 末卡
    /// **右缘**（col_gap 中线）。宽 3px、高 = 该卡高。行间垂直 gap 的插入位落在「下一行首」
    /// （= `row_page_start[r]`），线画在该行首卡左缘 = 插入到该行首（可视位对齐线性位）。
    ///
    /// 演算：线中心 `c`；`insert < n` = 目标卡 `x − col_gap/2`，`insert == n` = 末卡
    /// `x + w + col_gap/2`；矩形 = `(c − 1.5, 卡y, 3, 卡高)`。禁止读 `GRID_GAP`——列距由
    /// `col_gap`（渲染列间距，可能被拉伸/封顶）负责，插入线必须对应用户**看到的**列缝隙。
    fn grid_insert_line_rect(layout: &GridLayout, insert: usize) -> Option<(f32, f32, f32, f32)> {
        let n = layout.row_page_start.last().copied().unwrap_or(0);
        if layout.rows == 0 || n == 0 {
            return None;
        }
        if insert >= n {
            let (x, y, w, h) = layout.cell_rect(n - 1)?;
            let c = x + w + layout.col_gap / 2.0;
            Some((c - 1.5, y, 3.0, h.max(1.0)))
        } else {
            let (x, y, _w, h) = layout.cell_rect(insert)?;
            let c = x - layout.col_gap / 2.0;
            Some((c - 1.5, y, 3.0, h.max(1.0)))
        }
    }

    /// 网格页卡构建（[`PdfViewer::render_pdf_grid`] 的可视带内卡片）：绝对定位在 `cell_rect`
    /// 处，卡内 = 缩略图（`ImageSource::Render`）或「第 N 页 加载中…」占位底色，右下角叠
    /// 页码徽标。wave5 起叠加网格多选：
    /// - 右上角 **checkbox**（`pdf-grid-check-<i>`）：单击切换该页选中态，并 `stop_propagation`
    ///   独占（不落到卡的「本底单击」）；
    /// - **普通**左键点卡（无修饰键）= 维持 wave3：退出网格 + `scroll_to_page`（不改选中态）；
    /// - **Ctrl**+左键点卡 = 切换选中态（不退出网格）；
    /// - **Shift**+左键点卡 = 范围选中（锚点→当前闭区间加入选中集，其它保持；不退出网格）；
///
    /// 选中态视觉 = 强调色边框 + 全卡强调淡底 + 左上角「✓ 计数」角标（`pdf-grid-badge-<i>`，
    /// 测试经 `debug_bounds` 断言有无）。
    /// wave6：按下不再即时触发勾选/跳页（改记 `grid_press` 待决单击，松手未拖拽才消费——
    /// 与拖拽经位移阈值互斥）；整卡可 `on_drag` 拖拽重组。
    #[allow(clippy::too_many_arguments)]
    fn build_grid_cards(
        &self,
        cx: &mut Context<Self>,
        layout: &GridLayout,
        start: usize,
        end: usize,
        page_bg: Hsla,
        page_border: Hsla,
    ) -> Vec<gpui::AnyElement> {
        let colors = ui::theme::Theme::global(cx).colors;
        let fg: gpui::Hsla = colors.fg.into();
        let accent: gpui::Hsla = colors.accent.into();
        let border: gpui::Hsla = colors.border.into();
        let bg: gpui::Hsla = colors.bg.into();
        let selected_count = self.grid_selected_count();
        let mut cards = Vec::with_capacity(end - start + 1);
        for i in start..=end {
            let Some((x, y, w, h)) = layout.cell_rect(i) else {
                continue;
            };
            let selected = self.grid_selected(i);
            let inner: gpui::AnyElement = match self.grid_thumbs.get(&i).cloned() {
                Some(thumb) => {
                    // 有缩略图就画（当前桶 = 就绪图；旧桶 = 跨桶缩小/放大时的**桥**）：
                    // CSS 拉伸铺满卡盒。新桶图异步到达前由旧桶桥顶上，杜绝跨桶缩放整屏
                    // 占位白闪；桥与就绪图渲染同构（卡盒几何不变，命中/拖拽不依赖此分支）。
                    img(ImageSource::Render(thumb.image))
                        .w(px(w))
                        .h(px(h))
                        .into_any_element()
                }
                None => {
                    // 完全无图（无当前桶、也无旧桶桥）才画占位——网格占位计数。与单列
                    // `loading_placeholder_hits` 对称但独立（单列计数不覆盖网格卡路径）。
                    self.grid_placeholder_hits
                        .set(self.grid_placeholder_hits.get() + 1);
                    // 风暴期（快滚）占位**弱化**：只画中性底色、不画「第 N 页」文本（快滚
                    // 满屏页码文本闪烁干扰观感、高对比更刺眼）；idle 期保留既有提示语义。
                    // 计数不变（回归断言口径保持）：仅视觉降噪，不改卡盒布局/颜色来源。
                    let body = div()
                        .size_full()
                        .bg(page_bg)
                        .flex()
                        .items_center()
                        .justify_center();
                    if self.grid_storm {
                        body.into_any_element()
                    } else {
                        body.child(
                            div()
                                .text_size(px(12.))
                                .text_color(page_border)
                                .child(format!("第 {} 页", i + 1)),
                        )
                        .into_any_element()
                    }
                }
            };
            let mut card = div()
                .id(SharedString::from(format!("pdf-grid-item-{i}")))
                .debug_selector(move || format!("pdf-grid-item-{i}"))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(w))
                .h(px(h))
                .rounded(px(4.))
                .border_1()
                .border_color(if selected { accent } else { page_border })
                .overflow_hidden()
                .cursor(gpui::CursorStyle::PointingHand)
                .child(inner);
            // wave5 选中底：强调色淡底整卡覆盖（非交互层，无 handler 不拦截命中——K15）。
            if selected {
                card = card.child(div().absolute().inset_0().bg(accent.opacity(0.16)));
            }
            // wave5 checkbox：右上角勾选钮。单击只切选中态并 stop_propagation 独占，
            // 不落到卡本底（本底单击 = 退出网格）。无独立 handler 时选中视觉走卡边框/底。
            card = card.child(
                div()
                    .id(SharedString::from(format!("pdf-grid-check-{i}")))
                    .debug_selector(move || format!("pdf-grid-check-{i}"))
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .w(px(18.))
                    .h(px(18.))
                    .rounded_sm()
                    .border_1()
                    .border_color(if selected { accent } else { border })
                    .bg(if selected {
                        accent
                    } else {
                        page_bg.opacity(0.7)
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .when(selected, |this| {
                        this.child(div().text_size(px(12.)).text_color(bg).child("✓"))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                            this.grid_toggle_select(i, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
            // wave5 选中角标：左上角「✓ 计数」（选中态标记；headless 测试经 debug_bounds
            // 断言其出现/消失，是「选中视觉」的确定性证据）。
            if selected {
                card = card.child(
                    div()
                        .id(SharedString::from(format!("pdf-grid-badge-{i}")))
                        .debug_selector(move || format!("pdf-grid-badge-{i}"))
                        .absolute()
                        .top(px(6.))
                        .left(px(6.))
                        .px_1()
                        .rounded_sm()
                        .bg(accent)
                        .text_size(px(10.))
                        .text_color(bg)
                        .child(format!("✓ {selected_count}")),
                );
            }
            // 页码徽标：贴卡右下缘（半透明底，叠加在缩略图/占位之上）。
            card = card.child(
                div()
                    .absolute()
                    .bottom(px(3.))
                    .right(px(3.))
                    .px_1()
                    .rounded_sm()
                    .bg(page_bg.opacity(0.75))
                    .text_size(px(11.))
                    .text_color(fg)
                    .child(format!("{}", i + 1)),
            );
            // 卡交互（wave6 重写）：按下只记录「待决单击」（`grid_press`）——松手时若**未**
            // 进入拖拽（位移未超阈值）才按 wave5/wave3 语义消费（Ctrl 切换 / Shift 范围 /
            // 单击选中 / 双击跳页，见 `grid_click_commit`）；整卡可 `on_drag` 拖拽重组（位移
            // 超阈值即接管，拖拽中不触发勾选/跳页）。拖拽载荷只记页下标（选中组在拖拽开始快照）。
            let weak = cx.weak_entity();
            card = card
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _w, _cx| {
                        this.grid_press = Some(GridPress {
                            page: i,
                            modifiers: ev.modifiers,
                            // 点击计数随后 `grid_click_commit` 用：1 = 单击（选中）、≥2 = 双击（跳页）。
                            click_count: ev.click_count,
                        });
                    }),
                )
                .on_drag(
                    GridDragData { page: i },
                    move |_value: &GridDragData, _offset, _window, cx| {
                        if let Some(w) = weak.upgrade() {
                            w.update(cx, |this, _cx| {
                                this.grid_drag_begin(_value.page);
                            });
                        }
                        // gpui 拖拽 picture 置空（ghost/指示线由本视图渲染，避免双影）。
                        cx.new(|_| gpui::EmptyView)
                    },
                );
            cards.push(card.into_any_element());
        }
        cards
    }

    /// wave5 网格顶栏（`pdf-grid-toolbar`）：左组「全选 / 清空选择 / 退出网格」始终可用；
    /// 右组「删除 / 拆分 / 提取 / 合并」带选中计数 badge（如 `删除(3)`），无选中时置灰禁用。
    /// wave8：四个操作按钮已接线（确认框/文件选择 → `grid_apply_*` seam）。颜色全部走
    /// theme 语义（内容色 `ColorScheme` + 外壳 `ChromeColors`，禁裸色）。
    fn render_grid_toolbar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = ui::theme::Theme::global(cx);
        let colors = theme.colors;
        let chrome = theme.chrome;
        let fg: gpui::Hsla = colors.fg.into();
        let fg_disabled: gpui::Hsla = colors.fg_disabled.into();
        let hover_bg: gpui::Hsla = colors.selected_bg.into();
        let bar_bg: gpui::Hsla = chrome.bar_bg.into();
        let hairline: gpui::Hsla = chrome.hairline.into();
        let count = self.grid_selected_count();
        let has_sel = count > 0;

        div()
            .id("pdf-grid-toolbar")
            .debug_selector(|| "pdf-grid-toolbar".into())
            .h(px(GRID_TOOLBAR_H))
            .w_full()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(10.))
            .bg(bar_bg)
            .border_b_1()
            .border_color(hairline)
            // 左组：全选 / 清空选择（始终可用；清空对空集幂等）。
            .child(Self::grid_tool_button(
                "pdf-grid-tool-select-all",
                "全选".into(),
                true,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, _w: &mut Window, cx| {
                    this.grid_select_all(cx);
                })),
            ))
            .child(Self::grid_tool_button(
                "pdf-grid-tool-clear",
                "清空选择".into(),
                true,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, _w: &mut Window, cx| {
                    this.grid_clear_selection(cx);
                })),
            ))
            // 分隔 + 退出网格（= toggle 退出：回单列阅读、不定位）。
            .child(Self::grid_tool_divider(hairline))
            .child(Self::grid_tool_button(
                "pdf-grid-tool-exit",
                "退出网格".into(),
                true,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, _w: &mut Window, cx| {
                    this.set_grid_mode(false, cx);
                })),
            ))
            // 右推第二组：页操作（wave8 已接线：确认框/文件选择 → grid_apply_* seam；
            // 无选中禁用，handler 内再兜底空选中早退）。
            .child(div().flex_1())
            .child(Self::grid_tool_button(
                "pdf-grid-tool-delete",
                format!("删除({count})"),
                has_sel,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, window, cx| {
                    this.grid_delete_clicked(window, cx);
                })),
            ))
            .child(Self::grid_tool_button(
                "pdf-grid-tool-split",
                format!("拆分({count})"),
                has_sel,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, window, cx| {
                    this.grid_split_clicked(window, cx);
                })),
            ))
            .child(Self::grid_tool_button(
                "pdf-grid-tool-extract",
                format!("提取({count})"),
                has_sel,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, window, cx| {
                    this.grid_extract_clicked(window, cx);
                })),
            ))
            .child(Self::grid_tool_button(
                "pdf-grid-tool-merge",
                format!("合并({count})"),
                has_sel,
                fg,
                fg_disabled,
                hover_bg,
                cx,
                Some(Box::new(|this: &mut PdfViewer, window, cx| {
                    this.grid_merge_clicked(window, cx);
                })),
            ))
            .into_any_element()
    }

    /// 单个网格顶栏按钮：可用态挂 hover 基底 + 点击回调，禁用态只呈现置灰（不发事件、不
    /// hover）。`on_click` 携带 `&mut Window`（wave8 接线需弹原生确认框/文件框）。
    /// `debug_selector` 带可用态后缀（`-on`/`-off`），供 headless 测试断 UI 呈现。
    fn grid_tool_button(
        selector: &'static str,
        label: String,
        enabled: bool,
        fg: Hsla,
        fg_disabled: Hsla,
        hover_bg: Hsla,
        cx: &mut Context<Self>,
        on_click: Option<Box<dyn Fn(&mut Self, &mut Window, &mut Context<Self>)>>,
    ) -> gpui::AnyElement {
        let state = if enabled { "on" } else { "off" };
        div()
            .id(selector)
            .debug_selector(move || format!("{selector}-{state}"))
            .h(px(24.))
            .px_2()
            .rounded_md()
            .cursor(gpui::CursorStyle::PointingHand)
            .flex()
            .items_center()
            .gap(px(2.))
            .text_size(px(13.))
            .text_color(if enabled { fg } else { fg_disabled })
            .when(enabled, |this| this.hover(|this| this.bg(hover_bg)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    if !enabled {
                        return;
                    }
                    if let Some(f) = &on_click {
                        f(this, window, cx);
                    }
                }),
            )
            .child(label)
            .into_any_element()
    }

    /// 顶栏竖向分隔发丝线。
    fn grid_tool_divider(hairline: Hsla) -> gpui::AnyElement {
        div()
            .mx_1()
            .w(px(1.))
            .h(px(16.))
            .bg(hairline)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::tiles_layer_allowed;

    /// S 系列「同帧不混叠」门控纯函数表（R1/R2/R3）：
    /// 旧底 + 部分瓦片 → 禁止；当前密度 cover / 全新页（无底）/ 瓦片整组就位 → 允许。
    #[test]
    fn tiles_layer_allowed_only_uniform_density_or_full_group() {
        // R1 核心不变式：无当前密度 cover、有底图（旧/低密度）、瓦片**部分**到达 → 禁止绘制。
        assert!(
            !tiles_layer_allowed(false, true, false),
            "旧密度底图 + 部分瓦片：宁糊不混，禁止本帧出现瓦片层"
        );
        // R2 原子换：组齐全（全部可见格在槽）→ 允许一次全量呈现。
        assert!(
            tiles_layer_allowed(false, true, true),
            "底图旧但不混叠：整组一次换允许"
        );
        // 允许三例外：
        assert!(
            tiles_layer_allowed(true, true, false),
            "当前密度 cover 在位 → 底图与瓦片同密度，无密度差可混叠"
        );
        assert!(
            tiles_layer_allowed(false, false, false),
            "全无底图 → 全新页保持逐片流式（R3）"
        );
        assert!(tiles_layer_allowed(true, false, false), "cover 在位即允许");
        assert!(tiles_layer_allowed(true, true, true), "全满足当然允许");
    }
}
