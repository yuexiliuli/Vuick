//! 实时预览编辑（WYSIWYG，Phase 3 v1）：把 Markdown **渲染视图**做成可直接编辑的
//! 块级编辑器。
//!
//! 架构（独立于 [`crate::editor::CodeEditor`] 的显示层，另起炉灶）：
//! - **单一数据源 = [`crate::editor::buffer::Buffer`]**（Rope 纯文本 + 字节偏移），
//!   光标/选区/编辑全部在字节空间；WYSIWYG 只是它的一种**块级渲染滤镜**；
//! - 由 [`crate::markdown::parser::parse_to_rows_live`] 把源码解析成 [`LiveRow`]
//!   （含「渲染字节 ↔ 源码字节」映射），逐块绘制：标题用标题字号、加粗/斜体/代码/
//!   链接真实渲染、围栏代码块逐行等宽、表格/图片占位——**块与块高度不同**；
//! - 点击渲染文本 → 经映射换算源码字节落光标；键盘/IME 编辑直接改 Buffer，
//!   编辑后整篇重解析重渲染（v1 面向中小文档）。
//!
//! v1 取舍（明确不做，避免半成品）：
//! - 无撤销/重做、无跨行文本选择（单选区内只画本行底色）；
//! - 大文档整篇重解析；
//! - 代码块/表格/图片内部不做单元格/行内级编辑，点击落入块首。

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    div, font, point, px, size, AnyElement, App, Bounds, Context, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, Hsla, InteractiveElement, IntoElement, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, SharedString, ShapedLine,
    StatefulInteractiveElement, Styled, StyledText, UnderlineStyle, UTF16Selection, Window,
};
use crate::editor::buffer::Buffer;
use crate::editor::merge_highlights;
use crate::editor::selection::Selection;
use crate::markdown::parser::{LiveRow, RowKind, RowPayload, RunKind, is_text_kind, parse_to_rows_live};
use crate::markdown::view::style::StyleParams;

/// 行的左右内边距（px，文本起点 x）。
const ROW_PAD_X: f32 = 8.0;
/// 光标条宽（px）。
const CARET_WIDTH_PX: f32 = 2.0;

/// 渲染行的命中信息（渲染时写入共享表，鼠标点击读）。
struct RowRect {
    /// 全局渲染行下标。
    ix: usize,
    /// 内容坐标 y（滚动前）/ 自身高度。
    top: f32,
    height: f32,
    /// 文本起点 x（行内 padding）。
    text_left: f32,
    /// 文本字节长（命中换算越界夹）。
    text_len: usize,
    /// 本行文本的整形结果（命中测试用；非文本行 = None）。
    line: Option<ShapedLine>,
}

/// 实时预览编辑（块级 WYSIWYG）。
pub struct MarkdownLivePreview {
    buffer: Buffer,
    selection: Selection,
    marked_range: Option<Range<usize>>,
    /// 当前解析出的渲染行（含「渲染 ↔ 源码」映射）。
    live: Vec<LiveRow>,
    focus_handle: FocusHandle,
    /// 排版参数快照（主题字体/颜色；Rc 共享以在各行元素间便宜克隆）。
    p: Rc<StyleParams>,
    /// 上一帧是否深色（主题切换触发刷新）。
    dark: bool,
    /// 内容区元素的窗口边界（包装元素 paint 回写；命中/坐标换算用）。
    element_bounds: Bounds<Pixels>,
    /// 纵向滚动句柄（跟随原生 `overflow_y_scroll` 容器）。
    v_handle: gpui::ScrollHandle,
    /// 上帧行命中表。
    rects: Rc<RefCell<Vec<RowRect>>>,
    /// 字号覆盖（None = 主题默认），变化后下一帧刷新排版参数。
    font_override: Option<f32>,
    /// 排版参数是否待刷新（字号覆盖变化）。
    styles_dirty: bool,
}

impl MarkdownLivePreview {
    /// 以源码创建实时预览编辑器（光标在 0）。
    pub fn new(source: &str, cx: &mut App) -> Self {
        let buffer = Buffer::new(source);
        let live = parse_to_rows_live(source);
        let p = Rc::new(StyleParams::from_theme_with_font(cx, None));
        let dark = gpui_component::Theme::global(cx).is_dark();
        Self {
            buffer,
            selection: Selection::collapsed(0),
            marked_range: None,
            live,
            focus_handle: cx.focus_handle().tab_stop(true),
            p,
            dark,
            element_bounds: Bounds::default(),
            v_handle: gpui::ScrollHandle::new(),
            rects: Rc::new(RefCell::new(Vec::new())),
            font_override: None,
            styles_dirty: false,
        }
    }

    /// 当前全文（退出实时模式时写回/保存用）。
    pub fn text(&self) -> String {
        self.buffer.to_string()
    }

    /// 覆盖正文字号（应用 Ctrl+=/- 缩放用）；下一帧刷新排版参数。
    pub fn set_font_override(&mut self, size_px: Option<f32>) {
        if self.font_override != size_px {
            self.font_override = size_px;
            self.styles_dirty = true;
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 刷新主题排版参数（明暗 / 字号覆盖变化时）。
    fn refresh_styles(&mut self, cx: &mut Context<Self>) {
        let dark = gpui_component::Theme::global(cx).is_dark();
        if dark != self.dark || self.styles_dirty {
            self.dark = dark;
            self.styles_dirty = false;
            self.p = Rc::new(StyleParams::from_theme_with_font(cx, self.font_override));
        }
    }

    /// 编辑后重解析（整篇，v1）+ 清命中。
    fn reparse(&mut self) {
        self.live = parse_to_rows_live(&self.buffer.to_string());
        self.rects.borrow_mut().clear();
    }

    /// 应用一次替换的**核心**（无 `cx`，可单测）：改 Buffer → 选区移到新文本末尾 →
    /// 重解析。UI 包装见 [`Self::insert_edit`]（额外 notify）。
    fn apply_edit(&mut self, range: Range<usize>, text: &str) {
        let s = self.buffer.clip(range.start).min(self.buffer.len());
        let e = self.buffer.clip(range.end).min(self.buffer.len());
        let (s, e) = if s <= e { (s, e) } else { (e, s) };
        self.buffer.apply_edit(s..e, text);
        self.selection = Selection::collapsed(s + text.len());
        self.marked_range = None;
        self.reparse();
    }

    /// 应用一次替换：改 Buffer → 选区移到新文本末尾 → 重解析 + 通知。
    fn insert_edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.apply_edit(range, text);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 坐标换算：源码字节 ↔ 渲染位置
    // ------------------------------------------------------------------

    /// 光标（源码偏移）→ (渲染行下标, 渲染字节)。
    fn caret_target(&self) -> Option<(usize, usize)> {
        if self.live.is_empty() {
            return None;
        }
        let off = self.buffer.clip(self.selection.head());
        for (ix, lr) in self.live.iter().enumerate() {
            if lr.map.is_empty() {
                continue;
            }
            let lo = lr.map[0];
            let hi = *lr.map.last().unwrap();
            if off >= lo && off <= hi {
                // map 单调增：最大 ≤ off 的下标 = 渲染字节（字符起点）。
                let b = lr.map.partition_point(|&s| s <= off).saturating_sub(1);
                let b = b.min(lr.map.len() - 1);
                return Some((ix, b));
            }
        }
        // 兜底：按源码行归属（块落点 = 该块首）。
        let mut chosen = 0usize;
        for (ix, lr) in self.live.iter().enumerate() {
            if lr.source_start <= off {
                chosen = ix;
            } else {
                break;
            }
        }
        Some((chosen, 0))
    }

    /// 该行渲染区间内是否有选中内容；命中返回（渲染起, 渲染止）。
    fn selection_rendered(&self, lr: &LiveRow, sel: (usize, usize)) -> Option<Range<usize>> {
        if lr.map.is_empty() {
            return None;
        }
        let (s, e) = if sel.0 <= sel.1 { sel } else { (sel.1, sel.0) };
        let lo = lr.map[0];
        let hi = *lr.map.last().unwrap();
        if e <= lo || s >= hi {
            return None;
        }
        let rs = lr.map.partition_point(|&v| v <= s).saturating_sub(1);
        let re = lr.map.partition_point(|&v| v <= e).saturating_sub(1).min(lr.map.len() - 1);
        let rs = rs.min(re);
        Some(rs..re)
    }

    // ------------------------------------------------------------------
    // 布局与渲染
    // ------------------------------------------------------------------

    /// 行高（块级，各块不同：标题更高、代码按行数）。
    fn row_height(&self, lr: &LiveRow) -> f32 {
        let p = &self.p;
        match lr.row.kind {
            RowKind::Heading(lvl) => p.heading_metrics(lvl).1.as_f32() + p.para_gap.as_f32(),
            RowKind::Paragraph | RowKind::Bullet | RowKind::Quote => {
                p.para_line.as_f32() + p.para_gap.as_f32()
            }
            RowKind::Blank => p.blank_h.as_f32(),
            RowKind::Rule => p.rule_h.as_f32(),
            RowKind::Image => crate::config::markdown::IMAGE_ROW_DEFAULT_H as f32,
            RowKind::Code => {
                let n = lr.row.payload.as_ref().map_or(1.0, |pl| match &**pl {
                    RowPayload::Code { code_lines, .. } => code_lines.len().max(1) as f32,
                    _ => 1.0,
                });
                n * p.code_line.as_f32()
            }
            RowKind::Table => {
                let rows = lr.row.table().map_or(1.0, |t| (t.rows.len() + 1) as f32);
                rows * (p.para_line.as_f32() + p.para_gap.as_f32())
            }
        }
    }

    /// 行内样式（RunKind → 富样式）。
    fn run_highlight(&self, kind: RunKind) -> HighlightStyle {
        let mut s = HighlightStyle::default();
        match kind {
            RunKind::Bold => {
                s.font_weight = Some(FontWeight::BOLD);
            }
            RunKind::Italic => {
                s.font_style = Some(FontStyle::Italic);
            }
            RunKind::InlineCode => {
                s.background_color = Some(self.p.code_bg);
            }
            RunKind::Strikethrough => {
                s.strikethrough = Some(gpui::StrikethroughStyle {
                    thickness: px(1.0),
                    color: None,
                });
            }
            RunKind::Link | RunKind::Image => {
                s.color = Some(self.p.accent);
                s.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(self.p.accent),
                    wavy: false,
                });
            }
        }
        s
    }

    /// 组装某一文本行的 `StyledText`：行内富样式 + 本行选区底色（非重叠合并）。
    fn styled_row(&self, lr: &LiveRow, sel: Option<Range<usize>>) -> StyledText {
        let runs: Vec<(Range<usize>, HighlightStyle)> = lr
            .row
            .runs
            .iter()
            .map(|(r, k)| (r.clone(), self.run_highlight(*k)))
            .collect();
        let merged = merge_highlights(&runs, sel, self.p.accent.opacity(0.30));
        StyledText::new(lr.row.text.clone()).with_highlights(merged)
    }

    /// 文本行元素（段落/标题/引述/列表）。自持渲染，可注入光标/选区。
    fn text_row_element(
        &mut self,
        window: &mut Window,
        ix: usize,
        lr: &LiveRow,
        top: f32,
        height: f32,
        caret: Option<(usize, usize)>,
        sel: Option<Range<usize>>,
    ) -> AnyElement {
        let p = self.p.clone();
        let font_size = match lr.row.kind {
            RowKind::Heading(level) => p.heading_metrics(level).0,
            _ => p.para_size,
        };
        let text = lr.row.text.clone();
        let text_len = text.len();
        let run = gpui::TextRun {
            len: text_len,
            font: font(p.mono.clone()),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line(text, font_size, &[run], None);
        {
            let mut rects = self.rects.borrow_mut();
            if let Some(r) = rects.iter_mut().find(|r| r.ix == ix) {
                r.top = top;
                r.height = height;
                r.text_len = text_len;
                r.line = Some(line);
            }
        }

        let mut styled = div()
            .h(px(height))
            .w_full()
            .relative()
            .overflow_hidden()
            .px(px(ROW_PAD_X))
            .flex()
            .items_center()
            .text_color(p.fg)
            .text_size(font_size)
            .child(self.styled_row(lr, sel));

        if let Some((c_ix, c_byte)) = caret {
            if c_ix == ix {
                // 光标 x：用本帧刚整形的行（已存命中表）求字符起点。
                let x = {
                    let rects = self.rects.borrow();
                    rects
                        .iter()
                        .find(|r| r.ix == ix)
                        .and_then(|r| r.line.as_ref())
                        .map(|l| l.x_for_index(c_byte).as_f32())
                        .unwrap_or(0.0)
                };
                styled = styled.child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .bottom(px(2.0))
                        .left(px(ROW_PAD_X + x))
                        .w(px(CARET_WIDTH_PX))
                        .rounded(px(1.0))
                        .bg(gpui::rgb(0x0078d4))
                        .into_any_element(),
                );
            }
        }
        styled.into_any_element()
    }

    /// 非文本块元素（代码/表格/图片/分隔线/空行）——v1 简单占位渲染。
    fn block_row_element(
        &mut self,
        _window: &mut Window,
        ix: usize,
        lr: &LiveRow,
        top: f32,
        height: f32,
    ) -> AnyElement {
        let p = self.p.clone();
        {
            let mut rects = self.rects.borrow_mut();
            if let Some(r) = rects.iter_mut().find(|r| r.ix == ix) {
                r.top = top;
                r.height = height;
                r.text_len = 0;
                r.line = None;
            }
        }
        let base = div().relative().w_full().px(px(ROW_PAD_X));
        match lr.row.kind {
            RowKind::Blank => div().h(px(height)).w_full().into_any_element(),
            RowKind::Rule => base
                .h(px(height))
                .flex()
                .items_center()
                .child(div().w_full().h(px(1.0)).bg(p.divider))
                .into_any_element(),
            RowKind::Image => base
                .h(px(height))
                .flex()
                .items_center()
                .child(div().text_color(p.muted_fg).child("图片（实时模式只读占位）"))
                .into_any_element(),
            RowKind::Code => {
                let (code_lines, language) = match &lr.row.payload {
                    Some(pl) => match &**pl {
                        RowPayload::Code { code_lines, language } => {
                            (code_lines.clone(), language.clone())
                        }
                        _ => (Vec::new(), None),
                    },
                    None => (Vec::new(), None),
                };
                let mut col = div()
                    .h(px(height))
                    .w_full()
                    .relative()
                    .overflow_hidden()
                    .font_family(p.mono.clone())
                    .text_size(p.code_size)
                    .text_color(p.fg)
                    .bg(p.code_bg);
                if let Some(lang) = language {
                    col = col.child(
                        div().text_color(p.muted_fg).text_size(px(11.0)).child(lang),
                    );
                }
                for ln in code_lines.iter().take(64) {
                    col = col.child(div().whitespace_nowrap().child(SharedString::from(ln.as_ref())));
                }
                col.into_any_element()
            }
            RowKind::Table => {
                let mut col = div()
                    .h(px(height))
                    .w_full()
                    .relative()
                    .overflow_hidden()
                    .text_size(p.para_size)
                    .text_color(p.fg);
                if let Some(t) = lr.row.table() {
                    col = col
                        .child(
                            div()
                                .whitespace_nowrap()
                                .font_weight(FontWeight::BOLD)
                                .child(format!("| {}", t.header.join(" | "))),
                        )
                        .child(
                            div().whitespace_nowrap().child(
                                t.rows.first().map_or_else(String::new, |r| {
                                    format!(
                                        "| {}",
                                        r.iter()
                                            .map(|s| s.as_ref())
                                            .collect::<Vec<_>>()
                                            .join(" | ")
                                    )
                                }),
                            ),
                        );
                }
                col.into_any_element()
            }
            _ => base.h(px(height)).into_any_element(),
        }
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.live.is_empty() {
            self.rects.borrow_mut().clear();
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(self.p.muted_fg).child("空文档"))
                .into_any_element();
        }
        // 1) 布局：逐块累加高度 → 命中表。
        {
            let mut rects = self.rects.borrow_mut();
            rects.clear();
            rects.reserve(self.live.len());
            let mut y = 0.0f32;
            for (ix, lr) in self.live.iter().enumerate() {
                let h = self.row_height(lr);
                rects.push(RowRect {
                    ix,
                    top: y,
                    height: h,
                    text_left: ROW_PAD_X,
                    text_len: lr.row.text.len(),
                    line: None,
                });
                y += h;
            }
        }
        let caret = self.caret_target();
        let sel = {
            let s = self.selection;
            (s.start(), s.end())
        };
        let entity = cx.entity();
        // 2) 逐块建元素（文本行注入光标/选区；块行占位）。
        let mut blocks: Vec<AnyElement> = Vec::with_capacity(self.live.len());
        for ix in 0..self.live.len() {
            let (top, height) = {
                let r = self.rects.borrow();
                let r = r.get(ix);
                (r.map_or(0.0, |r| r.top), r.map_or(0.0, |r| r.height))
            };
            let lr = self.live[ix].clone();
            if is_text_kind(lr.row.kind) {
                let this_caret = caret.filter(|(c, _)| *c == ix);
                let this_sel = self.selection_rendered(&lr, sel);
                blocks.push(self.text_row_element(window, ix, &lr, top, height, this_caret, this_sel));
            } else {
                blocks.push(self.block_row_element(window, ix, &lr, top, height));
            }
        }
        let mut rows_el = div().w_full().flex().flex_col();
        for b in blocks {
            rows_el = rows_el.child(b);
        }

        let bg = self.p.bg;
        div()
            .id("md-live")
            .debug_selector(|| "md-live".into())
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(bg)
            .child(
                div()
                    .id("md-live-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .overflow_x_hidden()
                    .track_scroll(&self.v_handle)
                    .track_focus(&self.focus_handle)
                    .on_mouse_down(MouseButton::Left, window.listener_for(&entity, Self::on_mouse_down))
                    .on_key_down(window.listener_for(&entity, Self::on_key_down))
                    .child(rows_el),
            )
            .into_any_element()
    }

    /// 点击：命中渲染行 → 源码字节落光标。
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle, cx);
        let local = event.position - self.element_bounds.origin;
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        let cy = local.y.as_f32() + scroll_y;
        let x = local.x.as_f32();
        let source = {
            let rects = self.rects.borrow();
            let hit = rects
                .iter()
                .find(|r| cy >= r.top && cy <= r.top + r.height);
            hit.map(|hit| {
                let byte = hit
                    .line
                    .as_ref()
                    .map(|l| {
                        l.closest_index_for_x(px((x - hit.text_left).max(0.0))).min(hit.text_len)
                    })
                    .unwrap_or(0);
                let lr = &self.live[hit.ix];
                if lr.map.is_empty() {
                    lr.source_start
                } else {
                    lr.rendered_to_source(byte)
                }
            })
        };
        if let Some(source) = source {
            self.selection = Selection::collapsed(source);
            self.marked_range = None;
        }
        cx.notify();
    }

    /// 键盘：控制键（方向/退格/删除/回车/行首行尾）；可打印字符走输入通道。
    fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let k = &event.keystroke;
        let m = k.modifiers;
        // 无修饰可打印字符 → 输入通道（IME / replace_text_in_range）
        if !m.control && !m.alt && k.key_char.is_some() {
            return;
        }
        match k.key.to_ascii_lowercase().as_str() {
            "backspace" => {
                let off = self.selection.start();
                if off > 0 {
                    let prev = self.prev_char(off).unwrap_or(0);
                    self.insert_edit(prev..off, "", cx);
                }
            }
            "delete" => {
                let off = self.selection.end();
                if off < self.buffer.len() {
                    let next = self.next_char(off).unwrap_or(off + 1).max(off + 1);
                    let next = next.min(self.buffer.len());
                    self.insert_edit(off..next, "", cx);
                }
            }
            "enter" => {
                self.insert_edit(self.selection.range(), "\n", cx);
            }
            "left" | "right" | "up" | "down" | "home" | "end" => {
                self.move_caret(&k.key.to_ascii_lowercase());
                cx.notify();
            }
            _ => {}
        }
    }

    fn move_caret(&mut self, key: &str) {
        let sel = self.selection;
        let (s, e) = (sel.start(), sel.end());
        match key {
            "left" => {
                let head = if s != e { s } else { self.selection.head() };
                let target = if head > 0 { self.prev_char(head).unwrap_or(0) } else { 0 };
                self.selection = Selection::collapsed(target);
            }
            "right" => {
                let head = if s != e { e } else { self.selection.head() };
                let target = if head < self.buffer.len() {
                    self.next_char(head).unwrap_or(self.buffer.len()).min(self.buffer.len())
                } else {
                    self.buffer.len()
                };
                self.selection = Selection::collapsed(target);
            }
            "up" | "down" => {
                let row = self.buffer.line_of_offset(self.selection.head());
                let last = self.buffer.line_count().saturating_sub(1);
                let target_row = if key == "up" {
                    row.saturating_sub(1)
                } else {
                    (row + 1).min(last)
                };
                self.selection = Selection::collapsed(self.buffer.line_start(target_row));
            }
            "home" => {
                let row = self.buffer.line_of_offset(self.selection.head());
                self.selection = Selection::collapsed(self.buffer.line_start(row));
            }
            "end" => {
                let row = self.buffer.line_of_offset(self.selection.head());
                self.selection = Selection::collapsed(self.buffer.line_end(row));
            }
            _ => {}
        }
    }

    /// 前一字符边界（不拆 UTF-8）。
    fn prev_char(&self, off: usize) -> Option<usize> {
        if off == 0 {
            return None;
        }
        let upto = self.buffer.text().slice(0..off.min(self.buffer.len()));
        // 每个字符在切片内的起点（逐字符累计字节）→ 最后一个字符的起点。
        upto.chars()
            .scan(0usize, |pos, c| {
                let s = *pos;
                *pos += c.len_utf8();
                Some(s)
            })
            .last()
    }

    /// 下一字符边界（不拆 UTF-8）。
    fn next_char(&self, off: usize) -> Option<usize> {
        if off >= self.buffer.len() {
            return None;
        }
        let from = self.buffer.text().slice(off..);
        Some(off + from.chars().next()?.len_utf8())
    }
}

impl Focusable for MarkdownLivePreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---------------------------------------------------------------------------
// 输入（EntityInputHandler：IME / 剪贴板 / 文本输入）
// ---------------------------------------------------------------------------

impl EntityInputHandler for MarkdownLivePreview {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.len_utf16())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = self.selection;
        Some(UTF16Selection {
            range: self.buffer.byte_to_utf16(sel.start())..self.buffer.byte_to_utf16(sel.end()),
            reversed: sel.reversed(),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| {
            self.buffer.byte_to_utf16(r.start).min(self.buffer.len_utf16())
                ..self.buffer.byte_to_utf16(r.end).min(self.buffer.len_utf16())
        })
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let s = self.buffer.utf16_to_byte(range_utf16.start);
        let e = self.buffer.utf16_to_byte(range_utf16.end);
        Some(self.buffer.text_in_range(s..e).to_string())
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte_range = match range {
            Some(r) => {
                self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                    ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16()))
            }
            None => self.marked_range.clone().unwrap_or_else(|| self.selection.range()),
        };
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_edit(byte_range, &normalized, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte_range = match range_utf16 {
            Some(r) => {
                self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                    ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16()))
            }
            None => self.marked_range.clone().unwrap_or_else(|| self.selection.range()),
        };
        self.insert_edit(byte_range.clone(), new_text, cx);
        if new_text.is_empty() {
            self.marked_range = None;
            self.selection = Selection::collapsed(byte_range.start);
        } else {
            self.marked_range = Some(byte_range.start..byte_range.start + new_text.len());
            self.selection = Selection::collapsed(byte_range.start + new_text.len());
        }
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let _off = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let (row_ix, rendered_byte) = self.caret_target()?;
        let (text_left, x, y, height) = {
            let rects = self.rects.borrow();
            let hit = rects.iter().find(|r| r.ix == row_ix)?;
            let x = hit
                .line
                .as_ref()
                .map(|l| l.x_for_index(rendered_byte).as_f32())
                .unwrap_or(0.0);
            let y = hit.top - (-self.v_handle.offset().y.as_f32()).max(0.0);
            (hit.text_left, x, y, hit.height)
        };
        Some(Bounds::new(
            point(
                self.element_bounds.origin.x + px(text_left + x),
                self.element_bounds.origin.y + px(y),
            ),
            size(px(CARET_WIDTH_PX), px(height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = point - self.element_bounds.origin;
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        let cy = local.y.as_f32() + scroll_y;
        let source = {
            let rects = self.rects.borrow();
            let hit = rects
                .iter()
                .find(|r| cy >= r.top && cy <= r.top + r.height)?;
            let byte = hit
                .line
                .as_ref()
                .map(|l| {
                    l.closest_index_for_x(px((local.x.as_f32() - hit.text_left).max(0.0)))
                        .min(hit.text_len)
                })
                .unwrap_or(0);
            let lr = self.live.get(hit.ix)?;
            let src = if lr.map.is_empty() {
                lr.source_start
            } else {
                lr.rendered_to_source(byte)
            };
            Some(src)
        }?;
        let _ = window;
        Some(self.buffer.byte_to_utf16(source))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let s = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let e = self.buffer.utf16_to_byte(range_utf16.end.min(self.buffer.len_utf16()));
        self.selection = Selection::from_range(s, e);
    }

    fn paste(&mut self, item: gpui::ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert_edit(self.selection.range(), &normalized, cx);
        }
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::TextInputConfiguration {
        gpui::TextInputConfiguration::default()
    }
}

// ---------------------------------------------------------------------------
// 包装元素：捕获边界 + 挂输入
// ---------------------------------------------------------------------------

struct LiveElement {
    view: Entity<MarkdownLivePreview>,
    inner: AnyElement,
}

impl LiveElement {
    fn new(view: Entity<MarkdownLivePreview>, inner: AnyElement) -> Self {
        Self { view, inner }
    }
}

impl IntoElement for LiveElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for LiveElement {
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
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.inner.request_layout(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.view.update(cx, |view, _| view.element_bounds = bounds);
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(window, cx);
        let focus = self.view.read(cx).focus_handle();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
    }
}

impl Render for MarkdownLivePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_styles(cx);
        let inner = self.render_body(window, cx);
        let entity = cx.entity();
        LiveElement::new(entity, inner)
    }
}

#[cfg(test)]
impl MarkdownLivePreview {
    /// 测试：在指定字节偏移插入文本（走真实编辑核心），返回新光标。
    pub(crate) fn test_insert_at(&mut self, off: usize, text: &str) -> usize {
        self.apply_edit(off..off, text);
        self.selection.head()
    }

    /// 测试：直接设定光标（源码字节）。
    pub(crate) fn test_set_caret(&mut self, off: usize) {
        self.selection = Selection::collapsed(off);
    }

    /// 测试：光标对应的 (渲染行, 渲染字节)。
    pub(crate) fn test_caret_target(&self) -> Option<(usize, usize)> {
        self.caret_target()
    }

    /// 测试：当前渲染行数。
    pub(crate) fn test_live_len(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn live_edit_insert_updates_text_caret_and_reparses(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut ed = cx.update(|cx| MarkdownLivePreview::new("# 标题\n\n正文\n", cx));
        assert_eq!(ed.text(), "# 标题\n\n正文\n");
        // 行：标题 / 空行 / 正文 / 尾随空行
        assert_eq!(ed.test_live_len(), 4);

        // 在 "正文" 前（源字节 10）插入 "加" → 光标推进 3 字节，重解析结构不变。
        let caret = ed.test_insert_at(10, "加");
        assert_eq!(caret, 13);
        assert_eq!(ed.text(), "# 标题\n\n加正文\n");
        assert_eq!(ed.test_live_len(), 4);

        // 定位："加正文" 行是 (2,0)，把光标放回行首，映射正确。
        ed.test_set_caret(10);
        assert_eq!(ed.test_caret_target(), Some((2, 0)));
    }

    #[gpui::test]
    fn live_edit_insert_within_bold_updates_source(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 渲染文本 "a **b** c"，在加粗内容里插入 → 源字节同步插入（不影响记号）。
        let mut ed = cx.update(|cx| MarkdownLivePreview::new("a **b** c\n", cx));
        // 源码字节：a0 sp1 *2 *3 b4 *5 *6 sp7 c8 → 'b' 在 4
        let caret = ed.test_insert_at(4, "X");
        assert_eq!(caret, 5);
        assert_eq!(ed.text(), "a **Xb** c\n");
    }
}



