//! 光标移动：纯函数，输入 `&Buffer` + 字节偏移 → 新字节偏移（或 + 首选列）。
//!
//! 设计吸收 zed `navigation`/`movement`：上下移动不依赖文本内容，而是把目标换算成
//! 「目标列」（这里为字符列，见 [`crate::editor::selection::Selection::preferred_column`]），
//! 抗等宽/混排与行长度差异。

use crate::editor::buffer::Buffer;
use ropey::Rope;

/// 上一个字符的起始字节（已在偏移处或越界则不变）。
pub fn char_left(rope: &Rope, offset: usize) -> usize {
    let mut at = offset.min(rope.len());
    if at == 0 {
        return 0;
    }
    loop {
        at -= 1;
        if at == 0 || rope.is_char_boundary(at) {
            break;
        }
    }
    at
}

/// 下一个字符的起始字节（已在末尾则不变）。
pub fn char_right(rope: &Rope, offset: usize) -> usize {
    let at = offset.min(rope.len());
    if at >= rope.len() {
        return rope.len();
    }
    let ch = rope.slice(at..).chars().next();
    at + ch.map_or(0, |c| c.len_utf8())
}

/// 当前偏移左侧紧邻字符。
pub fn prev_char(rope: &Rope, offset: usize) -> Option<char> {
    let start = char_left(rope, offset);
    if start >= offset {
        return None;
    }
    rope.slice(start..offset).chars().next()
}

/// 当前偏移处字符。
pub fn next_char(rope: &Rope, offset: usize) -> Option<char> {
    if offset >= rope.len() {
        return None;
    }
    rope.slice(offset..).chars().next()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 移动到前一个词的词首（跳过左侧空白）。
pub fn word_left(rope: &Rope, offset: usize) -> usize {
    let mut pos = char_left(rope, offset);
    while let Some(c) = prev_char(rope, pos) {
        if c.is_whitespace() {
            pos = char_left(rope, pos);
        } else {
            break;
        }
    }
    while let Some(c) = prev_char(rope, pos) {
        if is_word_char(c) {
            pos = char_left(rope, pos);
        } else {
            break;
        }
    }
    pos
}

/// 移动到下一个词的词尾（跳过右侧空白）。
pub fn word_right(rope: &Rope, offset: usize) -> usize {
    let mut pos = offset.min(rope.len());
    while let Some(c) = next_char(rope, pos) {
        if c.is_whitespace() {
            pos = char_right(rope, pos);
        } else {
            break;
        }
    }
    while let Some(c) = next_char(rope, pos) {
        if is_word_char(c) {
            pos = char_right(rope, pos);
        } else {
            break;
        }
    }
    pos
}

/// 行首（字符列 0）。
pub fn to_line_start(buffer: &Buffer, offset: usize) -> usize {
    buffer.line_start(buffer.line_of_offset(offset))
}

/// 行尾（不含换行符）。
pub fn to_line_end(buffer: &Buffer, offset: usize) -> usize {
    buffer.line_end(buffer.line_of_offset(offset))
}

/// 整篇起点。
pub fn to_doc_start(_buffer: &Buffer) -> usize {
    0
}

/// 整篇终点。
pub fn to_doc_end(buffer: &Buffer) -> usize {
    buffer.len()
}

/// 向 `row_delta` 方向移动光标。`preferred_column` 为已有目标列；`None` 时以当前
/// 字符列作为目标列并回传（首次上下移动的记忆）。返回 `(新偏移, 目标列)`。
pub fn move_vertical(
    buffer: &Buffer,
    offset: usize,
    row_delta: isize,
    preferred_column: Option<u32>,
) -> (usize, u32) {
    let row = buffer.line_of_offset(offset);
    let total = buffer.line_count();
    if total == 0 {
        return (0, 0);
    }
    let target_row = (row as isize + row_delta).clamp(0, total as isize - 1) as usize;
    let col = preferred_column.unwrap_or_else(|| buffer.char_column(offset) as u32);
    let new_offset = buffer.offset_for_char_column(target_row, col as usize);
    (new_offset, col)
}
