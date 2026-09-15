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

/// 单字符的「渲染宽度单位」（等宽格数，供横向内容宽度估算）。
///
/// - Tab = 4 格；
/// - ASCII（0x00-0x7F）= 1 格；
/// - 其余（CJK 全角、emoji、复杂文字等）保守按 2 格。
///
/// 宁可高估（横向多留白）也绝不低估：低估会让 `StyledText` 在行内折行，行号、
/// 点击、光标全部错位并重叠（用户实测：中文长行自动"换行"、序号没对齐、点下一行
/// 文本重叠）。
pub(crate) fn char_width_units(c: char) -> usize {
    match c {
        '\t' => 4,
        c if c.is_ascii() => 1,
        _ => 2,
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
    /// 全文最长（字符数，Unicode 标量）的行 —— 保留给等宽列近似/测试。
    /// 一致性：`Buffer::new/from_rope/set_text` 全量重算；编辑类操作**只增不减**
    /// （`refresh_max` 被增大的行更新），删除最长行后保持旧值（横向过度留白可接受，
    /// 换来编辑最长行时 O(编辑大小) 而非 O(n) 每次重扫）。
    max_line_chars: usize,
    /// 全文最宽行（**宽度单位**，见 [`char_width_units`]；CJK/全角=2、ASCII=1、
    /// Tab=4）——横向内容宽度的依据。与 `max_line_chars` 同样「只增不减」维护，
    /// 保证任何行的真实字形宽 ≤ `max_line_units × 等宽格宽`，从而 StyledText 不会
    /// 行内折行（折行会让行号/点击/光标全部错位并重叠）。
    max_line_units: usize,
}

impl Default for Buffer {
    fn default() -> Self {
        Self {
            text: Rope::from(""),
            version: 0,
            max_line_chars: 0,
            max_line_units: 0,
        }
    }
}

impl Buffer {
    /// 以文本创建缓冲。
    pub fn new(source: impl AsRef<str>) -> Self {
        let mut b = Self {
            text: Rope::from(source.as_ref()),
            version: 0,
            max_line_chars: 0,
            max_line_units: 0,
        };
        b.recompute_max_line();
        b
    }

    /// 已经存在 Rope 时直接接管（避免一次整篇拷贝）。
    pub fn from_rope(text: Rope) -> Self {
        let mut b = Self {
            text,
            version: 0,
            max_line_chars: 0,
            max_line_units: 0,
        };
        b.recompute_max_line();
        b
    }

    /// 全文最长行（字符数）——O(1) 读，供等宽列近似/测试。
    pub fn max_line_chars(&self) -> usize {
        self.max_line_chars
    }

    /// 全文最宽行（宽度单位）——O(1) 读，供横向内容宽度（不折行的依据）。
    pub fn max_line_units(&self) -> usize {
        self.max_line_units
    }

    fn recompute_max_line(&mut self) {
        let mut mx_chars = 0usize;
        let mut mx_units = 0usize;
        for i in 0..self.text.lines_len() {
            let line = self.text.slice_line(i);
            mx_chars = mx_chars.max(line.chars().count());
            mx_units = mx_units.max(line.chars().map(char_width_units).sum());
        }
        self.max_line_chars = mx_chars;
        self.max_line_units = mx_units;
    }

    /// 编辑后增量更新最长行（字节区间 `[from, to)` 覆盖的新行）。
    /// 只增不减：不大于当前值就保持（避免删除最长行时每次 O(n) 重扫）。
    fn refresh_max_line(&mut self, from: usize, to: usize) {
        let from = self.clip(from);
        let to = self.clip(to.max(from));
        if from >= to {
            return;
        }
        let mut mx_chars = self.max_line_chars;
        let mut mx_units = self.max_line_units;
        for i in self.line_of_offset(from)..=self.line_of_offset(to) {
            let line = self.text.slice_line(i);
            mx_chars = mx_chars.max(line.chars().count());
            mx_units = mx_units.max(line.chars().map(char_width_units).sum());
        }
        self.max_line_chars = mx_chars;
        self.max_line_units = mx_units;
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

    /// 行数（LF 分隔；尾换行产生幻影空行）——编辑器光标模型语义。
    pub fn line_count(&self) -> usize {
        self.text.lines_len()
    }

    /// 显示语义行数（与 `str::lines()` 一致：`"a\nb\n"`→2，尾换行不追加一行）。
    /// 与 [`line_count`]（含幻影空行，供 ListState/光标定位用）不同——状态栏/统计用这个。
    /// O(log n)（只查末行是否为空），不物化全文。
    pub fn display_line_count(&self) -> usize {
        let n = self.text.lines_len();
        // 非空文本的末行为空 ⟺ 以换行结尾（ropey 幻影空行 = str::lines 丢弃的尾段；"\r\n" 同理）。
        let trailing_newline = self.len() > 0 && self.text.line_len(n - 1) == 0;
        (n - usize::from(trailing_newline)).max(1)
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

    /// 字节偏移在当前行内的「宽度单位」列（O(行)，见 [`char_width_units`])——
    /// 与渲染等宽格一致：CJK 一个占 2 格。横向滚动/光标 reveal 用这个，否则
    /// 中文长行按字符列 reveal 会少滚一半，光标跑出可视区。
    pub fn unit_column(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        let line = self.line_of_offset(offset);
        self.text
            .slice_line(line)
            .slice(..self.byte_column(offset))
            .chars()
            .map(char_width_units)
            .sum()
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
        self.refresh_max_line(range.start, range.start + new_text.len());
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
        self.refresh_max_line(edit.start, edit.start + edit.old_text.len());
    }

    /// 重做：把 `replaced_range` 重新替换为 `new_text`。
    pub fn redo_edit(&mut self, edit: &Edit) {
        self.text.replace(edit.replaced_range(), edit.new_text.as_ref());
        self.version += 1;
        self.refresh_max_line(edit.start, edit.start + edit.new_text.len());
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
