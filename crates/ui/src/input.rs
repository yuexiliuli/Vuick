//! 单行文本输入框（#12）：替代 gpui_component 的搜索框 `Input`/`InputState`/`InputEvent`。
//!
//! 自研实现：`InputState`（实体：文本/占位符/焦点/光标）+ `EntityInputHandler`
//! （IME/文本输入）+ 自定义 [`InputElement`]（paint 期挂 `window.handle_input`、画文本/
//! 占位符/光标）。键盘（Backspace/Delete/方向/Home/End/Ctrl 组合）走本地 `crate::actions`
//! 的 "Input" keymap（#10 自绑）：元素 `key_context("Input")` + `on_action` 接线；
//! 未实现的组合（如 Undo/Redo）按 gpui 分发语义静默不处理即可。
//!
//! IME 状态机本体已收敛到 `ime`（M2.5 双份合一）：本文件只实现
//! [`ImeTextHost`] 适配层（byte 语义、`&str` 遍历的 UTF-16 换算、经 `edit_range`
//! 落地 + 发 `Change`）。**维护 marked 区间**：Windows 组合更新以
//! `replace_and_mark_text_in_range(None, …)` 下发（None = 替换当前 marked/选区），
//! 不跟踪 marked 会把每键组合**追加**到光标后（中文 IME 乱码「拼音历史拼接」）——
//! 跟踪后组合更新正确替换旧组合、提交整体落地。单行框仍不画组合下划线（Windows IME
//! 自带组合窗口），组合内偏移只摆光标、不建选区。

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
use config::layout::DEFAULT_FONT_SIZE;
// P3a 断环关键点：EditIntent 已下沉 base，本 crate 不再经 editor 引用（editor↔ui 环断开）。
use base::EditIntent;
use ime::{self, ImeTextHost};
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
    /// 组合中（marked）区间（字节）。Windows 组合更新 `range=None` = 替换它；
    /// 提交/取消后由共享核心清空。单行框不画组合下划线，但必须跟踪以保证「替换不追加」。
    marked: Option<Range<usize>>,
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
            marked: None,
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
        self.marked = None;
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// 整体替换文本（粘贴到搜索框路径，发 `Change`）。
    pub fn replace(&mut self, text: impl Into<String>, _window: &mut Window, cx: &mut Context<Self>) {
        self.text = text.into();
        let len = self.text.len();
        self.selection = len..len;
        self.marked = None;
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
    /// 任何一次落地都清掉组合态（提交/删除/粘贴都结束组合；组合更新经
    /// [`ime::replace`] 在 apply 之后重新写回 marked，不受影响）。
    fn edit_range(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        self.text.replace_range(start..end, new_text);
        let caret = start + new_text.len();
        self.selection = caret..caret;
        self.marked = None;
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

/// 共享 IME 状态机的 byte 语义宿主适配层：内部坐标本来就是 byte，`&str` 遍历
/// 做 UTF-16 换算（单行框文本小，无需 rope 索引），落地经 `edit_range`（发
/// `Change`）。**跟踪 marked**（`supports_marked = true`）：Windows 组合更新
/// `range=None` 落当前 marked/选区，组合更新替换旧组合、提交整体落地（修中文
/// IME「逐键追加」乱码）；组合内偏移只摆光标、不建选区（`set_composition_caret`
/// 覆写），单行框仍不画组合下划线。
impl ImeTextHost for InputState {
    fn len_utf16(&self) -> usize {
        ime::utf16_len(self.text.as_str())
    }

    fn byte_to_utf16(&self, byte: usize) -> usize {
        ime::byte_to_utf16(self.text.as_str(), byte)
    }

    fn utf16_to_byte(&self, offset_utf16: usize) -> usize {
        ime::utf16_to_byte(self.text.as_str(), offset_utf16)
    }

    fn text_in_bytes(&self, range: Range<usize>) -> Option<String> {
        self.text.get(range).map(str::to_string)
    }

    fn selection_bytes(&self) -> Range<usize> {
        self.selection.clone()
    }

    fn set_selection_bytes(&mut self, range: Range<usize>) {
        self.selection = range;
    }

    fn marked_bytes(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn set_marked_bytes(&mut self, range: Option<Range<usize>>) {
        self.marked = range;
    }

    fn supports_marked(&self) -> bool {
        true
    }

    /// 组合内偏移只摆光标、不建选区（与 WYSIWYG 输入一致）：`None`（Windows）光标
    /// 落组合末尾，`Some(sel)`（macOS）光标落 sel.start。单行框不显示组合选区。
    fn set_composition_caret(
        &mut self,
        base_byte: usize,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
    ) {
        let caret = match new_selected_range {
            Some(sel) => base_byte + ime::utf16_to_byte(new_text, sel.start),
            None => base_byte + new_text.len(),
        };
        self.selection = caret.min(self.text.len())..caret.min(self.text.len());
    }

    fn apply_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        _intent: EditIntent,
        cx: &mut Context<Self>,
    ) {
        self.edit_range(range, text, cx);
    }
}

impl EntityInputHandler for InputState {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.len_utf16())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(ime::selected_text_range(self))
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        ime::marked_text_range(self)
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        ime::text_for_range(self, range_utf16)
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        ime::replace(self, range, text, None, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组合更新：`None` 区间 = 替换当前 marked/选区（共享核心跟踪 marked——
        // 不再逐键追加）；组合内光标经 `set_composition_caret` 只摆光标不建选区。
        ime::replace(self, range, new_text, new_selected_range, true, cx);
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        ime::unmark(self);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let byte = ime::utf16_to_byte(self.text.as_str(), range_utf16.start.min(range_utf16.end));
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
        Some(ime::byte_to_utf16(
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
        ime::set_selected_text_range(self, range_utf16);
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

// UTF-16 换算辅助（utf16_len / byte_to_utf16 / utf16_to_byte）已收敛到
// `ime`——见文件头与 [`ime::ImeTextHost`] 适配层。

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

    /// IME 组合路径回归（M2.5 收敛护栏 + 中文 IME 修复）：单行输入框跟踪 marked，
    /// `None` 区间 = 替换当前 marked/选区——组合更新**替换旧组合不追加**（修中文
    /// IME「逐键追加」乱码，与编辑器/WYSIWYG 一致）；显式 UTF-16 区间精确替换；
    /// 组合内偏移只摆光标；提交（普通 `replace_text_in_range`）整体落地并结束组合。
    #[gpui::test]
    async fn input_ime_composition_treated_as_replace(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "ab");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                // None 区间 = 当前 marked/选区：无 marked → 光标处插入
                s.replace_and_mark_text_in_range(None, "中", None, window, cx);
                // 第二次组合更新仍 None：替换旧 marked（不追加！）
                s.replace_and_mark_text_in_range(None, "文", None, window, cx);
                // 显式 UTF-16 区间（macOS 风格 marked range）：精确替换 utf16[1..4]
                // （=“b文”，含代理对粒度换算，非字节偏移）
                s.replace_and_mark_text_in_range(Some(1..4), "国", None, window, cx);
                // 提交：普通替换落当前 marked 并结束组合（marked 清理在核心统一处理）
                s.replace_text_in_range(None, "界", window, cx);
            });
        });
        let (value, caret) = wcx.update(|_w, cx| {
            (
                input.read(cx).value().to_string(),
                input.read(cx).selection.clone(),
            )
        });
        // 轨迹：ab → ab中（marked 替换）→ ab文 → a国（utf16[1..4]=“b文”被“国”替换）
        // → a界（提交替换 marked）
        assert_eq!(value, "a界");
        assert_eq!(caret, "a界".len().."a界".len(), "光标停在提交文本末尾");
        assert_eq!(*changes.borrow(), 4, "组合每步与提交各发一次 Change");
    }

    /// 复现：侧边栏搜索框中文 IME「逐键追加乱码」——用户输「编辑」输入框出现
    /// 「bbibi'abianbian'jbian'ji编辑」。Windows IME 组合更新走
    /// `replace_and_mark_text_in_range(None, 组合串, …)`、提交走
    /// `replace_text_in_range(None, 已提交中文)`（见 gpui_windows `handle_ime_composition_inner`）。
    /// `None` 区间 = 当前 marked/选区：若输入框不跟踪 marked，每键组合**追加**到光标
    /// 后而非替换旧组合，提交又拼一段中文。断言：组合替换不追加、提交后只剩中文。
    #[gpui::test]
    async fn input_ime_chinese_composition_does_not_stack(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                // Windows IME 组合更新序列：每键重发**完整**组合串（range=None）
                s.replace_and_mark_text_in_range(None, "b", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi'a", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi'an", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bian'j", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bian'ji", None, window, cx);
                // 确认：提交中文（range=None），组合应被整体替换
                s.replace_text_in_range(None, "编辑", window, cx);
            });
        });
        let (value, caret) = wcx.update(|_w, cx| {
            (
                input.read(cx).value().to_string(),
                input.read(cx).selection.clone(),
            )
        });
        assert_eq!(
            value, "编辑",
            "组合应逐键替换不追加，提交后只剩中文（而非拼音历史拼接）"
        );
        assert_eq!(caret, "编辑".len().."编辑".len(), "光标停在提交文本末尾");
        assert_eq!(*changes.borrow(), 7, "组合每步与提交各发一次 Change");
    }
}
