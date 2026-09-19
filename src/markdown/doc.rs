//! 「强大底座」第一层：**共享文档模型**（markdown::doc）。
//!
//! 目标（用户路线）：以本项目的 Markdown 预览底座（行级虚拟化、窗口化块索引、恒定内存）
//! 为基干，抽象出一个 preview 与 live（WYSIWYG）都从其上衍生的统一文档层：
//! - **语义** = [`ir::IrKind`]（`markdown::ir`，金标准）；
//! - **结构索引** = [`parser::BlockIndex`]（`scan_blocks`/`scan_blocks_rope`，
//!   O(1) 打开、窗口物化，与预览同一增量机制）；
//! - **行物化** = [`parser::parse_block_window`(_rope)]（与预览**同一份**
//!   文本/行内段，保证两分支渲染一致）。
//!
//! [`DocRow`] 是两分支的**统一行**：合并「IR 语义 + 预览文本 + 行号 + 行区间」。
//! 源支持 `Str`（小文档/测试）与 `Rope`（大文档，与编辑器 COW 共享底层，窗口化
//! 物化不整份持有——预览的性能底座）。后续阶段叠加编辑映射并让 preview/live
//! 迁移到本层消费；一致性由 `markdown::parity` 钉死。

use std::ops::Range;

use gpui::SharedString;
use gpui_component::input::Rope;

use super::ir::{build_ir, IrBlock, IrKind};
use super::parser::{parse_block_window, parse_block_window_rope, Row, RowKind, RunKind};
use super::parser;

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
    /// 编辑映射：渲染字节 → **该行**源字节（文本行，长度 = `text.len()+1`，末项为哨兵 =
    /// 源段尾）。非文本行（代码/表格/图片/规则/空行）为空。
    /// 由 `parser::text_segment` + `parser::inline_style_mapped` 装配（与预览同一段派生）。
    pub(crate) byte_map: Vec<usize>,
}

/// 文档源：`Str`（小文档）或 `Rope`（大文档，COW，与编辑器共享底层树）。
enum DocSrc {
    Str(SharedString),
    Rope(Rope),
}

/// 共享文档：IR（语义） + BlockIndex（结构索引） + 源（窗口化物化）。
pub(crate) struct Doc {
    src: DocSrc,
    index: parser::BlockIndex,
    ir: Vec<IrBlock>,
    /// 每行起始字节偏移（n 行 → n 项；`split('\n')` 语义）——行源文本定位用。
    line_starts: Vec<usize>,
}

impl Doc {
    /// 打开小/中文档（`Str` 源）。
    pub(crate) fn open(src: SharedString) -> Self {
        let index = parser::scan_blocks(src.as_ref());
        let ir = build_ir(src.as_ref());
        let line_starts = parser::build_line_starts_u32(src.as_ref())
            .into_iter()
            .map(|b| b as usize)
            .collect();
        Self { src: DocSrc::Str(src), index, ir, line_starts }
    }

    /// 打开大文档（`Rope` 源，COW）：块索引与 IR 各物化一次（O(n)，毫秒级），
    /// 但是**窗口化物化行**走 rope 切片，不整份持有字符串——与预览一致。
    pub(crate) fn open_rope(rope: Rope) -> Self {
        let index = parser::scan_blocks_rope(&rope);
        let ir = build_ir(&rope.to_string());
        let line_starts = {
            let mut v = vec![0usize];
            for i in 0..rope.len() {
                if rope.get_byte(i) == Some(b'\n') {
                    v.push(i + 1);
                }
            }
            v
        };
        Self { src: DocSrc::Rope(rope), index, ir, line_starts }
    }

    /// 更新内容（编辑后调用）。全量重建索引/IR；后续可改用增量（与预览一致）。
    pub(crate) fn set_source(&mut self, src: SharedString) {
        self.index = parser::scan_blocks(src.as_ref());
        self.ir = build_ir(src.as_ref());
        self.line_starts = parser::build_line_starts_u32(src.as_ref())
            .into_iter()
            .map(|b| b as usize)
            .collect();
        self.src = DocSrc::Str(src);
    }

    /// 块总数（虚拟化滚动条量程用，O(1)）。
    pub(crate) fn block_count(&self) -> usize {
        self.index.blocks.len()
    }

    /// 按块窗口物化统一行（`[lo_block, hi_block]`，含两端）。
    pub(crate) fn rows_in_block_window(&self, lo: usize, hi: usize) -> Vec<DocRow> {
        let rows = match &self.src {
            DocSrc::Str(s) => parse_block_window(s.as_ref(), &self.index, lo, hi),
            DocSrc::Rope(r) => parse_block_window_rope(r, &self.index, lo, hi),
        };
        rows.into_iter().map(|r| self.assemble(r)).collect()
    }

    /// 按源码行区间找覆盖的块窗口并物化（虚拟化视口色调）。
    pub(crate) fn rows_in_line_window(&self, lo_line: usize, hi_line: usize) -> Vec<DocRow> {
        let b = &self.index.blocks;
        if b.is_empty() {
            return Vec::new();
        }
        let lo = b
            .partition_point(|m| m.src_line < lo_line as u32)
            .saturating_sub(1)
            .min(b.len() - 1);
        let hi = b.partition_point(|m| m.src_line < hi_line as u32).max(lo);
        let hi = hi.saturating_sub(1).min(b.len() - 1).max(lo);
        self.rows_in_block_window(lo, hi)
    }

    /// 把预览侧一个块行装成统一行：kind/行区间取自 IR（金标准），文本取自预览，
    /// 文本行额外装配「渲染字节→源字节」编辑映射。
    fn assemble(&self, r: Row) -> DocRow {
        let block = self.ir.iter().find(|b| b.lines.start == r.src_line);
        let kind = block.map_or_else(|| row_kind(r.kind), |b| b.kind);
        let lines = block.map(|b| b.lines.clone()).unwrap_or(r.src_line..r.src_line + 1);
        let byte_map = if matches!(
            r.kind,
            RowKind::Paragraph
                | RowKind::Heading(_)
                | RowKind::Bullet
                | RowKind::Quote
        ) {
            let line = self.line_str(r.src_line);
            parser::text_segment(&line, r.kind)
                .map(|(off, seg)| parser::inline_style_mapped(seg).2.into_iter().map(|b| off + b).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        DocRow {
            kind,
            src_line: r.src_line,
            lines,
            text: r.text,
            runs: r.runs,
            byte_map,
        }
    }

    /// 第 `ix` 行的源码文本（仅该行切片，O(行长)）。
    fn line_str(&self, ix: usize) -> String {
        let s = self.line_starts.get(ix).copied().unwrap_or(0);
        let e = self
            .line_starts
            .get(ix + 1)
            .copied()
            .unwrap_or_else(|| self.src_len());
        let mut t = match &self.src {
            DocSrc::Str(str) => str.as_ref()[s..e].to_string(),
            DocSrc::Rope(r) => r.slice(s..e).to_string(),
        };
        // `[starts[i], starts[i+1])` 含该行尾 `\n`（行起始即上一行换行符后一字节）。
        if t.ends_with('\n') {
            t.pop();
        }
        if t.ends_with('\r') {
            t.pop();
        }
        t
    }

    fn src_len(&self) -> usize {
        match &self.src {
            DocSrc::Str(s) => s.len(),
            DocSrc::Rope(r) => r.len(),
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
