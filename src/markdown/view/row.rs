//! 渲染单个渲染行（固定高度、不换行、超出裁剪；超宽由外层横向滚动容器处理）。
//!
//! `wrap = true` 时为**自动换行模式**：文本行高度自适应、自然折行（用于 EPUB 阅读）；
//! `wrap = false`（默认）为单行 + 横向滚动模式。

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    div, fill, img, point, px, App, Bounds, Corners, Element, ElementId, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, Hsla, InspectorElementId, InteractiveElement, IntoElement,
    LayoutId, ObjectFit, ParentElement, Pixels, ScrollHandle, StatefulInteractiveElement,
    StrikethroughStyle, Styled, StyledImage, StyledText, TextStyle, WrappedLineLayout, Window,
};
use crate::ui::scrollbar::{Scrollbar, ScrollbarMode};

use crate::config::markdown::{
    CONTENT_EDGE_PAD_X_PX, IMAGE_MAX_W, TABLE_SCROLLBAR_RESERVE_PX,
};
use crate::markdown::doc::{DocPayload, DocRow};
use crate::markdown::ir::IrKind;
use crate::markdown::layout::{shape_docrow_shard, shape_docrow_shard_with_image, Shard};
use crate::markdown::parse::{Row, RowAlign, RunKind};
use crate::markdown::view::style::StyleParams;
use crate::markdown::view::table::render_table_geometry;
use crate::markdown::wysiwyg::WyTheme;
use crate::syntax::{github_theme_cached, resolve_line_styles_with, CodeLineRuns};

use super::{NoteCtx, NoteHit, TextCtx, TextHit};

/// 预览渲染行：统一行 [`DocRow`]（`kind/text/runs/payload/byte_map`，与
/// `Doc::rows_in_block_window` 逐位一致，WYSIWYG 同源）+ 预览侧**展示附加**——
/// [`DocRow`] 为 WYSIWYG 编辑侧语义，不携带纯展示字段：列表前缀 / 行对齐（EPUB
/// `%%ALIGN:X%%`）/ 引注标记（`[^n]`/`[!n]`）/ 图片固有尺寸与内存图。行物化时由
/// `parse::Row`（解析直出）装配（M5-B5 P3-1a：渲染行源切共享 `DocRow`）。
///
/// 行源不再直接是 `parse::Row`：渲染/命中只读它 + [`DocRow`]，为 P3-1b 删 `row.rs`
/// /`TextTrack` 的平替清路（`PreviewRow` 即「共享 `DocRow` + 展示附加」的预览行型）。
#[derive(Debug, Clone)]
pub(crate) struct PreviewRow {
    /// 统一行（共享装配核心产出；`byte_map`/`lines`/`table_rows` 语义与 `Doc` 一致）。
    pub docrow: DocRow,
    /// 列表项前缀（`•` / `-` / `1.`…；补在显示文本前；[`Row`] 同字段）。
    pub prefix: Option<gpui::SharedString>,
    /// 行对齐（EPUB 排版指令 `%%ALIGN:X%%`；None = 默认左对齐；[`Row`] 同字段）。
    pub align: Option<RowAlign>,
    /// 文内引注标记 `[^n]`：`(字节区间, 注记号)`，区间相对 `docrow.text`（[`Row`] 同字段）。
    pub notes: Vec<(Range<usize>, usize)>,
    /// 注释**定义**标记 `[!n]`：同上（[`Row`] 同字段）。
    pub note_defs: Vec<(Range<usize>, usize)>,
    /// 图片固有宽（px）；`docrow.payload` = 源（[`Row`] 载荷字段；DocRow 不携带）。
    pub img_w: Option<u32>,
    /// 图片固有高（px）。
    pub img_h: Option<u32>,
    /// 内存图数据（EPUB 不落盘场景；`ImageSource::Image` 解码用）。
    pub img_data: Option<std::sync::Arc<gpui::Image>>,
}

impl PreviewRow {
    /// 由共享装配出的 [`DocRow`] + 原始解析行 [`Row`] 的展示附加拼装（`Row` 仅此一处
    /// 被消费；图片源/尺寸须在调用前经 `resolve_image_rows` 解析）。
    pub(crate) fn from_parts(docrow: DocRow, r: &Row) -> Self {
        let (img_w, img_h, img_data) = match r.image() {
            Some(i) => (i.img_w, i.img_h, i.img_data.cloned()),
            None => (None, None, None),
        };
        Self {
            docrow,
            prefix: r.prefix.clone(),
            align: r.align,
            notes: r.notes.clone(),
            note_defs: r.note_defs.clone(),
            img_w,
            img_h,
            img_data,
        }
    }

    /// 行语义（IR 金标准）。
    pub(crate) fn kind(&self) -> IrKind {
        self.docrow.kind
    }

    /// 代码块的所有源码行（非代码行返回空切片）——从 [`DocPayload::Code`] 取。
    pub(crate) fn code_lines(&self) -> &[gpui::SharedString] {
        match &self.docrow.payload {
            DocPayload::Code { code_lines, .. } => code_lines,
            _ => &[],
        }
    }

    /// 代码块的语言标记（围栏首行信息串；无标记/非代码行返回 `None`）。
    pub(crate) fn code_language(&self) -> Option<&str> {
        match &self.docrow.payload {
            DocPayload::Code { language, .. } => language.as_deref(),
            _ => None,
        }
    }

    /// 图片源（仅 [`IrKind::Image`] 行）——从 [`DocPayload::Image`] 取。
    pub(crate) fn image_src(&self) -> &str {
        match &self.docrow.payload {
            DocPayload::Image { src } => src.as_ref(),
            _ => "",
        }
    }
}

/// 行内代码（反引号 `` ` `` 包裹）：等宽字体 + 主题配色文字；圆角底色由
/// `render_text_shard_rows` 的 [`ShardStrip`] 在 paint 阶段按代码段的像素几何自绘
/// （方角 highlight 背景无法圆角，且 gpui 文本 run 背景只有平面矩形）。
/// 配色单一源 = [`StyleParams::code_inline_fg`] / [`StyleParams::code_inline_bg`]
/// （不再在本文件写死色值），预览与 WYSIWYG 两区取同一来源。
fn inline_code_highlight(p: &StyleParams) -> HighlightStyle {
    HighlightStyle {
        color: Some(p.code_inline_fg),
        ..Default::default()
    }
}

/// 把背景区间向左右扩展吞掉紧邻空白：留给圆角背景的左右视觉间距。
/// （高亮区间即"代码文本 + 两侧各一个空格"，等宽 overrides 只覆盖代码本体，
/// 空格保持正文字宽 → 圆角灰底略宽于字形，左右有呼吸空间。）
/// 预览与 WYSIWYG 的行内代码 pill 共用同一扩展规则（单一行为源）。
pub(crate) fn expand_bg_over_whitespace(text: &str, range: &Range<usize>) -> Range<usize> {
    let mut start = range.start;
    let mut end = range.end;
    if start > 0 {
        let prev = text[..start].chars().next_back();
        if let Some(c) = prev
            && c.is_whitespace() {
                start -= c.len_utf8();
            }
    }
    if end < text.len() {
        let next = text[end..].chars().next();
        if let Some(c) = next
            && c.is_whitespace() {
                end += c.len_utf8();
            }
    }
    start..end
}

/// 渲染一行代码（等宽、不换行、浅灰底）。`styles` = 该行已解析的颜色 run
/// （`Some` 时用 [`StyledText`] 逐段着色；`None` 按纯文本渲染）。
fn code_band(
    t: &gpui::SharedString,
    styles: Option<&[(Range<usize>, HighlightStyle)]>,
    p: &StyleParams,
) -> gpui::AnyElement {
    // 有颜色 run 且有内容时用 StyledText（无命中段时 resolve 仍产出覆盖整行的
    // 默认 run → 走 Some 分支，颜色字段为 None 沿用外层 text_color）。
    let child: gpui::AnyElement = match styles.filter(|s| !s.is_empty()) {
        Some(styles) => StyledText::new(t.clone())
            .with_highlights(styles.to_vec())
            .into_any_element(),
        None => t.clone().into_any_element(),
    };
    div()
        .debug_selector(|| "md-code-band".into())
        .h(p.code_line)
        .w_full()
        .pl_3()
        .pr_3()
        .whitespace_nowrap()
        .font_family(p.mono.clone())
        .text_size(p.code_size)
        .text_color(p.fg)
        .bg(p.code_bg)
        .child(child)
        .into_any_element()
}

/// 代码块上下各补一行空白带（带同一背景色）：让代码块背景在首行文字上方 /
/// 末行文字下方各多出一整行的呼吸空间，视觉上从紧贴文字变成留出前/后一行空白。
/// `selector` 区分顶/底空白带，便于测试定位；顶带圆上角、底带圆下角使整块边缘变圆。
fn code_blank(selector: &'static str, p: &StyleParams) -> gpui::Div {
    div()
        .debug_selector(move || selector.into())
        .h(p.code_line)
        .w_full()
        .bg(p.code_bg)
}

/// 代码块＝首行空白带 + 代码行 + 末行空白带（背景连续铺平）。
/// 首/末空白带分别圆上角 / 下角（lg=8px），让代码块整体呈圆角卡片。
/// `runs`：整块每行的语义名高亮（可选；无语言/未知语法时按纯文本渲染）。颜色随 `dark`。
fn code_block_bands(
    lines: &[gpui::SharedString],
    runs: Option<&CodeLineRuns>,
    dark: bool,
    p: &StyleParams,
) -> Vec<gpui::AnyElement> {
    // 复用同一次明/暗 GitHub 色板解析各行的颜色（静态缓存，避免每帧/每块重建整套主题）。
    let resolver = github_theme_cached(dark);
    let mut bands: Vec<gpui::AnyElement> = Vec::with_capacity(lines.len() + 2);
    bands.push(
        code_blank("md-code-blank-top", p)
            .rounded_t_lg()
            .into_any_element(),
    );
    for (i, line) in lines.iter().enumerate() {
        let styles = runs
            .and_then(|all| all.get(i))
            .map(|line_runs| resolve_line_styles_with(line_runs, line.len(), resolver));
        bands.push(code_band(line, styles.as_deref(), p));
    }
    bands.push(
        code_blank("md-code-blank-bottom", p)
            .rounded_b_lg()
            .into_any_element(),
    );
    bands
}

/// 跳转加亮「目标 `[n]` 标记」的背景色（与搜索跳转同款暖琥珀，浅/暗各一版）。
fn marker_flash_color(dark: bool) -> Hsla {
    if dark {
        gpui::rgba(0xf59e0b47).into()
    } else {
        gpui::rgba(0xfef08a8c).into()
    }
}

/// 把（可能重叠的）半开区间高亮合并成互不重叠的段：覆盖同一段的多个样式按字段
/// 合并（后出现的非 None 字段覆盖前值）。gpui 的 `compute_runs`（
/// `StyledText::with_default_highlights` / `with_highlights`）假定区间不重叠，重叠
/// 会产出长度错误的 run 直接 panic——调用侧必须在喂入前合并。
/// **必须合并全部样式字段**（color/背景/`font_weight`/`font_style` 等）：只合并
/// 颜色会把与选区/引注色/搜索重叠的粗体、斜体、删除线段的字形样式丢掉（加粗失效）。
fn merge_highlights(
    highlights: Vec<(Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if highlights.len() <= 1 {
        return highlights;
    }
    // 扫掠事件：(位置, 进入=+1/离开=-1, 区间序号)；同位置先「进入」后「离开」。
    let mut events: Vec<(usize, i32, usize)> = Vec::with_capacity(highlights.len() * 2);
    for (i, (r, _)) in highlights.iter().enumerate() {
        events.push((r.start, 1, i));
        events.push((r.end, -1, i));
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut active: Vec<usize> = Vec::new();
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut prev = 0usize;
    for (pos, delta, idx) in events {
        if !active.is_empty() && pos > prev {
            let mut s = HighlightStyle::default();
            for &a in &active {
                let h = &highlights[a].1;
                if h.color.is_some() {
                    s.color = h.color;
                }
                if h.background_color.is_some() {
                    s.background_color = h.background_color;
                }
                if h.font_weight.is_some() {
                    s.font_weight = h.font_weight;
                }
                if h.font_style.is_some() {
                    s.font_style = h.font_style;
                }
                if h.strikethrough.is_some() {
                    s.strikethrough = h.strikethrough;
                }
                if h.underline.is_some() {
                    s.underline = h.underline;
                }
            }
            out.push((prev..pos, s));
        }
        if delta > 0 {
            active.push(idx);
        } else if let Some(p) = active.iter().position(|&a| a == idx) {
            active.remove(p);
        }
        prev = pos;
    }
    out
}

/// 把字节区间边界逐一吸收到最近的**字符边界**（向前进位），防止多字节字符
/// （CJK/emoji）被切半——任何非对齐偏移都会在 shaping 时崩（direct_write
/// panic on non-char-boundary）。空/越界安全；区间可能缩小为 0 长（返回同边界）。
///
/// M1.1-P5 起实现收敛到共享基元 [`crate::util::char_align_byte`]（与 WYSIWYG
/// `shape::runs_to_text_runs`、源码编辑器 `render::char_align_highlights` 三处合一），
/// 此处保留 `pub(crate)` 路径重导出，既有调用点与本模块测试不变。
pub(crate) use crate::util::char_align_range;

// =============================================================================
// M5-B5 P3-1b-1/2：预览渲染几何统一切共享 `shape_docrow_shard` 输出。
// 文本类行的排布不再由 gpui 元素树隐式排版，而是把 layout 的 `Shard` 序列逐一落元素：
// 每个 strip 的 h/x_off 直接取 `shard.h/x_off`；折行点在 `render_text_shard_rows` 里以
// 与 shard **同宽同 `ts.shape_text`** 的 StyledText 复现（`avail − gutter` 同一语义）。
// `量程（P3-0 = Σ shard）` 与渲染同真源（R2）：两者吃同一 `avail`（`preview_text_avail`）。
//
// **P3-1b-2 已删 legacy 元素树/`TextTrack`/`render_row` 旧路径**：文本/代码/空行/
// 分隔线一律 `render_text_shard_rows`/`render_code_shard_rows`（`ShardStrip` 分片级
// 命中/注记/行内代码底）；**P3-1c-2 表格/图片也切 shard 承载内容**——旧
// `render_table_row`/`render_image_row` 已删，改由 shard 的 `ShardTable`/`ShardImage`
// 几何直接绘制（列宽/单元格/盒高等与量程同源，R2）。
// =============================================================================

/// 把注记标记的窗口矩形写入共享命中表（[`ShardStrip`] 分片级登记）。
/// 分片键 = `(row_ix, hit_sub)`：按 shard 分片各自登记，同行多片互不清除
/// （跨折行的标记每片记一段，悬浮/跳转都能命中）。
#[allow(clippy::too_many_arguments)] // 纯函数矩形登记：输入互为正交
fn record_note_hit_rects(
    hits: &mut Vec<NoteHit>,
    row_ix: usize,
    hit_sub: usize,
    origin: gpui::Point<Pixels>,
    line_h: Pixels,
    layout: &WrappedLineLayout,
    notes: &[(Range<usize>, usize)],
    note_defs: &[(Range<usize>, usize)],
) {
    hits.retain(|h| h.row_ix != row_ix || h.hit_sub != hit_sub);
    let mut push_hit = |range: &Range<usize>, n: usize, is_def: bool| {
        let (Some(p0), Some(p1)) = (
            layout.position_for_index(range.start, line_h),
            layout.position_for_index(range.end, line_h),
        ) else {
            return;
        };
        let x0 = p0.x.as_f32().min(p1.x.as_f32());
        let y0 = p0.y.as_f32().min(p1.y.as_f32());
        let x1 = p0.x.as_f32().max(p1.x.as_f32());
        let y1 = (p1.y.as_f32() + line_h.as_f32()).max(p0.y.as_f32() + line_h.as_f32());
        hits.push(NoteHit {
            row_ix,
            n,
            is_def,
            hit_sub,
            bounds: Bounds {
                origin: point(px(origin.x.as_f32() + x0 - 1.0), px(origin.y.as_f32() + y0 - 1.0)),
                size: gpui::size(px(x1 - x0 + 2.0), px(y1 - y0 + 2.0)),
            },
        });
    };
    for (range, n) in notes {
        push_hit(range, *n, false);
    }
    for (range, n) in note_defs {
        push_hit(range, *n, true);
    }
}

/// 行内代码圆角灰底（`TextTrack` 与 `ShardStrip` 共用）：把 `code_bg`（已扩展空白）
/// 区间按折行切成多段矩形并 paint，与 `st` 的同一 layout 对齐。空区间不做事。
fn paint_code_ranges_over(
    st: &StyledText,
    code_bg: &[Range<usize>],
    bounds: Bounds<Pixels>,
    color: Hsla,
    radius: Pixels,
    window: &mut Window,
) {
    if code_bg.is_empty() {
        return;
    }
    let layout = st.layout();
    let line_height = layout.line_height();
    let layouts = layout.line_layouts();
    let Some(single) = layouts.first() else {
        return;
    };
    let line_h = line_height.as_f32();
    let text_origin = bounds.origin;
    for range in code_bg {
        let Some(p0) = single.position_for_index(range.start, line_height) else {
            continue;
        };
        let Some(p1) = single.position_for_index(range.end, line_height) else {
            continue;
        };
        let y0 = (p0.y.as_f32() / line_h).floor() as usize;
        let y1 = (p1.y.as_f32() / line_h).floor() as usize;
        if y0 == y1 {
            let b = Bounds {
                origin: point(
                    px(text_origin.x.as_f32() + p0.x.as_f32()),
                    px(text_origin.y.as_f32() + p0.y.as_f32()),
                ),
                size: gpui::size(px((p1.x.as_f32() - p0.x.as_f32()).max(0.0)), px(line_h)),
            };
            window.paint_quad(fill(b, color).corner_radii(Corners::all(radius)));
        } else {
            let line_origin_x = text_origin.x.as_f32();
            let origin_y = text_origin.y.as_f32();
            for real_ri in y0..=y1 {
                let (rx0, rx1) = if real_ri == y0 {
                    (p0.x.as_f32(), single.width().as_f32())
                } else if real_ri == y1 {
                    (0.0, p1.x.as_f32())
                } else {
                    (0.0, single.width().as_f32())
                };
                let b = Bounds {
                    origin: point(px(line_origin_x + rx0), px(origin_y + real_ri as f32 * line_h)),
                    size: gpui::size(px((rx1 - rx0).max(0.0)), px(line_h)),
                };
                window.paint_quad(fill(b, color).corner_radii(Corners::all(radius)));
            }
        }
    }
}

/// unified 单个 shard 条带的文本元素：一条**已按 shard 折行语义预整形的文本**（一行硬行、
/// 内部可多可视行）。paint 期完成 legacy `TextTrack` 的三件副作用、但语义是**分片级**：
/// - 文本命中（`TextCtx`）→ `TextHit.byte_offset`（= `shard.byte_map.start`，字节抬到整行）；
/// - 注记命中（`NoteCtx`）→ 区间 clip 到本条带、`hit_sub` 区分同行多片；
/// - 行内代码圆角底（`code_bg` = `shard.code_ranges`）。
///
/// **P3-1b-2 删除点**：unified 下 `TextTrack` 不再必要（选中/悬浮/行内代码底全由本元素
/// + `render_text_shard_rows` 承接），届时删 `TextTrack` 并保留本元素。
struct ShardStrip {
    /// 文本载体（一条 shard 文本；命中用其 `layout().index_for_position`）。
    st: StyledText,
    /// 行内代码（反引号）区间（已扩展空白，相对本条带文本），画圆角底用。
    code_bg: Vec<Range<usize>>,
    /// 代码底颜色 / 圆角半径。
    code_bg_color: Hsla,
    code_radius: Pixels,
    /// 显示文本（整行 docrow.text 的片段；命中后取文本长度钳制字节偏移）。
    text: gpui::SharedString,
    /// 本条带文本在整行 docrow.text 中的起始字节偏移（`shard.byte_map.start`）。
    byte_offset: usize,
    /// 全局块下标（共享命中表的行标识；同行多片以 `hit_sub` 区分）。
    row_ix: usize,
    /// 行内子条带序号（`shard.line_ix`）。
    hit_sub: usize,
    /// 注记区间（已 clip 到本条带、相对本条带文本）。
    notes: Vec<(Range<usize>, usize)>,
    note_defs: Vec<(Range<usize>, usize)>,
    /// 注记共享表（存在标记时记录）。
    note_ctx: Option<Rc<NoteCtx>>,
    /// 文本命中共享表（恒有）。
    text_ctx: Rc<TextCtx>,
}

impl IntoElement for ShardStrip {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ShardStrip {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (lid, ()) = self.st.request_layout(id, inspector_id, window, cx);
        (lid, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.st
            .prepaint(id, inspector_id, bounds, request_layout, window, cx);
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint_state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !crate::markdown::wysiwyg::warm_emoji_is_done() {
            crate::markdown::wysiwyg::warm_emoji_rasterizer(window, "preview-shard");
        }
        // 1) 记录文本命中（本片一条：只清本片旧条目，同行其它片保留）。
        let layout = self.st.layout();
        {
            let mut hits = self.text_ctx.hits.borrow_mut();
            hits.retain(|h| h.row_ix != self.row_ix || h.byte_offset != self.byte_offset);
            hits.push(TextHit {
                row_ix: self.row_ix,
                text: self.text.clone(),
                layout: layout.clone(),
                origin: bounds.origin,
                byte_offset: self.byte_offset,
            });
        }
        // 2) 记录注记命中（分片级矩形；`hit_sub` 区分同行多片）。
        let origin = bounds.origin;
        let line_h = layout.line_height();
        if let (Some(note_ctx), Some(single)) =
            (self.note_ctx.as_ref(), layout.line_layouts().first())
        {
            record_note_hit_rects(
                &mut note_ctx.hits.borrow_mut(),
                self.row_ix,
                self.hit_sub,
                origin,
                line_h,
                single,
                &self.notes,
                &self.note_defs,
            );
        }
        // 3) 行内代码圆角灰底（与 st 同一 layout，先画底）。
        paint_code_ranges_over(&self.st, &self.code_bg, bounds, self.code_bg_color, self.code_radius, window);
        // 4) 委托绘制文字。
        self.st.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint_state,
            window,
            cx,
        );
    }
}

/// 代码块外框（首/末空白带 + 代码行 + 块内横向滚动容器）。几何 = `block_h`（px）整块
/// 固定高；`lines` 为代码源码行（首/末空白带由 [`code_block_bands`] 自行补齐）。
/// legacy 与 unified 共用同一外壳——差异仅在行源/块高来源（legacy：`code_lines()` +
/// 公式；unified：shard 行文本 + `Σ shard.h`），几何完全一致。
fn code_block_element(
    lines: &[gpui::SharedString],
    block_h: f32,
    code_handle: Option<ScrollHandle>,
    code_runs: Option<&CodeLineRuns>,
    p: &Rc<StyleParams>,
    cx: &mut App,
) -> gpui::AnyElement {
    let dark = crate::ui::theme::Theme::global(cx).is_dark();
    let p_ = p.clone();
    let runs = code_runs;
    let h = code_handle.unwrap_or_default();
    let column = div()
        .debug_selector(|| "md-code-row".into())
        .flex()
        .flex_col()
        .flex_shrink_0()
        .min_w_full()
        .children(code_block_bands(lines, runs, dark, &p_));
    div()
        .debug_selector(|| "md-code-hscroll".into())
        .h(px(block_h))
        .w_full()
        .flex_shrink_0()
        .px(px(CONTENT_EDGE_PAD_X_PX))
        .child(
            div()
                .debug_selector(|| "md-code-hscroll-inner".into())
                .w_full()
                .h_full()
                .relative()
                .overflow_hidden()
                .child(
                    div()
                        .id("md-code-scroll")
                        .w_full()
                        .h_full()
                        .flex()
                        .flex_row()
                        .overflow_x_scroll()
                        .restrict_scroll_to_axis()
                        .track_scroll(&h)
                        .child(column),
                )
                .child(
                    div()
                        .debug_selector(|| "md-code-scrollbar".into())
                        .absolute()
                        .inset_0()
                        .child(
                            Scrollbar::horizontal(&h)
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                ),
        )
        .into_any_element()
}

/// **预览统一入口**：渲染几何切共享 `shape_docrow_shard` 输出的 [`Shard`]。
///
/// - 文本类行（Paragraph/Heading/Quote/Bullet/Blank/Rule）：每个 shard 按其
///   `h/x_off/lh/runs/code_ranges` 落一个 strip（折行点、行高、代码圆角底、空白/分隔
///   行高全部与 shard 一致——渲染与量程同真源，R2 落地）。
/// - 代码：每个源码行 = 一个 shard（nowrap），首/末空白带 + 块高 `Σ shard.h`。
/// - **表格/图片（P3-1c-2 起切 shard 承载内容）**：`shape_table_shard` 的
///   [`ShardTable`] / `shape_image_shards` 的 [`ShardImage`] 携带真实绘制几何
///   （列宽/单元格高/表头数据 / 盒宽/盒高/源）——[`render_table_shard_row`] /
///   [`render_image_shard_row`] 直接按 shard 几何画，旧 `render_table_row` /
///   `render_image_row` 内容路径已删（量程与渲染同源，R2）。
/// - `align`（EPUB `%%ALIGN:X%%` 排版）由 `render_text_shard_rows` 应用（P3-1b-2 起
///   unified 默认也保留对齐语义）。
#[allow(clippy::too_many_arguments)] // 统一入口：行渲染上下文 + 共享整形上下文
pub(crate) fn render_row_shard(
    row: &PreviewRow,
    ctx: &RowRenderCtx,
    cx: &mut App,
    ts: &gpui::WindowTextSystem,
    theme: &WyTheme,
    avail: f32,
    max_w: f32,
) -> gpui::AnyElement {
    let RowRenderCtx { p, table_handle, .. } = ctx;
    // 表格/图片：内容在 shard 几何里（P3-1c-2 收口），图片高度喂真实固尺寸使量程同源。
    if let IrKind::Table = row.kind() {
        let shards = shape_docrow_shard_with_image(ts, &row.docrow, theme, avail, max_w, None);
        return render_table_shard_row(ctx, &shards, table_handle.clone());
    }
    if let IrKind::Image = row.kind() {
        let shards = shape_docrow_shard_with_image(
            ts,
            &row.docrow,
            theme,
            avail,
            max_w,
            preview_row_img_size(row),
        );
        return render_image_shard_row(row, p, &shards);
    }
    let shards = shape_docrow_shard(ts, &row.docrow, theme, avail, max_w);
    if let IrKind::Code = row.kind() {
        return render_code_shard_rows(ctx, cx, &shards);
    }
    render_text_shard_rows(row, ctx, cx, &shards)
}

/// unified 代码块：shard 行文本即源码行（首/末空白带文本为空，过滤掉）；块高 = Σ shard.h。
fn render_code_shard_rows(ctx: &RowRenderCtx, cx: &mut App, shards: &[Shard]) -> gpui::AnyElement {
    let lines: Vec<gpui::SharedString> = shards
        .iter()
        .filter(|s| !s.text.is_empty())
        .map(|s| s.text.clone())
        .collect();
    let block_h: f32 = shards.iter().map(|s| s.h).sum();
    code_block_element(&lines, block_h, ctx.code_handle.clone(), ctx.code_hl.as_deref(), ctx.p, cx)
}

/// unified 文本类行（除分隔线/空行外每条 = 一个 shard strip）。每个 strip 高 = `shard.h`、
/// 文本左缘 = 行内边距 + `shard.x_off`、行高 = `shard.lh`，折行以与 shard **同宽**
/// （`avail − 本行 gutter`）的 StyledText 复现 → 折行点/行高与 shard 逐位一致。
///
/// 展示附加照常叠加：行内样式/搜索/选区/注记色/跳转加亮按字节 clip 到本 shard；列表项
/// 前缀以 `shard.x_off` 宽的固定槽画在首片左侧（`shard` 只载正文，前缀属展示层）。
fn render_text_shard_rows(
    row: &PreviewRow,
    ctx: &RowRenderCtx,
    cx: &mut App,
    shards: &[Shard],
) -> gpui::AnyElement {
    let RowRenderCtx {
        p,
        epub,
        search,
        selection,
        marker_hl,
        row_ix,
        text_ctx,
        note_ctx,
        ..
    } = ctx;
    let p: &Rc<StyleParams> = p;
    let epub: bool = *epub;
    let search: Option<(Range<usize>, Hsla)> = search.clone();
    let selection: Option<(Range<usize>, Hsla)> = selection.clone();
    let text_ctx: &Rc<TextCtx> = text_ctx;
    let note_ctx: &Rc<NoteCtx> = note_ctx;
    let row_ix: usize = *row_ix;
    let kind = row.kind();
    // EPUB `%%ALIGN:X%%` 排版（P3-1b-2 起 unified 默认，保留旧路径对齐语义）：
    // 标题在 EPUB 阅读模式下默认居中；其余行按显式 align（Center/Right）。
    // 列表项不打对齐（legacy 同）。None/Left 走默认左对齐（无额外 flex 壳）。
    let h_align: Option<RowAlign> = match kind {
        IrKind::Heading(_) => row.align.or(if epub { Some(RowAlign::Center) } else { None }),
        IrKind::Bullet => None,
        _ => row.align,
    };
    let dark = crate::ui::theme::Theme::global(cx).is_dark();
    let marker_hl = marker_hl.map(|(n, is_def)| (n, is_def, marker_flash_color(dark)));
    // 行级基础样式（与 legacy 各 render_wrapped_* 同源）。
    let (fs, color, weight) = match kind {
        IrKind::Heading(n) => {
            let (fs, _) = p.heading_metrics(n);
            (fs, p.fg, gpui::FontWeight::BOLD)
        }
        IrKind::Quote => (p.para_size, p.muted_fg, gpui::FontWeight::default()),
        _ => (p.para_size, p.fg, gpui::FontWeight::default()),
    };
    let bullet = kind == IrKind::Bullet;
    let link_color: Hsla = p.accent;
    // 显式整块高 = Σ shard.h：列表以 MinContent 高度测量条目，flex 列若只靠子条带
    // 的 `.h()`（子项 min-content 会塌缩到首行文本高）会被量成首行高 → 显式钉死
    // 条目高，保证 `bounds_for_item`（量程的构成）== shard 总高（R2）。
    let sum: f32 = shards.iter().map(|s| s.h).sum();
    let mut col = div()
        .h(px(sum))
        .w_full()
        .flex_shrink_0()
        .px(px(CONTENT_EDGE_PAD_X_PX))
        .flex()
        .flex_col();

    for (k, sh) in shards.iter().enumerate() {
        // 分隔线：shard 几何 = 1 条 para_line 高条带；画 1px 分隔线（display 层，非几何）。
        if kind == IrKind::Rule {
            let sel = format!("md-shard-{row_ix}-{k}");
            col = col.child(
                div()
                    .debug_selector(move || sel)
                    .h(px(sh.h))
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .child(div().h(px(1.)).w_full().bg(p.divider).rounded_full()),
            );
            continue;
        }
        // 空条带（空行等）：仅高度 = shard.h。
        if sh.text.is_empty() {
            let sel = format!("md-shard-{row_ix}-{k}");
            col = col.child(
                div().debug_selector(move || sel).h(px(sh.h)).w_full().flex_shrink_0(),
            );
            continue;
        }
        // —— 有文本：按 shard 区间裁剪行级附加 → 构造本片 StyledText ——
        let seg = &sh.byte_map;
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        let mut mono: Vec<(Range<usize>, gpui::SharedString)> = Vec::new();
        for (range, rk) in &sh.runs {
            match rk {
                RunKind::InlineCode => {
                    highlights.push((range.clone(), inline_code_highlight(p)));
                    mono.push((range.clone(), p.mono.clone()));
                }
                RunKind::Bold => highlights.push((
                    range.clone(),
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    },
                )),
                RunKind::Italic => highlights.push((
                    range.clone(),
                    HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    },
                )),
                RunKind::Strikethrough => highlights.push((
                    range.clone(),
                    HighlightStyle {
                        strikethrough: Some(StrikethroughStyle::default()),
                        ..Default::default()
                    },
                )),
                _ => {}
            }
        }
        // 注记色 + 跳转加亮 + 搜索 + 选区：整行字节区间 clip 到本 shard 再映射到条带内。
        let clip_marker = |hits: &mut Vec<(Range<usize>, HighlightStyle)>,
                           range: &Range<usize>,
                           style: HighlightStyle| {
            let s = range.start.max(seg.start);
            let e = range.end.min(seg.end);
            if s >= e {
                return;
            }
            let a = crate::util::char_align_byte(sh.text.as_ref(), s - seg.start);
            let b = crate::util::char_align_byte(sh.text.as_ref(), e - seg.start);
            if b > a {
                hits.push((a..b, style));
            }
        };
        for (range, _) in row.notes.iter().chain(row.note_defs.iter()) {
            clip_marker(
                &mut highlights,
                range,
                HighlightStyle {
                    color: Some(link_color),
                    ..Default::default()
                },
            );
        }
        if let Some((hn, is_def, hcolor)) = marker_hl {
            let src = if is_def { &row.note_defs } else { &row.notes };
            for (range, _) in src.iter().filter(|(_, n)| *n == hn) {
                clip_marker(
                    &mut highlights,
                    range,
                    HighlightStyle {
                        background_color: Some(hcolor),
                        ..Default::default()
                    },
                );
            }
        }
        if let Some((range, c)) = &search {
            clip_marker(
                &mut highlights,
                range,
                HighlightStyle {
                    background_color: Some(*c),
                    ..Default::default()
                },
            );
        }
        if let Some((range, c)) = &selection {
            clip_marker(
                &mut highlights,
                range,
                HighlightStyle {
                    background_color: Some(*c),
                    ..Default::default()
                },
            );
        }
        let merged = merge_highlights(highlights);
        // 用 `with_highlights`（继承条带 div 的 text_size/color/line_height/字体）而非
        // `with_default_highlights`：后者把「空 font_family 的 base」烙进 runs，measure
        // 阶段字体宽度与 shard 的 `theme.font` 不一致 → 折行漂移（R2）。legacy 同款。
        let mut st = if !sh.runs.is_empty() || search.is_some() {
            StyledText::new(sh.text.clone()).with_highlights(merged)
        } else {
            let base = TextStyle {
                font_size: fs.into(),
                color,
                font_weight: weight,
                line_height: gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(px(sh.lh))),
                ..TextStyle::default()
            };
            StyledText::new(sh.text.clone()).with_default_highlights(&base, merged)
        };
        if !mono.is_empty() {
            st = st.with_font_family_overrides(mono);
        }
        // clip 到本条带的注记区间（相对本条带文本）。
        let clip_notes = |ranges: &[(Range<usize>, usize)]| {
            ranges
                .iter()
                .filter_map(|(rg, n)| {
                    let s = rg.start.max(seg.start);
                    let e = rg.end.min(seg.end);
                    (s < e).then(|| (s - seg.start..e - seg.start, *n))
                })
                .collect::<Vec<_>>()
        };
        let notes = clip_notes(&row.notes);
        let note_defs = clip_notes(&row.note_defs);
        let has_notes = !notes.is_empty() || !note_defs.is_empty();
        let strip: gpui::AnyElement = ShardStrip {
            st,
            code_bg: sh.code_ranges.clone(),
            code_bg_color: p.code_inline_bg,
            code_radius: px(5.),
            text: sh.text.clone(),
            byte_offset: seg.start,
            row_ix,
            hit_sub: k,
            notes,
            note_defs,
            note_ctx: has_notes.then(|| note_ctx.clone()),
            text_ctx: text_ctx.clone(),
        }
        .into_any_element();
        // 条带外壳：显式 h = shard.h、pl = shard.x_off（列表首片 = 前缀槽 + 文本）。
        // EPUB 对齐（Center/Right）：flex 行 + justify，把文本块居中/靠右（整行宽内，
        // 与 legacy `justify_center/end` 同语义；折满的整行不受影响，短行/末行对齐）。
        let sel = format!("md-shard-{row_ix}-{k}");
        let base_div = div()
            .debug_selector(move || sel)
            .h(px(sh.h))
            .w_full()
            .flex_shrink_0()
            .whitespace_normal()
            .text_size(fs)
            .text_color(color)
            .line_height(px(sh.lh));
        let wrapper = if bullet && k == 0 {
            base_div.flex().flex_row().child(
                div()
                    .w(px(sh.x_off))
                    .flex_shrink_0()
                    .text_size(fs)
                    .text_color(color)
                    .child(row.prefix.clone().unwrap_or_default()),
            ).child(div().flex_1().child(strip))
        } else if matches!(h_align, Some(RowAlign::Center) | Some(RowAlign::Right)) {
            let mut d = base_div.flex().flex_row().pl(px(sh.x_off));
            match h_align {
                Some(RowAlign::Center) => d = d.justify_center(),
                Some(RowAlign::Right) => d = d.justify_end(),
                _ => unreachable!("上面已过滤 Left/None"),
            }
            d.child(strip)
        } else {
            base_div.pl(px(sh.x_off)).child(strip)
        };
        col = col.child(wrapper);
    }
    col.into_any_element()
}

/// 渲染单个行。`code_handle` / `table_handle` 为各自稳定的横向滚动句柄
/// （None 时自动创建），供滚动条拖动使用。
/// `code_hl`：该代码块的语法高亮缓存（每行语义名 run）；None = 无高亮（纯文本）。
/// `search`：搜索跳转高亮 (命中文本字节区间, 背景色)，只给命中文本着色（不吃整行）。
/// `epub`：EPUB 阅读排版（标题居中、正文首行缩进 2 字符）。
/// `note_ctx`：注记共享上下文（[`NoteCtx`]；EPUB 脚注悬浮卡片用）。
/// `text_ctx`：文本命中共享上下文（[`TextCtx`]；鼠标点选字符用）。
/// `selection`: 本行选中的 (字节区间, 背景色)；None = 无选区。
/// `marker_hl`：引用双向跳转的定位 (注记号, 是否定义)；本行是被定位目标时由 `text_child`
/// 给对应 `[n]` 标记加亮（背景色按主题取）。
/// `row_ix`：全局块下标（注记/文本命中表行标识）。
///
/// 渲染上下文：同一帧内对所有行恒定的预览级参数汇总（`render_row`/`text_child`
/// 共用，替代 14 参长签名）。
pub(crate) struct RowRenderCtx<'a> {
    pub p: &'a Rc<StyleParams>,
    pub code_handle: Option<ScrollHandle>,
    pub code_hl: Option<Rc<CodeLineRuns>>,
    pub table_handle: Option<ScrollHandle>,
    pub search: Option<(Range<usize>, Hsla)>,
    pub epub: bool,
    pub note_ctx: &'a Rc<NoteCtx>,
    pub text_ctx: &'a Rc<TextCtx>,
    pub selection: Option<(Range<usize>, Hsla)>,
    pub marker_hl: Option<(usize, bool)>,
    pub row_ix: usize,
}

/** 表格行（GFM pipe table 整体折叠成一渲染行）。
/// 页面恒换行下表格**始终**自带横向滚动容器 + Always 滚动条：超宽表格在块内横向滚动，
/// 不再被视口裁剪遮挡（对齐代码块方案）。
/// P3-1c-2：网格按 `shape_table_shard` 的 [`ShardTable`] 几何绘制（`md-table`），
/// 列宽/单元格高/表头数据与量程同源（R2）。 */
fn render_table_shard_row(
    ctx: &RowRenderCtx,
    shards: &[Shard],
    table_handle: Option<ScrollHandle>,
) -> gpui::AnyElement {
    let p = ctx.p;
    // Table shard 单片恒携带网格几何（`shape_table_shard` 保证）；防御：异常时空占位。
    let Some(geo) = shards.first().and_then(|s| s.table.as_ref()) else {
        debug_assert!(false, "Table shard 必须携带网格几何");
        return div()
            .w_full()
            .flex_shrink_0()
            .px(px(CONTENT_EDGE_PAD_X_PX))
            .py_1()
            .into_any_element();
    };
    // 块内横向滚动容器 + Always 滚动条：超宽表格在块内滚动，不撑大页面列宽。
    let h = table_handle.unwrap_or_default();
    div()
        .w_full()
        .flex_shrink_0()
        .px(px(CONTENT_EDGE_PAD_X_PX))
        .py_1()
        .child(
            div()
                .debug_selector(|| "md-table-hscroll".into())
                // 自然高度 = 网格高 + 底部滚动条预留条（shard.h 同式）；不强制高度，
                // 避免表格边框导致估算偏差、末行被 overflow_hidden 裁剪。
                .w_full()
                .relative()
                .overflow_hidden()
                .child(
                    div()
                        .id("md-table-scroll")
                        .w_full()
                        .flex()
                        .flex_row()
                        .overflow_x_scroll()
                        // 禁止滚轮纵向滚动被本容器横向消费，事件冒泡给页面正常翻页
                        .restrict_scroll_to_axis()
                        .track_scroll(&h)
                        // 底部预留滚动条轨道高度，横向滚动条不遮挡表格内容
                        .pb(px(TABLE_SCROLLBAR_RESERVE_PX))
                        .child(render_table_geometry(geo, p)),
                )
                .child(
                    div()
                        .debug_selector(|| "md-table-scrollbar".into())
                        .absolute()
                        .inset_0()
                        .child(
                            Scrollbar::horizontal(&h)
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                ),
        )
        .into_any_element()
}

/** 独立图片行：按 shard 的 [`ShardImage`] 盒几何显示（固尺寸等比 → 高度随可用宽
/// 自适应，缩小窗口不出现大片留白）；未加载/无尺寸用占位高（与现状一致）。
/// P3-1c-2：盒几何来自 `shape_image_shards`（量程同源），图片源/内存图仍走
/// `PreviewRow` 展示附加。 */
fn render_image_shard_row(
    row: &PreviewRow,
    p: &Rc<StyleParams>,
    shards: &[Shard],
) -> gpui::AnyElement {
    let src = row.image_src().to_string();
    let alt = row.docrow.text.clone();
    let fallback_color = p.muted_fg;
    // Image shard 首片恒携带盒几何（`shape_image_shards` 保证）；防御：异常时空占位。
    let Some(geo) = shards.first().and_then(|s| s.image.as_ref()) else {
        debug_assert!(false, "Image shard 必须携带盒几何");
        return div()
            .w_full()
            .flex_shrink_0()
            .px(px(CONTENT_EDGE_PAD_X_PX))
            .py_1()
            .text_size(px(13.))
            .text_color(fallback_color)
            .child(alt)
            .into_any_element();
    };

    let img_el = match row.img_data.clone() {
        // 内存图片（EPUB 不落盘）：ImageSource::Image 走 ImageDecoder 由字节解码
        Some(image) => img(gpui::ImageSource::Image(image))
            .object_fit(ObjectFit::Contain)
            .w_full()
            .h_full()
            .with_fallback(move || {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.))
                    .text_color(fallback_color)
                    .child(alt.clone())
                    .into_any_element()
            })
            .into_any_element(),
        None if src.is_empty() => div()
            .text_size(px(13.))
            .text_color(fallback_color)
            .child(alt)
            .into_any_element(),
        None => {
            // 本地绝对路径 → PathBuf（ImageAssetLoader 走 Resource::Path = fs::read）；
            // URL → &str（走向 Resource::Uri，经 HTTP 加载）。
            let src_is_url = src.starts_with("http://")
                || src.starts_with("https://")
                || src.starts_with("file://");
            let source: gpui::ImageSource = if src_is_url {
                src.as_str().into()
            } else {
                std::path::PathBuf::from(&src).into()
            };
            img(source)
                .object_fit(ObjectFit::Contain)
                .w_full()
                .h_full()
                .with_fallback(move || {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(fallback_color)
                        .child(alt.clone())
                        .into_any_element()
                })
                .into_any_element()
        }
    };

    // 图片盒宽/高直接取 shard 几何（`geo.box_w/box_h` = `min(avail, IMAGE_MAX_W)` × 固尺寸
    // 比例）：用 aspect_ratio 表达（宽 `100%·max IMAGE_MAX_W`，高随实际布局宽等比）——
    // 高度随视口宽自适应与旧 `aspect_ratio` 缩放语义逐位一致，且布局期重算（R2：量程
    // `box_h` 与渲染同源）。未给固尺寸时盒高 = 占位高（ratio = box_w/占位高）。
    let box_ = div()
        .w_full()
        .max_w(px(IMAGE_MAX_W))
        .aspect_ratio(geo.box_w / geo.box_h.max(1.))
        .debug_selector(|| "md-img-box".into());
    div()
        .w_full()
        .flex_shrink_0()
        .px(px(CONTENT_EDGE_PAD_X_PX))
        .py_1()
        .flex()
        .justify_center()
        .items_center()
        .child(box_.child(img_el))
        .into_any_element()
}

/// `PreviewRow` 的固尺寸（`img_w/img_h` 同时有效）——喂 `shape_docrow_shard_with_image`
/// 让图片盒高按真实比例换算（量程与渲染同源）。
pub(crate) fn preview_row_img_size(row: &PreviewRow) -> Option<(u32, u32)> {
    match (row.img_w, row.img_h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_highlights_keeps_bold_when_overlapping_selection_bg() {
        // 回归：选区背景与粗体 run 重叠时，合并段必须**同时**保留 font_weight 与
        // 背景色（旧实现只合并 color/背景，与选区重叠的粗体段被抹成非粗体）。
        let bold = HighlightStyle {
            font_weight: Some(gpui::FontWeight::BOLD),
            ..Default::default()
        };
        let sel_bg = HighlightStyle {
            background_color: Some(gpui::rgba(0x4d6bfe42).into()),
            ..Default::default()
        };
        let merged = merge_highlights(vec![(2..6, bold), (0..8, sel_bg)]);
        // [0,2) 仅背景；[2,6) 粗体+背景；[6,8) 仅背景。
        let seg = merged
            .iter()
            .find(|(_, s)| s.font_weight == bold.font_weight)
            .expect("重叠段应保留粗体");
        assert!(seg.1.background_color == sel_bg.background_color, "重叠段应保留背景色");
        assert_eq!(merged.len(), 3, "三个互不重叠的段：{merged:?}");
    }

    #[test]
    fn char_align_range_absorbs_cjk_boundaries() {
        let text = "你说得对，好。行不行。";
        // 终点 19 切在 `。`（字节 18..21）中间 → 前向吸收到 21（完整句号）。
        assert_eq!(char_align_range(text, 0..19), 0..21);
        // 起点同样吸收；已对齐的保持不变。
        assert_eq!(char_align_range(text, 3..6), 3..6);
        // 越界安全。
        assert_eq!(char_align_range(text, 18..999), 18..text.len());
    }
}
