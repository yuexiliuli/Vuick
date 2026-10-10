//! 文本输入 / IME：`EntityInputHandler` 实现（经 `EditorElement::paint` 里的
//! `window.handle_input` 挂钩）+ `paste_text` 程序化粘贴。
//!
//! IME 状态机本体已收敛到 `crate::ime`（M2.5 双份合一，按 byte 计算）：本文件只
//! 实现 [`ImeTextHost`] 适配层（byte 语义、rope 支撑的 O(log n) UTF-16 换算、经
//! `insert_edit` 落地 + undo 意图）+ 编辑器专属几何（组合窗口定位/点命中）。

use std::ops::Range;

use gpui::{
    point, px, size, Bounds, ClipboardItem, Context, EntityInputHandler, Pixels, Point,
    TextInputConfiguration, UTF16Selection, Window,
};

use crate::editor::undo::EditIntent;
use crate::editor::Selection;
use crate::editor::view::{CARET_WIDTH_PX, CodeEditor};
use crate::ime::{self, ImeTextHost};

impl CodeEditor {
    /// 在选区处粘贴（归一化 CRLF→LF；程序化/应用侧调用）。
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_edit(self.selection.range(), &normalized, EditIntent::Atomic, cx);
    }
}

// -------------------------------------------------------------------------
// IME / 文本输入
// -------------------------------------------------------------------------

/// 共享 IME 状态机的 byte 语义宿主适配层：编辑器内部坐标本来就是 byte，
/// UTF-16 换算走 rope（O(log n)），编辑落地经 `insert_edit`（携带 undo 意图）。
impl ImeTextHost for CodeEditor {
    fn len_utf16(&self) -> usize {
        self.buffer.len_utf16()
    }

    fn byte_to_utf16(&self, byte: usize) -> usize {
        self.buffer.byte_to_utf16(byte)
    }

    fn utf16_to_byte(&self, offset_utf16: usize) -> usize {
        self.buffer.utf16_to_byte(offset_utf16)
    }

    fn text_in_bytes(&self, range: Range<usize>) -> Option<String> {
        Some(self.buffer.text_in_range(range).to_string())
    }

    fn selection_bytes(&self) -> Range<usize> {
        self.selection.range()
    }

    fn selection_reversed(&self) -> bool {
        self.selection.reversed()
    }

    fn set_selection_bytes(&mut self, range: Range<usize>) {
        self.selection = Selection::from_range(range.start, range.end);
        self.kick_caret();
    }

    fn marked_bytes(&self) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn set_marked_bytes(&mut self, range: Option<Range<usize>>) {
        self.marked_range = range;
    }

    fn apply_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        intent: EditIntent,
        cx: &mut Context<Self>,
    ) {
        self.insert_edit(range, text, intent, cx);
    }
}

impl EntityInputHandler for CodeEditor {
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

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
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
        // 提交/直输会结束一次 IME 组合：Windows 提交候选时只发
        // `replace_text_in_range(None, …)`（不调 unmark_text），共享核心在
        // 提交后清掉 marked_range——否则下一次组合会把已提交文本当成旧 marked
        // 区域再次替换（回归见 `editor::tests::view::ime_composition_*`）。
        ime::replace(self, range, text, None, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `None` 区间 = 替换当前 marked（正在组合的）文本；没有任何 marked 时
        // 首个字符才落在普通选区上。`new_selected_range` 相对组合文本起点，
        // 语义与 AppKit setMarkedText 一致（见 `crate::ime::replace`）。
        ime::replace(self, range_utf16, new_text, new_selected_range, true, cx);
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
        let off = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let row = self.buffer.line_of_offset(off);
        let byte_col = off - self.buffer.line_start(row);
        let lh = self.line_height.as_f32().max(1.);
        // 顶部与行高都在视觉空间：逻辑行先映射成视觉行再算 y（折叠体塌缩到首行）。
        let (top_abs, off_px) = self.abs_scroll_top();
        let vis = self.folds.visual_row(row);
        let y = (vis as f32 - top_abs as f32) * lh - off_px;
        // 字形真实宽度（IME 组合窗口/预选高亮定位不能再用等宽列近似，否则中文错位）。
        let x_text = self
            .row_layout(window, row)
            .map(|layout| layout.x_for_index(byte_col).as_f32())
            .unwrap_or_else(|| byte_col as f32 * self.char_width.as_f32().max(1.));
        let x = self.gutter_width_px.as_f32() + x_text - self.hscroll_px();
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
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = point - self.element_bounds.origin;
        Some(self.buffer.byte_to_utf16(self.offset_at_point(local, window)))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // 编辑器现状不在此处 notify（`set_selection_bytes` 已负责光标），保持原样。
        ime::set_selected_text_range(self, range_utf16);
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
