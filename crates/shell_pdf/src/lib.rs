//! PDF 域 crate（2026-11-11 自 `crates/shell` 的 `app/pdf_bookmarks` + `app/pdf_bg` +
//! `app/pdf_menu` 拆出）。
//!
//! 纯结构移动 + 域内解耦：把 PDF 域的**书签持久化 / 背景重着色 / 页操作动作逻辑**从
//! `EditorApp` 的 `impl` 块中整迁出来，使这些逻辑可独立编译 / 独立测试。宿主 `EditorApp`
//! 只保留薄适配：
//!
//! - **模型字段**：`pdf_bookmarks` / `pdf_bg` / `pdf_bg_custom` / `pdf_last_error_seen`
//!   仍在 `shell_core::AppModel`（跨 crate 命名由 shell_core 提供，本 crate 直接消费）。
//! - **宿主服务面**：toast 通知 / 弹窗互关统一经 [`menu::PdfHost`]（对照
//!   `shell_sidebar::SidebarHost` 先例）反调宿主；书签模型读数/写数、背景解析/重着色、
//!   页操作确认语义与错误映射全部落本 crate。
//! - **留宿主的薄适配**：PDF 右键菜单的**视图/挂载**（`ui::menu` 弹出结构 + EditorApp 的
//!   `pdf_menu_*` 字段）与 `pick_custom_pdf_bg` 的 win32 取色宿主 window 部分留 shell。
//!
//! ## 模块
//!
//! - [`bookmarks`]：阅读位置书签持久化（纯函数 + 模型读数/写数）。
//! - [`bg`]：画布色 / 纸色重着色解析与应用到 PdfViewer。
//! - [`menu`]：页操作动作逻辑（插入/删除/提取/删除注释/失败 toast 判定）与错误文案
//!   映射、[`PdfHost`] 宿主服务面、[`PdfToast`] 通知标记。
//!
//! 依赖只重分配 workspace 既有包（gpui / config / base / shell_core / pdf / serde_json /
//! rfd），不新增外部包。

pub mod bg;
pub mod bookmarks;
pub mod menu;

pub use menu::{PdfHost, PdfToast};
