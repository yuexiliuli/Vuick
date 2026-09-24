/* ---------- UTF16 ↔ 字节（IME/选择器用） ---------- */

use std::ops::Range;

use gpui::{px, Context, EntityInputHandler, Pixels, UTF16Selection, Window};

use super::WyEditor;

/// 小串内（组合文本 `new_text`，`new_selected_range` 是相对它起点的 UTF-16 偏移）的
/// UTF-16 → 字节扫描，O(串长)（组合文本只有几个字符，不需要行索引）。
pub(crate) fn offset_from_utf16(s: &str, utf16: usize) -> usize {
    let mut count = 0usize;
    for (i, c) in s.char_indices() {
        if count >= utf16 {
            return i;
        }
        count += c.len_utf16();
    }
    s.len()
}

impl EntityInputHandler for WyEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        // 行粒度 UTF-16 前缀索引换算（O(log n + 行长)）+ 区间切片——不再物化全文。
        let range =
            self.doc.utf16_to_byte(range_utf16.start)..self.doc.utf16_to_byte(range_utf16.end);
        *actual_range = Some(self.doc.byte_to_utf16(range.start)..self.doc.byte_to_utf16(range.end));
        Some(self.doc.slice_str(range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        match self.selection() {
            Some(r) => Some(UTF16Selection {
                range: self.doc.byte_to_utf16(r.start)..self.doc.byte_to_utf16(r.end),
                reversed: self.caret < self.sel_anchor.unwrap_or(self.caret),
            }),
            None => Some(UTF16Selection {
                range: {
                    let b = self.doc.byte_to_utf16(self.caret.min(self.len()));
                    b..b
                },
                reversed: false,
            }),
        }
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|r| self.doc.byte_to_utf16(r.start)..self.doc.byte_to_utf16(r.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_utf16(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_and_mark_utf16(range, new_text, new_selected_range, cx);
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        let r =
            self.doc.utf16_to_byte(range_utf16.start)..self.doc.utf16_to_byte(range_utf16.end);
        self.caret = r.end.min(self.len());
        self.sel_anchor = Some(r.start.min(self.len()));
        *self.frame.borrow_mut() = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: gpui::Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let v = super::caret_vis();
        // `caret_vis` 是元素局部坐标（`paint` 时相对元素原点记录），需叠回元素在
        // 窗口中的原点，IME/输入法候选窗才落在光标处（否则整体偏移元素原点）。
        // 组合时画在标记文本上（从 marked 起点到本次输入终点），否则画光标竖条。
        // 换算走行索引（O(log n)），不再为判等物化全文。
        let r =
            self.doc.utf16_to_byte(range_utf16.start)..self.doc.utf16_to_byte(range_utf16.end);
        let w = if self.marked.as_ref().is_some_and(|m| r == *m) && r.end > r.start {
            // 组合候选：让候选窗落在整个标记文本宽度上，而不是零宽竖条。
            // 真实几何很难按字符溯源，这里用“应盖住的位置”转成可见块（至少一字符宽）。
            px(24.)
        } else {
            px(2.)
        };
        Some(gpui::Bounds::new(
            gpui::point(element_bounds.origin.x + v.x, element_bounds.origin.y + v.top),
            gpui::size(w, v.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        // 真实命中：经 frame 反查源偏移（平台 IME/辅助功能用，坐标元素局部）。
        let marker = self.frame.borrow();
        let f = marker.as_ref().map(|r| &**r)?;
        let off = super::element::source_offset_at_local(self, f, point);
        drop(marker);
        Some(self.doc.byte_to_utf16(off))
    }

    fn text_length_utf16(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.doc.total_utf16())
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        true
    }
}
