//! 阶段 4（路线 1）第一步：共享语义 IR —— 「两套管线的金标准」。
//!
//! 单一实现 [`build_ir`] 从源码产出块级语义（kind + 源码行区间），作为预览（`parser`）
//! 与 WYSIWYG（`live`）共同的「该文档是什么结构」标准答案。三方的逐行一致性由
//! `markdown::parity::per_line_kinds_agree_across_rich_docs` 钉死（IR 是金标准）。
//!
//! 现在的定位：**金标准 + 一致性护栏**。live/preview 的渲染尚未迁到本 IR（各自仍用
//! 自己的扫描做几何/编辑），但任何一方再引入新的块类判定时，只需对着本模块对齐。
//! 后续（渐进迁移）可以让 `live` 的 `ScanData` / 预览的 `parse_to_rows` 从本 IR 派生，
//! 届时本模块才是真正的单一实现。

use std::ops::Range;

use super::live::markdown_syntax::{
    blockquote_prefix, heading_level, image_row, list_prefix, table_regions, thematic_break,
};

/// 块级语义 kind（与两套管线的 taxonomy 求「可对齐并集」；
/// live 的 Todo/Callout/Math 细分与预览的 EPUB 指令不在 IR 的块级范围）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IrKind {
    Paragraph,
    Heading(u8),
    Bullet,
    Quote,
    Rule,
    Image,
    Blank,
    Code,
    Table,
}

/// 一个 IR 块：kind + 覆盖的源码行区间（半开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IrBlock {
    pub(crate) kind: IrKind,
    pub(crate) lines: Range<usize>,
}

/// 从源码产出一份块级 IR。围栏/表格判定与 live 一致（` ``` ` 围栏奇偶、
/// `is_table_row` 表区域），行内构造识别复用 `markdown_syntax` 纯函数
/// （见 parity 差异清单：`~~~`/math/缩进标题等已知不在此对齐子集）。
pub(crate) fn build_ir(src: &str) -> Vec<IrBlock> {
    let lines: Vec<&str> = src.split('\n').collect();
    let n = lines.len();
    let tables = table_regions(src);

    // 与 live `scan_data` 相同的围栏奇偶（行首 ` ``` ` 翻转，`odd`=起点在围栏内）。
    let mut fence = vec![false; n];
    let mut odd = false;
    for (i, l) in lines.iter().enumerate() {
        fence[i] = odd;
        if l.trim_start().starts_with("```") {
            odd = !odd;
        }
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        // 表格（块级，多行）：以 live `table_regions` 的区域为准。
        if let Some(t) = tables.iter().find(|t| t.lines.start == i) {
            out.push(IrBlock {
                kind: IrKind::Table,
                lines: t.lines.clone(),
            });
            i = t.lines.end;
            continue;
        }
        // 代码围栏（块级，多行）：开门行到同字符关门行（未闭合到文末）。
        if lines[i].trim_start().starts_with("```") {
            let mut j = i + 1;
            while j < n && !lines[j].trim_start().starts_with("```") {
                j += 1;
            }
            let end = if j < n { j + 1 } else { n };
            out.push(IrBlock {
                kind: IrKind::Code,
                lines: i..end,
            });
            i = end;
            continue;
        }
        // 单行块：判定顺序与 live `block_kind_at` 一致。
        let kind = if lines[i].trim().is_empty() {
            IrKind::Blank
        } else if let Some(h) = heading_level(lines[i]) {
            IrKind::Heading(h)
        } else if blockquote_prefix(lines[i]).is_some() {
            IrKind::Quote
        } else if list_prefix(lines[i]).is_some() {
            IrKind::Bullet
        } else if thematic_break(lines[i]) {
            IrKind::Rule
        } else if image_row(lines[i]).is_some() {
            IrKind::Image
        } else {
            IrKind::Paragraph
        };
        out.push(IrBlock {
            kind,
            lines: i..i + 1,
        });
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_covers_every_source_line_exactly_once() {
        let src = "# 标题\n正文\n- 列表\n> 引\n---\n```rs\nx\n```\n| a | b |\n| - | - |\n| 1 | 2 |\n![p](i.png)\n";
        let blocks = build_ir(src);
        let mut covered = Vec::new();
        for b in &blocks {
            covered.extend(b.lines.clone());
        }
        covered.sort_unstable();
        assert_eq!(covered, (0..src.split('\n').count()).collect::<Vec<_>>());
        let kinds: Vec<IrKind> = blocks.iter().map(|b| b.kind).collect();
        assert!(kinds.contains(&IrKind::Heading(1)));
        assert!(kinds.contains(&IrKind::Code));
        assert!(kinds.contains(&IrKind::Table));
        assert!(kinds.contains(&IrKind::Image));
    }
}
