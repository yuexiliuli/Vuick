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
use std::time::{Duration, Instant};

use gpui::{
    div, point, size, AnyElement, App, Bounds, ClipboardItem, Context, Element, ElementId, Entity,
    EntityInputHandler, EventEmitter, Focusable, FocusHandle, GlobalElementId, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Styled as _,
    TextInputConfiguration, TextRun, UTF16Selection, Window, px,
};

use crate::actions::{
    Backspace, Copy, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, MoveEnd, MoveHome,
    MoveLeft, MoveRight, MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, SelectAll,
    SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart, SelectToStart,
    SelectToStartOfLine,
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
/// 光标闪烁周期（ms，对齐编辑器 `BLINK_MS`）。
const CARET_BLINK_MS: u64 = 500;
/// 光标交互后的静止期（ms，期间光标常亮不闪，对齐编辑器 `BLINK_PAUSE_MS`）。
const CARET_BLINK_PAUSE_MS: u64 = 300;

/// 单行输入框状态（实体）：文本 + 占位符 + 焦点 + 光标（字节区间）+ 元素边界。
pub struct InputState {
    text: String,
    placeholder: SharedString,
    focus_handle: FocusHandle,
    selection: Range<usize>,
    /// 选区活动端点（head）：光标所在端（`selection` 恒为 min..max）。
    /// 拖选/Shift 移动时 head 随鼠标/按键走、锚点（另一端）不动；光标绘制看 head。
    selection_head: usize,
    /// 鼠标拖选中（元素内已按下、尚未松开）。`on_mouse_up`/`on_mouse_up_out`/失焦复位。
    dragging: bool,
    /// 光标可见性（闪烁驱动）。
    caret_visible: bool,
    /// 光标交互后静止期起点（期间光标常亮不闪）。
    caret_pause_until: Option<Instant>,
    /// 闪烁代际：失焦/重新聚焦后旧循环自然退出。
    blink_epoch: u64,
    blink_running: bool,
    /// 本实体是否持焦（on_focus/on_blur + render 对齐维护，供闪烁循环判退）。
    focused: bool,
    /// 焦点事件是否已注册（仅一次）。
    focus_registered: bool,
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
            selection_head: 0,
            dragging: false,
            caret_visible: false,
            caret_pause_until: None,
            blink_epoch: 0,
            blink_running: false,
            focused: false,
            focus_registered: false,
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
        self.selection_head = len;
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
        self.selection_head = len;
        self.marked = None;
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// 整体替换文本（粘贴到搜索框路径，发 `Change`）。
    pub fn replace(&mut self, text: impl Into<String>, _window: &mut Window, cx: &mut Context<Self>) {
        self.text = text.into();
        let len = self.text.len();
        self.selection = len..len;
        self.selection_head = len;
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
        self.selection_head = caret;
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
        self.selection_head = index;
        self.kick_caret();
        cx.notify();
    }

    /// 扩展选区：把 head（活动端点，光标所在端）移到 `index`，锚点（另一端）不动——
    /// 拖选 / Shift（含 Shift+Ctrl）方向扩选共用。选区恒归一化为 `min..max`。
    fn extend_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let index = index.min(self.text.len());
        // 锚点 = 当前非 head 端点（反选中 head==start 时锚点在 end，否则在 start）。
        let anchor = if self.selection_head == self.selection.start {
            self.selection.end
        } else {
            self.selection.start
        };
        if index >= anchor {
            self.selection = anchor..index;
        } else {
            self.selection = index..anchor;
        }
        self.selection_head = index;
        self.kick_caret();
        cx.notify();
    }

    fn move_delta(&mut self, delta: isize, cx: &mut Context<Self>) {
        let cur = self.selection_head;
        let next = if delta < 0 {
            self.prev_char(cur)
        } else {
            self.next_char(cur)
        };
        self.move_to(next, cx);
    }

    /// 往回一个词边界（字节，纯计算，不改状态）。
    fn prev_word(&self, cur: usize) -> usize {
        let mut cur = cur.min(self.text.len());
        while cur > 0 && self.text[..cur].ends_with(char::is_whitespace) {
            cur = self.prev_char(cur);
        }
        while cur > 0 && !self.text[..cur].ends_with(char::is_whitespace) {
            cur = self.prev_char(cur);
        }
        cur
    }

    /// 往前一个词边界（字节，纯计算，不改状态）。
    fn next_word(&self, cur: usize) -> usize {
        let mut cur = cur.min(self.text.len());
        while cur < self.text.len() && self.text[cur..].starts_with(char::is_whitespace) {
            cur = self.next_char(cur);
        }
        while cur < self.text.len() && !self.text[cur..].starts_with(char::is_whitespace) {
            cur = self.next_char(cur);
        }
        cur
    }

    fn move_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        let head = self.selection_head;
        let next = if forward {
            self.next_word(head)
        } else {
            self.prev_word(head)
        };
        self.move_to(next, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let len = self.text.len();
        self.selection = 0..len;
        self.selection_head = len;
        self.kick_caret();
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
            let start = self.prev_word(sel.start);
            self.edit_range(start..sel.start, "", cx);
        } else {
            let end = self.next_word(sel.end);
            self.edit_range(sel.end..end, "", cx);
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

    /// 前缀文本整形宽度（px，空串 → 0）。供光标 x、拖选两端 x 共用。
    fn text_width(&self, s: &str, window: &mut Window) -> Pixels {
        if s.is_empty() {
            return px(0.);
        }
        let run = TextRun {
            len: s.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(SharedString::from(s.to_string()), px(DEFAULT_FONT_SIZE), &[run], None)
            .width()
    }

    /// 光标矩形 x（元素原点 + pad + 前缀宽），供 IME `bounds_for_range`。
    fn caret_px(&self, byte: usize, element_bounds: Bounds<Pixels>, window: &mut Window) -> Pixels {
        let prefix = &self.text[..byte.min(self.text.len())];
        element_bounds.origin.x + px(PAD_PX) + self.text_width(prefix, window)
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

    fn on_select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(0, cx);
    }

    fn on_select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(self.text.len(), cx);
    }

    fn on_select_to_start_of_line(
        &mut self,
        _: &SelectToStartOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_to(0, cx);
    }

    fn on_select_to_end_of_line(
        &mut self,
        _: &SelectToEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_to(self.text.len(), cx);
    }

    fn on_select_prev_word(
        &mut self,
        _: &SelectToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.selection_head;
        self.extend_to(self.prev_word(head), cx);
    }

    fn on_select_next_word(
        &mut self,
        _: &SelectToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.selection_head;
        self.extend_to(self.next_word(head), cx);
    }

    /// 统一按键：只处理 keymap 未覆盖的 **Shift+← / Shift+→** 逐字符扩选（与主编辑器
    /// `CodeEditor::on_key_down` 同套路——`Select{Left,Right}` 是组件/编辑器内部 action、
    /// 公共 keymap 未绑我们无法注册 handler，故在原样 key_down 层处理）；其余全走
    /// `on_action`。可打印字符（无修饰）放行给 IME/输入处理。
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let m = k.modifiers;
        if !m.control && !m.alt && !m.platform && k.key_char.is_some() {
            return;
        }
        if m.shift && !m.control && !m.alt && !m.platform {
            match k.key.to_ascii_lowercase().as_str() {
                "left" => self.extend_to(self.prev_char(self.selection_head), cx),
                "right" => self.extend_to(self.next_char(self.selection_head), cx),
                _ => {}
            }
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window, cx);
        let x = (ev.position.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        let index = self.index_at_x_impl(x, window);
        // Shift+点按 = 从当前光标扩选到点击处（编辑器同语义）；否则落点摆光标。
        if ev.modifiers.shift {
            self.extend_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
        self.dragging = true;
        cx.notify();
    }

    /// 拖选：按住左键移动 → 扩展选区 head 到鼠标落点（锚点固定在按下处）。
    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let x = (ev.position.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        let index = self.index_at_x_impl(x, window);
        self.extend_to(index, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 松开点落在元素 hitbox 外 / 顶层叠层 / 窗口外：元素级 `on_mouse_up`（受
    /// `hitbox.is_hovered` 门控）不派发，本钩子（Capture 阶段）无条件收口拖选态
    /// （对齐本仓 scrollbar / editor / wysiwyg 的 `on_mouse_up_out` 拖拽清态惯例）。
    fn on_mouse_up_out(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 拖选态生命周期终点之一：松开/失焦复位 `dragging`（对齐编辑器
    /// `CodeEditor::cancel_mouse_drag`；只清拖拽态，不动光标/选区）。
    fn cancel_mouse_drag(&mut self) {
        self.dragging = false;
    }

    // ---------------------------------------------------------------------
    // 光标闪烁（对齐编辑器 `CodeEditor` 的 blink 实现）
    // ---------------------------------------------------------------------

    fn on_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = true;
        self.kick_caret();
        if !self.blink_running {
            self.start_blink(cx);
        }
        cx.notify();
    }

    fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = false;
        self.caret_visible = true;
        self.cancel_mouse_drag();
        cx.notify();
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_running {
            return;
        }
        self.blink_running = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| loop {
            let elapsed = this
                .update(cx, |this, _| {
                    this.caret_pause_until
                        .map_or(u128::MAX, |p| p.elapsed().as_millis())
                })
                .unwrap_or(u128::MAX);
            if elapsed >= CARET_BLINK_PAUSE_MS as u128 {
                let _ = this.update(cx, |this, cx| {
                    this.caret_visible = !this.caret_visible;
                    cx.notify();
                });
            }
            cx.background_executor()
                .timer(Duration::from_millis(CARET_BLINK_MS))
                .await;
            let keep = this
                .update(cx, |this, _| this.focused && epoch == this.blink_epoch)
                .unwrap_or(false);
            if !keep {
                this.update(cx, |this, _| this.blink_running = false).ok();
                break;
            }
        })
        .detach();
    }

    /// 光标交互后常亮静止期（拖选/键移后先亮住再继续闪）。
    fn kick_caret(&mut self) {
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
    }

    /// 当前帧光标是否应绘制：失焦不画；交互静止期内常亮；否则跟随闪烁翻转。
    fn caret_shown_now(&self) -> bool {
        if !self.focused {
            return false;
        }
        let paused = self
            .caret_pause_until
            .is_some_and(|p| p.elapsed() < Duration::from_millis(CARET_BLINK_PAUSE_MS));
        paused || self.caret_visible
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

    /// 是否反选（`head < anchor`），驱动 `selected_text_range.reversed`。
    fn selection_reversed(&self) -> bool {
        self.selection.start < self.selection.end && self.selection_head == self.selection.start
    }

    fn set_selection_bytes(&mut self, range: Range<usize>) {
        self.selection = range.clone();
        self.selection_head = range.end;
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
        self.selection_head = caret.min(self.text.len());
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首次渲染注册焦点事件（blink 启动/失焦复位），并做启动期焦点态对齐
        // （对齐编辑器 sync_render_state：window.focus() 若发生在注册前，事件会丢）。
        if !self.focus_registered {
            let _ = cx.on_focus(&self.focus_handle, window, Self::on_focus);
            let _ = cx.on_blur(&self.focus_handle, window, Self::on_blur);
            self.focus_registered = true;
        }
        if self.focus_handle.is_focused(window) && !self.focused {
            self.focused = true;
            self.kick_caret();
            if !self.blink_running {
                self.start_blink(cx);
            }
            cx.notify();
        } else if !self.focus_handle.is_focused(window) && self.focused {
            self.focused = false;
            self.cancel_mouse_drag();
            cx.notify();
        }
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
            .on_action(cx.listener(Self::on_select_prev_word))
            .on_action(cx.listener(Self::on_select_next_word))
            .on_action(cx.listener(Self::on_select_to_start))
            .on_action(cx.listener(Self::on_select_to_end))
            .on_action(cx.listener(Self::on_select_to_start_of_line))
            .on_action(cx.listener(Self::on_select_to_end_of_line))
            .on_action(cx.listener(Self::on_copy))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up_out))
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
        base::util::source_location()
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

        let (focused, text, placeholder, selection, selection_head, caret_shown) = {
            let state = self.entity.read(cx);
            (
                state.focused(window, cx),
                state.text.clone(),
                state.placeholder.clone(),
                state.selection.clone(),
                state.selection_head,
                state.caret_shown_now(),
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
        // 选区背景（拖选/键选时高亮，对齐编辑器选区底色）+ 光标：仅聚焦且显示真实
        // 文本（非占位符）时。光标画在选区 head 端、由闪烁驱动显示。
        if focused && !show_placeholder {
            if selection.start < selection.end {
                let s_x = text_origin.x
                    + self.entity.read(cx).text_width(&text[..selection.start], window);
                let e_x = text_origin.x
                    + self.entity.read(cx).text_width(&text[..selection.end], window);
                let sel_bg = theme.colors.editor_selection_bg;
                window.paint_quad(gpui::PaintQuad {
                    bounds: Bounds::new(point(s_x, text_origin.y), size(e_x - s_x, line_h)),
                    corner_radii: gpui::Corners::default(),
                    background: sel_bg.into(),
                    border_widths: gpui::Edges::default(),
                    border_color: sel_bg.into(),
                    border_style: gpui::BorderStyle::Solid,
                });
            }
            if caret_shown {
                let caret_byte = selection_head.min(text.len());
                let prefix = &text[..caret_byte];
                let caret_x = if prefix.is_empty() {
                    text_origin.x
                } else {
                    text_origin.x + self.entity.read(cx).text_width(prefix, window)
                };
                window.paint_quad(gpui::PaintQuad {
                    bounds: Bounds::new(
                        point(caret_x, text_origin.y),
                        size(px(CARET_PX), line_h),
                    ),
                    corner_radii: gpui::Corners::default(),
                    background: theme.colors.fg.into(),
                    border_widths: gpui::Edges::default(),
                    border_color: theme.colors.fg.into(),
                    border_style: gpui::BorderStyle::Solid,
                });
            }
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

    /// 跑几帧，等渲染/layout/prepaint 就绪（element_bounds 供鼠标命中换算）。
    fn settle_window(wcx: &mut VisualTestContext) {
        for _ in 0..3 {
            let _ = wcx.refresh();
            wcx.run_until_parked();
        }
    }

    /// 复现 1：侧边栏搜索框「无法按住鼠标拖动选中文字」——`on_mouse_down` 只摆光标、
    /// 没有线内 mouse-move 拖动扩展选区。断言：按下左键后拖动（移动鼠标）应形成选区。
    #[gpui::test]
    async fn mouse_drag_selects_range(cx: &mut TestAppContext) {
        let (input, wcx, _) = add_input(cx, "hello world");
        settle_window(wcx);
        let bounds = wcx.update(|window, cx| {
            let b = input.read(cx).element_bounds.clone();
            let _ = window;
            b
        });
        let y = bounds.origin.y + px(4.);
        // 按下：文本起始处（元素左缘 + 左内边距）→ 光标落 0
        let down = point(bounds.origin.x + px(PAD_PX), y);
        wcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: down,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "按下落点应先把光标坍缩到 0"
        );
        // 按住左键拖到元素右缘 → 选区应扩展（而非仍坍缩）
        let drag_to = point(bounds.origin.x + bounds.size.width - px(1.), y);
        wcx.simulate_event(gpui::MouseMoveEvent {
            position: drag_to,
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::default(),
        });
        let sel = wcx.update(|_, cx| input.read(cx).selection.clone());
        assert_eq!(
            sel,
            0.."hello world".len(),
            "按住左键拖到右缘应形成 0..len 选区（拖选）"
        );
        // 反向拖回落点 → 选区收缩坍缩（锚点固定）
        wcx.simulate_event(gpui::MouseMoveEvent {
            position: down,
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::default(),
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "拖回落点应收缩到坍缩"
        );
        // 松开左键 → 拖拽闭环
        wcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: down,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
        assert!(
            !wcx.update(|_, cx| input.read(cx).dragging),
            "松开后拖拽态应复位"
        );
    }

    /// 复现 2：搜索框「Ctrl+← / Ctrl+→ 无法扩选词」。keymap 已绑
    /// `ctrl-shift-left/right` → `SelectToPreviousWordStart/SelectToNextWordEnd`（见
    /// `base::actions::bind_input_keys`），但 input.rs 没有对应 `on_action` handler。
    /// 断言：Ctrl+Shift+方向应扩选词；普通 Ctrl+方向仍只移动光标（回归护栏）。
    #[gpui::test]
    async fn ctrl_shift_arrow_selects_word(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "foo bar baz");
        settle_window(wcx);
        // 点按聚焦并落光标到文本首
        let bounds = wcx.update(|window, cx| {
            let b = input.read(cx).element_bounds.clone();
            let _ = window;
            b
        });
        let y = bounds.origin.y + px(4.);
        wcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        wcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "点按文本首应坍缩光标到 0"
        );
        let w = wcx.windows().first().copied().expect("有测试窗口");
        // Ctrl+Shift+→：扩选第一个词 foo（0..3）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-shift-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..3,
            "Ctrl+Shift+→ 应扩选到词尾（foo）"
        );
        // 再按：扩到 "foo bar"（0..7）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-shift-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..7,
            "连续 Ctrl+Shift+→ 应扩展到第二个词尾"
        );
        // Ctrl+Shift+←：收缩一个词（head 7 → 4）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-shift-left").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..4,
            "Ctrl+Shift+← 应收缩到词头"
        );
        // 普通 Ctrl+→：仅移动光标（不扩选）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            7..7,
            "普通 Ctrl+→ 应只移动光标"
        );
    }

    /// 复现 2b：Shift+← / Shift+→ 逐字符扩选（与主编辑器 on_key_down 同套路——
    /// `Select{Left,Right}` 无本地 action，走原样 key_down 层）。
    #[gpui::test]
    async fn shift_arrow_selects_by_char(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "abcd");
        settle_window(wcx);
        let bounds = wcx.update(|window, cx| {
            let b = input.read(cx).element_bounds.clone();
            let _ = window;
            b
        });
        let y = bounds.origin.y + px(4.);
        wcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        wcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
        let w = wcx.windows().first().copied().expect("有测试窗口");
        // 先三个右移 → 光标 3
        for _ in 0..3 {
            wcx.dispatch_keystroke(w, gpui::Keystroke::parse("right").unwrap());
        }
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            3..3,
            "右移三次光标应在 3"
        );
        // Shift+Right：扩到 3..4
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("shift-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            3..4,
            "Shift+Right 应扩选一个字符"
        );
        // Shift+Left：收缩回 3..3
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("shift-left").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            3..3,
            "Shift+Left 应收缩一个字符"
        );
    }

    /// 复现 3：搜索框「光标不闪烁」——聚焦后应启动 blink 循环、光标先可见；暂停期
    /// 结束后可见性随计时翻转（闪烁）；失焦后循环退出不再翻转。
    #[gpui::test]
    async fn caret_blinks_with_timer(cx: &mut TestAppContext) {
        let (input, wcx, _) = add_input(cx, "ab");
        // 未聚焦：光标不画
        assert!(
            !wcx.update(|_, cx| input.read(cx).caret_shown_now()),
            "未聚焦光标应不可见"
        );
        // 聚焦 → 光标可见（静止期开始）、blink 循环启动
        wcx.update(|window, cx| window.focus(&input.read(cx).focus_handle(cx), cx));
        settle_window(wcx);
        let (shown, running, visible) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.caret_shown_now(), s.blink_running, s.caret_visible)
        });
        assert!(shown, "聚焦后光标应可见");
        assert!(running, "聚焦后应启动 blink 循环");
        assert!(visible, "聚焦后 caret_visible 应为 true");
        // 模拟静止期结束（无暂停标记 → 循环下一次 tick 翻转），推进虚拟时钟驱动计时器
        wcx.update(|_, cx| input.update(cx, |s, _| s.caret_pause_until = None));
        wcx.executor()
            .advance_clock(Duration::from_millis(CARET_BLINK_MS + 10));
        wcx.run_until_parked();
        let after = wcx.update(|_, cx| input.read(cx).caret_visible);
        assert!(!after, "光标可见性应随 blink 计时翻转（闪烁）");
        // 失焦：真正 blur 窗口焦点态 + 直调 on_blur（gpui 测试环境不派发真实 focus/blur
        // 事件，与编辑器 active_line_bg_requires_focus 的「focused=false 分支直测」同套路）。
        // 不真正清焦点的话，render 焦点对齐会把 `focused` 拉回 true。
        wcx.update(|window, cx| {
            window.blur(cx);
            input.update(cx, |s, cx| s.on_blur(window, cx));
        });
        // 失焦后循环还要再睡满周期才到退出判定（toggle → sleep → keep-check），分拍
        // 推进 + 每拍 park（对齐本仓 pdf/editor 的 advance_clock 驱动惯用法）。
        for _ in 0..8 {
            wcx.executor()
                .advance_clock(Duration::from_millis(CARET_BLINK_MS));
            wcx.run_until_parked();
        }
        let (running2, shown2) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.blink_running, s.caret_shown_now())
        });
        assert!(!running2, "失焦后 blink 循环应退出");
        assert!(!shown2, "失焦后光标应不可见");
    }
}
