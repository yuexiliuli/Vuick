//! 自研 Markdown 渲染 + 所见即所得编辑的**共享解析/文档层 crate**（M5-B5 P4-2 终态：
//! MD 与 EPUB 走 `markdown-wysiwyg` 的 `WyEditor`，本 crate 提供其上游共享层）。
//!
//! 自 `src/markdown/` 整体迁出的 crate（P3c，71 文件含全部 tests/ 子目录）；
//! M6 将 `wysiwyg` 面（35.8k 行/56 文件）与 `layout` 核心（依赖 `WyTheme`）拆为
//! 独立 `markdown-wysiwyg` crate——本 crate 保留 `parse`/`doc`/`ir`/`view` + 根文件，
//! 向新 crate 以 `pub mod` 暴露所需面（`#[doc(hidden)]` 压低文档噪音）。
//!
//! - `parse`：解析管线（行解析 `rows` / 块索引 `blocks`(`_scan`) / 引用索引 `citations`…）
//! - `doc`：统一文档模型（`Doc`/`DocRow`，行整形与量程单源）
//! - `ir`：共享语义 IR（`IrKind` 金标准）
//! - `view`：共享助手（`style::StyleParams` 排版参数快照 / `dimensions` 图片头嗅探 /
//!   `selection_color` / `find_text_range`），不再是独立预览引擎。
//!
//! `wysiwyg` / `layout` 已迁至 `crates/markdown-wysiwyg`，本 crate 不再含其代码。

/// 共享助手（`StyleParams` / `dimensions` / `selection_color` / `find_text_range`）。
pub mod view;

/// 解析管线（`rows` / `blocks`(`_scan`) / `citations` 等）。对外部消费面
/// （`markdown-wysiwyg` / shell）以 `pub mod` 暴露；`#[doc(hidden)]` 压低文档噪音。
#[doc(hidden)]
pub mod parse;
/// 统一文档模型（`Doc`/`DocRow`）。同上以 `pub mod` 暴露给新 crate。
#[doc(hidden)]
pub mod doc;
/// 共享语义 IR（金标准）。同上以 `pub mod` 暴露给新 crate。
#[doc(hidden)]
pub mod ir;

pub use parse::RowAlign;
/// 块索引扫描（供根包 app/tests 与 shell 跨 crate 消费）。
pub use parse::scan_blocks;
// pub use wysiwyg::{WyEditor, WyEvent}; // M6：wysiwyg 已拆出为新 crate，此导出随迁抬平。

// ---------------------------------------------------------------------------
// 迁移期 crate-root 短别名（与根包 P1/P2/P3a/b 同一策略）：
// 让本 crate 内部既有的 `crate::markdown::`（自引用）/ `config::` /
// `crate::prof::` / `crate::util::` / `ui::` / `editor::` /
// `syntax::` / `ime::` 旧路径在拆 crate 后零改动即可解析。
// ---------------------------------------------------------------------------
/// 迁移期别名：自引用。本 crate 根 = 原 bin 里 `markdown` 模块，`crate::markdown::X` → `crate::X`。
use crate as markdown;
/// 迁移期别名：`crate::prof::` → `base::prof`。
use base::prof;
/// 迁移期别名：`crate::util::` → `base::util`（wysiwyg 拆出后本 crate 暂无人用，
/// 保留别名供后续消费面即插即用）。
#[allow(unused_imports)]
use base::util;
