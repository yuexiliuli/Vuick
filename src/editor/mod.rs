//! 自研的文本/代码编辑器组件（首版，`src/editor/`）。
//!
//! 这是**独立于 gpui-component `Editor`** 的自有实现：从缓冲、选区、光标移动、
//! 撤销/重做、语法高亮到视图渲染全部自持，可作为预览/编辑区的可替换组件。
//!
//! # 设计来源（调研吸收）
//!
//! - **zed `editor`/`text` crates**（本地 `.../zed-*/399258f/crates`）：
//!   - 只读快照 + 一次性查询模型：交互查询尽量只读、一步到位；
//!   - 坐标分层：字节偏移 ↔（行, 字符列）双射；选区存 `anchor/head`；
//!   - `SelectionGoal`（首选列）：上下移动在不等长行下保持目标列
//!     （[`selection::Selection::preferred_column`]）；
//!   - 事务 = 编辑单元，撤销/重做连选区一起恢复（[`undo`]）。
//! - **VSCode/Monaco**（[Text Buffer Reimplementation](https://code.visualstudio.com/blogs/2018/03/23/text-buffer-reimplementation)）：
//!   - 不建逐行对象：`ropey::Rope` 一棵树承载全文 + O(log n) 行号查询
//!     （[`buffer::Buffer`]），内存正比于文档；
//!   - 编辑记「操作」而非快照：撤销/重做成本 O(k)（[`buffer::Edit`]）；
//!   - 统一行高 + 只渲染可见行（gpui `ListState` 行级虚拟化）。
//! - **gpui-component `input`**（本地 `.../gpui-component-*/a8d1d26/crates/base/src/input`）：
//!   - 单一编辑入口 `insert_edit`（一切修改汇聚，撤销分组统一）；
//!   - 无定时器确定性事务合并：连续打字邻接合并 / 退格首尾相接 / 原子编辑永不合并
//!     （[`undo::UndoManager`]）；
//!   - 窗口式高亮：解析限定「可见窗口 + 回看行」，缓存 `(版本, 行窗口)`
//!     （[`highlight::Highlighter`]）；
//!   - IME/文本输入经 `EntityInputHandler`（`EditorElement::paint` →
//!     `window.handle_input`），与组件库同款挂钩。
//!
//! # 里程碑/取舍（首版）
//!
//! - 无行内软换行（display 行 ≡ buffer 行）；光标列按等宽字符近似命中；
//! - 单选区（模型已支持扩展）；IME 组合各步为独立撤销单元；
//! - 行数变更用 `ListState::splice` 增量对齐（大文档避免整树重建）；
//!   打开/整篇替换/撤销重做仍是单次 O(n)（API 所限）。

#![allow(dead_code)] // 组件库公共 API：接入 app 主视图前，bin crate 会对未使用项误报 dead_code

pub mod buffer;
pub mod fold;
pub mod highlight;
pub mod movement;
pub mod selection;
pub mod undo;
mod view;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use buffer::{Buffer, Edit};
#[allow(unused_imports)]
pub use selection::Selection;
#[allow(unused_imports)]
pub use undo::{EditIntent, Transaction, UndoManager};
#[allow(unused_imports)]
pub use view::{CodeEditor, EditorEvent};
