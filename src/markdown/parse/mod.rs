//! 自研 Markdown **解析管线**：源码 → 行（`rows`）→ 块索引（`blocks` / `blocks_scan`）
//! → 引用索引（`citations`），外加底层字节工具 `cells` / 行内样式 `inline_style` /
//! 构造识别 `recognition`。
//!
//! 模块布局（2026-09-24 目录重组）：原 `parser.rs`（3327 行，4 套职责混杂）按职责拆成
//! 子模块后整体迁入本目录；`markdown/parser.rs` 保留为**兼容门面**，外部
//! `crate::markdown::parse::*` /
//! `crate::markdown::parse::inline_style::*` 等路径不变。

mod blocks;
mod blocks_scan;
pub(crate) mod cells;
mod citations;
pub(crate) mod inline_style;
#[cfg(test)]
pub(crate) mod parity;
pub(crate) mod recognition;
mod rows;
#[cfg(test)]
mod tests;
#[allow(unused_imports)] // 兼容重导出：ImageInfo 经 Row::image() 返回面消费（无人逐名导入）
pub use rows::{
    ImageInfo, Row, RowAlign, RowKind, RowPayload, RunKind, TableAlign, TableData, parse_to_rows,
};
#[allow(unused_imports)] // 兼容重导出：ImgRef/parse_ref 走类型推断；split_table_cells 供 markdown::parity 测试消费
pub(crate) use rows::{
    ImgRef, inline_style_mapped, parse_image_line, parse_ref, split_table_cells,
    split_table_cells_mapped, text_segment,
};
#[allow(unused_imports)] // 兼容重导出：citation_index_from_rows 在 preview 迁移到 PreviewRow 后仅 parse 测试消费
pub use citations::{CitationIndex, citation_index_from_rows, scan_citations};
#[allow(unused_imports)] // 兼容重导出：BlocksIter 为公开类型但内部无人逐名引用；scan_blocks 供 markdown 级 #[cfg(test)] 重导出消费
pub use blocks::{BlockIndex, BlockMeta, Blocks, BlocksIter, BlocksSeg, Toc, scan_blocks};
pub(crate) use blocks::build_line_starts_u32;
#[cfg(test)] pub(crate) use blocks::BLOCKS_MAX_SEGS;
pub use blocks_scan::{
    parse_block_window, parse_block_window_rope, scan_blocks_incremental_rope, scan_blocks_rope,
};
pub(crate) use blocks_scan::scan_blocks_incremental_rope_blocks_only;
#[allow(unused_imports)] // 兼容重导出：rows_memory_estimate 在 preview 迁移到 PreviewRow 后仅 parse 测试消费
pub(crate) use blocks_scan::rows_memory_estimate;
#[cfg(test)] pub(crate) use blocks_scan::next_line_byte;
