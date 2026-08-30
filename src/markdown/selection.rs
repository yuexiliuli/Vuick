//! Cross-leaf Markdown text selection and selectable GPUI elements.

use super::{
    model::{Block, MarkdownTheme, Span},
    parser::parse_markdown,
    text::{break_long_runs, spans_to_styled},
};
use gpui::{
    div, point as gpui_point, px, relative, rgba, size, App, Bounds, CursorStyle, ElementId,
    GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton,
    ParentElement, Pixels, SharedString, Style, Styled, TextRun, Window, WrappedLine,
};
use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc};

pub struct SelectCtx {
    pub turn: u64,
    pub focus: gpui::FocusHandle,
    pub anchor: Rc<RefCell<Option<usize>>>,
    pub selection: Rc<RefCell<Option<Range<usize>>>>,
    pub cache: Rc<RefCell<HashMap<usize, LeafCache>>>,
    pub geom: Rc<RefCell<HashMap<usize, LeafGeom>>>,
}
impl SelectCtx {
    #[allow(dead_code)]
    pub fn new(turn: u64, focus: gpui::FocusHandle) -> Self {
        Self {
            turn,
            focus,
            anchor: Rc::new(RefCell::new(None)),
            selection: Rc::new(RefCell::new(None)),
            cache: Rc::new(RefCell::new(HashMap::new())),
            geom: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

#[derive(Clone, Default)]
pub struct LeafCache {
    pub text: SharedString,
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub wrap: Pixels,
    pub rows: usize,
    pub shaped: Option<Rc<WrappedLine>>,
}
impl LeafCache {
    fn matches(&self, wrap: Pixels, font_size: Pixels, text: &str) -> bool {
        self.shaped.is_some()
            && self.wrap == wrap
            && self.font_size == font_size
            && self.text.as_ref() == text
    }
}
#[derive(Clone, Default)]
pub struct LeafGeom {
    pub left: Pixels,
    pub top: Pixels,
    pub line_height: Pixels,
    pub text: SharedString,
    pub layout: Option<Rc<WrappedLine>>,
}
impl LeafGeom {
    pub fn char_at(&self, point: &gpui::Point<Pixels>) -> usize {
        let Some(wl) = &self.layout else { return 0 };
        let rel = gpui_point(point.x - self.left, point.y - self.top);
        char_at_byte(
            &self.text,
            wl.index_for_position(rel, self.line_height)
                .unwrap_or_else(|b| b),
        )
    }
}

#[allow(dead_code)]
pub fn selected_markdown(md: &str, selection: &Range<usize>) -> String {
    let blocks = parse_markdown(md);
    let mut out = String::new();
    let mut stream = 0;
    for_each_text_leaf(&blocks, |spans| {
        let len = spans.iter().map(|s| s.text.chars().count()).sum::<usize>();
        let end = stream + len;
        let (s, e) = (selection.start.max(stream), selection.end.min(end));
        if s < e {
            if !out.is_empty() {
                out.push('\n');
            }
            let (ls, le) = (s - stream, e - stream);
            let mut at = 0;
            for span in spans {
                let next = at + span.text.chars().count();
                let (from, to) = (ls.max(at), le.min(next));
                if from < to {
                    let part: String = span.text.chars().skip(from - at).take(to - from).collect();
                    if from == at && to == next {
                        out.push_str(&span_markdown(span, &part));
                    } else {
                        out.push_str(&part);
                    }
                }
                at = next;
            }
        }
        stream = end;
    });
    out.replace('\u{200b}', "")
}
fn span_markdown(span: &Span, text: &str) -> String {
    if span.code {
        format!("`{text}`")
    } else if span.link || span.bold || span.italic || span.strike {
        let (left, right) = if span.bold {
            ("**", "**")
        } else if span.italic {
            ("*", "*")
        } else if span.strike {
            ("~~", "~~")
        } else {
            ("", "")
        };
        format!("{left}{text}{right}")
    } else {
        text.to_owned()
    }
}
pub(crate) fn for_each_text_leaf(blocks: &[Block], mut f: impl FnMut(&[Span])) {
    for block in blocks {
        match block {
            Block::Paragraph(lines) => {
                for line in lines {
                    f(line)
                }
            }
            Block::Heading { spans, .. } => f(spans),
            Block::Code(code) => {
                for line in code.split('\n') {
                    f(&[Span {
                        text: break_long_runs(line),
                        bold: false,
                        italic: false,
                        strike: false,
                        code: false,
                        link: false,
                    }])
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    f(item)
                }
            }
            Block::Table { headers, rows } => {
                for cell in headers {
                    f(cell)
                }
                for row in rows {
                    for cell in row {
                        f(cell)
                    }
                }
            }
            Block::Quote(_) | Block::Hr => {}
        }
    }
}

struct SelectText {
    data: SelectLeafData,
    ctx: Rc<SelectCtx>,
}
struct SelectLeafData {
    text: SharedString,
    runs: Vec<TextRun>,
    stream_start: usize,
}
impl SelectLeafData {
    fn char_len(&self) -> usize {
        self.text.chars().count()
    }
}
impl SelectText {
    fn new(data: SelectLeafData, ctx: Rc<SelectCtx>) -> Self {
        Self { data, ctx }
    }
}
impl IntoElement for SelectText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl gpui::Element for SelectText {
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
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.min_size.width = relative(0.).into();
        let (start, text, runs, ctx) = (
            self.data.stream_start,
            self.data.text.clone(),
            self.data.runs.clone(),
            self.ctx.clone(),
        );
        (
            window.request_measured_layout(style, move |known, available, window, _| {
                let wrap = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(x) => Some(x),
                    _ => None,
                });
                let font = window.text_style().font_size.to_pixels(window.rem_size());
                let cached = ctx
                    .cache
                    .borrow()
                    .get(&start)
                    .filter(|c| c.matches(wrap.unwrap_or(px(0.)), font, &text))
                    .map(|c| (c.rows, c.line_height));
                let (rows, height) = cached.unwrap_or_else(|| {
                    (
                        shape_rows(&text, font, &runs, wrap, window),
                        window.line_height(),
                    )
                });
                size(wrap.unwrap_or(px(0.)), height * rows as f32)
            }),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        let start = self.data.stream_start;
        let font = window.text_style().font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let wrap = bounds.size.width;
        let text = &self.data.text;
        let shaped = {
            let cache = self.ctx.cache.borrow();
            cache
                .get(&start)
                .filter(|c| c.matches(wrap, font, text))
                .and_then(|c| c.shaped.clone())
        };
        let shaped = shaped.or_else(|| {
            window
                .text_system()
                .shape_text(text.clone(), font, &self.data.runs, Some(wrap), None)
                .ok()
                .and_then(|v| v.into_iter().next())
                .map(Rc::new)
        });
        let rows = shaped
            .as_ref()
            .map(|w| w.wrap_boundaries().len() + 1)
            .unwrap_or(1);
        self.ctx.cache.borrow_mut().insert(
            start,
            LeafCache {
                text: text.clone(),
                font_size: font,
                line_height,
                wrap,
                rows,
                shaped: shaped.clone(),
            },
        );
        self.ctx.geom.borrow_mut().insert(
            start,
            LeafGeom {
                left: bounds.left(),
                top: bounds.top(),
                line_height,
                text: text.clone(),
                layout: shaped,
            },
        );
    }
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let start = self.data.stream_start;
        let len = self.data.char_len();
        let (from, to) = match self.ctx.selection.borrow().clone() {
            Some(s) => (
                s.start.saturating_sub(start).min(len),
                s.end.saturating_sub(start).min(len),
            ),
            None => (0, 0),
        };
        if from < to {
            if let Some(g) = self.ctx.geom.borrow().get(&start).cloned() {
                if let Some(w) = &g.layout {
                    let (a, b) = (byte_for_char(&g.text, from), byte_for_char(&g.text, to));
                    for row in 0..=w.wrap_boundaries().len() {
                        let rs = if row == 0 {
                            0
                        } else {
                            boundary_byte(w, row - 1)
                        };
                        let re = if row == w.wrap_boundaries().len() {
                            w.len()
                        } else {
                            boundary_byte(w, row)
                        };
                        if a < re && b > rs {
                            let x0 = if a <= rs {
                                px(0.)
                            } else {
                                w.position_for_index(a, g.line_height)
                                    .map(|p| p.x)
                                    .unwrap_or_default()
                            };
                            let x1 = if b >= re {
                                w.position_for_index(re, g.line_height)
                                    .map(|p| p.x)
                                    .unwrap_or_default()
                            } else {
                                w.position_for_index(b, g.line_height)
                                    .map(|p| p.x)
                                    .unwrap_or_default()
                            };
                            if x1 > x0 {
                                window.paint_quad(
                                    gpui::fill(
                                        Bounds::new(
                                            gpui_point(
                                                g.left + x0,
                                                g.top + g.line_height * row as f32,
                                            ),
                                            size(x1 - x0, g.line_height),
                                        ),
                                        rgba(0x93c5fd80),
                                    )
                                    .corner_radii(px(1.)),
                                );
                            }
                        }
                    }
                }
            }
        }
        let cache = self.ctx.cache.borrow();
        let shaped = cache.get(&start).and_then(|c| c.shaped.clone());
        let height = cache
            .get(&start)
            .map(|c| c.line_height)
            .unwrap_or_else(|| window.line_height());
        drop(cache);
        if let Some(w) = shaped {
            let _ = w.paint(
                bounds.origin,
                height,
                gpui::TextAlign::Left,
                Some(bounds),
                window,
                cx,
            );
        }
    }
}
fn boundary_byte(w: &WrappedLine, i: usize) -> usize {
    w.wrap_boundaries()
        .get(i)
        .and_then(|b| w.unwrapped_layout.runs[b.run_ix].glyphs.get(b.glyph_ix))
        .map(|g| g.index)
        .unwrap_or(w.len())
}
fn byte_for_char(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}
fn char_at_byte(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}
fn shape_rows(
    text: &str,
    font: Pixels,
    runs: &[TextRun],
    wrap: Option<Pixels>,
    window: &mut Window,
) -> usize {
    window
        .text_system()
        .shape_text(SharedString::from(text.to_owned()), font, runs, wrap, None)
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|w| w.wrap_boundaries().len() + 1)
        .unwrap_or(1)
}

pub(crate) fn render_selectable_spans(
    spans: &[Span],
    theme: &MarkdownTheme,
    force_bold: bool,
    sel: &Rc<SelectCtx>,
    stream: &mut usize,
) -> impl IntoElement {
    let data = SelectLeafData {
        text: SharedString::from(spans.iter().map(|s| s.text.as_str()).collect::<String>()),
        runs: spans_to_styled(spans, theme, force_bold).runs,
        stream_start: *stream,
    };
    *stream += data.char_len();
    let (ctx, start) = (sel.clone(), data.stream_start);
    div()
        .id(ElementId::Name(SharedString::from(format!(
            "sel-{}-{start}",
            sel.turn
        ))))
        .min_w(px(0.))
        .cursor(CursorStyle::IBeam)
        .on_mouse_down(MouseButton::Left, {
            let ctx = ctx.clone();
            move |ev, window, cx| {
                window.focus(&ctx.focus, cx);
                let Some(g) = ctx.geom.borrow().get(&start).cloned() else {
                    return;
                };
                let global = start + g.char_at(&ev.position);
                let changed = *ctx.selection.borrow() != Some(global..global);
                *ctx.anchor.borrow_mut() = Some(global);
                *ctx.selection.borrow_mut() = Some(global..global);
                if changed {
                    window.refresh();
                }
            }
        })
        .on_mouse_move({
            let ctx = ctx.clone();
            move |ev, window, _| {
                let Some(anchor) = *ctx.anchor.borrow() else {
                    return;
                };
                let Some(g) = ctx.geom.borrow().get(&start).cloned() else {
                    return;
                };
                let current = start + g.char_at(&ev.position);
                let range = anchor.min(current)..anchor.max(current);
                if *ctx.selection.borrow() != Some(range.clone()) {
                    *ctx.selection.borrow_mut() = Some(range);
                    window.refresh();
                }
            }
        })
        .on_mouse_up(MouseButton::Left, {
            let ctx = ctx.clone();
            move |_, _, _| *ctx.anchor.borrow_mut() = None
        })
        .child(SelectText::new(data, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_markdown_preserves_markup() {
        let md = "# 标题\n\n**加粗** 和 `code`\n\n- 甲\n- 乙";
        let blocks = parse_markdown(md);
        let mut total = 0;
        for_each_text_leaf(&blocks, |spans| {
            total += spans.iter().map(|s| s.text.chars().count()).sum::<usize>()
        });
        let out = selected_markdown(md, &(0..total));
        assert!(out.contains("**加粗**"));
        assert!(out.contains("`code`"));
        assert!(out.contains("标题") && out.contains("甲") && out.contains("乙"));
    }

    #[test]
    fn selected_markdown_partial_and_char_precision() {
        assert_eq!(selected_markdown("一二三\n\n**甲**乙", &(0..3)), "一二三");
        assert_eq!(selected_markdown("**加粗加粗**", &(1..3)), "粗加");
        assert_eq!(selected_markdown("**加粗加粗**", &(0..4)), "**加粗加粗**");
        assert_eq!(selected_markdown("**粗**加", &(0..2)), "**粗**加");
    }
}
