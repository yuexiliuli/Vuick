//! 单行文本输入框（#12）：替代 gpui_component 的搜索框 `Input`/`InputState`/`InputEvent`。
//!
//! 自研实现：`InputState`（实体：文本/占位符/焦点/光标）+ `EntityInputHandler`
//! （IME/文本输入）+ 自定义 [`InputElement`]（paint 期挂 `window.handle_input`、画文本/
//! 占位符/光标）。键盘（Backspace/Delete/方向/Home/End/Ctrl 组合）走本地 `crate::actions`
//! 的 "Input" keymap（#10 自绑）：元素 `key_context("Input")` + `on_action` 接线；
//! 未实现的组合（如 Undo/Redo）按 gpui 分发语义静默不处理即可。

use std::ops::Range;

use gpui::{
    div, point, size, AnyElement, App, Bounds, ClipboardItem, Context, Element, ElementId, Entity,
    EntityInputHandler, EventEmitter, Focusable, FocusHandle, GlobalElementId, Hsla,
    InteractiveElement as _, IntoElement, LayoutId, MouseButton, MouseDownEvent, Pixels, Point,
    Render, SharedString, Styled as _, TextInputConfiguration, TextRun, UTF16Selection, Window, px,
};

use crate::actions::{
    Backspace, Copy, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, MoveEnd, MoveHome,
    MoveLeft, MoveRight, MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, SelectAll,
};
use crate::config::layout::DEFAULT_FONT_SIZE;
use crate::ui::theme::Theme;

/// 单行输入框事件（仅实现 App 用到的最小面）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// 文本内容变化（搜索框变化 → 刷新搜索结果）。
    Change,
}

/// 左右内边距（文本/光标相对元素左缘）。
const PAD_PX: f32 = 2.0;
/// 光标宽。
const CARET_PX: f32 = 1.0;

/// 单行输入框状态（实体）：文本 + 占位符 + 焦点 + 光标（字节区间）+ 元素边界。
pub struct InputState {
    text: String,
    placeholder: SharedString,
    focus_handle: FocusHandle,
    selection: Range<usize>,
    element_bounds: Bounds<Pixels>,
}

impl InputState {
    /// 创建输入框（原 gpui_component 签名，`window` 不再需要）。
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            placeholder: SharedString::default(),
            focus_handle: cx.focus_handle(),
            selection: 0..0,
            element_bounds: Bounds::default(),
        }
    }

    /// 占位符（为空文本且未聚焦时显示）。
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// 初始文本。
    pub fn default_value(mut self, value: impl Into<String>) -> Self {
        self.text = value.into();
        let len = self.text.len();
        self.selection = len..len;
        self
    }

    /// 当前文本。
    pub fn value(&self) -> SharedString {
        SharedString::from(self.text.clone())
    }

    /// 整体替换文本（程序化设置，发 `Change`）。
    /// 测试/程序化调用专用（与 gpui_component `InputState::set_value` 同 API）。
    #[allow(dead_code)]
    pub fn set_value(
        &mut self,
        value: impl Into<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text = value.into();
        let len = self.text.len();
        self.selection = len..len;
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// 整体替换文本（粘贴到搜索框路径，发 `Change`）。
    pub fn replace(&mut self, text: impl Into<String>, _window: &mut Window, cx: &mut Context<Self>) {
        self.text = text.into();
        let len = self.text.len();
        self.selection = len..len;
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn focused(&self, window: &Window, cx: &App) -> bool {
        window.focused(cx) == Some(self.focus_handle.clone())
    }

    /// 在指定字节区间替换文本并收敛光标（IME/编辑共用），发 `Change`。
    fn edit_range(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        self.text.replace_range(start..end, new_text);
        let caret = start + new_text.len();
        self.selection = caret..caret;
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    // -- 光标/编辑原语（字节区间，始终落在 char 边界）--

    fn prev_char(&self, index: usize) -> usize {
        self.text[..index.min(self.text.len())]
            .char_indices()
            .next_back()
            .map(|(b, _)| b)
            .unwrap_or(0)
    }

    fn next_char(&self, index: usize) -> usize {
        if index >= self.text.len() {
            return self.text.len();
        }
        index
            + self.text[index..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0)
    }

    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let index = index.min(self.text.len());
        self.selection = index..index;
        cx.notify();
    }

    fn move_delta(&mut self, delta: isize, cx: &mut Context<Self>) {
        let cur = self.selection.end;
        let next = if delta < 0 {
            self.prev_char(cur)
        } else {
            self.next_char(cur)
        };
        self.move_to(next, cx);
    }

    fn move_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        let mut cur = self.selection.end;
        if forward {
            while cur < self.text.len() && self.text[cur..].starts_with(char::is_whitespace) {
                cur = self.next_char(cur);
            }
            while cur < self.text.len() && !self.text[cur..].starts_with(char::is_whitespace) {
                cur = self.next_char(cur);
            }
        } else {
            while cur > 0 && self.text[..cur].ends_with(char::is_whitespace) {
                cur = self.prev_char(cur);
            }
            while cur > 0 && !self.text[..cur].ends_with(char::is_whitespace) {
                cur = self.prev_char(cur);
            }
        }
        self.move_to(cur, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let len = self.text.len();
        self.selection = 0..len;
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if self.selection.start < self.selection.end
            && let Some(text) = self.text.get(self.selection.clone())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    /// 删除：`backward` 退格 / `forward` 删除；有选区则删选区。
    fn delete(&mut self, backward: bool, cx: &mut Context<Self>) {
        let sel = self.selection.clone();
        if sel.start < sel.end {
            self.edit_range(sel, "", cx);
        } else if backward {
            let start = self.prev_char(sel.start);
            self.edit_range(start..sel.start, "", cx);
        } else {
            let end = self.next_char(sel.end);
            self.edit_range(sel.end..end, "", cx);
        }
    }

    fn delete_word(&mut self, backward: bool, cx: &mut Context<Self>) {
        let sel = self.selection.clone();
        if sel.start < sel.end {
            self.edit_range(sel, "", cx);
        } else if backward {
            let mut cur = sel.start;
            while cur > 0 && self.text[..cur].ends_with(char::is_whitespace) {
                cur = self.prev_char(cur);
            }
            while cur > 0 && !self.text[..cur].ends_with(char::is_whitespace) {
                cur = self.prev_char(cur);
            }
            self.edit_range(cur..sel.start, "", cx);
        } else {
            let mut cur = sel.end;
            while cur < self.text.len() && self.text[cur..].starts_with(char::is_whitespace) {
                cur = self.next_char(cur);
            }
            while cur < self.text.len() && !self.text[cur..].starts_with(char::is_whitespace) {
                cur = self.next_char(cur);
            }
            self.edit_range(sel.end..cur, "", cx);
        }
    }

    /// 点按 → 字节偏移（文本前缀宽度二分；用克隆规避对 `self` 的双重可变借用）。
    fn index_at_x_impl(&self, x: Pixels, window: &mut Window) -> usize {
        if self.text.is_empty() {
            return 0;
        }
        let run = TextRun {
            len: self.text.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(SharedString::from(self.text.clone()), px(DEFAULT_FONT_SIZE), &[run], None);
        let x_f = x.as_f32();
        let text = self.text.as_str();
        let mut cursor = 0usize;
        for (i, c) in text.char_indices() {
            let next = i + c.len_utf8();
            if line.x_for_index(next).as_f32() <= x_f {
                cursor = next;
            } else {
                break;
            }
        }
        cursor
    }

    /// 光标矩形 x（元素原点 + pad + 前缀宽），供 IME `bounds_for_range`。
    fn caret_px(&self, byte: usize, element_bounds: Bounds<Pixels>, window: &mut Window) -> Pixels {
        let prefix = &self.text[..byte.min(self.text.len())];
        let pad = element_bounds.origin.x + px(PAD_PX);
        if prefix.is_empty() {
            return pad;
        }
        let run = TextRun {
            len: prefix.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let w = window
            .text_system()
            .shape_line(
                SharedString::from(prefix.to_string()),
                px(DEFAULT_FONT_SIZE),
                &[run],
                None,
            )
            .width();
        pad + w
    }

    // -- 事件回调 --

    fn on_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.delete(true, cx);
    }

    fn on_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete(false, cx);
    }

    fn on_delete_prev_word(
        &mut self,
        _: &DeleteToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(true, cx);
    }

    fn on_delete_next_word(
        &mut self,
        _: &DeleteToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(false, cx);
    }

    fn on_move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_delta(-1, cx);
    }

    fn on_move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_delta(1, cx);
    }

    fn on_move_home(&mut self, _: &MoveHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn on_move_end(&mut self, _: &MoveEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn on_move_to_start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn on_move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn on_move_prev_word(
        &mut self,
        _: &MoveToPreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_word(false, cx);
    }

    fn on_move_next_word(
        &mut self,
        _: &MoveToNextWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_word(true, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    fn on_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy(cx);
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        let x = (ev.position.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        let index = self.index_at_x_impl(x, window);
        self.move_to(index, cx);
    }
}

impl EventEmitter<InputEvent> for InputState {}

impl Focusable for InputState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// -------------------------------------------------------------------------
// 文本输入（IME）——单行，整行是一个 UTF-16 区段
// -------------------------------------------------------------------------

impl EntityInputHandler for InputState {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(utf16_len(self.text.as_str()))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = self.selection.clone();
        Some(UTF16Selection {
            range: byte_to_utf16(self.text.as_str(), sel.start)
                ..byte_to_utf16(self.text.as_str(), sel.end),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let s = utf16_to_byte(self.text.as_str(), range_utf16.start);
        let e = utf16_to_byte(self.text.as_str(), range_utf16.end);
        self.text.get(s..e).map(str::to_string)
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
                utf16_to_byte(self.text.as_str(), r.start)..utf16_to_byte(self.text.as_str(), r.end)
            }
            None => self.selection.clone(),
        };
        self.edit_range(byte_range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 搜索框不画组合下划线，与 replace_text_in_range 等价。
        let byte_range = match range {
            Some(r) => {
                utf16_to_byte(self.text.as_str(), r.start)..utf16_to_byte(self.text.as_str(), r.end)
            }
            None => self.selection.clone(),
        };
        self.edit_range(byte_range, new_text, cx);
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let byte = utf16_to_byte(self.text.as_str(), range_utf16.start.min(range_utf16.end));
        let x = self.caret_px(byte, element_bounds, window);
        let line_h = line_height();
        Some(Bounds::new(
            point(x, element_bounds.origin.y + (element_bounds.size.height - line_h) / 2.),
            size(px(CARET_PX), line_h),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let x = (point.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        Some(byte_to_utf16(
            self.text.as_str(),
            self.index_at_x_impl(x, window),
        ))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let s = utf16_to_byte(self.text.as_str(), range_utf16.start);
        let e = utf16_to_byte(self.text.as_str(), range_utf16.end);
        self.selection = s..e;
        cx.notify();
    }

    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.edit_range(self.selection.clone(), &normalized, cx);
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

fn line_height() -> Pixels {
    px(DEFAULT_FONT_SIZE * 1.4)
}

// -------------------------------------------------------------------------
// 渲染：paint 期挂输入句柄 + 画文本/占位符/光标
// -------------------------------------------------------------------------

/// 标记类型：保持原 gpui_component API 形状，`Input::new(&state)` 返回实体（视图即元素）。
pub struct Input;

impl Input {
    #[allow(clippy::new_ret_no_self)] // API 兼容垫片：返回的就是传入的实体，非新建
    pub fn new(state: &Entity<InputState>) -> Entity<InputState> {
        state.clone()
    }
}

impl Render for InputState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let inner = div()
            .id("ui-input-box")
            .key_context("Input")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_prev_word))
            .on_action(cx.listener(Self::on_delete_next_word))
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_move_home))
            .on_action(cx.listener(Self::on_move_end))
            .on_action(cx.listener(Self::on_move_to_start))
            .on_action(cx.listener(Self::on_move_to_end))
            .on_action(cx.listener(Self::on_move_prev_word))
            .on_action(cx.listener(Self::on_move_next_word))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .w_full()
            .h(px(24.))
            .overflow_hidden();
        InputElement {
            entity: cx.entity(),
            inner: inner.into_any_element(),
        }
    }
}

/// 包装元素：内部 UI 走 gpui 布局，paint 期挂 `handle_input` 并画文本/占位符/光标。
struct InputElement {
    entity: Entity<InputState>,
    inner: AnyElement,
}

impl IntoElement for InputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("ui-input".into()))
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
    ) -> Self::PrepaintState {
        self.entity.update(cx, |state, _| state.element_bounds = bounds);
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
        let focus = self.entity.read(cx).focus_handle(cx);
        window.handle_input(
            &focus,
            gpui::ElementInputHandler::new(bounds, self.entity.clone()),
            cx,
        );
        self.inner.paint(window, cx);

        let (focused, text, placeholder, selection) = {
            let state = self.entity.read(cx);
            (
                state.focused(window, cx),
                state.text.clone(),
                state.placeholder.clone(),
                state.selection.clone(),
            )
        };
        let theme = Theme::global(cx).clone();
        let show_placeholder = text.is_empty() && !focused;
        let display: &str = if show_placeholder {
            placeholder.as_str()
        } else {
            text.as_str()
        };
        let color: Hsla = if show_placeholder {
            theme.colors.fg_muted.into()
        } else {
            theme.colors.fg.into()
        };
        let fs = px(DEFAULT_FONT_SIZE);
        let run = TextRun {
            len: display.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line_h = line_height();
        let text_origin = point(bounds.origin.x + px(PAD_PX), bounds.origin.y + (bounds.size.height - line_h) / 2.);
        if !display.is_empty() {
            let line = window
                .text_system()
                .shape_line(SharedString::from(display.to_string()), fs, &[run], None);
            let _ = line.paint(text_origin, line_h, gpui::TextAlign::Left, None, window, cx);
        }
        // 光标：仅聚焦且显示真实文本（非占位符）时。
        if focused && !show_placeholder {
            let caret_byte = selection.start.min(text.len());
            let prefix = &text[..caret_byte];
            let caret_x = if prefix.is_empty() {
                text_origin.x
            } else {
                let p_run = TextRun {
                    len: prefix.len(),
                    font: gpui::font(SharedString::from(".SystemUIFont")),
                    color: Hsla::default(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let w = window
                    .text_system()
                    .shape_line(SharedString::from(prefix.to_string()), fs, &[p_run], None)
                    .width();
                text_origin.x + w
            };
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(point(caret_x, text_origin.y), size(px(CARET_PX), line_h)),
                corner_radii: gpui::Corners::default(),
                background: theme.colors.fg.into(),
                border_widths: gpui::Edges::default(),
                border_color: theme.colors.fg.into(),
                border_style: gpui::BorderStyle::Solid,
            });
        }
    }
}

// -------------------------------------------------------------------------
// UTF-16 换算辅助
// -------------------------------------------------------------------------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn byte_to_utf16(s: &str, byte: usize) -> usize {
    s[..byte.min(s.len())].encode_utf16().count()
}

fn utf16_to_byte(s: &str, offset_utf16: usize) -> usize {
    let mut bytes = 0usize;
    let mut units = 0usize;
    for ch in s.chars() {
        if units >= offset_utf16 {
            break;
        }
        let cu = ch.len_utf16();
        if units + cu > offset_utf16 {
            break;
        }
        units += cu;
        bytes += ch.len_utf8();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, ParentElement as _, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    struct InputHost {
        input: Entity<InputState>,
        _sub: gpui::Subscription,
    }
    impl Render for InputHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(self.input.clone())
        }
    }

    /// 窗口宿主：div 包裹输入框（Render 走 InputElement），订阅 Change 计数。
    fn add_input<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (Entity<InputState>, &'a mut VisualTestContext, Rc<RefCell<usize>>) {
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let changes = Rc::new(RefCell::new(0usize));
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索…")
                    .default_value(text)
            });
            let sub = changes.clone();
            let _sub = cx.subscribe(&input, move |_, _, ev: &InputEvent, _| match ev {
                InputEvent::Change => *sub.borrow_mut() += 1,
            });
            *holder.borrow_mut() = Some(input.clone());
            InputHost { input, _sub }
        });
        let _ = _root;
        (holder.borrow().clone().unwrap(), wcx, changes)
    }

    /// `default_value`/`value`/占位符 + `set_value` 发 Change。
    #[gpui::test]
    async fn input_value_and_change_emission(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "abc");
        let got = wcx.update(|_w, cx| {
            (
                input.read(cx).value().to_string(),
                input.read(cx).placeholder.clone().to_string(),
            )
        });
        assert_eq!(got.0, "abc");
        assert_eq!(got.1, "搜索…");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| s.set_value("你好 world", window, cx));
        });
        let value = wcx.update(|_w, cx| input.read(cx).value().to_string());
        assert_eq!(value, "你好 world");
        assert_eq!(*changes.borrow(), 1, "set_value 应发出一次 Change");
    }

    /// `replace_text_in_range`（IME/直输）正确落点 + 发 Change + 中文 UTF-16 换算。
    #[gpui::test]
    async fn input_ime_replace_lands_at_utf16(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "abcde");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_text_in_range(Some(1..4), "X", window, cx);
            });
        });
        assert_eq!(
            wcx.update(|_w, cx| input.read(cx).value().to_string()),
            "aXe"
        );
        assert_eq!(*changes.borrow(), 1);
        // 中文：utf16[2] 是第二个字符后（"中文"后），字节偏移 ≠ utf16
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| s.set_value("中文测试", window, cx));
        });
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_text_in_range(Some(2..2), "！", window, cx);
            });
        });
        assert_eq!(
            wcx.update(|_w, cx| input.read(cx).value().to_string()),
            "中文！测试"
        );
    }
}
