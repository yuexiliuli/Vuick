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
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::config::colors::RESIZE_ACCENT;
use crate::config::markdown::{
    IMAGE_MAX_W, IMAGE_ROW_DEFAULT_H, QUOTE_BAR_COLOR, QUOTE_BAR_WIDTH_PX,
    TABLE_SCROLLBAR_RESERVE_PX,
};
use crate::markdown::parser::{Row, RowAlign, RowKind, RunKind};
use crate::markdown::view::style::StyleParams;
use crate::markdown::view::table::render_table;
use crate::syntax::{github_theme_cached, resolve_line_styles_with, CodeLineRuns};

use super::{NoteCtx, NoteHit, TextCtx, TextHit};

/// 行内代码（反引号 `` ` `` 包裹）：等宽字体 + 主题配色文字；圆角底色由
/// [`TextTrack::paint_code_ranges`] 在 paint 阶段按代码段的像素几何自绘（方角
/// highlight 背景无法圆角，且 gpui 文本 run 背景只有平面矩形）。
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

/// 把行内样式段的字节区间整体右移 `shift`（列表项前缀占位后偏移对齐显示文本）。
fn shift_runs(runs: &[(Range<usize>, RunKind)], shift: usize) -> Vec<(Range<usize>, RunKind)> {
    if shift == 0 {
        return runs.to_vec();
    }
    runs.iter()
        .map(|(r, k)| ((r.start + shift)..(r.end + shift), *k))
        .collect()
}

/// 把搜索高亮区间整体右移 `shift`：列表项显示串 = "前缀 + 文本"，命中区间按行内文本定位，
/// 转成显示串坐标后须同样平移（与 [`shift_runs`] 同理）。
fn shift_search(
    search: &Option<(Range<usize>, Hsla)>,
    shift: usize,
) -> Option<(Range<usize>, Hsla)> {
    search
        .as_ref()
        .map(|(r, c)| ((r.start + shift)..(r.end + shift), *c))
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
pub(crate) fn char_align_range(text: &str, range: Range<usize>) -> Range<usize> {
    let len = text.len();
    let mut start = range.start.min(len);
    while start < len && !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = range.end.min(len);
    while end < len && !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

/// 行内文本子元素（统一入口）：按需要构建 [`StyledText`]（行内样式/搜索高亮/
/// 选区高亮/引用标记着色+跳转加亮），再包一层 [`TextTrack`]——paint 阶段把文本
/// 布局与注记矩形写进共享表，供「鼠标点选字符」与「注记双向跳转/悬浮卡片」反查。
///
/// - `base`：**纯文本行**的基础样式（字号/颜色要跟行级 div 一致，保证命中布局一致）；
///   有行内样式/搜索/选区的行沿用主题默认作为基（旧观感）。
/// - `selection`：本行选中的 (字节区间, 背景色)；None = 无选区。
/// - `marker_hl`：跳转定位 (注记号, 是否定义, 背景色)；命中该行的对应 `[n]` 标记加亮。
fn text_child(
    text: &gpui::SharedString,
    runs: &[(Range<usize>, RunKind)],
    p: &StyleParams,
    search: Option<(Range<usize>, Hsla)>,
    selection: Option<(Range<usize>, Hsla)>,
    base: &TextStyle,
    notes: &[(Range<usize>, usize)],
    note_defs: &[(Range<usize>, usize)],
    marker_hl: Option<(usize, bool, Hsla)>,
    row_ix: usize,
    text_ctx: &Rc<TextCtx>,
    note_ctx: &Rc<NoteCtx>,
) -> gpui::AnyElement {
    // 1) 汇集行内样式段 → highlights（粗体/斜体/删除线/等宽 + 搜索背景 + 选区背景）
    let mut highlights = Vec::with_capacity(runs.len() + 2);
    let mut mono: Vec<(Range<usize>, gpui::SharedString)> = Vec::new();
    for (range, kind) in runs {
        match kind {
            RunKind::InlineCode => {
                // 等宽字体 overrides 只覆盖代码本体；文字色/背景单一源 StyleParams（背景圆角自绘）。
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
            // 链接/图片：v1 按普通文字渲染（去掉 URL/括号后的文本），不额外着色
            RunKind::Link | RunKind::Image => {}
        }
    }
    // 引用标记 `[n]`（文内 `[^n]` 与末尾定义 `[!n]` 都显示成 `[n]`）→ 全局强调色
    // RESIZE_ACCENT，与正文区分开、醒目地表明它是可点击的链接（点击做
    // 「文内 ↔ 末尾定义」双向跳转）。
    let link_color: Hsla = gpui::rgb(RESIZE_ACCENT).into();
    for (range, _) in notes.iter().chain(note_defs.iter()) {
        let end = range.end.min(text.len());
        if range.start < end {
            highlights.push((
                range.start..end,
                HighlightStyle {
                    color: Some(link_color),
                    ..Default::default()
                },
            ));
        }
    }
    let push_bg = |range: &Range<usize>, color: Hsla, highlights: &mut Vec<_>| {
        // 防御：把区间边界吸收到字符边界（防止任何非对齐字节偏移切进多字节字符，
        // 影响后续 shaping——CJK 下会 direct_write panic on non char boundary）。
        let aligned = char_align_range(text.as_ref(), range.clone());
        if aligned.end > aligned.start {
            highlights.push((
                aligned,
                HighlightStyle {
                    background_color: Some(color),
                    ..Default::default()
                },
            ));
        }
    };
    // 跳转加亮：本行是被跳转定位的目标行时，给对应的 `[n]` 标记加背景色。
    if let Some((hn, is_def, hcolor)) = marker_hl {
        let src = if is_def { note_defs } else { notes };
        for (range, _) in src.iter().filter(|(_, n)| *n == hn) {
            push_bg(range, hcolor, &mut highlights);
        }
    }
    if let Some((range, color)) = &search {
        push_bg(range, *color, &mut highlights);
    }
    if let Some((range, color)) = &selection {
        push_bg(range, *color, &mut highlights);
    }
    // 2) 构建 StyledText。**所有高亮先经 `merge_highlights` 扫拢成互不重叠的段**——
    //    选区背景 / 跳转加亮 / 搜索背景会与行内样式色 run 重叠，gpui 的
    //    `compute_runs` 不处理重叠区间，会产出**长度错误的 run**：单字节 ASCII 碰巧
    //    全是字符边界不炸，遇到 CJK（多字节）run 边界就会切进字符内 → direct_write
    //    panic（byte index not a char boundary）。
    let merged = merge_highlights(highlights);
    let st = if !runs.is_empty() || search.is_some() {
        let mut s = StyledText::new(text.clone()).with_highlights(merged);
        if !mono.is_empty() {
            s = s.with_font_family_overrides(mono);
        }
        s
    } else {
        StyledText::new(text.clone()).with_default_highlights(base, merged)
    };
    // 3) 行内代码（反引号）圆角灰底（自绘）。TextTrack 始终持有 `st`（同一
    //    layout）：既画代码底，也**记录文本命中** → 含行内代码的行同样可鼠标点选，
    //    与无代码行行为一致（旧实现把 st 移进 RoundedInlineText，使这类行无法点选）。
    let code_bg: Vec<Range<usize>> = runs
        .iter()
        .filter(|(_, k)| *k == RunKind::InlineCode)
        .map(|(r, _)| expand_bg_over_whitespace(text.as_ref(), r))
        .collect();
    TextTrack {
        st,
        code_bg,
        code_radius: px(5.),
        code_bg_color: p.code_inline_bg,
        text: text.clone(),
        row_ix,
        notes: notes.to_vec(),
        note_defs: note_defs.to_vec(),
        note_ctx: (!notes.is_empty() || !note_defs.is_empty()).then(|| note_ctx.clone()),
        text_ctx: Some(text_ctx.clone()),
    }
    .into_any_element()
}

/// 文本行包裹元素：委托被包裹文本 [`StyledText`]（含行内代码时的**同一**文本）完成
/// 测量/布局/paint，并在 paint 阶段把三种反查数据写进共享表：
/// - 文本布局（`TextCtx.hits`）→ 鼠标点选字符定位（**与是否含行内代码无关**，两类行
///   一致可点选——这保证预览/web 选区的行为统一，只有「可编辑」与否的差别）；
/// - 注记矩形（`NoteCtx.hits`，文内 `[^n]` 与末尾定义 `[!n]` 都记）→ 悬浮卡片 + 双向跳转；
/// - 行内代码圆角底：paint 前先用 [`Self::paint_code_ranges`] 画底，再委托画文字。
struct TextTrack {
    /// 文本载体（每行必有；命中时用其 `layout().index_for_position`）。
    st: StyledText,
    /// 行内代码（反引号）区间（已扩展空白），画圆角灰底用；空 = 无代码。
    code_bg: Vec<Range<usize>>,
    /// 代码底圆角半径。
    code_radius: Pixels,
    /// 代码底颜色。
    code_bg_color: Hsla,
    /// 显示文本（命中后取文本长度钳制字节偏移）。
    text: gpui::SharedString,
    /// 全局块下标（共享命中表的行标识；同帧同行使旧条目失效）。
    row_ix: usize,
    /// 文内引用标记 `[^n]` 的（字节区间, 注记号），区间相对 `text`。
    notes: Vec<(Range<usize>, usize)>,
    /// 末尾定义标记 `[!n]` 的（字节区间, 注记号），区间相对 `text`。
    note_defs: Vec<(Range<usize>, usize)>,
    /// 注记共享表（存在标记时记录）。
    note_ctx: Option<Rc<NoteCtx>>,
    /// 文本命中共享表。
    text_ctx: Option<Rc<TextCtx>>,
}

impl TextTrack {
    /// 把每个注记标记（`[^n]` / `[!n]`）的窗口坐标矩形写入共享命中表
    /// （同帧同行使旧条目失效）。命中框上下左右各放大 1px，方便鼠标悬停/点击。
    fn record_note_hits(
        &self,
        origin: gpui::Point<Pixels>,
        line_h: Pixels,
        layout: &WrappedLineLayout,
    ) {
        let Some(ctx) = self.note_ctx.as_ref() else {
            return;
        };
        let mut hits = ctx.hits.borrow_mut();
        hits.retain(|h| h.row_ix != self.row_ix);
        let mut push_hit = |range: &Range<usize>, n: usize, is_def: bool| {
            let (Some(p0), Some(p1)) = (
                layout.position_for_index(range.start, line_h),
                layout.position_for_index(range.end, line_h),
            ) else {
                return;
            };
            // 标记通常单行；跨行取整段包围盒。
            let x0 = p0.x.as_f32().min(p1.x.as_f32());
            let y0 = p0.y.as_f32().min(p1.y.as_f32());
            let x1 = p0.x.as_f32().max(p1.x.as_f32());
            let y1 = (p1.y.as_f32() + line_h.as_f32()).max(p0.y.as_f32() + line_h.as_f32());
            hits.push(NoteHit {
                row_ix: self.row_ix,
                n,
                is_def,
                bounds: Bounds {
                    origin: point(
                        px(origin.x.as_f32() + x0 - 1.0),
                        px(origin.y.as_f32() + y0 - 1.0),
                    ),
                    size: gpui::size(px(x1 - x0 + 2.0), px(y1 - y0 + 2.0)),
                },
            });
        };
        for (range, n) in &self.notes {
            push_hit(range, *n, false);
        }
        for (range, n) in &self.note_defs {
            push_hit(range, *n, true);
        }
    }

    /// 行内代码圆角灰底：把 `code_bg`（已扩展空白）区间按折行切成多段矩形并 paint，
    /// 与正文共用 `st` 的同一 layout，保证背景与字形/折行对齐。空区间不做事。
    fn paint_code_ranges(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        if self.code_bg.is_empty() {
            return;
        }
        let layout = self.st.layout();
        let line_height = layout.line_height();
        let layouts = layout.line_layouts();
        let Some(single) = layouts.first() else {
            return;
        };
        let line_h = line_height.as_f32();
        let text_origin = bounds.origin;
        for range in &self.code_bg {
            let Some(p0) = single.position_for_index(range.start, line_height) else {
                continue;
            };
            let Some(p1) = single.position_for_index(range.end, line_height) else {
                continue;
            };
            let y0 = (p0.y.as_f32() / line_h).floor() as usize;
            let y1 = (p1.y.as_f32() / line_h).floor() as usize;
            if y0 == y1 {
                // 单行内：一段矩形
                let b = Bounds {
                    origin: point(
                        px(text_origin.x.as_f32() + p0.x.as_f32()),
                        px(text_origin.y.as_f32() + p0.y.as_f32()),
                    ),
                    size: gpui::size(
                        px((p1.x.as_f32() - p0.x.as_f32()).max(0.0)),
                        px(line_h),
                    ),
                };
                window.paint_quad(
                    fill(b, self.code_bg_color).corner_radii(Corners::all(self.code_radius)),
                );
            } else {
                // 跨行：首段画到该行行尾、中间整行、末段从行首到终点
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
                        origin: point(
                            px(line_origin_x + rx0),
                            px(origin_y + real_ri as f32 * line_h),
                        ),
                        size: gpui::size(px((rx1 - rx0).max(0.0)), px(line_h)),
                    };
                    window.paint_quad(
                        fill(b, self.code_bg_color).corner_radii(Corners::all(self.code_radius)),
                    );
                }
            }
        }
    }
}

impl IntoElement for TextTrack {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextTrack {
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
        // 首帧光栅预热（一次/进程）：先打热 DWrite 彩字管线再画任何文字——冷进程首次
        // COLR 彩字光栅失败 → emoji 变空白/方框且被 sprite_atlas 钉住（见
        // `crate::markdown::wysiwyg::warm_emoji_rasterizer`）。预览若先于其它窗格画文本，
        // 也必须先预热，否则会先于其它窗格共享光栅状态。
        if !crate::markdown::wysiwyg::warm_emoji_is_done() {
            crate::markdown::wysiwyg::warm_emoji_rasterizer(window, "preview");
        }
        // 1) 记录文本命中（layout 与可见文本一致；bounds.origin 已含滚动/布局变换）。
        //    每行必有 `st`（含行内代码的行也不例外）→ 字符级点选对两类行一致可用。
        let layout = self.st.layout();
        if let Some(ctx) = self.text_ctx.as_ref() {
            let mut hits = ctx.hits.borrow_mut();
            hits.retain(|h| h.row_ix != self.row_ix);
            hits.push(TextHit {
                row_ix: self.row_ix,
                text: self.text.clone(),
                layout: layout.clone(),
                origin: bounds.origin,
            });
        }
        // 2) 记录注记命中矩形（EPUB 悬浮卡片）。
        let origin = bounds.origin;
        let line_h = layout.line_height();
        if let Some(single) = layout.line_layouts().first() {
            self.record_note_hits(origin, line_h, single);
        }
        // 3) 行内代码圆角灰底（与 text_child 里的同一 layout，先画底）。
        self.paint_code_ranges(bounds, window);
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
pub(crate) fn render_row(
    row: &Row,
    p: &Rc<StyleParams>,
    wrap: bool,
    code_handle: Option<ScrollHandle>,
    code_hl: Option<Rc<CodeLineRuns>>,
    table_handle: Option<ScrollHandle>,
    search: Option<(Range<usize>, Hsla)>,
    epub: bool,
    note_ctx: &Rc<NoteCtx>,
    text_ctx: &Rc<TextCtx>,
    selection: Option<(Range<usize>, Hsla)>,
    marker_hl: Option<(usize, bool)>,
    row_ix: usize,
    cx: &mut App,
) -> gpui::AnyElement {
    // 当前主题明/暗（代码块语法着色色板随主题）。
    let dark = gpui_component::Theme::global(cx).is_dark();
    // 跳转加亮：本行是目标行时把 (注记号, 是否定义, 背景色) 交给 text_child。
    let marker_hl = marker_hl.map(|(n, is_def)| (n, is_def, marker_flash_color(dark)));
    let code_runs = code_hl.as_deref();
    // 表格行（GFM pipe table 整体折叠成一渲染行；换行模式自带横向滚动容器）。
    if let RowKind::Table = row.kind {
        return render_table_row(row, p, wrap, table_handle);
    }
    // 独立图片行：按固有比例显示，高度随可用宽度自适应。
    if let RowKind::Image = row.kind {
        return render_image_row(row, p);
    }

    // ---------- 自动换行模式：仅对文本行生效，代码/规则/空行保持原样 ----------
    if wrap {
        // 换行模式下代码保持不换行：整个围栏折叠成一个渲染行（多行紧贴相连成连续块）。
        // 该块按内容宽撑开（content_w 计入最宽代码行 → 外层 md-harea 统一横向滚动，
        // 底部单一横向滚动条可拖动）；正文仍限制在视口宽内折行。
        if row.kind == RowKind::Code {
            let lines = row.code_lines().to_vec();
            let p_ = p.clone();
            let runs = code_runs;
            let block_h = (lines.len() + 2) as f32 * p.code_line.as_f32();
            let h = code_handle.unwrap_or_default();
            let column = div()
                .debug_selector(|| "md-code-row".into())
                .flex()
                .flex_col()
                .flex_shrink_0()
                .min_w_full()
                .children(code_block_bands(&lines, runs, dark, &p_));
            return div()
                .debug_selector(|| "md-code-hscroll".into())
                .h(px(block_h))
                .w_full()
                .flex_shrink_0()
                .px_3()
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
                                // 只允许鼠标左键拖动滚动条，禁止滚轮滚动代码块：
                                // restrict_scroll_to_axis 使纵向滚轮不被本容器横向消费（delta_x=0），
                                // 事件继续冒泡给页面做纵向滚动。
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
                .into_any_element();
        }
        match row.kind {
            RowKind::Paragraph => {
                // 正文首行缩进 2 字符由 EPUB 转换器在行首前置「　　」实现（本行只负责对齐）。
                let t = text_child(
                    &row.text,
                    &row.runs,
                    p,
                    search,
                    selection,
                    &TextStyle {
                        font_size: p.para_size.into(),
                        color: p.fg,
                        ..TextStyle::default()
                    },
                    &row.notes,
                    &row.note_defs,
                    marker_hl,
                    row_ix,
                    text_ctx,
                    note_ctx,
                );
                let mut el = div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg);
                match row.align {
                    Some(RowAlign::Center) => el = el.flex().flex_row().justify_center(),
                    Some(RowAlign::Right) => el = el.flex().flex_row().justify_end(),
                    _ => {}
                }
                return el.child(t).into_any_element();
            }
            RowKind::Heading(n) => {
                let (fs, lh) = p.heading_metrics(n);
                // EPUB 章/节标题居中（行内也可显式指定对齐）。
                let h_align = row
                    .align
                    .or(if epub { Some(RowAlign::Center) } else { None });
                let t = text_child(
                    &row.text,
                    &row.runs,
                    p,
                    search,
                    selection,
                    &TextStyle {
                        font_size: fs.into(),
                        color: p.fg,
                        font_weight: FontWeight::BOLD,
                        ..TextStyle::default()
                    },
                    &row.notes,
                    &row.note_defs,
                    marker_hl,
                    row_ix,
                    text_ctx,
                    note_ctx,
                );
                let mut el = div()
                    .w_full()
                    .min_h(lh)
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(fs)
                    .text_color(p.fg)
                    .font_weight(FontWeight::BOLD);
                match h_align {
                    Some(RowAlign::Center) => el = el.flex().flex_row().justify_center(),
                    Some(RowAlign::Right) => el = el.flex().flex_row().justify_end(),
                    _ => {}
                }
                return el.child(t).into_any_element();
            }
            RowKind::Quote => {
                // 引述：外层 px_3 内缩（与正文对齐），背景带/左边框移到内层元素，
                // 避免背景带顶到左边缘（同代码块做法）。
                // 引述块**连续化**：去掉每行的外层纵向外衬与段落行距——相邻引述行的
                // 灰带紧贴 → 整段引述（跨多行/多段）呈现一整片背景，无白色分割。
                // 块内行距由每行灰带的 `py_1`（背景内衬）提供。
                // 行内对齐（EPUB 引述里的落款 `%%ALIGN:RIGHT%%` 等）作用于引述带内容。
                let bar_color: Hsla = gpui::rgb(QUOTE_BAR_COLOR).into();
                let t = text_child(
                    &row.text,
                    &row.runs,
                    p,
                    search,
                    selection,
                    &TextStyle {
                        font_size: p.para_size.into(),
                        color: p.muted_fg,
                        ..TextStyle::default()
                    },
                    &row.notes,
                    &row.note_defs,
                    marker_hl,
                    row_ix,
                    text_ctx,
                    note_ctx,
                );
                let mut band = div()
                    .debug_selector(|| "md-quote-band".into())
                    .w_full()
                    .pl_3()
                    .pr_3()
                    .py_1()
                    // 左侧深色指示条：宽度/颜色常量在 config（QUOTE_BAR_WIDTH_PX / QUOTE_BAR_COLOR），
                    // 比灰底略深，标识引用块。
                    .border_l(px(QUOTE_BAR_WIDTH_PX))
                    .border_color(bar_color)
                    .bg(p.quote_bg)
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.muted_fg);
                match row.align {
                    Some(RowAlign::Center) => band = band.flex().flex_row().justify_center(),
                    Some(RowAlign::Right) => band = band.flex().flex_row().justify_end(),
                    _ => {}
                }
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32()))
                    .flex_shrink_0()
                    .px_3()
                    .child(band.child(t))
                    .into_any_element();
            }
            RowKind::Bullet => {
                let mut text = row.text.to_string();
                if let Some(prefix) = &row.prefix {
                    text = format!("{prefix} {text}");
                }
                let display = gpui::SharedString::from(text);
                let shift = row.prefix.as_ref().map_or(0, |s| s.len() + 1);
                let runs = shift_runs(&row.runs, shift);
                let notes: Vec<(Range<usize>, usize)> = row
                    .notes
                    .iter()
                    .map(|(r, n)| ((r.start + shift)..(r.end + shift), *n))
                    .collect();
                let note_defs: Vec<(Range<usize>, usize)> = row
                    .note_defs
                    .iter()
                    .map(|(r, n)| ((r.start + shift)..(r.end + shift), *n))
                    .collect();
                let selection2 = selection
                    .as_ref()
                    .map(|(r, c)| ((r.start + shift)..(r.end + shift), *c));
                let t = text_child(
                    &display,
                    &runs,
                    p,
                    shift_search(&search, shift),
                    selection2,
                    &TextStyle {
                        font_size: p.para_size.into(),
                        color: p.fg,
                        ..TextStyle::default()
                    },
                    &notes,
                    &note_defs,
                    marker_hl,
                    row_ix,
                    text_ctx,
                    note_ctx,
                );
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .pl_5()
                    .pr_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg)
                    .child(t)
                    .into_any_element();
            }
            _ => {}
        }
    }

    let el: gpui::Div = match row.kind {
        RowKind::Blank => div().h(p.blank_h).w_full().flex_shrink_0(),
        RowKind::Rule => div()
            .h(p.rule_h)
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_3()
            .child(div().h(px(1.)).w_full().bg(p.divider).rounded_full()),
        RowKind::Code => {
            // 代码块：整栏折叠为一行的所有代码行按 h(code_line) 纵向堆叠成连续块，
            // 上下各补一行空白带（同背景），使背景在首/末行之外多出一行呼吸空间。
            // 每行浅灰背景带（左右内缩 px_3 与正文对齐），等宽字体 + 语法着色。超宽由外层横向滚动。
            let lines = row.code_lines().to_vec();
            let p_ = p.clone();
            let runs = code_runs;
            let block_h = (lines.len() + 2) as f32 * p.code_line.as_f32();
            div()
                .h(px(block_h))
                .w_full()
                .flex_shrink_0()
                .px_3()
                .child(
                    div()
                        .debug_selector(|| "md-code-row".into())
                        .h_full()
                        .w_full()
                        .flex()
                        .flex_col()
                        .children(code_block_bands(&lines, runs, dark, &p_)),
                )
        }
        RowKind::Heading(n) => {
            let (fs, lh) = p.heading_metrics(n);
            let t = text_child(
                &row.text,
                &row.runs,
                p,
                search,
                selection,
                &TextStyle {
                    font_size: fs.into(),
                    color: p.fg,
                    font_weight: FontWeight::BOLD,
                    ..TextStyle::default()
                },
                &row.notes,
                &row.note_defs,
                marker_hl,
                row_ix,
                text_ctx,
                note_ctx,
            );
            div()
                .h(lh)
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(fs)
                .text_color(p.fg)
                .font_weight(FontWeight::BOLD)
                .child(t)
        }
        RowKind::Quote => {
            // 引述：外层 px_3 内缩（与正文对齐），背景带/左边框移到内层元素，
            // 避免背景带和“>”边条顶到左边缘（同代码块做法）。
            let t = text_child(
                &row.text,
                &row.runs,
                p,
                search,
                selection,
                &TextStyle {
                    font_size: p.para_size.into(),
                    color: p.muted_fg,
                    ..TextStyle::default()
                },
                &row.notes,
                &row.note_defs,
                marker_hl,
                row_ix,
                text_ctx,
                note_ctx,
            );
            div()
                .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
                .w_full()
                .flex_shrink_0()
                .px_3()
                .flex()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .debug_selector(|| "md-quote-band".into())
                        .h_full()
                        .w_full()
                        .pl_3()
                        .pr_3()
                        // 左侧深色指示条：宽度/颜色常量在 config（QUOTE_BAR_WIDTH_PX / QUOTE_BAR_COLOR），
                        // 与换行分支 / WYSIWYG 完全一致（旧实现此处误用 accent2px）。
                        .border_l(px(QUOTE_BAR_WIDTH_PX))
                        .border_color(gpui::rgb(QUOTE_BAR_COLOR))
                        .bg(p.quote_bg)
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(p.para_size)
                        .text_color(p.muted_fg)
                        .child(t),
                )
        }
        RowKind::Bullet => {
            let mut text = row.text.to_string();
            if let Some(prefix) = &row.prefix {
                text = format!("{prefix} {text}");
            }
            let display = gpui::SharedString::from(text);
            let shift = row.prefix.as_ref().map_or(0, |s| s.len() + 1);
            let runs = shift_runs(&row.runs, shift);
            let notes: Vec<(Range<usize>, usize)> = row
                .notes
                .iter()
                .map(|(r, n)| ((r.start + shift)..(r.end + shift), *n))
                .collect();
            let note_defs: Vec<(Range<usize>, usize)> = row
                .note_defs
                .iter()
                .map(|(r, n)| ((r.start + shift)..(r.end + shift), *n))
                .collect();
            let selection2 = selection
                .as_ref()
                .map(|(r, c)| ((r.start + shift)..(r.end + shift), *c));
            let t = text_child(
                &display,
                &runs,
                p,
                shift_search(&search, shift),
                selection2,
                &TextStyle {
                    font_size: p.para_size.into(),
                    color: p.fg,
                    ..TextStyle::default()
                },
                &notes,
                &note_defs,
                marker_hl,
                row_ix,
                text_ctx,
                note_ctx,
            );
            div()
                .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
                .w_full()
                .flex_shrink_0()
                .ml_3()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(p.para_size)
                .text_color(p.fg)
                .child(t)
        }
        RowKind::Paragraph => div()
            .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.fg)
            .child(
                text_child(
                    &row.text,
                    &row.runs,
                    p,
                    search,
                    selection,
                    &TextStyle {
                        font_size: p.para_size.into(),
                        color: p.fg,
                        ..TextStyle::default()
                    },
                    &row.notes,
                    &row.note_defs,
                    marker_hl,
                    row_ix,
                    text_ctx,
                    note_ctx,
                ),
            ),
        RowKind::Table => unreachable!("表格已在上方提前返回"),
        RowKind::Image => unreachable!("图片行已在上方提前返回"),
    };
    el.into_any_element()
}

/** 表格行（GFM pipe table 整体折叠成一渲染行）。
/// 换行模式（默认）：表格自带横向滚动容器 + Always 滚动条，超宽表格在块内横向滚动，
/// 不再被视口裁剪遮挡（对齐代码块方案）；非换行模式仍交外层 md-harea 统一横向滚动。 */
fn render_table_row(
    row: &Row,
    p: &Rc<StyleParams>,
    wrap: bool,
    table_handle: Option<ScrollHandle>,
) -> gpui::AnyElement {
    let table = row.table().expect("Table 行必须携带表格结构");
    // 换行模式（默认，预览视口不横向撑宽）：表格自带横向滚动容器 + Always 滚动条，
    // 超宽表格在块内横向滚动，不再被视口裁剪遮挡（对齐代码块方案）；
    // 非换行模式仍交外层 md-harea 统一横向滚动。
    if wrap {
        let h = table_handle.unwrap_or_default();
        return div()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_1()
            .child(
                div()
                    .debug_selector(|| "md-table-hscroll".into())
                    // 自然高度 = 表格自身高度 + 底部滚动条预留条；不强制高度，
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
                            .child(render_table(table, p)),
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
            .into_any_element();
    }
    return div()
        .w_full()
        .flex_shrink_0()
        .px_3()
        .py_1()
        .child(render_table(table, p))
        .into_any_element();
}


/** 独立图片行：按图片固有比例显示，高度随可用宽度自适应（缩小窗口不出现大片留白）。 */
fn render_image_row(row: &Row, p: &Rc<StyleParams>) -> gpui::AnyElement {
    let info = row.image();
    let src = info.map(|i| i.src.to_string()).unwrap_or_default();
    let alt = row.text.clone();
    let fallback_color = p.muted_fg;

    let img_el = match info.and_then(|i| i.img_data.cloned()) {
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

    // 已知固尺寸：图片盒宽 = min(视口宽, IMAGE_MAX_W)，高由宽 * h/w 得出
    //（aspect_ratio 使高度随可用宽度缩放 → 缩小窗口不会留白）；
    // 外层铺满宽度并 justify_center，使图片盒在窄于 600px 的视口里也水平居中。
    let (img_w, img_h) = match row.image() {
        Some(i) => (i.img_w, i.img_h),
        None => (None, None),
    };        let box_ = match (img_w, img_h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => div()
            .w_full()
            .max_w(px(IMAGE_MAX_W))
            .aspect_ratio(w as f32 / h as f32)
            .debug_selector(|| "md-img-box".into()),
        _ => div()
            .w(px(IMAGE_MAX_W))
            .max_w_full()
            .h(px(IMAGE_ROW_DEFAULT_H))
            .min_h(px(IMAGE_ROW_DEFAULT_H))
            .debug_selector(|| "md-img-box".into()),
    };
    return div()
        .w_full()
        .flex_shrink_0()
        .px_3()
        .py_1()
        .flex()
        .justify_center()
        .items_center()
        .child(box_.child(img_el))
        .into_any_element();
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
        let merged = merge_highlights(vec![(2..6, bold.clone()), (0..8, sel_bg.clone())]);
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
