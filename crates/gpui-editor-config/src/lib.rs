//! 集中管理全项目的「写死配置常量」。
//!
//! 目的：把散落在各模块的编译期常量（几何/字号、性能阈值、时间、配色）集中到
//! 一处统一管理、跨模块共享；按领域分子模块，调一个参数不用满项目翻。
//!
//! 约定：
//! - 保持编译期 `const`（零开销内联）；涉及 gpui 类型（如 `Pixels`）的常量也在
//!   配置内直接表达，使用处无需转换。
//! - 颜色语义字段已迁入 `gpui_editor_ui::theme::ColorScheme`（拆 crate 后由根包
//!   `crate::config` 别名指到本 crate）；此处保留的色值常量（如 `markdown::QUOTE_BAR_COLOR`）
//!   是跨预览/WYSIWYG 共享的少量几何配套色。
//! - 惰性全局资源（语法集、pdfium 句柄等）不属于「配置值」，不抽到这里。

pub mod image;
pub mod layout;
pub mod limits;
pub mod markdown;
pub mod math;
pub mod pdf;
pub mod timing;
pub mod user;
