//! `CodeEditor` 视图：自研的文本/代码编辑组件（首版）。
//!
//! 渲染：复用 gpui 原生 `ListState`（统一行高、行级虚拟化、内置竖滚动条）+ 外圈
//! `overflow_x_scroll` 横向容器（`h_handle` + gpui-component 滚动条）——
//! 与 markdown 预览同一套成熟模式。每行 = 行号 gutter + `StyledText`
//! （语法着色 + 选区底色）+ 光标竖条。
//!
//! 输入：
//! - 可打印字符/IME 经 [`EntityInputHandler`]（`EditorElement::paint` 里
//!   `window.handle_input`，与 gpui-component 同款挂钩）；
//! - 控制键（方向/Home/End/PageUp/Backspace/Delete/Enter/Tab）+ Ctrl 组合
//!   （C/X/V/A/Z/Y、词移动/删除）全部在 `on_key_down` 按 `Keystroke` 处理，
//!   不依赖 keymap/action 注册，天然拿到 `Modifiers`（Shift 扩展选区），
//!   也不与组件库按键映射打架；
//! - 鼠标：点击定位 / 拖动扩展 / Shift+点击扩展。
//!
//! 撤销：见 [`crate::editor::undo`]；高亮：见 [`crate::editor::highlight`]。
//!
//! 首版取舍：不放行内折行；光标列按等宽字符近似；无多光标；IME 组合各步为独立撤销单元。

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    div, list, point, px, rgb, size, AnyElement, App, Bounds, ClipboardItem, Context, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, StyledText, TextInputConfiguration, UTF16Selection, Window,
    prelude::FluentBuilder as _,
};
use gpui_component::scroll::ScrollableElement;

use crate::editor::highlight::Highlighter;
use crate::editor::movement;
use crate::editor::undo::{EditIntent, UndoManager};
use crate::editor::{Buffer, Selection};

/// 代码行高亮回看行数（多行注释/字符串上下文不错位）。
const HL_LOOKBACK_LINES: usize = 16;
/// 光标闪烁半周期（ms）。
const BLINK_MS: u64 = 500;
/// 输入后光标暂停闪烁（实心显示，ms）。
const BLINK_PAUSE_MS: u64 = 300;
/// 光标条宽（px）。
const CARET_WIDTH_PX: f32 = 2.;
/// 可见行数下限（首帧视口未知时也至少预取这么多）。
const MIN_VISIBLE_ROWS: usize = 12;
/// 横向滚动右缘留白的列数缓冲。
const HSCROLL_MARGIN_COLS: usize = 4;

/// 编辑器对外事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// 文本被修改（撤销/重做/输入/删除均触发）。
    Edited,
    Focus,
    Blur,
}

/// 行渲染数据（`list` 闭包只读）。
#[derive(Clone)]
struct RowData {
    /// 渲染窗口内的文本（已按横向滚动裁切）。
    text: SharedString,
    /// 样式段（相对 `text` 字节；语法色 + 选区底）。
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    /// 本行是否绘制光标 + 光标 x（相对文本起点，px）。
    caret: Option<f32>,
}

impl RowData {
    fn empty() -> Self {
        Self {
            text: SharedString::new_static(""),
            highlights: Vec::new(),
            caret: None,
        }
    }
}

/// `list` 闭包捕获的只读渲染上下文。
struct RowView {
    lo: usize,
    rows: Vec<RowData>,
    line_height: Pixels,
    gutter_width: Pixels,
    mono: SharedString,
    font_size: Pixels,
    gutter_fg: Hsla,
    caret_color: Hsla,
}

/// 自研代码编辑器（首版）。
pub struct CodeEditor {
    buffer: Buffer,
    selection: Selection,
    undo: UndoManager,
    focus_handle: FocusHandle,
    list_state: ListState,
    h_handle: ScrollHandle,
    highlighter: Highlighter,
    language: Option<String>,

    // —— 字体度量（render 时由主题解析）——
    mono: SharedString,
    font_size: Pixels,
    line_height: Pixels,
    char_width: Pixels,
    metrics_ready: bool,

    // —— 布局状态 ——
    row_count: usize,
    hscroll_cols: usize,
    visible_chars: usize,
    gutter_chars: usize,
    gutter_width_px: Pixels,
    content_w_px: f32,
    uniform_applied: Option<Pixels>,

    // —— 主题色快照 ——
    bg: Hsla,
    fg: Hsla,
    gutter_fg: Hsla,
    sel_bg: Hsla,
    caret_color: Hsla,

    // —— IME ——
    marked_range: Option<Range<usize>>,

    // —— 交互状态 ——
    dragging: bool,
    focused: bool,
    caret_visible: bool,
    caret_pause_until: Option<Instant>,
    blink_epoch: u64,
    focus_registered: bool,
    element_bounds: Bounds<Pixels>,

    // —— 配置 ——
    tab_size: usize,
}

impl CodeEditor {
    /// 创建编辑器（空文档）。
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let buffer = Buffer::default();
        let row_count = buffer.line_count().max(1);
        let list_state = ListState::new(row_count, ListAlignment::Top, px(16.));
        Self {
            buffer,
            selection: Selection::collapsed(0),
            undo: UndoManager::new(),
            focus_handle,
            list_state,
            h_handle: ScrollHandle::new(),
            highlighter: Highlighter::new(),
            language: None,
            mono: SharedString::new_static(""),
            font_size: px(0.),
            line_height: px(0.),
            char_width: px(0.),
            metrics_ready: false,
            row_count,
            hscroll_cols: 0,
            visible_chars: 80,
            gutter_chars: 3,
            gutter_width_px: px(0.),
            content_w_px: 0.,
            uniform_applied: None,
            bg: rgb(0xffffff).into(),
            fg: rgb(0x000000).into(),
            gutter_fg: rgb(0x9aa0a6).into(),
            sel_bg: rgb(0xd0d7de).into(),
            caret_color: rgb(0x000000).into(),
            marked_range: None,
            dragging: false,
            focused: false,
            caret_visible: false,
            caret_pause_until: None,
            blink_epoch: 0,
            focus_registered: false,
            element_bounds: Bounds::default(),
            tab_size: 4,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 设置语法高亮语言（`Some("rust")` / `None` 关闭）。
    pub fn set_language(&mut self, language: Option<&str>) {
        self.language = language.map(|s| s.to_owned());
        self.highlighter.set_language(language);
    }

    /// 设置文档文本（清空撤销/重做与选区）。
    pub fn set_text(&mut self, text: &str) {
        self.buffer = Buffer::new(text);
        self.undo.clear();
        self.selection = Selection::collapsed(0);
        self.hscroll_cols = 0;
        self.marked_range = None;
        self.resync_list();
    }

    /// 在选区处输入文本（等同连续打字：合并撤销、走同一编辑主链）。
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.insert_edit(self.selection.range(), text, EditIntent::Typing, cx);
    }

    pub fn text(&self) -> String {
        self.buffer.to_string()
    }

    pub fn buffer_text(&self) -> &gpui_component::input::Rope {
        self.buffer.text()
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// 光标移动到字节偏移（折叠选区）。
    pub fn set_caret_to(&mut self, offset: usize) {
        self.selection.place(self.buffer.clip(offset));
        self.undo.set_cooperative(false);
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    // -----------------------------------------------------------------
    // 度量与布局
    // -----------------------------------------------------------------

    fn refresh_metrics(&mut self, window: &mut Window, cx: &mut App) {
        let theme = gpui_component::Theme::global(cx);
        let typo = theme.semantic_tokens().typography;
        let colors = theme.semantic_tokens().colors;
        let dark = theme.is_dark();

        let mono = typo.mono.clone();
        let mono_md = typo.mono_md.clone();
        let font_size = mono_md.size;
        let line_height = mono_md.line_height;

        let text_system = window.text_system();
        let font = gpui::font(mono.clone());
        let char_width = text_system
            .ch_advance(text_system.resolve_font(&font), font_size)
            .unwrap_or(px(font_size.as_f32() * 0.6));

        self.mono = mono;
        self.font_size = font_size;
        self.line_height = line_height;
        self.char_width = char_width;
        self.bg = colors.background;
        self.fg = colors.foreground;
        self.gutter_fg = colors.muted_foreground;
        self.caret_color = colors.accent;
        self.sel_bg = colors.accent.opacity(if dark { 0.4 } else { 0.3 });
        self.metrics_ready = true;

        let digits = self.buffer.line_count().max(1).to_string().len();
        self.gutter_chars = digits.max(3);
        self.gutter_width_px = px(self.gutter_chars as f32 * char_width.as_f32() + 14.);
    }

    /// 列表条目数与行数一致（reset 场景：首次/整篇替换/撤销重做）。
    fn resync_list(&mut self) {
        let new_n = self.buffer.line_count().max(1);
        if new_n != self.list_state.item_count() {
            self.list_state.reset(new_n);
            self.uniform_applied = None;
        }
        self.row_count = new_n;
    }

    /// 就地编辑并用 `splice` 增量同步行数（O(log n)，避免大文档整树重建）。
    fn apply_edit_synced(&mut self, range: Range<usize>, text: &str) -> crate::editor::buffer::Edit {
        let old_a = self.buffer.line_of_offset(range.start);
        let old_end = self.buffer.clip(range.end.max(range.start));
        let old_b = self.buffer.line_of_offset(old_end);
        let old_span = old_b - old_a + 1;
        let edit = self.buffer.apply_edit(range, text);
        let new_end = self.buffer.clip(edit.inserted_range().end.max(edit.inserted_range().start));
        let new_b = self.buffer.line_of_offset(new_end);
        let new_span = new_b - old_a + 1;
        if new_span != old_span {
            self.list_state
                .splice(old_a..old_a + old_span, new_span);
        }
        self.row_count = self.buffer.line_count().max(1);
        edit
    }

    /// 统一高度的量程提示：仅在行高/条目结构变化后重挂（避免每帧 O(n)）。
    fn ensure_uniform_hint(&mut self) {
        if !self.metrics_ready {
            return;
        }
        if self.uniform_applied != Some(self.line_height)
            && self.list_state.item_count() > 0
        {
            self.list_state = self
                .list_state
                .clone()
                .with_uniform_item_height(self.line_height);
            self.uniform_applied = Some(self.line_height);
        }
    }

    fn visible_chars_from_width(&self, width_px: f32) -> usize {
        let cw = self.char_width.as_f32().max(1.);
        (((width_px - self.gutter_width_px.as_f32()).max(0.) / cw).ceil() as usize)
            .saturating_add(8)
            .max(80)
    }

    // -----------------------------------------------------------------
    // 行数据
    // -----------------------------------------------------------------

    fn row_data(&mut self, row: usize, show_caret: bool) -> RowData {
        if self.row_count == 0 || self.buffer.line_count() == 0 || row >= self.row_count {
            return RowData::empty();
        }

        let from_char = self.hscroll_cols;
        let win_chars = self.visible_chars.max(1);
        let text = self.buffer.line_text_window(row, from_char, win_chars);
        if text.is_empty() {
            return RowData {
                text,
                highlights: Vec::new(),
                caret: if show_caret && self.selection.head() == self.buffer.line_end(row) {
                    // 空行/行尾：光标画在行首（x=0，若在横向可见区）
                    if self.buffer.char_column(self.selection.head()) >= from_char {
                        Some(0.)
                    } else {
                        None
                    }
                } else {
                    None
                },
            };
        }

        // 语法着色：高亮缓存整行范围，按窗口字节裁剪平移
        let line_start = self.buffer.line_start(row);
        let line_end = self.buffer.line_end(row);
        let win_start = self
            .buffer
            .offset_for_char_column(row, from_char)
            .max(line_start);
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();

        let hl_lines = self
            .highlighter
            .highlight_window(&self.buffer, row, row, HL_LOOKBACK_LINES);
        if let Some(runs) = hl_lines.first() {
            for (r, color) in runs {
                if r.end > win_start && r.start < line_end {
                    let s = r.start.max(win_start).saturating_sub(win_start);
                    let e = r.end.min(line_end).saturating_sub(win_start);
                    if e > s {
                        highlights.push((
                            s..e,
                            HighlightStyle {
                                color: Some(*color),
                                ..Default::default()
                            },
                        ));
                    }
                }
            }
        }

        // 选区底色（当前行与选区交叠）
        if !self.selection.is_collapsed() {
            let sel = self.selection.range();
            let lo = sel.start.max(line_start);
            let hi = sel.end.min(line_end);
            if hi > lo {
                let s = lo.max(win_start).saturating_sub(win_start);
                let e = hi.saturating_sub(win_start);
                if e > s {
                    highlights.push((
                        s..e,
                        HighlightStyle {
                            background_color: Some(self.sel_bg),
                            ..Default::default()
                        },
                    ));
                }
            }
        }

        // 光标
        let caret = if show_caret && self.selection.head() >= line_start && self.selection.head() <= line_end {
            let col = self.buffer.char_column(self.selection.head());
            if col >= from_char {
                Some((col - from_char) as f32 * self.char_width.as_f32())
            } else {
                None
            }
        } else {
            None
        };

        RowData {
            text,
            highlights,
            caret,
        }
    }

    /// 行数据：渲染窗口内（`[lo, hi]`）每行。
    fn build_rows(&mut self, lo: usize, hi: usize, show_caret: bool) -> Vec<RowData> {        if self.row_count == 0 {
            return Vec::new();
        }
        let last = self.row_count.saturating_sub(1);
        let lo = lo.min(last);
        let hi = hi.min(last);
        if hi < lo {
            return Vec::new();
        }
        (lo..=hi).map(|r| self.row_data(r, show_caret)).collect()
    }

    // -----------------------------------------------------------------
    // 光标可见性
    // -----------------------------------------------------------------

    fn caret_shown_now(&self) -> bool {
        if !self.focused {
            return false;
        }
        let paused = self
            .caret_pause_until
            .is_some_and(|p| p.elapsed() < Duration::from_millis(BLINK_PAUSE_MS));
        paused || self.caret_visible
    }

    fn reveal_caret(&mut self) {
        let head = self.selection.head();
        let row = self.buffer.line_of_offset(head);
        self.list_state.scroll_to_reveal_item(row);

        let col = self.buffer.char_column(head);
        let margin = HSCROLL_MARGIN_COLS;
        let right_edge = self.visible_chars.saturating_sub(margin).max(1);
        if col < self.hscroll_cols + margin
            || col >= self.hscroll_cols.saturating_add(right_edge)
        {
            let target = if col < margin {
                col
            } else {
                col.saturating_sub(right_edge)
            };
            self.set_hscroll(target);
        }
    }

    fn set_hscroll(&mut self, cols: usize) {
        self.hscroll_cols = cols;
        let x = cols as f32 * self.char_width.as_f32();
        self.h_handle.set_offset(point(px(-x), px(0.)));
    }

    // -----------------------------------------------------------------
    // 坐标换算（元素本地坐标）
    // -----------------------------------------------------------------

    fn offset_at_point(&self, position: Point<Pixels>) -> usize {
        if self.row_count == 0 || self.line_height.as_f32() <= 0. {
            return 0;
        }
        let lh = self.line_height.as_f32();
        let scroll = self.list_state.logical_scroll_top();
        let content_y =
            scroll.item_ix as f32 * lh + scroll.offset_in_item.as_f32() + position.y.as_f32();
        let row = (content_y / lh).floor().max(0.) as usize;
        let row = row.min(self.row_count.saturating_sub(1));

        let x_text = position.x.as_f32() - self.gutter_width_px.as_f32();
        let col_rel = if x_text > 0. {
            (x_text / self.char_width.as_f32().max(1.)).floor() as usize
        } else {
            0
        };
        let col = col_rel.saturating_add(self.hscroll_cols);
        let line_chars = self.buffer.line_text(row).chars().count();
        let col = col.min(line_chars);
        self.buffer.offset_for_char_column(row, col)
    }
}

// -------------------------------------------------------------------------
// 交互：焦点 / 鼠标 / 键盘 / 编辑
// -------------------------------------------------------------------------

#[cfg(test)]
impl CodeEditor {
    /// 测试用度量：`(char_width, line_height, gutter_width)`（px 的 f32）。
    pub(crate) fn test_metrics(&self) -> (f32, f32, f32) {
        (
            self.char_width.as_f32(),
            self.line_height.as_f32(),
            self.gutter_width_px.as_f32(),
        )
    }
}

impl CodeEditor {
    fn on_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = true;
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
        cx.emit(EditorEvent::Focus);
        self.start_blink(cx);
    }

    fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = false;
        self.undo.set_cooperative(false);
        self.marked_range = None;
        cx.emit(EditorEvent::Blur);
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| loop {
            let elapsed = this
                .update(cx, |this, _| {
                    this.caret_pause_until
                        .map_or(u128::MAX, |p| p.elapsed().as_millis())
                })
                .unwrap_or(u128::MAX);
            if elapsed >= BLINK_PAUSE_MS as u128 {
                let _ = this.update(cx, |this, cx| {
                    this.caret_visible = !this.caret_visible;
                    cx.notify();
                });
            }
            cx.background_executor()
                .timer(Duration::from_millis(BLINK_MS))
                .await;
            let keep = this
                .update(cx, |this, _| this.focused && epoch == this.blink_epoch)
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    fn kick_caret(&mut self) {
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window, cx);
        self.undo.set_cooperative(false);
        let local = event.position - self.element_bounds.origin;
        let offset = self.offset_at_point(local);
        if event.modifiers.shift {
            self.selection.set_head(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        } else {
            self.selection.place(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        }
        self.dragging = true;
        self.kick_caret();
        self.reveal_caret();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let local = event.position - self.element_bounds.origin;
        let offset = self.offset_at_point(local);
        self.selection.set_head(offset);
        self.selection.remember_column(self.buffer.char_column(offset));
        self.kick_caret();
        self.reveal_caret();
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.dragging = false;
    }

    /// 统一按键：非文本键在此完成。
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let m = k.modifiers;

        // 无修饰键的可打印字符 → 交给 IME/InputHandler（此处不收）
        if !m.control && !m.alt && !m.platform && k.key_char.is_some() && k.key.len() <= 1 {
            return;
        }

        // Ctrl 组合
        if m.control {
            match k.key.to_ascii_lowercase().as_str() {
                "c" => return self.copy(window, cx),
                "x" => return self.cut(window, cx),
                "v" => return self.paste(window, cx),
                "a" => return self.select_all(cx),
                "z" => return self.undo(cx),
                "y" => return self.redo(cx),
                "left" => {
                    if m.shift {
                        return self.select_word_left(cx);
                    }
                    return self.move_word_left(cx);
                }
                "right" => {
                    if m.shift {
                        return self.select_word_right(cx);
                    }
                    return self.move_word_right(cx);
                }
                "backspace" => return self.backspace_word(cx),
                "delete" => return self.delete_word_forward(cx),
                _ => {}
            }
        }

        let select = m.shift;
        match k.key.to_ascii_lowercase().as_str() {
            "left" => return self.arrow(select, -1, 0, cx),
            "right" => return self.arrow(select, 1, 0, cx),
            "up" => return self.arrow(select, 0, -1, cx),
            "down" => return self.arrow(select, 0, 1, cx),
            "home" => return self.move_line_start(select, cx),
            "end" => return self.move_line_end(select, cx),
            "pageup" => return self.page(select, -1, cx),
            "pagedown" => return self.page(select, 1, cx),
            "backspace" => return self.backspace(cx),
            "delete" => return self.delete_forward(cx),
            "enter" | "return" => return self.insert_newline(cx),
            "tab" => return self.tab(m.shift, cx),
            _ => {}
        }
    }

    /// 统一的移动+选区应用点。
    fn apply_move(&mut self, select: bool, new_head: usize, preferred: Option<u32>, cx: &mut Context<Self>) {
        if select {
            let col = self.buffer.char_column(new_head);
            self.selection.set_head(new_head);
            if preferred.is_some() {
                self.selection.remember_column(col);
            }
        } else {
            self.selection.move_cursor(new_head, preferred);
        }
        self.kick_caret();
        self.reveal_caret();
        cx.notify();
    }

    fn arrow(&mut self, select: bool, dc: isize, dr: isize, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        if dr != 0 {
            let (off, col) = movement::move_vertical(
                &self.buffer,
                self.selection.head(),
                dr,
                self.selection.preferred_column,
            );
            self.apply_move(select, off, Some(col), cx);
        } else {
            // 水平：有选区且不按住 Shift → 先收敛到选区起点/终点
            let head = self.selection.head();
            let mut new_head = if dc < 0 {
                movement::char_left(self.buffer.text(), head)
            } else {
                movement::char_right(self.buffer.text(), head)
            };
            if !select && !self.selection.is_collapsed() {
                new_head = if dc < 0 {
                    self.selection.start()
                } else {
                    self.selection.end()
                };
            }
            self.apply_move(select, new_head, None, cx);
        }
    }

    fn move_word_left(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_left(self.buffer.text(), self.selection.head());
        self.apply_move(false, new, None, cx);
    }

    fn move_word_right(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_right(self.buffer.text(), self.selection.head());
        self.apply_move(false, new, None, cx);
    }

    fn select_word_left(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_left(self.buffer.text(), self.selection.head());
        self.apply_move(true, new, None, cx);
    }

    fn select_word_right(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_right(self.buffer.text(), self.selection.head());
        self.apply_move(true, new, None, cx);
    }

    fn move_line_start(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::to_line_start(&self.buffer, self.selection.head());
        self.apply_move(select, new, None, cx);
    }

    fn move_line_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::to_line_end(&self.buffer, self.selection.head());
        self.apply_move(select, new, None, cx);
    }

    fn page_row_count(&self) -> usize {
        let vh = self.list_state.viewport_bounds().size.height.as_f32();
        let lh = self.line_height.as_f32().max(1.);
        ((vh / lh).floor() as usize).saturating_sub(1).max(1)
    }

    fn page(&mut self, select: bool, dir: isize, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let rows = self.page_row_count() as isize;
        let (off, col) = movement::move_vertical(
            &self.buffer,
            self.selection.head(),
            dir * rows,
            self.selection.preferred_column,
        );
        self.apply_move(select, off, Some(col), cx);
    }

    // -----------------------------------------------------------------
    // 编辑主链
    // -----------------------------------------------------------------

    fn insert_edit(&mut self, range: Range<usize>, text: &str, intent: EditIntent, cx: &mut Context<Self>) {
        self.undo
            .start_tx(intent, (self.selection.anchor, self.selection.head));
        let edit = self.apply_edit_synced(range, text);
        let head = edit.inserted_range().end;
        self.undo.push_edit(edit);
        self.selection.place(head);
        // 打字/退格/删除保留合作（连续合并）；换行/原子编辑断开
        self.undo.set_cooperative(match intent {
            EditIntent::Atomic => false,
            _ => !text.contains(['\n', '\r']),
        });
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head == 0 {
            return;
        }
        let start = movement::char_left(self.buffer.text(), head);
        self.insert_edit(start..head, "", EditIntent::Backspace, cx);
    }

    fn backspace_word(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head == 0 {
            return;
        }
        let start = movement::word_left(self.buffer.text(), head);
        self.insert_edit(start..head, "", EditIntent::Backspace, cx);
    }

    fn delete_forward(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head >= self.buffer.len() {
            return;
        }
        let end = movement::char_right(self.buffer.text(), head);
        self.insert_edit(head..end, "", EditIntent::Delete, cx);
    }

    fn delete_word_forward(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head >= self.buffer.len() {
            return;
        }
        let end = movement::word_right(self.buffer.text(), head);
        self.insert_edit(head..end, "", EditIntent::Delete, cx);
    }

    fn delete_selection_atomic(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        if range.is_empty() {
            return;
        }
        self.insert_edit(range, "", EditIntent::Atomic, cx);
    }

    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        let at = self.selection.head();
        let row = self.buffer.line_of_offset(at);
        let indent: String = self
            .buffer
            .line_text(row)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let text = format!("\n{indent}");
        let range = if self.selection.is_collapsed() {
            at..at
        } else {
            self.selection.range()
        };
        self.insert_edit(range, &text, EditIntent::Atomic, cx);
    }

    fn tab(&mut self, outdent: bool, cx: &mut Context<Self>) {
        if outdent {
            return self.outdent(cx);
        }
        self.insert_edit(self.selection.range(), "\t", EditIntent::Atomic, cx);
    }

    fn outdent(&mut self, cx: &mut Context<Self>) {
        let at = self.selection.head();
        let row = self.buffer.line_of_offset(at);
        let ls = self.buffer.line_start(row);
        let line = self.buffer.line_text(row);
        let prefix: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if prefix.is_empty() {
            return;
        }
        let remove_bytes = if prefix.starts_with('\t') {
            '\t'.len_utf8()
        } else {
            let spaces = prefix
                .chars()
                .take(self.tab_size)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            spaces.min(ls + line.len() - ls).max(1)
        };
        let end = (ls + remove_bytes).min(self.buffer.line_end(row) - ls + ls);
        let range = ls..end.max(ls).min(self.buffer.line_end(row));
        if range.end <= range.start {
            return;
        }
        self.insert_edit(range, "", EditIntent::Atomic, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.selection = Selection::from_range(0, self.buffer.len());
        self.kick_caret();
        cx.notify();
    }

    fn copy(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            let text = self.buffer.text_in_range(self.selection.range());
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    fn cut(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            let text = self.buffer.text_in_range(self.selection.range());
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            self.delete_selection_atomic(cx);
        }
    }

    fn paste(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            if let Some(text) = item.text() {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                self.insert_edit(self.selection.range(), &normalized, EditIntent::Atomic, cx);
            }
        }
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.undo() else {
            return;
        };
        for edit in tx.edits.iter().rev() {
            self.buffer.undo_edit(edit);
        }
        self.selection = Selection::from_range(tx.before.0, tx.before.1);
        self.resync_list();
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.redo() else {
            return;
        };
        for edit in tx.edits.iter() {
            self.buffer.redo_edit(edit);
        }
        self.selection = Selection::from_range(tx.after.0, tx.after.1);
        self.resync_list();
        self.kick_caret();
        self.reveal_caret();
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }
}

// -------------------------------------------------------------------------
// IME / 文本输入
// -------------------------------------------------------------------------

impl EntityInputHandler for CodeEditor {
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

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
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
            Some(r) => self.buffer.utf16_to_byte(r.start)..self.buffer.utf16_to_byte(r.end),
            None => self
                .marked_range
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        self.insert_edit(byte_range, text, EditIntent::Typing, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte_range = match range_utf16 {
            Some(r) => self.buffer.utf16_to_byte(r.start)..self.buffer.utf16_to_byte(r.end),
            None => self.selection.range(),
        };
        self.insert_edit(byte_range.clone(), new_text, EditIntent::Atomic, cx);
        self.marked_range = Some(byte_range.start..byte_range.start + new_text.len());
        if let Some(sel) = new_selected_range {
            let s = self.buffer.utf16_to_byte(sel.start.min(self.buffer.len_utf16()));
            let e = self.buffer.utf16_to_byte(sel.end.min(self.buffer.len_utf16()));
            self.selection = Selection::from_range(s, e);
            self.kick_caret();
        }
        cx.notify();
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let off = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let row = self.buffer.line_of_offset(off);
        let col = self.buffer.char_column(off);
        let lh = self.line_height.as_f32().max(1.);
        let scroll = self.list_state.logical_scroll_top();
        let y = (row as f32 - scroll.item_ix as f32) * lh - scroll.offset_in_item.as_f32();
        let x = self.gutter_width_px.as_f32()
            + (col as f32 - self.hscroll_cols as f32) * self.char_width.as_f32().max(1.);
        Some(Bounds::new(
            point(
                element_bounds.origin.x + px(x),
                element_bounds.origin.y + px(y),
            ),
            size(px(CARET_WIDTH_PX), px(lh)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = point - self.element_bounds.origin;
        Some(self.buffer.byte_to_utf16(self.offset_at_point(local)))
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
        self.kick_caret();
    }

    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert_edit(self.selection.range(), &normalized, EditIntent::Atomic, cx);
        }
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
}

// -------------------------------------------------------------------------
// Render / Focusable / 事件
// -------------------------------------------------------------------------

impl Focusable for CodeEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<EditorEvent> for CodeEditor {}

impl Render for CodeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();

        // 首次渲染注册焦点事件
        if !self.focus_registered {
            let _ = cx.on_focus(&self.focus_handle, window, Self::on_focus);
            let _ = cx.on_blur(&self.focus_handle, window, Self::on_blur);
            self.focus_registered = true;
        }

        self.refresh_metrics(window, cx);
        self.ensure_uniform_hint();

        // 可见字符数（用上一帧元素宽，首帧退回窗口宽）
        let avail_w = if self.element_bounds.size.width > px(0.) {
            self.element_bounds.size.width.as_f32()
        } else {
            window.bounds().size.width.as_f32()
        };
        self.visible_chars = self.visible_chars_from_width(avail_w);

        // 高亮语言（主题明暗同步）
        let dark = gpui_component::Theme::global(cx).is_dark();
        self.highlighter.set_dark(dark);

        // 行级数据窗口（可见 + 过扫）
        let lh = self.line_height.as_f32().max(1.);
        let vis_top = self.list_state.logical_scroll_top().item_ix;
        let view_h = self.list_state.viewport_bounds().size.height.as_f32();
        let nrows = ((view_h / lh).ceil() as usize).max(MIN_VISIBLE_ROWS);
        let lo = vis_top.saturating_sub(nrows);
        let last = self.row_count.saturating_sub(1);
        let hi = vis_top.saturating_add(nrows * 2).min(last);
        let show_caret = self.caret_shown_now();
        let rows = self.build_rows(lo, hi, show_caret);

        // 横向内容宽度（单调上修：只增不减，避免 List 宽度抖动 → 清量程）
        let mut max_cols = self.content_w_px / self.char_width.as_f32().max(1.);
        for rd in &rows {
            let cols = rd.text.chars().count() as f32 + self.hscroll_cols as f32;
            max_cols = max_cols.max(cols);
        }
        let content_w_px = (max_cols * self.char_width.as_f32().max(1.))
            .max(avail_w)
            .max(1.);
        self.content_w_px = content_w_px;

        let rv = RowView {
            lo,
            rows,
            line_height: self.line_height,
            gutter_width: self.gutter_width_px,
            mono: self.mono.clone(),
            font_size: self.font_size,
            gutter_fg: self.gutter_fg,
            caret_color: self.caret_color,
        };
        let rv = std::rc::Rc::new(rv);
        let list_state = self.list_state.clone();
        let bg = self.bg;
        let fg = self.fg;

        let content = list(list_state.clone(), move |ix, _window, _cx| {
            let rd = if ix >= rv.lo && ix - rv.lo < rv.rows.len() {
                &rv.rows[ix - rv.lo]
            } else {
                &RowData::empty()
            };
            row_element(rd, &rv, ix).into_any_element()
        });

        let inner = div()
            .id("ce-list-wrap")
            .debug_selector(|| "ce-list-wrap".into())
            .w(px(content_w_px))
            .min_w_full()
            .h_full()
            .child(content);
        let harea = div()
            .id("ce-harea")
            .debug_selector(|| "ce-harea".into())
            .size_full()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.h_handle)
            .child(inner);

        let core = div()
            .id("code-editor")
            .debug_selector(|| "code-editor".into())
            .size_full()
            .relative()
            .bg(bg)
            .text_color(fg)
            .key_context("Input")
            .track_focus(&self.focus_handle)
            .on_key_down(window.listener_for(&entity, CodeEditor::on_key_down))
            .on_mouse_down(MouseButton::Left, window.listener_for(&entity, CodeEditor::on_mouse_down))
            .on_mouse_move(window.listener_for(&entity, CodeEditor::on_mouse_move))
            .on_mouse_up(MouseButton::Left, window.listener_for(&entity, CodeEditor::on_mouse_up))
            .child(harea)
            .vertical_scrollbar(&list_state)
            .horizontal_scrollbar(&self.h_handle);

        EditorElement::new(entity, core.into_any_element())
    }
}

/// 行元素：gutter + 文本 + 光标。
fn row_element(rd: &RowData, rv: &RowView, ix: usize) -> gpui::Div {
    let number = SharedString::from((ix + 1).to_string());
    div()
        .h(rv.line_height)
        .relative()
        .w_full()
        .child(
            div()
                .absolute()
                .top(px(0.))
                .bottom(px(0.))
                .left(px(0.))
                .w(rv.gutter_width)
                .flex()
                .items_center()
                .justify_end()
                .pr(px(6.))
                .text_color(rv.gutter_fg)
                .text_size(rv.font_size)
                .font_family(rv.mono.clone())
                .child(number),
        )
        .child(
            div()
                .absolute()
                .top(px(0.))
                .left(rv.gutter_width)
                .h(rv.line_height)
                .font_family(rv.mono.clone())
                .text_size(rv.font_size)
                .child(styled_row_text(rd, rv)),
        )
        .when(rd.caret.is_some(), |this| {
            this.child(
                div()
                    .absolute()
                    .top(px(0.))
                    .left(rv.gutter_width + px(rd.caret.unwrap_or(0.)))
                    .w(px(CARET_WIDTH_PX))
                    .h(rv.line_height)
                    .bg(rv.caret_color),
            )
        })
}

fn styled_row_text(rd: &RowData, _rv: &RowView) -> StyledText {
    StyledText::new(rd.text.clone()).with_highlights(rd.highlights.clone())
}

/// 包装元素：把内部 UI 委托给 gpui 布局/绘制，同时在 paint 阶段挂上
/// `hand_input`（IME/文本输入），并回写元素边界供坐标换算。
struct EditorElement {
    editor: Entity<CodeEditor>,
    inner: AnyElement,
}

impl EditorElement {
    fn new(editor: Entity<CodeEditor>, inner: AnyElement) -> Self {
        Self { editor, inner }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
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
        self.editor
            .update(cx, |editor, _| editor.element_bounds = bounds);
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
        let focus = self.editor.read(cx).focus_handle();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
    }
}
