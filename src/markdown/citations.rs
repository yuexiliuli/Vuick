//! 引用的**双向跳转**索引扫描（EPUB 脚注）：注记号 → 文内引用 / 末尾定义所在渲染行。
//!
//! 从 `parser`（行扫描器 + 块索引 + 引用索引 + 增量分析）拆出的「引用索引」部分
//! （2026-09-24 重构）：只依赖 `rows`（行解析）产出的 `Row`，无状态、无增量；
//! `parser` 门面经重导出保持 `parser::CitationIndex` 等路径不变。

use std::ops::Range;

use gpui::SharedString;

use crate::markdown::rows::{
    extract_notes, fence_len, inline_style, is_rule, list_prefix, parse_image_line,
    strip_align_directive, trim_paragraph, try_parse_table, Row,
};

/// 引用的**双向跳转**索引：注记号 → 文内引用 / 末尾定义所在渲染行。
///
/// 渲染行坐标 = 全局块下标（小文档 = `Vec<Row>` 下标；窗口模式 = [`BlockIndex`] 块下标），
/// 与视图层 `scroll_to_row` 的入参一致。
///
/// - [`Self::refs`]：`[^n]`（正文引用标记，显示成 `[n]`）→ (渲染行, 显示文本字节区间)
/// - [`Self::defs`]：`[!n]`（末尾定义标记，显示成 `[n]`）→ (渲染行, 显示文本字节区间)
///
/// 多个同号标记只记**第一个**（点击跳转取首处足够）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CitationIndex {
    /// 注记号 → 文内引用 `[^n]` 的 (渲染行, 显示文本字节区间)。
    pub refs: std::collections::BTreeMap<usize, (usize, Range<usize>)>,
    /// 注记号 → 末尾定义 `[!n]` 的 (渲染行, 显示文本字节区间)。
    pub defs: std::collections::BTreeMap<usize, (usize, Range<usize>)>,
    /// 注记号 → 引用详情正文（定义 `[!n]` 所在行剥掉 `[n]` 标记后的文本，去首空白）。
    /// 悬浮卡片在非 EPUB 时展示这个；EPUB 场景优先用外部注记表。
    pub details: std::collections::BTreeMap<usize, SharedString>,
}

/// 从**已解析行集**构建引用跳转索引（小文档全量路径；多位置取第一个）。
pub fn citation_index_from_rows(rows: &[Row]) -> CitationIndex {
    let mut idx = CitationIndex::default();
    for (ix, r) in rows.iter().enumerate() {
        for (range, n) in &r.notes {
            idx.refs.entry(*n).or_insert((ix, range.clone()));
        }
        for (range, n) in &r.note_defs {
            idx.defs.entry(*n).or_insert((ix, range.clone()));
            // 详情正文：定义标记 `[n]` 之后的文本（去首尾空白）。
            idx.details.entry(*n).or_insert_with(|| {
                let start = range.end.min(r.text.len());
                r.text[start..].trim().into()
            });
        }
    }
    idx
}

/// 提取一段**已剥行内样式**文本里的引用标记：返回 (清理后文本, 标记列表)。
/// 每个标记是 (显示文本字节区间, 注记号, 是否定义)；区间与 [`parse_to_rows`] 内
/// [`extract_notes`] 的产出一致。
fn extract_citation_markers(text: &str) -> (SharedString, Vec<(Range<usize>, usize, bool)>) {
    let (out, _, notes, note_defs) = extract_notes(text.into(), Vec::new());
    let markers = notes
        .into_iter()
        .map(|(r, n)| (r, n, false))
        .chain(note_defs.into_iter().map(|(r, n)| (r, n, true)))
        .collect();
    (out, markers)
}

/// 从整篇源码**逐块**扫描出引用跳转索引（窗口模式懒重建用）。
///
/// 与 [`scan_blocks`] 共用同一套切块判定（围栏 / 空行 / 标题 / 引述 / 列表 / 分隔线 /
/// 独立图片 / 表格 / 段落），块序号即渲染行下标——因此产出的渲染行坐标与窗口模式的
/// [`BlockIndex`] 完全一致，按需跳转不会错位。只对**文本块**（标题 / 引述 / 列表 /
/// 段落）剥行内样式后提取 `[^n]` / `[!n]` 标记（与 [`parse_to_rows`] 定义一致）；
/// 代码块 / 表格 / 图片不产生引用标记。
pub fn scan_citations(src: &str) -> CitationIndex {
    let mut idx = CitationIndex::default();
    let lines: Vec<&str> = src.split('\n').collect();
    let mut row_ix = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        let (_, line) = strip_align_directive(line);
        let t = line.trim_start();
        let (cleaned, markers, consumed) = if let Some((fc, _)) = fence_len(t) {
            // 代码块：整体折叠成一渲染行（围栏行 + 到闭合行），不解析标记。
            let mut k = i + 1;
            while k < lines.len() {
                let l = lines[k].trim_end_matches('\r').trim_start();
                if fence_len(l).is_some_and(|(c, _)| c == fc) {
                    k += 1;
                    break;
                }
                k += 1;
            }
            (SharedString::default(), Vec::new(), k - i)
        } else if t.is_empty() {
            (SharedString::default(), Vec::new(), 1)
        } else {
            let hashes = t.chars().take_while(|&c| c == '#').count();
            if (1..=6).contains(&hashes) && t.len() > hashes {
                let (cleaned, m) =
                    extract_citation_markers(inline_style(t[hashes..].trim_start()).0.as_ref());
                (cleaned, m, 1)
            } else if t.starts_with('>') {
                let (_, rest) = strip_align_directive(t.trim_start_matches('>').trim_start());
                let (cleaned, m) = extract_citation_markers(inline_style(rest).0.as_ref());
                (cleaned, m, 1)
            } else if let Some((_, content)) = list_prefix(t) {
                let (_, content) = strip_align_directive(content);
                let (cleaned, m) = extract_citation_markers(inline_style(content).0.as_ref());
                (cleaned, m, 1)
            } else if is_rule(t) {
                (SharedString::default(), Vec::new(), 1)
            } else if parse_image_line(t).is_some() {
                (SharedString::default(), Vec::new(), 1)
            } else if let Some((_table, consumed)) = try_parse_table(&lines, i) {
                (SharedString::default(), Vec::new(), consumed)
            } else {
                let (cleaned, m) =
                    extract_citation_markers(inline_style(trim_paragraph(line)).0.as_ref());
                (cleaned, m, 1)
            }
        };
        for (range, n, is_def) in markers {
            if is_def {
                idx.defs.entry(n).or_insert((row_ix, range.clone()));
                // 详情正文：定义标记 `[n]` 之后的文本（去首尾空白）。
                let detail = cleaned[range.end..].trim();
                idx.details.entry(n).or_insert_with(|| detail.into());
            } else {
                idx.refs.entry(n).or_insert((row_ix, range));
            }
        }
        row_ix += 1;
        i += consumed;
    }
    idx
}