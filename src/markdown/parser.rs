//! 兼容门面：`crate::markdown::parser::*` 的全部公开路径在此保持可用。
//!
//! 2026-09-24 重构：真正的实现已按职责拆入 [`super::parse`] 子目录
//! （`rows` / `blocks` / `blocks_scan` / `citations` / `cells` / `inline_style` /
//! `recognition` / `parity`），本模块仅做重导出，不包含任何业务逻辑。

pub use super::parse::*;
pub(crate) use super::parse::{
    build_line_starts_u32, inline_style_mapped, parse_image_line, parse_ref,
    rows_memory_estimate, split_table_cells_mapped, text_segment,
};
