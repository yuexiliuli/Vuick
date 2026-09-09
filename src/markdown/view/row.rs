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
    StrikethroughStyle, Styled, StyledImage, StyledText, WrappedLineLayout, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::markdown::parser::{Row, RowKind, RunKind};
use crate::markdown::view::dimensions::{IMAGE_MAX_W, IMAGE_ROW_DEFAULT_H};
use crate::markdown::view::style::StyleParams;
use crate::markdown::view::table::{render_table, table_height};

/// 行内代码（反引号 `` ` `` 包裹）主题感知配色：白/黑主题各一套。
/// 取值对齐参考截图（白主题.png / 黑主题.png）的像素：白=底 #f3f4f4 + 文字 #222222，
/// 黑=底 #3e3f40 + 文字 #dadada；文字与正文同色，靠底色 + 等宽字体区分。
struct InlineCodeColors {
    fg: Hsla,
    bg: Hsla,
}

/// 按主题取行内代码配色（浅色/暗色定制）。
fn inline_code_colors(cx: &App) -> InlineCodeColors {
    let dark = gpui_component::Theme::global(cx).is_dark();
    if dark {
        InlineCodeColors {
            fg: gpui::rgb(0xdadada).into(),
            bg: gpui::rgb(0x3e3f40).into(),
        }
    } else {
        InlineCodeColors {
            fg: gpui::rgb(0x222222).into(),
            bg: gpui::rgb(0xf3f4f4).into(),
        }
    }
}

/// 行内代码（反引号 `` ` `` 包裹）：等宽字体 + 主题配色文字；圆角底色由
/// [`RoundedInlineText`] 在 paint 阶段按代码段的像素几何自绘（方角 highlight 背景
/// 无法圆角，且 gpui 文本 run 背景只有平面矩形）。
fn inline_code_highlight(colors: &InlineCodeColors) -> HighlightStyle {
    HighlightStyle {
        color: Some(colors.fg),
        ..Default::default()
    }
}

/// 把背景区间向左右扩展吞掉紧邻空白：留给圆角背景的左右视觉间距。
/// （高亮区间即"代码文本 + 两侧各一个空格"，等宽 overrides 只覆盖代码本体，
/// 空格保持正文字宽 → 圆角灰底略宽于字形，左右有呼吸空间。）
fn expand_bg_over_whitespace(text: &str, range: &Range<usize>) -> Range<usize> {
    let mut start = range.start;
    let mut end = range.end;
    if start > 0 {
        let prev = text[..start].chars().next_back();
        if let Some(c) = prev {
            if c.is_whitespace() {
                start -= c.len_utf8();
            }
        }
    }
    if end < text.len() {
        let next = text[end..].chars().next();
        if let Some(c) = next {
            if c.is_whitespace() {
                end += c.len_utf8();
            }
        }
    }
    start..end
}

/// 把行内的样式段（`runs`，字节区间）套到整段文本上，生成 [`StyledText`]；
/// 没有任何样式段时返回 `None`，调用方直接渲染纯文本以省一次分配。
/// 反引号代码 → 等宽 + 灰底；粗体/斜体/删除线 → 对应字重/风格；
/// 链接/图片（v1 渲染按文字处理）不特殊着色。
fn styled_text(
    text: &gpui::SharedString,
    runs: &[(Range<usize>, RunKind)],
    p: &StyleParams,
    colors: &InlineCodeColors,
) -> Option<StyledText> {
    if runs.is_empty() {
        return None;
    }
    let mut highlights = Vec::with_capacity(runs.len());
    let mut mono: Vec<(Range<usize>, gpui::SharedString)> = Vec::new();
    for (range, kind) in runs {
        match kind {
            RunKind::InlineCode => {
                // 等宽字体 overrides 只覆盖代码本体；文字色用主题配色（背景圆角自绘）。
                highlights.push((range.clone(), inline_code_highlight(colors)));
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
    let mut st = StyledText::new(text.clone()).with_highlights(highlights);
    if !mono.is_empty() {
        st = st.with_font_family_overrides(mono);
    }
    Some(st)
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

/// 渲染一行代码（等宽、不换行、浅灰底）。
fn code_band(t: gpui::SharedString, p: &StyleParams) -> gpui::Div {
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
        .bg(p.muted_bg)
        .child(t)
}

/// 代码块上下各补一行空白带（带同一背景色）：让代码块背景在首行文字上方 /
/// 末行文字下方各多出一整行的呼吸空间，视觉上从紧贴文字变成留出前/后一行空白。
/// `selector` 区分顶/底空白带，便于测试定位。
fn code_blank(selector: &'static str, p: &StyleParams) -> gpui::Div {
    div()
        .debug_selector(move || selector.into())
        .h(p.code_line)
        .w_full()
        .bg(p.muted_bg)
}

/// 代码块＝首行空白带 + 代码行 + 末行空白带（背景连续铺平）。
fn code_block_bands(lines: &[gpui::SharedString], p: &StyleParams) -> Vec<gpui::Div> {
    let mut bands = Vec::with_capacity(lines.len() + 2);
    bands.push(code_blank("md-code-blank-top", p));
    bands.extend(lines.iter().map(|l| code_band(l.clone(), p)));
    bands.push(code_blank("md-code-blank-bottom", p));
    bands
}

/// 把行内样式段转成可直接作 `.child()` 的元素：
/// 有样式段时用 [`StyledText`]（等宽代码 / 粗斜体 / 删除线），否则回退纯文本。
/// 行内代码（反引号）段会额外套一层圆角灰底背景（[`RoundedInlineText`]），
/// 保持与正文同行/折行的同时呈现圆润 pill 效果。
fn row_text_child(
    text: &gpui::SharedString,
    runs: &[(Range<usize>, RunKind)],
    p: &StyleParams,
    colors: &InlineCodeColors,
) -> gpui::AnyElement {
    // 主题自带的 HighlightStyle 背景只能是平面矩形；要圆角必须自绘。
    // 这里提取"扩展空白后的代码背景区间"，用自绘 Element 画圆角。
    let code_bg: Vec<Range<usize>> = runs
        .iter()
        .filter(|(_, k)| *k == RunKind::InlineCode)
        .map(|(r, _)| expand_bg_over_whitespace(text.as_ref(), r))
        .collect();
    if code_bg.is_empty() {
        return match styled_text(text, runs, p, colors) {
            Some(st) => st.into_any_element(),
            None => text.clone().into_any_element(),
        };
    }
    let Some(st) = styled_text(text, runs, p, colors) else {
        return text.clone().into_any_element();
    };
    RoundedInlineText {
        text: st,
        code_bg,
        radius: px(5.),
        bg: colors.bg,
    }
    .into_any_element()
}

/// 自绘行内代码圆角背景的元素：委托 [`StyledText`] 完成文本的测量/布局，
/// paint 阶段先按代码段的像素几何画圆角灰底，再委托绘制文字本身。
/// 这样代码串与正文共用同一条文本流（正确同行、折行、光标对齐），
/// 背景则是独立可控的圆角矩形。
struct RoundedInlineText {
    text: StyledText,
    /// 代码段的背景区间（已扩展空白），字节坐标相对整段文本。
    code_bg: Vec<Range<usize>>,
    radius: Pixels,
    bg: Hsla,
}

impl RoundedInlineText {
    /// 把代码背景区间按折行切成多段矩形并 paint 圆角背景。
    /// `layout` 是整段文本的 wrap 布局（`TextLayout::line_layouts()`），
    /// 用 `position_for_index` 把字节区间映射到像素位置；跨行时逐行切开。
    fn paint_ranges(
        &self,
        text_origin: gpui::Point<Pixels>,
        line_height: Pixels,
        layout: &WrappedLineLayout,
        window: &mut Window,
    ) {
        let line_h = line_height.as_f32();
        for range in &self.code_bg {
            let Some(p0) = layout.position_for_index(range.start, line_height) else {
                continue;
            };
            let Some(p1) = layout.position_for_index(range.end, line_height) else {
                continue;
            };
            let y0 = (p0.y.as_f32() / line_h).floor() as usize;
            let y1 = (p1.y.as_f32() / line_h).floor() as usize;
            if y0 == y1 {
                // 单行内：一段矩形
                let bounds = Bounds {
                    origin: point(
                        px(text_origin.x.as_f32() + p0.x.as_f32()),
                        px(text_origin.y.as_f32() + p0.y.as_f32()),
                    ),
                    size: gpui::size(
                        px((p1.x.as_f32() - p0.x.as_f32()).max(0.0)),
                        px(line_h),
                    ),
                };
                window.paint_quad(fill(bounds, self.bg).corner_radii(Corners::all(self.radius)));
            } else {
                // 跨行：首段画到该行行尾、中间整行、末段从行首到终点
                let line_origin_x = text_origin.x.as_f32();
                let origin_y = text_origin.y.as_f32();
                for real_ri in y0..=y1 {
                    let (rx0, rx1) = if real_ri == y0 {
                        (p0.x.as_f32(), layout.width().as_f32())
                    } else if real_ri == y1 {
                        (0.0, p1.x.as_f32())
                    } else {
                        (0.0, layout.width().as_f32())
                    };
                    let bounds = Bounds {
                        origin: point(
                            px(line_origin_x + rx0),
                            px(origin_y + real_ri as f32 * line_h),
                        ),
                        size: gpui::size(px((rx1 - rx0).max(0.0)), px(line_h)),
                    };
                    window
                        .paint_quad(fill(bounds, self.bg).corner_radii(Corners::all(self.radius)));
                }
            }
        }
    }
}

impl IntoElement for RoundedInlineText {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RoundedInlineText {
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
        // 委托给文本本身布局
        let (lid, ()) = self.text.request_layout(id, inspector_id, window, cx);
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
        self.text
            .prepaint(id, inspector_id, bounds, request_layout, window, cx);
        ()
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
        // 1) 画代码段的圆角背景
        let layout = self.text.layout();
        let origin = bounds.origin;
        let line_h = layout.line_height();
        if let Some(single) = layout.line_layouts().first() {
            self.paint_ranges(origin, line_h, single, window);
        }
        // 2) 委托绘制文字
        self.text
            .paint(id, inspector_id, bounds, request_layout, prepaint_state, window, cx);
    }
}

/// 渲染单个行。`code_handle` / `table_handle` 为各自稳定的横向滚动句柄
/// （None 时自动创建），供滚动条拖动使用。
pub(crate) fn render_row(
    row: &Rc<Row>,
    p: &Rc<StyleParams>,
    wrap: bool,
    code_handle: Option<ScrollHandle>,
    table_handle: Option<ScrollHandle>,
    cx: &mut App,
) -> gpui::AnyElement {
    // 每行渲染时取一次行内代码配色快照（浅色/暗色各一套，决定文字色与 pill 底/描边）。
    let inline_colors = inline_code_colors(cx);
    // 表格是带 id 的有状态元素，直接整体转换，不参与下方 Div 匹配；
    // 外层加 px_3 内缩，避免表格网格顶到左边缘（与正文对齐）。
    if let RowKind::Table = row.kind {
        let table = row.table.as_ref().expect("Table 行必须携带表格结构");
        // 换行模式（默认，预览视口不横向撑宽）：表格自带横向滚动容器 + Always 滚动条，
        // 超宽表格在块内横向滚动，不再被视口裁剪遮挡（对齐代码块方案）；
        // 非换行模式仍交外层 md-harea 统一横向滚动。
        if wrap {
            let h = table_handle.unwrap_or_else(ScrollHandle::new);
            let table_h = table_height(table, p);
            return div()
                .w_full()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .child(
                    div()
                        .debug_selector(|| "md-table-hscroll".into())
                        .h(px(table_h))
                        .w_full()
                        .relative()
                        .overflow_hidden()
                        .child(
                            div()
                                .id("md-table-scroll")
                                .w_full()
                                .h_full()
                                .flex()
                                .flex_row()
                                .overflow_x_scroll()
                                // 禁止滚轮纵向滚动被本容器横向消费，事件冒泡给页面正常翻页
                                .restrict_scroll_to_axis()
                                .track_scroll(&h)
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
    // 独立图片行：按图片固有比例显示，高度随可用宽度自适应（缩小窗口不出现大片留白）。
    // 宽不超过 IMAGE_MAX_W；未知尺寸（URL/不可读）回退固定 IMAGE_ROW_DEFAULT_H。
    // src 为空/加载失败时显示 alt 作为 fallback。
    if let RowKind::Image = row.kind {
        let src = row.src.clone().unwrap_or_default().to_string();
        let alt = row.text.clone();
        let fallback_color = p.muted_fg;

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
                        .text_color(fallback_color.clone())
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
                            .text_color(fallback_color.clone())
                            .child(alt.clone())
                            .into_any_element()
                    })
                    .into_any_element()
            }
        };

        // 已知固尺寸：图片盒宽 = min(视口宽, IMAGE_MAX_W)，高由宽 * h/w 得出
        //（aspect_ratio 使高度随可用宽度缩放 → 缩小窗口不会留白）；
        // 外层铺满宽度并 justify_center，使图片盒在窄于 600px 的视口里也水平居中。
        let img_w = row.img_w;
        let img_h = row.img_h;
        let box_ = match (img_w, img_h) {
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

    // ---------- 自动换行模式：仅对文本行生效，代码/规则/空行保持原样 ----------
    if wrap {
        // 换行模式下代码保持不换行：整个围栏折叠成一个渲染行（多行紧贴相连成连续块）。
        // 该块按内容宽撑开（content_w 计入最宽代码行 → 外层 md-harea 统一横向滚动，
        // 底部单一横向滚动条可拖动）；正文仍限制在视口宽内折行。
        if row.kind == RowKind::Code {
            let lines = row.code_lines.clone();
            let p_ = p.clone();
            let block_h = (lines.len() + 2) as f32 * p.code_line.as_f32();
            let h = code_handle.unwrap_or_else(ScrollHandle::new);
            let column = div()
                .debug_selector(|| "md-code-row".into())
                .flex()
                .flex_col()
                .flex_shrink_0()
                .min_w_full()
                .children(code_block_bands(&lines, &p_));
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
                let t = row_text_child(&row.text, &row.runs, &p, &inline_colors);
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Heading(n) => {
                let (fs, lh) = p.heading_metrics(n);
                let t = row_text_child(&row.text, &row.runs, &p, &inline_colors);
                return div()
                    .w_full()
                    .min_h(lh)
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(fs)
                    .text_color(p.fg)
                    .font_weight(FontWeight::BOLD)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Quote => {
                // 引述：外层 px_3 内缩（与正文对齐），背景带/左边框移到内层元素，
                // 避免背景带顶到左边缘（同代码块做法）。
                let t = row_text_child(&row.text, &row.runs, &p, &inline_colors);
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .child(
                        div()
                            .debug_selector(|| "md-quote-band".into())
                            .w_full()
                            .pl_3()
                            .pr_3()
                            .py_1()
                            .border_l_2()
                            .border_color(p.accent)
                            .bg(p.quote_bg)
                            .whitespace_normal()
                            .text_size(p.para_size)
                            .text_color(p.muted_fg)
                            .child(t),
                    )
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
                let t = row_text_child(&display, &runs, &p, &inline_colors);
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
            // 每行浅灰背景带（左右内缩 px_3 与正文对齐），等宽字体。超宽由外层横向滚动。
            let lines = row.code_lines.clone();
            let p_ = p.clone();
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
                        .children(code_block_bands(&lines, &p_)),
                )
        }
        RowKind::Heading(n) => {
            let (fs, lh) = p.heading_metrics(n);
            let t = row_text_child(&row.text, &row.runs, &p, &inline_colors);
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
            let t = row_text_child(&row.text, &row.runs, &p, &inline_colors);
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
                        .border_l_2()
                        .border_color(p.accent)
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
            let t = row_text_child(&display, &runs, &p, &inline_colors);
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
            .child(row_text_child(&row.text, &row.runs, &p, &inline_colors)),
        RowKind::Table => unreachable!("表格已在上方提前返回"),
        RowKind::Image => unreachable!("图片行已在上方提前返回"),
    };
    el.into_any_element()
}
