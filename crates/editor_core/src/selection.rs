//! 选区与光标模型。
//!
//! 设计吸收：
//! - **gpui-component**：单选区用 `anchor`/`head` 两端点 + 折叠方向，头（`head`）即光标。
//! - **zed**：`SelectionGoal`（首选列）让上下移动在不等长行下保持目标列
//!   （这里简化为「字符列」，见 [`crate::editor::movement`]）。
//!
//! 坐标统一为 UTF-8 字节偏移。

use std::ops::Range;

/// 单一选区：`head` 是活动端（光标），`anchor` 是另一端（Shift/拖拽固定端）。
///
/// `preferred_column`：上下移动的目标「字符列」，跨行记忆（`None` = 尚无目标列，
/// 首次上下移动时以当前列为准并记下）；左右移动/点击后清除。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    pub preferred_column: Option<u32>,
}

impl Selection {
    /// 折叠光标（无选区）。
    pub fn collapsed(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
            preferred_column: None,
        }
    }

    /// 显式选区。
    pub fn from_range(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            preferred_column: None,
        }
    }

    /// 活动端（光标）。
    pub fn head(&self) -> usize {
        self.head
    }

    /// 锚点端。
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// 是否折叠（无选区）。
    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    pub fn is_empty(&self) -> bool {
        self.is_collapsed()
    }

    /// 是否反选（`head < anchor`）。
    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }

    /// 选区起点。
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// 选区终点。
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// 字节区间（`start..end`）。
    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    /// 选区字节长。
    pub fn len_bytes(&self) -> usize {
        self.end() - self.start()
    }

    /// 只移动光标到 `offset`（折叠选区、清空首选列）。
    pub fn place(&mut self, offset: usize) {
        self.anchor = offset;
        self.head = offset;
        self.preferred_column = None;
    }

    /// 把 `head` 设为新位置（拓展/收缩选区），固定 `anchor`。
    pub fn set_head(&mut self, new_head: usize) {
        self.head = new_head;
    }

    /// 光标移动到 `offset`，并（可选）记录首选列。
    pub fn move_cursor(&mut self, offset: usize, preferred_column: Option<u32>) {
        self.place(offset);
        self.preferred_column = preferred_column;
    }

    /// 记录当前所处字符列为首选列。
    pub fn remember_column(&mut self, column_char: usize) {
        self.preferred_column = Some(column_char as u32);
    }
}
