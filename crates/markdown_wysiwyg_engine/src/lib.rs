//! WYSIWYG 元素引擎核心 crate：宿主 `WyEditor` 的**渲染模型状态**（[`WyRenderModel`]）、
//! 纯类型/几何（[`element`]）、主题快照（[`theme`]）、渲染缓存状态机（[`cache`]/
//! [`prewarm`]）与 **`shape`/`frame`/`paint`/`hit` 渲染管线函数体**（自宿主
//! `wysiwyg/shape`、`wysiwyg/frame`、`wysiwyg/element/{paint,hit}` 迁入）。
//!
//! 宿主 `markdown_wysiwyg` 只保留 `WyEditorElement`（gpui `Element` 实现）与事件/编辑等
//! glue，实现 [`host::WyRenderHost`]（宿主只读态面）后把 `(&self.rd, self)` 传给引擎
//! 各渲染入口；`shape`/`frame`/`paint`/`hit` 模块函数是公开面（`pub`），宿主薄 shim
//! 经 re-export / 转发胶水保持 `wysiwyg::element::…` / `wysiwyg::frame::…` 既有路径不变。

pub mod cache;
pub mod code_parse;
pub mod docutil;
pub mod element;
pub mod frame;
pub mod hit;
pub mod host;
pub mod layout;
pub mod model;
pub mod paint;
pub mod prewarm;
pub mod shape;
pub mod theme;

/// 根级便捷重导出：宿主经 `pub use …::{WyTheme, WyRenderHost}` 保持既有路径。
pub use host::WyRenderHost;
pub use model::WyRenderModel;
pub use theme::WyTheme;

/// 迁移期别名：`crate::prof::` → `base::prof`（宿主同款，渲染管线现经服务端记录）。
use base::prof;
