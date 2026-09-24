//! 文本输入 / IME：`EntityInputHandler` 实现（经 `EditorElement::paint` 里的
//! `window.handle_input` 挂钩）+ `paste_text` 程序化粘贴 + UTF-16 偏移换算。

use std::ops::Range;

use gpui::{
    point, px, size, Bounds, ClipboardItem, Context, EntityInputHandler, Pixels, Point,
    TextInputConfiguration, UTF16Selection, Window,
};

use crate::editor::undo::EditIntent;
use crate::editor::Selection;
use crate::editor::view::{CARET_WIDTH_PX, CodeEditor};

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
            Some(r) => self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16())),
            None => self
                .marked_range
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        self.insert_edit(byte_range, text, EditIntent::Typing, cx);
        // 提交/直输会结束一次 IME 组合：Windows 提交候选时只发
        // `replace_text_in_range(None, …)`（不调 unmark_text），不清掉
        // marked_range 的话，下一次组合会把已提交文本当成旧 marked 区域再次替换。
        self.marked_range = None;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `None` 区间 = 替换当前 marked（正在组合的）文本；只有没有任何 marked 时的
        // 首个字符才落在普通 selection 上。以前用 selection 会让每次组合更新都
        // 「追加」到旧组合后面 → 垃圾残留。
        let byte_range = match range_utf16 {
            Some(r) => self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16())),
            None => self
                .marked_range
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        self.insert_edit(byte_range.clone(), new_text, EditIntent::Atomic, cx);
        if new_text.is_empty() {
            // 组合被取消：清掉 marked，光标停在原地（等价于删掉刚标记的占位）。
            self.marked_range = None;
            self.selection = Selection::collapsed(byte_range.start);
        } else {
            self.marked_range = Some(byte_range.start..byte_range.start + new_text.len());
            // `new_selected_range` 是**相对被标记文本（new_text）起点**的 UTF-16
            // 偏移（AppKit setMarkedText 的 selectedRange 语义），不是文档绝对偏移。
            // 以前当绝对偏移用 → 光标跳到文档开头附近（大文档任意位置输入中文时
            // 若光标落点恰在多字节字符中间，后续 insert 直接撞 `byte index is not
            // on a char boundary` 崩溃）。
            let (s, e) = match new_selected_range {
                Some(sel) => (
                    byte_range.start + ime_utf16_offset_to_byte(new_text, sel.start),
                    byte_range.start + ime_utf16_offset_to_byte(new_text, sel.end),
                ),
                None => (
                    byte_range.start + new_text.len(),
                    byte_range.start + new_text.len(),
                ),
            };
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

/// IME `new_selected_range` 辅助：把「相对被标记文本（`text`）起点」的 UTF-16
/// 偏移换算成 `text` 内的字节偏移。偏到两半的代理对按字符边界取整（停在该字符
/// 起点），越界夹到文本末尾。
fn ime_utf16_offset_to_byte(text: &str, offset_utf16: usize) -> usize {
    let mut bytes = 0usize;
    let mut units = 0usize;
    for ch in text.chars() {
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
