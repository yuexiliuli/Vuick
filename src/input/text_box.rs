//! 单行文本框（`TextBox`）。
//!
//! 与多行的 [`crate::input::text_area::TextArea`] 解耦：
//! - 无换行（粘贴/输入时自动剔除 `\r\n`）；
//! - 无滚动条、无 ↑/↓ 行导航；
//! - 宽度随内容增长，超出容器宽度时被裁剪；
//! - Enter 可选"提交"（发出 [`TextInputEvent`]）。
//!
//! 其余能力与 TextArea 对齐：IME（UTF-16 换算）、撤销/重做、剪贴板复制/剪切/粘贴、
//! 全选、字素/词级导航与删除、双击选词。公共纯逻辑在 [`crate::input::shared`]。

// TextBox 为二进制 crate 中"预留"的组件（暂无调用方），`pub` 不抑制死代码告警，
// 故在本模块内显式关闭；将来接入 UI 后即可移除。
#![allow(dead_code)]

use std::ops::Range;

use gpui::{
    div, fill, hsla, point, px, rgba, size, App, Bounds, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, SharedString, Styled,
    Style, TextRun, UTF16Selection, Window, WrappedLine,
};

use crate::input::shared::*;

// ---------------------------------------------------------------- 控件本体

pub struct TextBox {
    /// 焦点句柄（决定键盘事件是否发给本控件）
    pub focus_handle: FocusHandle,
    /// 占位文本（内容为空时显示、半透明）
    pub placeholder: SharedString,
    /// 当前内容（UTF-8 字节串，不含换行）
    pub content: SharedString,
    /// 当前选区（UTF-8 字节区间）
    pub selected_range: Range<usize>,
    /// 选中方向（向前/向后），用于 Shift+方向键调整选区
    selection_reversed: bool,
    /// IME 组合中的临时选区
    marked_range: Option<Range<usize>>,
    /// Enter 是否发出提交事件
    pub submit_on_enter: bool,
    /// 单次编辑前的快照（用于合并一次手势的多步编辑为一条历史）
    pending: Option<HistoryItem>,
    /// 撤销/重做栈
    history: History,
    /// 是否处于"按住鼠标拖选"状态
    is_selecting: bool,

    /// 绘制阶段记录的本控件布局边界（坐标换算/命中测试用）
    pub last_bounds: Option<Bounds<Pixels>>,
    /// 最后记录的行高（像素）
    pub last_line_height: Pixels,
    /// 上一次 shape 好的（未换行）单行布局，供绘制与命中测试复用
    layout: Option<WrappedLine>,
    /// 为 `layout` 缓存的源内容
    shaped_content: Option<SharedString>,
}

impl TextBox {
    /// 创建一个单行文本框实体（在 App 上下文中以 `cx.new(...)` 包装使用）。
    pub fn new(
        focus_handle: FocusHandle,
        placeholder: SharedString,
        content: SharedString,
    ) -> Self {
        let len = content.len();
        Self {
            focus_handle,
            placeholder,
            content,
            selected_range: len..len,
            selection_reversed: false,
            marked_range: None,
            submit_on_enter: false,
            pending: None,
            history: History::new(),
            is_selecting: false,
            last_bounds: None,
            last_line_height: px(0.),
            layout: None,
            shaped_content: None,
        }
    }

    /// 开启"回车即提交"模式。
    pub fn with_submit_on_enter(mut self) -> Self {
        self.submit_on_enter = true;
        self
    }

    /// 光标位置（选区终点；向后选择时是起点）。
    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// 把光标移到 `new_offset`，并清空选区。
    pub fn move_to(&mut self, new_offset: usize, cx: &mut Context<Self>) {
        self.selected_range = new_offset..new_offset;
        self.selection_reversed = false;
        cx.notify();
    }

    /// 把选区扩展到 `new_offset`（保留方向语义）。
    pub fn select_to(&mut self, new_offset: usize, _: &mut Context<Self>) {
        let start = self.selected_range.start;
        let cursor = self.cursor_offset();
        if new_offset >= start {
            self.selected_range = start..new_offset;
            self.selection_reversed = false;
        } else {
            self.selected_range = new_offset..cursor.max(start);
            self.selection_reversed = true;
        }
    }

    pub fn select_range(&mut self, range: Range<usize>, _: &mut Context<Self>) {
        self.selected_range = range;
        self.selection_reversed = false;
    }

    /// 上一个字素边界（UTF-8 字节偏移）。
    pub fn previous_boundary(&self, offset: usize) -> usize {
        previous_grapheme_boundary(self.content.as_ref(), offset)
    }

    /// 下一个字素边界。
    pub fn next_boundary(&self, offset: usize) -> usize {
        next_grapheme_boundary(self.content.as_ref(), offset)
    }

    /// 上一个词起点（Ctrl+← / 双击选词左边界）。
    pub fn previous_word_boundary(&self, offset: usize) -> usize {
        previous_word_boundary_in(self.content.as_ref(), offset)
    }

    /// 下一个词边界（Ctrl+→ / 双击选词右边界）。
    pub fn next_word_boundary(&self, offset: usize) -> usize {
        next_word_boundary_in(self.content.as_ref(), offset)
    }

    /// 生成词选区（双击选词用）。
    fn word_range(&self, offset: usize) -> Range<usize> {
        let s = self.content.as_ref();
        if s.is_empty() {
            return 0..0;
        }
        let o = offset.min(s.len());
        let on_word = is_word_char(s, o)
            || (o > 0
                && previous_grapheme_boundary(s, o) < o
                && is_word_char(s, previous_grapheme_boundary(s, o)));
        if !on_word {
            return o..o;
        }
        let mut start = o;
        while start > 0 && is_word_char(s, start) {
            start = previous_grapheme_boundary(s, start);
        }
        let mut end = o;
        while end < s.len() && is_word_char(s, end) {
            end = next_grapheme_boundary(s, end);
        }
        start..end
    }

    // ------------------------------------------------------------ 编辑操作

    /// 记录一次编辑前的快照（用于撤销合并）。
    fn begin_edit(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_none() {
            self.pending = Some(HistoryItem {
                text: self.content.to_string(),
                range: self.selected_range.clone(),
            });
        }
        cx.notify();
    }

    /// 结束一次编辑手势，把快照压入历史栈。
    fn commit(&mut self) {
        if let Some(item) = self.pending.take() {
            self.history.push(item);
        }
    }

    /// 替换 `[start, end)` 为 `new_text`；单行会剔除换行。
    fn replace_in_range(&mut self, range: Range<usize>, new_text: &str) {
        let s = self.content.as_ref();
        let range = range.start.min(s.len())..range.end.min(s.len());
        let new_text = new_text.replace("\r\n", " ").replace(['\n', '\r'], " ");
        self.content = SharedString::from(format!(
            "{}{}{}",
            &s[..range.start],
            new_text,
            &s[range.end..]
        ));
        let pos = range.start + new_text.len();
        self.selected_range = pos..pos;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    /// 通用删除入口（记录历史快照）。
    fn delete_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.begin_edit(cx);
        self.replace_in_range(range, "");
        cx.notify();
    }

    /// 键入/IME 替换入口（若为一段连续键入，合并为一条历史）。
    fn typed_replace(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        if self.pending.is_none() {
            self.begin_edit(cx);
        }
        self.replace_in_range(range, new_text);
        cx.notify();
    }

    // ------------------------------------------------------------ 动作处理

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let range = if !self.selected_range.is_empty() {
            self.selected_range.clone()
        } else if cursor > 0 {
            self.previous_boundary(cursor)..cursor
        } else {
            return;
        };
        self.delete_range(range, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let range = if !self.selected_range.is_empty() {
            self.selected_range.clone()
        } else if cursor < self.content.len() {
            cursor..self.next_boundary(cursor)
        } else {
            return;
        };
        self.delete_range(range, cx);
    }

    fn delete_word_backward(&mut self, _: &DeleteWordBackward, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let range = if self.selected_range.is_empty() {
            self.previous_word_boundary(cursor)..cursor
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, cx);
    }

    fn delete_word_forward(&mut self, _: &DeleteWordForward, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let range = if self.selected_range.is_empty() {
            cursor..self.next_word_boundary(cursor)
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn move_prev_word(&mut self, _: &MoveToPreviousWordBoundary, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn move_next_word(&mut self, _: &MoveToNextWordBoundary, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    fn select_prev_word(&mut self, _: &SelectToPreviousWordBoundary, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn select_next_word(&mut self, _: &SelectToNextWordBoundary, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_all_action(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_range(0..self.content.len(), cx);
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .content
            .as_ref()
            .get(self.selected_range.clone())
            .unwrap_or("")
            .to_string();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .content
            .as_ref()
            .get(self.selected_range.clone())
            .unwrap_or("")
            .to_string();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.delete_range(self.selected_range.clone(), cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(clipboard) = cx.read_from_clipboard() {
            let text = clipboard.text().unwrap_or_default();
            self.begin_edit(cx);
            self.replace_in_range(self.selected_range.clone(), &text);
            self.commit();
            cx.notify();
        }
    }

    fn undo(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.history.undo() {
            self.content = SharedString::from(item.text.clone());
            let len = self.content.len();
            let range = item.range.start.min(len)..item.range.end.min(len);
            self.selected_range = range;
            self.selection_reversed = false;
        }
        cx.notify();
    }

    fn redo(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.history.redo() {
            self.content = SharedString::from(item.text.clone());
            let len = self.content.len();
            let range = item.range.start.min(len)..item.range.end.min(len);
            self.selected_range = range;
            self.selection_reversed = false;
        }
        cx.notify();
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        if self.submit_on_enter {
            cx.emit(TextInputEvent);
        }
    }

    // ------------------------------------------------------------ 鼠标交互

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle.clone());
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            match event.click_count {
                0 | 1 => self.move_to(offset, cx),
                2 => {
                    let w = self.word_range(offset);
                    self.select_range(w, cx);
                }
                _ => self.select_range(0..self.content.len(), cx),
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = false;
        self.commit();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    /// 屏幕坐标 → 内容字节偏移（命中测试）。
    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(layout), lh) = (&self.last_bounds, &self.layout, self.last_line_height)
        else {
            return self.cursor_offset();
        };
        if lh <= px(0.) {
            return self.cursor_offset();
        }
        let point_in_line = gpui::point(position.x - bounds.left(), px(0.));
        layout
            .closest_index_for_position(point_in_line, lh)
            .map(|i| i.min(self.content.len()))
            .unwrap_or(self.cursor_offset())
    }
}

impl Focusable for TextBox {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl gpui::EventEmitter<TextInputEvent> for TextBox {}

impl EntityInputHandler for TextBox {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = range_from_utf16(self.content.as_ref(), &range_utf16);
        actual_range.replace(range_to_utf16(self.content.as_ref(), &range));
        Some(self.content.as_ref()[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: range_to_utf16(self.content.as_ref(), &self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|r| range_to_utf16(self.content.as_ref(), r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = range_from_utf16(self.content.as_ref(), &range_utf16);
        let layout = self.layout.as_ref()?;
        let lh = self.last_line_height;
        let pos = layout.position_for_index(range.start.min(layout.len()), lh)?;
        Some(Bounds::new(
            point(bounds.left() + pos.x, bounds.top() + pos.y),
            size(px(2.), lh),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if self.content.is_empty() {
            return Some(0);
        }
        let bounds = self.last_bounds.as_ref()?;
        let layout = self.layout.as_ref()?;
        let p = gpui::point(point.x - bounds.left(), point.y - bounds.top());
        let utf8 = layout.closest_index_for_position(p, self.last_line_height).ok()?;
        Some(offset_to_utf16(self.content.as_ref(), utf8))
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| range_from_utf16(self.content.as_ref(), r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.typed_replace(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| range_from_utf16(self.content.as_ref(), r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        if self.pending.is_none() {
            self.begin_edit(cx);
        }
        self.replace_in_range(range.clone(), new_text);
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| range_from_utf16(self.content.as_ref(), r))
            .map(|nr| nr.start + range.start..nr.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        cx.notify();
    }
}

impl Render for TextBox {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut el = div()
            .id("text-box")
            .key_context("TextInput")
            .track_focus(&self.focus_handle.clone())
            .flex()
            .flex_col()
            .justify_center()
            .min_h(px(24.))
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextBoxElement { input: cx.entity() });
        el = el
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::move_prev_word))
            .on_action(cx.listener(Self::move_next_word))
            .on_action(cx.listener(Self::select_prev_word))
            .on_action(cx.listener(Self::select_next_word))
            .on_action(cx.listener(Self::delete_word_backward))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_all_action))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(|this: &mut TextBox, _: &Undo, window, cx| this.undo(window, cx)))
            .on_action(cx.listener(|this: &mut TextBox, _: &Redo, window, cx| this.redo(window, cx)));
        el
    }
}

// ------------------------------------------------------------- 底层绘制元素

/// 单行文本框的真正排版（shape 未换行单行）与绘制。
pub struct TextBoxElement {
    input: Entity<TextBox>,
}

impl gpui::IntoElement for TextBoxElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextBoxElement {
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
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style::default();
        // 以内容宽度为准增长；高度单行。
        let input = self.input.clone();
        let layout_id = window.request_measured_layout(style, move |_known, _available, window, cx| {
            // 行高优先沿用上一次 paint 记录的逻辑值（首帧兜底取 window.line_height()），
            // 避免量布局阶段的行高放大值造成"测量高度 ≠ 绘制高度"（历史 bug 同源）。
            let lh = {
                let stored = input.read(cx).last_line_height;
                if stored > px(0.) {
                    stored
                } else {
                    window.line_height()
                }
            };
            input.update(cx, |input, _| input.ensure_shaped(window, lh));
            let (w, h) = input
                .read(cx)
                .layout
                .as_ref()
                .map(|l| (l.unwrapped_layout.width, lh))
                .unwrap_or((px(0.), lh));
            size(w, h)
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // 量与绘共用同一份缓存；量布局阶段已 shape，这里补齐首帧/宽度变化的情况
        let lh = {
            let stored = self.input.read(cx).last_line_height;
            if stored > px(0.) {
                stored
            } else {
                window.line_height()
            }
        };
        self.input.update(cx, |input, _| input.ensure_shaped(window, lh));
        ()
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let line_height = window.line_height();
        let (selected_range, cursor_offset) = {
            let input = self.input.read(cx);
            (input.selected_range.clone(), input.cursor_offset())
        };
        let is_focused = focus_handle.is_focused(window);

        // 克隆布局以释放对 `cx` 的借用（WrappedLine 内部是 Arc，克隆开销很小）
        let Some(layout) = self.input.read(cx).layout.clone() else {
            return;
        };

        // 选区高亮（单行只有一个换行片段）
        if !selected_range.is_empty() {
            for rect in selection_rects_in_line(&layout, line_height, selected_range.start, selected_range.end)
            {
                let quad = fill(
                    Bounds::from_corners(
                        point(bounds.left() + rect.origin.x, bounds.top() + rect.origin.y),
                        point(
                            bounds.left() + rect.origin.x + rect.size.width,
                            bounds.top() + rect.origin.y + rect.size.height,
                        ),
                    ),
                    rgba(0x3311ff30),
                );
                window.paint_quad(quad);
            }
        }

        // 绘制单行文本
        layout
            .paint(
                point(bounds.left(), bounds.top()),
                line_height,
                gpui::TextAlign::default(),
                None,
                window,
                cx,
            )
            .unwrap();

        // 光标（聚焦 + 无选区）
        if is_focused && selected_range.is_empty() {
            if let Some(pos) = layout.position_for_index(cursor_offset.min(layout.len()), line_height) {
                let quad = fill(
                    Bounds::new(
                        point(bounds.left() + pos.x, bounds.top() + pos.y),
                        size(px(2.), line_height),
                    ),
                    gpui::blue(),
                );
                window.paint_quad(quad);
            }
        }

        // 记录实体的布局元信息（命中测试/IME 锚定用）
        self.input.update(cx, |input, _| {
            input.last_bounds = Some(bounds);
            input.last_line_height = line_height;
        });
    }
}

impl TextBox {
    /// 内容/行高变化时重新 shape 未换行的单行布局；否则复用缓存。
    /// 在 `&mut App` 的 measure 闭包或 `paint` 阶段均可调用。
    ///
    /// `line_height` 由调用方传入（优先沿用上一次 paint 记录的逻辑行高），
    /// 避免量布局阶段 `window.line_height()` 返回放大值导致"测量值≠绘制值"。
    fn ensure_shaped(&mut self, window: &mut Window, line_height: Pixels) {
        let needs = self.layout.is_none()
            || self.shaped_content.as_ref() != Some(&self.content)
            || self.last_line_height != line_height;
        if !needs {
            return;
        }
        let content = self.content.clone();
        let placeholder = self.placeholder.clone();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let text = if content.is_empty() {
            placeholder
        } else {
            content.clone()
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: if content.is_empty() {
                hsla(0., 0., 0., 0.4)
            } else {
                style.color
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let lines = window
            .text_system()
            .shape_text(
                text,
                font_size,
                std::slice::from_ref(&run),
                None, // 不换行
                None,
            )
            .unwrap_or_default();
        self.layout = lines.into_iter().next();
        self.shaped_content = Some(content);
        self.last_line_height = line_height;
    }
}
