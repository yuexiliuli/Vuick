//! 自研 Markdown 渲染器 + 所见即所得编辑（WYSIWYG）。M5-B5 P4-2 终态：**MD 与
//! EPUB 全部走统一 `WyEditor`（`wysiwyg` 的 Doc+shard 管线 + 只读宿主）**，A 侧
//! `MarkdownPreview` 预览引擎已删除；`view` 仅留共享助手（style/dimensions/
//! selection_color/find_text_range）。
//!
//! 自 `src/markdown/` 整体迁出的 crate（P3c，71 文件含全部 tests/ 子目录）。
//! 本 crate 依赖 `editor`（WYSIWYG 内嵌 `CodeEditor`，合法单向）；`editor` 不依赖
//! 本 crate（warm_emoji 已下沉 base）——`editor↔markdown` 环断开。
//!
//! - `parse`：解析管线（行解析 `rows` / 块索引 `blocks`(`_scan`) / 引用索引 `citations`…）
//! - `doc`：统一文档模型（`Doc`/`DocRow`，行整形与量程单源）
//! - `layout`：共享行整形核心（`DocRow → 横向分片几何`，`shape_docrow_shard`）
//! - `wysiwyg`：本项目自研 WYSIWYG 编辑器（[`WyEditor`]，auto-grow + 宿主滚动；
//!   元素级所见即所得，直接编辑 `Doc` 数据源；只读态 = 统一只读预览宿主）。
//! - `view`：仅共享助手（见上），不再是独立预览引擎。

pub mod view;

mod parse;
mod doc;
mod ir;
/// 共享行整形核心（B5 蓝图 Phase 1）：`DocRow → 横向分片几何`。
/// M5-B5 P4-2 后：预览引擎已删，`layout` 的 `Shard`/`shape_docrow_shard` 目前只被
/// 自己的 `tests` 消费（`wysiwyg` 用自己的 `shape/*`）——按蓝图「DocRow/shard 保留」
/// 保留本模块及其回归测试，`#[allow(dead_code)]` 挂起未消化的死代码告警。
#[allow(dead_code)]
mod layout;
/// WYSIWYG 面（原 `pub(crate) mod wysiwyg` → `pub mod`，供 shell/app 与测试跨 crate 消费）。
pub mod wysiwyg;

pub use parse::RowAlign;
/// 块索引扫描（原 `#[cfg(test)]` 门控改无条件 pub，供根包 app/tests 跨 crate 消费）。
pub use parse::scan_blocks;
pub use wysiwyg::{WyEditor, WyEvent};

// ---------------------------------------------------------------------------
// 迁移期 crate-root 短别名（与根包 P1/P2/P3a/b 同一策略）：
// 让本 crate 内部既有的 `crate::markdown::`（自引用）/ `crate::config::` /
// `crate::prof::` / `crate::util::` / `crate::ui::` / `crate::editor::` /
// `crate::syntax::` / `crate::ime::` 旧路径在拆 crate 后零改动即可解析。
// ---------------------------------------------------------------------------
/// 迁移期别名：自引用。本 crate 根 = 原 bin 里 `markdown` 模块，`crate::markdown::X` → `crate::X`。
use crate as markdown;
/// 迁移期别名：`crate::config::` → `gpui_editor_config`。
use gpui_editor_config as config;
/// 迁移期别名：`crate::prof::` → `gpui_editor_base::prof`。
use gpui_editor_base::prof;
/// 迁移期别名：`crate::util::` → `gpui_editor_base::util`。
use gpui_editor_base::util;
/// 迁移期别名：`crate::ui::` → `gpui_editor_ui`。
use gpui_editor_ui as ui;
/// 迁移期别名：`crate::editor::` → `gpui_editor_editor`。
use gpui_editor_editor as editor;
/// 迁移期别名：`crate::syntax::` → `gpui_editor_syntax`。
use gpui_editor_syntax as syntax;
/// 迁移期别名：`crate::ime::` → `gpui_editor_ime`。
use gpui_editor_ime as ime;

/// 测试键位助手：`markdown` 独立 crate 后其测试不能再调根包的
/// `crate::actions::bind_local_keymaps`（该组合函数定义在根包，含 ui::root 上下文）。
/// 本助手 = `base::bind_input_keys`（Input 键上下文）+ `markdown::wysiwyg::bind_keys`
/// （wy-editor 键上下文），保留原组合对 markdown 测试关键的两套上下文
/// （markdown 测试不用 Root 键上下文）。
#[cfg(test)]
pub mod test_support;
