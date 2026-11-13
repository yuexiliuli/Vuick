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

#[cfg(test)]
use std::ops::Range;

#[cfg(test)]
use super::parse::cells::{self, CellAlign};
#[cfg(test)]
use super::parse::inline_style::{
    blockquote_prefix, heading_level, image_row, list_prefix, table_regions, thematic_break,
};

/// 块级语义 kind（与两套管线的 taxonomy 求「可对齐并集」；
/// live 的 Todo/Callout/Math 细分与预览的 EPUB 指令不在 IR 的块级范围）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrKind {
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

/// 表格块的结构（raw，保留转义，与 live `table_cells` 同语义；预览显示层在其上解义）。
/// 仅测试金标准（`parity`）与模块自测使用。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IrTable {
    pub(crate) header: Vec<String>,
    pub(crate) rows: Vec<Vec<String>>,
    pub(crate) aligns: Vec<CellAlign>,
}

/// 一个 IR 块：kind + 覆盖的源码行区间（半开）。
/// 仅测试金标准（`parity`）消费。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IrBlock {
    pub(crate) kind: IrKind,
    pub(crate) lines: Range<usize>,
    /// Table 块的结构（非 Table 块为 None）。
    pub(crate) table: Option<IrTable>,
    /// Code 块的围栏语言（信息串首个词；None = 无标记 / 非 Code 块）。
    pub(crate) code_language: Option<String>,
}

/// 从源码产出一份块级 IR。围栏/表格判定与 live 一致（` ``` ` 围栏奇偶、
/// `is_table_row` 表区域），行内构造识别复用 `inline_style` 纯函数
/// （见 parity 差异清单：`~~~`/math/缩进标题等已知不在此对齐子集）。
///
/// 仅测试用途（`parity` 用它作为「两套管线的块级语义金标准」），生产渲染
/// 仍各自用自己的扫描。
#[cfg(test)]
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
        // 表格（块级，多行）：以 live `table_regions` 的区域为准；结构用共享
        // 单元格/对齐实现（`cells::raw_cells` / `cells::parse_aligns`，与 parser、
        // live 同源，见 parity）。
        if let Some(t) = tables.iter().find(|t| t.lines.start == i) {
            let region: Vec<&str> = lines[t.lines.clone()].to_vec();
            let table = (region.len() >= 2).then(|| IrTable {
                header: cells::raw_cells(region[0]),
                rows: region[2..]
                    .iter()
                    .map(|l| cells::raw_cells(l))
                    .collect(),
                aligns: {
                    let sep = cells::raw_cells(region[1]);
                    cells::parse_aligns(&sep.iter().map(String::as_str).collect::<Vec<_>>())
                        .unwrap_or_default()
                },
            });
            out.push(IrBlock {
                kind: IrKind::Table,
                lines: t.lines.clone(),
                table,
                code_language: None,
            });
            i = t.lines.end;
            continue;
        }
        // 代码围栏（块级，多行）：开门行到同字符关门行（未闭合到文末）。
        if lines[i].trim_start().starts_with("```") {
            let language = {
                let info = lines[i].trim_start().trim_start_matches('`').trim();
                info.split_whitespace().next().filter(|s| !s.is_empty()).map(String::from)
            };
            let mut j = i + 1;
            while j < n && !lines[j].trim_start().starts_with("```") {
                j += 1;
            }
            let end = if j < n { j + 1 } else { n };
            out.push(IrBlock {
                kind: IrKind::Code,
                lines: i..end,
                code_language: language,
                table: None,
            });
            i = end;
            continue;
        }
        // 引号区：收集连续 `>` 行，剥前缀后对 body 递归 build_ir；
        // 引号区内段落/空行降级为 Quote（与 rows/blocks 侧一致，2026-11-13）。
        if blockquote_prefix(lines[i]).is_some() {
            let mut j = i;
            while j < n && blockquote_prefix(lines[j]).is_some() {
                j += 1;
            }
            let body: Vec<&str> = (i..j)
                .map(|k| match blockquote_prefix(lines[k]) {
                    Some(p) => lines[k][p..].trim_end_matches('\r').trim_start(),
                    None => lines[k],
                })
                .collect();
            let mut sub = build_ir(&body.join("\n"));
            for b in &mut sub {
                if b.kind == IrKind::Paragraph || b.kind == IrKind::Blank {
                    b.kind = IrKind::Quote;
                }
                b.lines = (i + b.lines.start)..(i + b.lines.end);
            }
            out.extend(sub);
            i = j;
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
            table: None,
            code_language: None,
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
