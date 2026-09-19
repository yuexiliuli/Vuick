//! 「强大底座」第一层：**共享文档模型**（markdown::doc）。
//!
//! 目标（用户路线）：以本项目的 Markdown 预览底座（行级虚拟化、窗口化块索引、恒定内存）
//! 为基干，抽象出一个 preview 与 live（WYSIWYG）都从其上衍生的统一文档层：
//! - **语义** = [`ir::IrKind`]（`markdown::ir`，金标准）；
//! - **结构索引** = [`parser::BlockIndex`]（`scan_blocks` / 增量 rope，O(1) 打开、窗口物化）；
//! - **行物化** = [`parser::parse_block_window`]（与预览**同一份** 文本/行内段，保证两分支渲染一致）。
//!
//! [`DocRow`] 是两分支的**统一行**：合并「IR 语义 + 预览文本 + 行号 + 行区间」。
//! 后续阶段叠加编辑映射（渲染字节↔源字节）并让 preview/live 迁移到本层消费
//! （见会话交接：Phase 2 迁移 preview，Phase 3 迁移 live 编辑态）。
//! 一致性由 `markdown::parity::doc_model_agrees_with_preview_and_ir` 钉死。

use std::ops::Range;

use gpui::SharedString;

use super::ir::{build_ir, IrBlock, IrKind};
use super::parser::{parse_block_window, scan_blocks, BlockIndex, Row, RowKind, RunKind};

/// 统一渲染行（块粒度：Code/Table 等一个块 = 一个 DocRow，跨多行）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DocRow {
    /// 语义（IR 金标准）。
    pub(crate) kind: IrKind,
    /// 该块首个源码行（0 基）。
    pub(crate) src_line: usize,
    /// 覆盖的源码行区间（多行块 = 块区间；单行块 = src_line..src_line+1）。
    pub(crate) lines: Range<usize>,
    /// 渲染文本（与预览 `parse_to_rows` 逐字节一致）。
    pub(crate) text: SharedString,
    /// 行内样式段（与预览同源）。
    pub(crate) runs: Vec<(Range<usize>, RunKind)>,
}

/// 共享文档：IR（语义） + BlockIndex（结构索引） + 打开时的源码。
pub(crate) struct Doc {
    src: SharedString,
    index: BlockIndex,
    ir: Vec<IrBlock>,
}

impl Doc {
    /// 打开一份 Markdown 源码：一次建 IR + 结构索引（O(n)，毫秒级；编辑后走
    /// 增量索引重建，替换文本只重建受影响段——与预览同一套增量机制）。
    pub(crate) fn open(src: SharedString) -> Self {
        let index = scan_blocks(src.as_ref());
        let ir = build_ir(src.as_ref());
        Self { src, index, ir }
    }

    /// 更新内容（编辑后调用）。当前先全量重建；后续可改用
    /// `scan_blocks_incremental_rope` 做增量（与预览一致）。
    pub(crate) fn set_source(&mut self, src: SharedString) {
        self.index = scan_blocks(src.as_ref());
        self.ir = build_ir(src.as_ref());
        self.src = src;
    }

    /// 块总数（虚拟化滚动条量程用，O(1)）。
    pub(crate) fn block_count(&self) -> usize {
        self.index.blocks.len()
    }

    /// 按块窗口物化统一行（`[lo_block, hi_block]`，含两端）。
    pub(crate) fn rows_in_block_window(&self, lo: usize, hi: usize) -> Vec<DocRow> {
        parse_block_window(self.src.as_ref(), &self.index, lo, hi)
            .into_iter()
            .map(|r| self.assemble(r))
            .collect()
    }

    /// 按源码行区间找覆盖的块窗口并物化（虚拟化视口色调）。
    pub(crate) fn rows_in_line_window(&self, lo_line: usize, hi_line: usize) -> Vec<DocRow> {
        let b = &self.index.blocks;
        if b.is_empty() {
            return Vec::new();
        }
        // [lo_line, hi_line) → 覆盖这些行的块下标范围（二分，O(log n)）。
        let lo = b
            .partition_point(|m| m.src_line < lo_line as u32)
            .saturating_sub(1)
            .min(b.len() - 1);
        let hi = b.partition_point(|m| m.src_line < hi_line as u32).max(lo);
        let hi = hi.saturating_sub(1).min(b.len() - 1).max(lo);
        self.rows_in_block_window(lo, hi)
    }

    /// 把预览侧一个块行装成统一行：kind/行区间取自 IR（金标准），文本取自预览。
    fn assemble(&self, r: Row) -> DocRow {
        let block = self
            .ir
            .iter()
            .find(|b| b.lines.start == r.src_line);
        let kind = block.map_or_else(|| row_kind(r.kind), |b| b.kind);
        let lines = block.map(|b| b.lines.clone()).unwrap_or(r.src_line..r.src_line + 1);
        DocRow {
            kind,
            src_line: r.src_line,
            lines,
            text: r.text,
            runs: r.runs,
        }
    }
}

/// 预览 `RowKind` → IR 语义（对齐子集上恒等；parity 保证二者一致）。
fn row_kind(k: RowKind) -> IrKind {
    match k {
        RowKind::Paragraph => IrKind::Paragraph,
        RowKind::Heading(n) => IrKind::Heading(n),
        RowKind::Bullet => IrKind::Bullet,
        RowKind::Quote => IrKind::Quote,
        RowKind::Rule => IrKind::Rule,
        RowKind::Image => IrKind::Image,
        RowKind::Blank => IrKind::Blank,
        RowKind::Code => IrKind::Code,
        RowKind::Table => IrKind::Table,
    }
}
