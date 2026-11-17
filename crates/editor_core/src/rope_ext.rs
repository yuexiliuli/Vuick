//! `ropey::Rope` 的本地扩展（收编自 gpui-component `input::RopeExt` 的应用用法子集）。
//!
//! `gpui_component::input::Rope` 就是对 `ropey::Rope` 的 re-export（同一版本依赖）。
//! 本模块把本项目用到的几个便利方法（行切片 / 行号↔字节 / 区间替换 / 字节↔点）就地
//! 移植，替换 `gpui_component::input::RopeExt`，行为与原文**逐字节一致**（含越界、
//! CRLF、字符边界语义），为彻底移除 gpui-component 依赖清障（见 `docs/ui-migration.md`
//! #1）。未用到的 RopeExt 方法（word 查询、UTF-16 换算等）暂不移植。

use std::ops::Range;

use ropey::{LineType, Rope, RopeSlice};

/// 行内位置：`row` 行号 + `column` **字节**列（0 基，对齐原 gpui-component `Point`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// 字符边界裁剪方向（对齐原 `sum_tree::Bias` 的 Left/Right 语义，仅这两档所需）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}

/// 逐行迭代器：每行 slice 含 `\r`、不含 `\n`（对齐原 `RopeLines`）。
pub struct RopeLines<'a> {
    rope: &'a Rope,
    row: usize,
    end_row: usize,
}

impl<'a> RopeLines<'a> {
    pub fn new(rope: &'a Rope) -> Self {
        let end_row = rope.lines_len();
        Self {
            row: 0,
            end_row,
            rope,
        }
    }
}

impl<'a> Iterator for RopeLines<'a> {
    type Item = RopeSlice<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.row >= self.end_row {
            return None;
        }
        let line = self.rope.slice_line(self.row);
        self.row += 1;
        Some(line)
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.row = self.row.saturating_add(n);
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.end_row - self.row;
        (len, Some(len))
    }
}

impl ExactSizeIterator for RopeLines<'_> {}
impl std::iter::FusedIterator for RopeLines<'_> {}

/// `ropey::Rope` 便捷方法（收编自 gpui-component `input::RopeExt` 的应用用法子集）。
pub trait RopeExt {
    /// 行首字节偏移（0 基）。
    fn line_start_offset(&self, row: usize) -> usize;
    /// 行尾字节偏移（含 `\n`）；行越界时返回 rope 末尾。
    fn line_end_offset(&self, row: usize) -> usize;
    /// 行切片（含 `\r`、不含 `\n`）；行越界返回空切片。
    fn slice_line(&self, row: usize) -> RopeSlice<'_>;
    /// 逐行迭代（每个 slice 含 `\r`、不含 `\n`）。
    fn iter_lines(&self) -> RopeLines<'_>;
    /// 行数（LF 计行）。
    fn lines_len(&self) -> usize;
    /// 行的字节长度（含 `\r`、不含 `\n`）；越界返回 0。
    fn line_len(&self, row: usize) -> usize;
    /// 把字节区间替换为新文本（区间端点裁到字符边界）。
    fn replace(&mut self, range: Range<usize>, new_text: &str);
    /// 字节偏移 → (行, 字节列)。
    fn offset_to_point(&self, offset: usize) -> Point;
    /// (行, 字节列) → 字节偏移（行越界返回 rope 末尾）。
    fn point_to_offset(&self, point: Point) -> usize;
    /// 把偏移裁到字符边界（`Left` 向前取整 / `Right` 向后取整）。
    fn clip_offset(&self, offset: usize, bias: Bias) -> usize;
}

impl RopeExt for Rope {
    fn slice_line(&self, row: usize) -> RopeSlice<'_> {
        let total_lines = self.lines_len();
        if row >= total_lines {
            return self.slice(0..0);
        }

        let line = self.line(row, LineType::LF);
        if line.len() > 0 {
            let line_end = line.len() - 1;
            if line.is_char_boundary(line_end) && line.char(line_end) == '\n' {
                return line.slice(..line_end);
            }
        }

        line
    }

    fn iter_lines(&self) -> RopeLines<'_> {
        RopeLines::new(self)
    }

    fn line_len(&self, row: usize) -> usize {
        self.slice_line(row).len()
    }

    fn line_start_offset(&self, row: usize) -> usize {
        self.point_to_offset(Point::new(row, 0))
    }

    fn offset_to_point(&self, offset: usize) -> Point {
        let offset = self.clip_offset(offset, Bias::Left);
        let row = self.byte_to_line_idx(offset, LineType::LF);
        let line_start = self.line_to_byte_idx(row, LineType::LF);
        let column = offset.saturating_sub(line_start);
        Point::new(row, column)
    }

    fn point_to_offset(&self, point: Point) -> usize {
        if point.row >= self.lines_len() {
            return self.len();
        }

        let line_start = self.line_to_byte_idx(point.row, LineType::LF);
        // 行尾偏移 = 行首 + 行内容长度（`slice_line` 含 `\r`、不含 `\n`），与
        // `line_end_offset` 同口径；但不得调用 `line_end_offset`（它经 `line_start_offset`
        // 回指本函数 → 递归）。列超行尾夹到行尾（与 `offset_to_point` 逆映射口径一致），
        // 行首加列用 saturating 防 `usize::MAX` 列 debug 溢出 panic。
        let line_end = line_start + self.line_len(point.row);
        line_start.saturating_add(point.column).min(line_end)
    }

    fn line_end_offset(&self, row: usize) -> usize {
        if row > self.lines_len() {
            return self.len();
        }

        self.line_start_offset(row) + self.line_len(row)
    }

    fn lines_len(&self) -> usize {
        self.len_lines(LineType::LF)
    }

    fn replace(&mut self, range: Range<usize>, new_text: &str) {
        let range = self.clip_offset(range.start, Bias::Left)
            ..self.clip_offset(range.end, Bias::Right);
        self.remove(range.clone());
        self.insert(range.start, new_text);
    }

    fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        if offset > self.len() {
            return self.len();
        }

        if self.is_char_boundary(offset) {
            return offset;
        }

        if bias == Bias::Left {
            self.floor_char_boundary(offset)
        } else {
            self.ceil_char_boundary(offset)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    /// 收编后行为与 gpui-component `RopeExt` 原文一致（回归护栏：含 CRLF/中文/越界）。
    #[test]
    fn ported_semantics_match_original() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.lines_len(), 4);
        assert_eq!(rope.slice_line(0).to_string(), "Hello");
        assert_eq!(rope.slice_line(1).to_string(), "World\r");
        assert_eq!(rope.slice_line(2).to_string(), "This is a test 中文");
        assert_eq!(rope.slice_line(3).to_string(), "Rope");
        assert_eq!(rope.slice_line(6).to_string(), "", "越界返回空行");
        assert_eq!(rope.line_len(1), 6, "World\\r = 6");
        assert_eq!(rope.line_len(4), 0, "越界行长为 0");
        assert_eq!(rope.line_start_offset(1), 6);
        assert_eq!(rope.line_end_offset(0), 5);
        assert_eq!(rope.line_end_offset(3), 39);
        assert_eq!(rope.line_end_offset(10), 39, "越界返回 rope 末尾");

        let mut m = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        m.replace(6..11, "Universe");
        assert_eq!(m.to_string(), "Hello\nUniverse\r\nThis is a test 中文\nRope");
        let mut m = Rope::from("中文");
        m.replace(0..1, "New");
        assert_eq!(m.to_string(), "New文", "非字符边界替换应裁到边界");

        // 偏移 → 点（字节列）
        assert_eq!(
            Rope::from("a 中文🎉 test\nRope").offset_to_point("a 中".len()),
            Point::new(0, 5),
        );
        assert_eq!(
            Rope::from("a 中文🎉 test\nRope")
                .offset_to_point("a 中文🎉 test\nR".len()),
            Point::new(1, 1),
        );

        // 边界裁剪
        let rope = Rope::from("Hello 中文🎉 test\nRope");
        assert_eq!(rope.clip_offset(7, Bias::Left), 6);
        assert_eq!(rope.clip_offset(7, Bias::Right), 9);
        assert_eq!(rope.clip_offset(100, Bias::Left), rope.len());

        // 逐行迭代
        let lines: Vec<_> = rope.iter_lines().map(|r| r.to_string()).collect();
        assert_eq!(lines, vec!["Hello 中文🎉 test", "Rope"]);
    }

    /// M 红测：`point_to_offset` 对超出行长的列不夹取、不截断——`line_start + column`
    /// 可能越到下一行/越出 rope（错位）。
    ///
    /// 期望语义：列超行尾 → 夹到行尾（offset 等价于该行 `line_end_offset`，与
    /// `offset_to_point` 逆映射口径一致），不越界。当前返回越界偏移 → 红。
    #[test]
    fn point_to_offset_clamps_column_at_line_end() {
        let rope = Rope::from("ab\nlong line here\nx");
        // 行 1 = "long line here"（14B），行首偏移 3，行尾偏移 = 3+14 = 17。
        let line_end = rope.line_end_offset(1);
        assert_eq!(line_end, 17, "行尾偏移先验（夹具演算）");
        // 超出行尾大列：应夹到行尾，不得越到下一行/越出 rope。
        let col = rope.line_len(1) + 1000;
        let got = rope.point_to_offset(Point::new(1, col));
        assert_eq!(
            got, line_end,
            "列 {} 超出行尾应夹取到行尾 {}，实测 {}",
            col, line_end, got
        );
    }

    /// M 红测（极限列）：`usize::MAX` 列当前 `line_start + column` 直接 debug 溢出 panic。
    /// 期望：不 panic 并把列夹取到行尾。
    #[test]
    fn point_to_offset_no_panic_on_max_column() {
        let rope = Rope::from("ab\nlong line here\nx");
        let line_end = rope.line_end_offset(1);
        let got = rope.point_to_offset(Point::new(1, usize::MAX));
        assert_eq!(got, line_end, "usize::MAX 列不得溢出，应夹取到行尾");
    }
}
