//! 自研 Markdown 渲染器 + 所见即所得编辑（WYSIWYG）。M5-B5 P4-2 终态：**MD 与
//! EPUB 全部走统一 `WyEditor`（`wysiwyg` 的 Doc+shard 管线 + 只读宿主）**，A 侧
//! `MarkdownPreview` 预览引擎已删除；`view` 仅留共享助手（style/dimensions/
//! selection_color/find_text_range）。
//!
//! - `parse`：解析管线（行解析 `rows` / 块索引 `blocks`(`_scan`) / 引用索引 `citations`…）
//! - `doc`：统一文档模型（`Doc`/`DocRow`，行整形与量程单源）
//! - `layout`：共享行整形核心（`DocRow → 横向分片几何`，`shape_docrow_shard`）
//! - `wysiwyg`：本项目自研 WYSIWYG 编辑器（[`WyEditor`]，auto-grow + 宿主滚动；
//!   元素级所见即所得，直接编辑 `Doc` 数据源；只读态 = 统一只读预览宿主）。
//! - `view`：仅共享助手（见上），不再是独立预览引擎。

pub(crate) mod view;

mod parse;
mod doc;
mod ir;
/// 共享行整形核心（B5 蓝图 Phase 1）：`DocRow → 横向分片几何`。
/// M5-B5 P4-2 后：预览引擎已删，`layout` 的 `Shard`/`shape_docrow_shard` 目前只被
/// 自己的 `tests` 消费（`wysiwyg` 用自己的 `shape/*`）——按蓝图「DocRow/shard 保留」
/// 保留本模块及其回归测试，`#[allow(dead_code)]` 挂起未消化的死代码告警。
#[allow(dead_code)]
mod layout;
pub(crate) mod wysiwyg;

pub use parse::RowAlign;
#[cfg(test)]
pub use parse::scan_blocks;
pub use wysiwyg::{WyEditor, WyEvent};
