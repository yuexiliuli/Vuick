//! 文本缓冲：**单绳存储 + 可逆编辑记录**。
//!
//! 设计吸收：
//! - **VSCode/Monaco**：不建逐行对象（`ropey::Rope` 一棵树承载全文，行号查询 O(log n)），
//!   内存与文档大小成正比；编辑只记录「操作」而非整篇快照。
//! - **zed**：编辑事务是撤销/重做的最小单位，光标随事务一起恢复（见 [`crate::editor::undo`]）。
//!
//! 坐标约定：对外一律 **UTF-8 字节偏移**（Rope 切片天然按字节）；行/列换算由
//! [`RopeExt`]（`gpui_component::input::RopeExt`）提供，列在本模块里指「字符列」
//! （Unicode 标量数），与渲染的等宽列同向。

use std::ops::Range;

use gpui::SharedString;
use gpui_component::input::{Rope, RopeExt};
use ropey::LineType;

/// 一条已应用的文本编辑（可逆），撤销/重做的原子单位。
///
/// 语义：把 ``[`start`, `start+old_text.len()`)`` 替换为 `new_text`。
/// `range`（替换位置）在「编辑后」文本中依然有效，因为撤销/重做按严格 LIFO 回放，
/// 之前/之后的编辑不会改变本记录锚点之后的相对位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// 被替换区间的起点（同时也是撤销/重做替换的锚点）。
    pub start: usize,
    /// 被移除的旧文本（撤销时插回）。
    pub old_text: SharedString,
    /// 插入的新文本（重做时插回）。
    pub new_text: SharedString,
}

impl Edit {
    /// 编辑前被替换的区间。
    pub fn replaced_range(&self) -> Range<usize> {
        self.start..self.start + self.old_text.len()
    }

    /// 编辑后新文本占据的区间。
    pub fn inserted_range(&self) -> Range<usize> {
        self.start..self.start + self.new_text.len()
    }

    /// 全局字节增量（新文长度 − 旧文长度）。
    pub fn delta(&self) -> isize {
        self.new_text.len() as isize - self.old_text.len() as isize
    }

    /// `new_text` 是否包含换行（Enter/粘贴会因此成为独立撤销单元）。
    pub fn inserts_newline(&self) -> bool {
        self.new_text.contains(['\n', '\r'])
    }

    /// 逆操作：把 `new_text` 换回 `old_text`（撤销时应用）。
    pub fn reversed(&self) -> Edit {
        Edit {
            start: self.start,
            old_text: self.new_text.clone(),
            new_text: self.old_text.clone(),
        }
    }
}

/// 文本缓冲：Rope 全文 + 版本号（供缓存失效判断）。
///
/// 行模型：以 LF 为行分隔符（`metric_lines_lf`，CR 视为普通字符）；
/// 尾部换行会产生一个「幻影空行」（`"a\n"` 视为 2 行）——与编辑器
/// 的行模型一致：光标可停留在末尾空行上。
#[derive(Clone)]
pub struct Buffer {
    text: Rope,
    version: u64,
}

impl Default for Buffer {
    fn default() -> Self {
        Self {
            text: Rope::from(""),
            version: 0,
        }
    }
}

impl Buffer {
    /// 以文本创建缓冲。
    pub fn new(source: impl AsRef<str>) -> Self {
        Self {
            text: Rope::from(source.as_ref()),
            version: 0,
        }
    }

    /// 已经存在 Rope 时直接接管（避免一次整篇拷贝）。
    pub fn from_rope(text: Rope) -> Self {
        Self { text, version: 0 }
    }

    /// 底层文本（只读）。
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// 缓存失效版本号：每次编辑 +1，高亮/布局缓存据此判定是否过期。
    pub fn version(&self) -> u64 {
        self.version
    }

    /// 总字节数。
    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    /// 总字符数（Unicode 标量）。
    pub fn char_count(&self) -> usize {
        self.text.len_chars()
    }

    /// UTF-16 code unit 总数（IME 用）。
    pub fn len_utf16(&self) -> usize {
        self.text.len_utf16()
    }

    /// 字节偏移 → UTF-16 偏移（O(log n)）。
    pub fn byte_to_utf16(&self, offset: usize) -> usize {
        self.text.byte_to_utf16_idx(offset.min(self.text.len()))
    }

    /// UTF-16 偏移 → 字节偏移（O(log n)）。
    pub fn utf16_to_byte(&self, offset_utf16: usize) -> usize {
        self.text.utf16_to_byte_idx(offset_utf16.min(self.text.len_utf16()))
    }

    /// 行数（LF 分隔；尾换行产生幻影空行）。
    pub fn line_count(&self) -> usize {
        self.text.lines_len()
    }

    /// 行首字节偏移。
    pub fn line_start(&self, line: usize) -> usize {
        self.text.line_start_offset(line)
    }

    /// 行尾字节偏移（不含换行符）。
    pub fn line_end(&self, line: usize) -> usize {
        self.text.line_end_offset(line)
    }

    /// 行内容字节长（不含换行符及之外的尾部）。
    pub fn line_len(&self, line: usize) -> usize {
        self.text.line_len(line)
    }

    /// 字节偏移所在行。
    pub fn line_of_offset(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        self.text.byte_to_line_idx(offset, LineType::LF)
    }

    /// 字节偏移在当前行内的字节列。
    pub fn byte_column(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        offset - self.line_start(self.line_of_offset(offset))
    }

    /// 字节偏移在当前行内的字符列（Unicode 标量，O(行)）。
    pub fn char_column(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        let line = self.line_of_offset(offset);
        self.text
            .slice_line(line)
            .slice(..self.byte_column(offset))
            .chars()
            .count()
    }

    /// 把「行内字符列」换算回字节偏移（越界夹到行尾；O(行)）。
    pub fn offset_for_char_column(&self, line: usize, column: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let ls = self.line_start(line);
        let le = self.line_end(line);
        let mut bytes = 0usize;
        let mut n = 0usize;
        for ch in self.text.slice(ls..le).chars() {
            if n >= column {
                break;
            }
            bytes += ch.len_utf8();
            n += 1;
        }
        ls + bytes
    }

    /// 某行的文本（不含换行符；越界返回空）。
    pub fn line_text(&self, line: usize) -> SharedString {
        SharedString::from(self.text.slice_line(line).to_string())
    }

    /// 截取某行的部分文本（`from_char` ~ `from_char+max_chars`，字符列），
    /// 用于横向滚动的窗口渲染（避免每次整行整形）。
    pub fn line_text_window(&self, line: usize, from_char: usize, max_chars: usize) -> SharedString {
        let ls = self.line_start(line);
        let le = self.line_end(line);
        if ls >= le {
            return SharedString::new_static("");
        }
        let mut start_byte = ls;
        let mut n = 0usize;
        for ch in self.text.slice(ls..le).chars() {
            if n >= from_char {
                break;
            }
            start_byte += ch.len_utf8();
            n += 1;
        }
        let mut end_byte = start_byte;
        n = 0;
        for ch in self.text.slice(start_byte..le).chars() {
            if n >= max_chars {
                break;
            }
            end_byte += ch.len_utf8();
            n += 1;
        }
        SharedString::from(self.text.slice(start_byte..end_byte).to_string())
    }

    /// 字节区间文本。
    pub fn text_in_range(&self, range: Range<usize>) -> SharedString {
        let start = self.clip(range.start);
        let end = self.clip(range.end);
        if start >= end {
            return SharedString::new_static("");
        }
        SharedString::from(self.text.slice(start..end).to_string())
    }

    /// 把偏移夹到 [0, len] 的合法区间。
    pub fn clip(&self, offset: usize) -> usize {
        offset.min(self.text.len())
    }

    /// 应用一次替换（返回可逆记录）。`range` 需在字符边界上。
    pub fn apply_edit(&mut self, range: Range<usize>, new_text: &str) -> Edit {
        let range = self.clip(range.start).min(self.clip(range.end))..self.clip(range.end);
        let old = SharedString::from(self.text.slice(range.clone()).to_string());
        self.text.replace(range.clone(), new_text);
        self.version += 1;
        Edit {
            start: range.start,
            old_text: old,
            new_text: new_text.into(),
        }
    }

    /// 撤销：把 `inserted_range` 还原为 `old_text`。
    pub fn undo_edit(&mut self, edit: &Edit) {
        self.text.replace(edit.inserted_range(), edit.old_text.as_ref());
        self.version += 1;
    }

    /// 重做：把 `replaced_range` 重新替换为 `new_text`。
    pub fn redo_edit(&mut self, edit: &Edit) {
        self.text.replace(edit.replaced_range(), edit.new_text.as_ref());
        self.version += 1;
    }

    /// 整篇文本（测试/导出用；O(n) 物化）。
    pub fn to_string(&self) -> String {
        self.text.to_string()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn assert_edit_roundtrip(source: &str, range: Range<usize>, new: &str) {
        let mut b = Buffer::new(source);
        let before = b.to_string();
        let e = b.apply_edit(range.clone(), new);
        let after = b.to_string();
        assert_eq!(
            after,
            format!("{}{}{}", &before[..range.start], new, &before[range.end..]),
            "apply 应等价于直接字符串替换"
        );
        b.undo_edit(&e);
        assert_eq!(b.to_string(), before, "undo 应还原");
        b.redo_edit(&e);
        assert_eq!(b.to_string(), after, "redo 应重放");
    }
}
