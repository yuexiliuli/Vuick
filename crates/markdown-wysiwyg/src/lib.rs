//! `markdown-wysiwyg` crate：所见即所得编辑（WYSIWYG）独立 crate。
//!
//! 自 `crates/markdown` 的 `wysiwyg` 子树拆出（编译级解耦）：承载原
//! `crates/markdown/src/wysiwyg/**`（[`WyEditor`] 编辑器本体及其模块树）与
//! `crates/markdown/src/layout/**`（共享行整形核心，其 `WyTheme` 依赖属 wy 面、
//! 拆 crate 时随 wy 同时迁出，打破 layout↔wysiwyg 的 crate 内双向引用）。
//! 向上依赖 `markdown`（共享 `parse`/`doc`/`ir`/`view` 层），行为与拆分前完全等价。
//!
//! 公共面与拆分前的 `markdown::wysiwyg::*` 一一对应（crate 根扁平重导出）：
//! `markdown_wysiwyg::WyEditor` / [`WyEvent`] / [`WyOpenResult`] / [`TocEntries`] /
//! [`NoteMarkerHit`] / [`build_frame`] / [`bind_keys`] / [`frame::build_prebuild_pass1`] …
//! `IrKind` / `MathAlign` / `LinkHit` 等跨层重导出保持原位。

// ---------------------------------------------------------------------------
// 迁移期 crate-root 短别名（与 markdown / 其它拆 crate 同一策略）：让 wysiwyg 子树内
// 既有的 `crate::markdown::<core>`（向上依赖）/ `crate::markdown::wysiwyg::`（自引用）/
// `crate::markdown::layout::`（本地 layout）/ `crate::prof::` / `crate::util::` /
// `crate::ir::` / `crate::test_support::` 旧路径在拆 crate 后零改动即可解析。
// ---------------------------------------------------------------------------
/// 迁移期别名：`crate::prof::` → `base::prof`；`crate::util::` → `base::util`。
use base::prof;
use base::util;
/// 迁移期别名：`crate::doc::` / `crate::ir::` / `crate::view::` → `markdown` 对应模块
/// （`super::doc` / `super::view`（doc 内链）/ `crate::ir::IrKind` 等根级路径原样解析）。
use markdown::{doc, ir};
/// `markdown_wysiwyg::view` 仅供包内 doc 内链引用（`wysiwyg/mod.rs` 的
/// `[`super::view::style::StyleParams`]`），非真实代码消费 → allow 压未用告警。
#[allow(unused_imports)]
use markdown::view;
/// 迁移期别名（私有 shim）：`crate::markdown::<core>` → `::markdown::<core>`（`::markdown`
/// 是依赖 crate，非内部模块）；`crate::markdown::wysiwyg` / `crate::markdown::layout`
/// → 本 crate 的本地模块（拆 crate 后它们不再是 markdown 的导出面）。
mod markdown {
    pub use ::markdown::*;
    pub use crate::wysiwyg;
    pub(crate) use crate::layout;
}

/// 所见即所得编辑器（原 `markdown::wysiwyg` 模块树整体迁入，含 `element`/`frame`/
/// `mermaid`/`shape`/`math` 等子模块）。
pub mod wysiwyg;
/// 共享行整形核心（原 `markdown::layout`，随 wy 迁出；仅供本 crate 消费）。
/// 保留原 `markdown` lib.rs 对 `mod layout` 的既有 `#[allow(dead_code)]`：
/// `Shard`/`shape_docrow_shard` 目前只被自己的 `tests` 消费，`shape/*` 走 wy 自带实现。
#[allow(dead_code)]
pub(crate) mod layout;
/// 测试键位助手（`bind_local_keymaps`，见文件头）。
#[cfg(test)]
pub mod test_support;

/// 扁平重导出：`wysiwyg::*` 的 pub 面搬上 crate 根，保持 `markdown_wysiwyg::WyEditor`
/// 等与旧 `markdown::wysiwyg::WyEditor` 对应（shell 等消费方只换 crate 前缀）。
pub use wysiwyg::*;
