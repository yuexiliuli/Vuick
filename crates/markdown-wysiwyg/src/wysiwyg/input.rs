/* ---------- IME：`ImeTextHost` 适配层 + `EntityInputHandler` 委托 ---------- */

use std::ops::Range;

use gpui::{px, Context, EntityInputHandler, Pixels, UTF16Selection, Window};

use super::WyEditor;
use base::EditIntent;
use ime::{self, ImeTextHost};

impl ImeTextHost for WyEditor {
    fn len_utf16(&self) -> usize {
        self.doc.total_utf16()
    }

    fn byte_to_utf16(&self, byte: usize) -> usize {
        self.doc.byte_to_utf16(byte)
    }

    fn utf16_to_byte(&self, offset_utf16: usize) -> usize {
        self.doc.utf16_to_byte(offset_utf16)
    }

    fn text_in_bytes(&self, range: Range<usize>) -> Option<String> {
        Some(self.doc.slice_str(range))
    }

    fn selection_bytes(&self) -> Range<usize> {
        self.selection().unwrap_or(self.caret..self.caret)
    }

    fn selection_reversed(&self) -> bool {
        self.caret < self.sel_anchor.unwrap_or(self.caret)
    }

    fn set_selection_bytes(&mut self, range: Range<usize>) {
        self.caret = range.end.min(self.len());
        self.sel_anchor = Some(range.start.min(self.len()));
        // IME/平台改选区 → 静止期常亮（对齐 editor/ui 的 `set_selection_bytes` kick）。
        self.kick_caret();
        *self.frame.borrow_mut() = None;
    }

    fn marked_bytes(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn set_marked_bytes(&mut self, range: Option<Range<usize>>) {
        self.marked = range;
    }

    /// 只取 `new_selected_range.start` 作组合内光标、不建立选区。
    ///
    /// 现状（未并前 `replace_and_mark`）即「`offset_from_utf16(new_text, sel.start)`
    /// 摆 caret、不回写 sel_anchor」（`apply_edit` 已由 `replace_range` 清掉旧选区）。
    /// 用共享 `ime::utf16_to_byte` 替代自研开销的 O(len) 扫描——两者除「偏移落
    /// 在代理对内」的边角：自研版返回下一字符起点，共享版取该字符起点（更正确），
    /// 其余字节逐位一致。不改动 editor/ui 的默认实现（选区 + head）。
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
        self.caret = caret.min(self.len());
        // 组合内光标移动 → 静止期常亮（editor/ui 的默认实现经 `set_selection_bytes`
        // 间接 kick，这里与主支座直接对齐）。
        self.kick_caret();
    }

    /// 落地编辑：`replace_range` 承载全部意图（undo 事务粒度按 `removed/text`
    /// 自推导 `EditKind`，与 editor 的 `EditIntent` 无关——`intent` 无需透传）。
    fn apply_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        _intent: EditIntent,
        cx: &mut Context<Self>,
    ) {
        self.replace_range(range, text, cx);
    }
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
        // 保留上报 `actual_range`（平台读它校正 UTF-16 换算后的真实区间；共享
        // `ime::text_for_range` 未暴露该出参，故此处走宿主自译而非共享面）。
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
        // 共享：byte 选区 → UTF-16；`reversed` 由 `selection_reversed` 提供。
        Some(ime::selected_text_range(self))
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        ime::marked_text_range(self)
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        ime::unmark(self);
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A1 只读门控：输入短路（IME 与平台直送都走这里；`replace_range` 也硬门控）。
        if !self.editable {
            return;
        }
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
        if !self.editable {
            return;
        }
        self.replace_and_mark_utf16(range, new_text, new_selected_range, cx);
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        // 共享：UTF-16 → byte 后经 `set_selection_bytes` 落定（不在此 notify，
        // 与现状一致）。
        ime::set_selected_text_range(self, range_utf16);
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
        // 居中阅读栏：内容列相对元素再平移 `reading_col_shift`，候选窗/竖条要与 paint
        // 画到的光标位置一致（元素原点 + 列移位 + 帧内光标 x）。
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
        let shift = self.reading_col_shift(f32::from(element_bounds.size.width));
        Some(gpui::Bounds::new(
            gpui::point(element_bounds.origin.x + px(shift) + v.x, element_bounds.origin.y + v.top),
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
        // 居中阅读栏：平台给的是元素相对坐标，需再减掉内容列水平移位才落在帧内坐标系。
        let marker = self.frame.borrow();
        let f = marker.as_ref().map(|r| &**r)?;
        // 元素可用宽取最近一帧 `last_bounds`（paint 写入）；无则无移位（0）。
        let elem_w = self.last_bounds().map(|b| f32::from(b.size.width)).unwrap_or(0.);
        let shift = self.reading_col_shift(elem_w);
        let p = gpui::point(point.x - gpui::px(shift), point.y);
        let off = super::element::source_offset_at_local(self, f, p);
        drop(marker);
        Some(self.doc.byte_to_utf16(off))
    }

    fn text_length_utf16(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.len_utf16())
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        // A1 只读门控：不接收 IME/文本输入（复制仍可用，走 selected_text_range）。
        self.editable
    }
}
