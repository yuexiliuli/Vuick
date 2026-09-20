//! 新管线 WYSIWYG 自绘元素：把 [`Doc`] 统一的 `DocRow` 行排版成可编辑视图。
//! 数据完全来自本项目 `Doc`（`rows_in_block_window` + `byte_map`），零 zorite 依赖。

use std::ops::Range;

use gpui::{
    App, Bounds, Element, ElementId, ElementInputHandler, Entity, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, Window, point, px, relative, size,
};

use super::{CaretVis, WyEditor};
use crate::markdown::parser::RunKind;

/// 一次布局后的行网格（measure→prepaint→paint 跨阶段复用）。
pub(crate) struct Frame {
    pub padding: Pixels,
    pub line_h: Pixels,
    pub rows: Vec<ShapedRow>,
    pub total: Pixels,
    pub caret_x: Pixels,
    pub caret_top: Pixels,
    pub caret_on: bool,
}

pub(crate) struct ShapedRow {
    pub src_line: usize,
    pub src_lines: Range<usize>,
    /// 该块的可视行（`shape_text` 结果；一个块 = 一个或多个 `WrappedLine`）。
    pub wrapped: Vec<gpui::WrappedLine>,
    pub sub_heights: Vec<Pixels>,
    pub byte_map: std::rc::Rc<Vec<usize>>,
    pub editable: bool,
    pub height: Pixels,
    pub top: Pixels,
}

/// `DocRow.runs` → gpui `TextRun`（`RunKind` → 视觉样式）。
fn runs_to_text_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    color: gpui::Hsla,
    mono: &gpui::Font,
    accent: gpui::Hsla,
    code_bg: gpui::Hsla,
) -> Vec<gpui::TextRun> {
    use gpui::{StrikethroughStyle, TextRun, UnderlineStyle};
    let len = text.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    for (range, kind) in runs {
        let s = range.start.min(len);
        let e = range.end.min(len);
        if s > i {
            out.push(TextRun {
                len: s - i,
                font: base.clone(),
                color,
                ..Default::default()
            });
        }
        let mut tr = TextRun {
            len: e.saturating_sub(s),
            font: base.clone(),
            color,
            ..Default::default()
        };
        match kind {
            RunKind::Bold => tr.font.weight = gpui::FontWeight::BOLD,
            RunKind::Italic => tr.font.style = gpui::FontStyle::Italic,
            RunKind::InlineCode => {
                tr.font = mono.clone();
                tr.background_color = Some(code_bg);
                tr.color = accent;
            }
            RunKind::Strikethrough => {
                tr.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                });
            }
            RunKind::Link | RunKind::Image => {
                tr.color = accent;
                tr.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(accent),
                    wavy: false,
                });
            }
        }
        out.push(tr);
        i = e;
    }
    if i < len {
        out.push(TextRun {
            len: len - i,
            font: base.clone(),
            color,
            ..Default::default()
        });
    }
    out
}

/// 构建一帧：物化全块窗口行并 shape。
pub(crate) fn build_frame(editor: &WyEditor, window: &mut Window, width: Pixels) -> Frame {
    let ts = window.text_style();
    let font_size = ts.font_size.to_pixels(window.rem_size());
    let line_h = font_size * 1.5;
    let theme = &editor.theme;
    let base_font = ts.font();
    let doc = editor.doc();
    let n = doc.block_count();
    let rows_in = if n == 0 {
        Vec::new()
    } else {
        doc.rows_in_block_window(0, n.saturating_sub(1))
    };
    let mut rows: Vec<ShapedRow> = Vec::with_capacity(rows_in.len());
    let mut top = px(0.);
    for r in rows_in {
        let runs = runs_to_text_runs(
            r.text.as_ref(),
            &r.runs,
            &base_font,
            theme.text,
            &theme.mono,
            theme.accent,
            theme.code_bg,
        );
        let wrapped = window
            .text_system()
            .shape_text(r.text.clone(), font_size, &runs, Some(width), None)
            .map(|ls| ls.into_vec())
            .unwrap_or_default();
        let mut height = px(0.);
        let mut sub_heights = Vec::with_capacity(wrapped.len());
        for wl in &wrapped {
            let sh = line_h * (wl.wrap_boundaries().len() + 1) as f32;
            height += sh;
            sub_heights.push(sh);
        }
        if height == px(0.) {
            height = line_h;
            sub_heights.push(line_h);
        }
        let editable = !r.byte_map.is_empty();
        rows.push(ShapedRow {
            src_line: r.src_line,
            src_lines: r.lines.clone(),
            wrapped,
            sub_heights,
            byte_map: std::rc::Rc::new(r.byte_map),
            editable,
            height,
            top,
        });
        top += height;
    }
    if rows.is_empty() {
        rows.push(ShapedRow {
            src_line: 0,
            src_lines: 0..1,
            wrapped: Vec::new(),
            sub_heights: vec![line_h],
            byte_map: std::rc::Rc::new(Vec::new()),
            editable: false,
            height: line_h,
            top,
        });
        top += line_h;
    }
    let (caret_on, caret_x, caret_top) = caret_geometry(editor, &rows, line_h);
    Frame {
        padding: px(12.),
        line_h,
        rows,
        total: top,
        caret_x,
        caret_top,
        caret_on,
    }
}

/// 源列 → 渲染列（左界：首个 `byte_map[v] >= target`；与 `display_to_source` 互逆，
/// 光标才落回点击处——用 `>` 会整格右移一字符）。
fn source_to_display(byte_map: &[usize], target: usize) -> usize {
    byte_map.partition_point(|&b| b < target)
}

/// 渲染列 → 源列（byte_map 查值；越界钳到末端哨兵 = 源段尾）。
fn display_to_source(byte_map: &[usize], dcol: usize) -> usize {
    let idx = dcol.min(byte_map.len().saturating_sub(1));
    *byte_map.get(idx).unwrap_or(byte_map.last().unwrap_or(&0))
}

/// 单个可编辑块的渲染列 → 块内光标位置（`position_for_index` 对折行返回
/// `(子行内 x, 子行偏移·行高)`，谓以块首为原点）。
fn render_point_of(wrapped: &[gpui::WrappedLine], line_h: Pixels, dcol: usize) -> Point<Pixels> {
    if let Some(w0) = wrapped.first() {
        w0.position_for_index(dcol, line_h)
            .unwrap_or_else(|| point(px(0.), px(0.)))
    } else {
        point(px(0.), px(0.))
    }
}

/// 源 caret → 光标条几何。非文本行停在块首（只读）。
fn caret_geometry(
    editor: &WyEditor,
    rows: &[ShapedRow],
    line_h: Pixels,
) -> (bool, Pixels, Pixels) {
    let (cl, col) = editor.row_col(editor.caret());
    let line_start = editor.line_start(cl);
    let line_end = editor.line_end(cl);
    for r in rows {
        if !r.src_lines.contains(&cl) {
            continue;
        }
        if r.editable {
            let target = col.min(line_end.saturating_sub(line_start));
            // 源列 → 渲染列（`source_to_display`，左界）——与 `source_offset_at`
            // 的反查互为逆映射，光标才落在点击处。
            let dcol = source_to_display(&r.byte_map, target);
            // 折行时 p.y 是子行偏移，叠加到块首才算准缓存所在的可视行。
            let p = render_point_of(&r.wrapped, line_h, dcol);
            return (true, px(12.) + p.x, r.top + p.y);
        } else {
            return (false, px(12.), r.top);
        }
    }
    let y = rows.last().map_or(line_h, |r| r.top + r.height);
    (true, px(12.), y)
}

/// 点命中 → 源字节偏移（帧内 byte_map 反查）。
pub(crate) fn source_offset_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    // `MouseDownEvent.position` 是窗口坐标；`frame.rows[*].top`/行内 x 是元素局部
    // （paint 时以 `origin = bounds.origin` 为起点绘制），先把命中换算回局部——
    // 否则元素被 `live-body` 内边距/滚动/窗口位置偏移后，点击会映射到错误的行/列。
    let origin = editor.last_bounds().map(|b| b.origin).unwrap_or_else(|| point(px(0.), px(0.)));
    let rel_y = f32::from(p.y) - f32::from(origin.y);
    let rel_x = (f32::from(p.x) - f32::from(origin.x) - f32::from(frame.padding)).max(0.);
    let line_h = frame.line_h;
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if rel_y >= top && rel_y < bottom {
            if r.editable {
                if let Some(w0) = r.wrapped.first() {
                    // y 用点击在块内的**子行偏移**（`rel_y - top`）：`closest_index_for_position`
                    // 据此选折行的第几可视行——写成 0 会把第二行的点击映射回第一行。
                    let rel = point(px(rel_x), px(rel_y - top));
                    // `closest_index_for_position` 的 `Err` 是越界钳制后的有效索引
                    // （点过行尾/行首）；把它当 0 会让点行尾时光标跳回行首。
                    let dcol = match w0.closest_index_for_position(rel, line_h) {
                        Ok(i) | Err(i) => i,
                    };
                    let local = display_to_source(&r.byte_map, dcol);
                    let ls = editor.line_start(r.src_line);
                    let le = editor.line_end(r.src_line);
                    return (ls + local).min(le);
                }
            } else {
                return editor.line_end(r.src_line).max(editor.line_start(r.src_line));
            }
        }
    }
    editor.caret()
}

/// 元素：auto-grow、宿主滚动。
pub(crate) struct WyEditorElement {
    pub editor: Entity<WyEditor>,
}

impl IntoElement for WyEditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WyEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let editor = self.editor.clone();
        let mut style = gpui::Style::default();
        style.size.width = relative(1.).into();
        let id = window.request_measured_layout(style, move |known, available, window, cx| {
            let e = editor.read(cx);
            let width = match available.width {
                gpui::AvailableSpace::Definite(w) => w,
                _ => known.width.unwrap_or(px(800.)),
            };
            let width = if f32::from(width).is_finite() && f32::from(width) > 1. {
                width
            } else {
                px(800.)
            };
            let frame = build_frame(e, window, width);
            let total = frame.total;
            drop(e);
            let frame = std::rc::Rc::new(frame);
            editor.update(cx, |e, _| {
                *e.frame.borrow_mut() = Some(frame);
            });
            size(width.max(px(1.)), total)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        _window: &mut Window,
        cx: &mut App,
    ) {
        let frame = {
            let e = self.editor.read(cx);
            e.frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        if let Some(f) = frame.as_ref() {
            super::set_caret_vis(CaretVis {
                x: f.caret_x,
                top: f.caret_top,
                line_height: f.line_h,
            });
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus, theme) = {
            let e = self.editor.read(cx);
            (e.focus_handle(), e.theme.clone())
        };
        let frame = {
            let e = self.editor.read(cx);
            e.frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        let Some(f) = frame.as_ref() else {
            return;
        };
        // 记录本帧元素在窗口中的 bounds：`MouseDownEvent.position` 是窗口坐标，
        // `source_offset_at` 需用它把命中换算回元素局部坐标。
        self.editor
            .update(cx, |e, _| e.set_last_bounds(Some(bounds)));
        let origin = bounds.origin;
        for row in &f.rows {
            let mut sub_top = row.top;
            for wl in &row.wrapped {
                let o = point(origin.x + f.padding, origin.y + sub_top);
                if !row.editable {
                    window.paint_quad(gpui::PaintQuad {
                        bounds: Bounds::new(o, size(f.padding + wl.width(), f.line_h)),
                        corner_radii: gpui::Corners::all(px(4.)),
                        background: theme.code_bg.into(),
                        border_widths: gpui::Edges::default(),
                        border_color: gpui::Hsla::default(),
                        border_style: gpui::BorderStyle::Solid,
                    });
                }
                let _ = wl.paint(o, f.line_h, gpui::TextAlign::Left, None, window, cx);
                sub_top += f.line_h * (wl.wrap_boundaries().len() + 1) as f32;
            }
        }
        if f.caret_on && focus.is_focused(window) {
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(
                    point(origin.x + f.caret_x, origin.y + f.caret_top),
                    size(px(2.), f.line_h),
                ),
                corner_radii: gpui::Corners::default(),
                background: theme.text.into(),
                border_widths: gpui::Edges::default(),
                border_color: gpui::Hsla::default(),
                border_style: gpui::BorderStyle::Solid,
            });
        }
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_frame, caret_geometry, display_to_source, source_offset_at, source_to_display,
        WyEditor,
    };
    use gpui::{
        AppContext, Context, Entity, Render, TestAppContext, VisualTestContext, Window, point, px,
    };

    struct WyHost {
        ed: Entity<WyEditor>,
    }
    impl Render for WyHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.ed.clone()
        }
    }

    #[test]
    fn byte_map_round_trip_is_identity_for_plain_text() {
        // 普通段落：渲染 == 源（map[k] = k，末位哨兵 = len）。
        let map: Vec<usize> = (0..=5).collect(); // "abcde" + 哨兵
        for j in 0..=5 {
            let src = display_to_source(&map, j);
            assert_eq!(src, j);
            assert_eq!(
                source_to_display(&map, src),
                j,
                "点渲染列 {j} → 源 {src} → 应回渲染列 {j}"
            );
        }
    }

    #[test]
    fn hidden_markers_map_round_trip() {
        // `**bold**`：渲染文本 "bold"，map = [2,3,4,5,6]（哨兵 6 = 源段尾）。
        let map: Vec<usize> = vec![2, 3, 4, 5, 6];
        for j in 0..4 {
            let src = display_to_source(&map, j);
            assert_eq!(src, 2 + j);
            assert_eq!(
                source_to_display(&map, src),
                j,
                "点渲染列 {j} 与光标回渲染列应一致"
            );
        }
        // 光标在段尾（源 6）→ 渲染列 4（行尾）；点行尾（渲染列 4）→ 源段尾 6。
        assert_eq!(source_to_display(&map, 6), 4);
        assert_eq!(display_to_source(&map, 4), 6);
    }

    #[gpui::test]
    fn wrapped_row_click_on_second_visual_line_lands_in_second_half(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 长文本，在 300px 宽度窗口内必然折成 ≥2 个可视行。
        let words = "1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(words));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (_, o2) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.));
            assert_eq!(frame.rows.len(), 1, "单段文档应只有一行（块）");
            let w0 = frame.rows[0]
                .wrapped
                .first()
                .expect("行应有 shaped 结果");
            assert!(
                w0.wrap_boundaries().len() >= 1,
                "长文本应折行：wrap 边界 {} 个",
                w0.wrap_boundaries().len()
            );
            let line_h = f32::from(frame.line_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(frame.rows[0].top);
            // 第一可视行与第二可视行各自的「行内靠左」点击（x 相同、y 分属两个可视行）。
            let p1 = point(px(pad + 5.), px(top + line_h * 0.5));
            let p2 = point(px(pad + 5.), px(top + line_h * 1.5));
            let o1 = source_offset_at(ed.read(cx), &frame, p1);
            let o2 = source_offset_at(ed.read(cx), &frame, p2);
            // 第二可视行的点击必须落在第一可视行文本之后（而非被映射回第一行）。
            assert!(
                o2 > o1,
                "第二可视行点击的源偏移 {o2} 应大于第一可视行行尾 {o1}"
            );
            (o1, o2)
        });
        // 用 `source_offset_at` 反查出的第二可视行源偏移直接落光标，验证
        // `caret_geometry` 把光标竖条画在第二可视行（top = 块首 + 一行高）。
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| e.set_caret(o2));
            let frame = build_frame(ed.read(cx), window, px(300.));
            let (on, _, caret_top) = caret_geometry(ed.read(cx), &frame.rows, frame.line_h);
            let line_h = f32::from(frame.line_h);
            let top = f32::from(frame.rows[0].top);
            let top2 = f32::from(caret_top);
            assert!(on, "折行行应可放置光标");
            assert!(
                (top2 - (top + line_h)).abs() < 0.5,
                "光标竖条应画在第二可视行：块首 top={top}，得到 {top2}"
            );
        });
    }
}

