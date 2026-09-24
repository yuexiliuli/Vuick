//! 手写 Markdown 行扫描器：把源码**逐行**转成渲染行（[`Row`]）。
//!
//! 设计目标：**行级渲染** —— 一个代码围栏就是 N 个等高的普通行，
//! 天然适合行级虚拟化；再大的代码块 / 长文档也不会出现“单个超大渲染块”，
//! 从根上避免滚动 / 渲染卡顿。
//!
//! 块级表格（GFM pipe table）作为**单个渲染行**消费多行源码：表头 + 分隔行 +
//! 若干数据行合并成 [`RowKind::Table`]，渲染层画成网格（表格通常不大，
//! 单个块不会破坏行级虚拟化的性能前提）。

#[cfg(test)] use std::ops::Range;

#[cfg(test)] use gpui::{Image, SharedString};
#[cfg(test)] use gpui_component::input::Rope;

#[allow(unused_imports)] // 兼容重导出：ImageInfo 经 Row::image() 返回面消费（无人逐名导入）
pub use super::rows::{
    ImageInfo, Row, RowAlign, RowKind, RowPayload, RunKind, TableAlign, TableData, parse_to_rows,
};
#[allow(unused_imports)] // 兼容重导出：ImgRef/parse_ref 走类型推断；split_table_cells 供 markdown::parity 测试消费
pub(crate) use super::rows::{
    ImgRef, inline_style_mapped, parse_image_line, parse_ref, split_table_cells,
    split_table_cells_mapped, text_segment,
};
#[cfg(test)] use super::rows::{
    fence_len, inline_strip, is_rule, list_prefix, try_parse_table,
};
pub use super::citations::{CitationIndex, citation_index_from_rows, scan_citations};
#[allow(unused_imports)] // 兼容重导出：BlocksIter 为公开类型但内部无人逐名引用；scan_blocks 供 markdown 级 #[cfg(test)] 重导出消费
pub use super::blocks::{BlockIndex, BlockMeta, Blocks, BlocksIter, BlocksSeg, scan_blocks};
pub(crate) use super::blocks::build_line_starts_u32;
#[cfg(test)] pub(crate) use super::blocks::BLOCKS_MAX_SEGS;
pub use super::blocks_scan::{
    parse_block_window, parse_block_window_rope, scan_blocks_incremental_rope, scan_blocks_rope,
};
pub(crate) use super::blocks_scan::rows_memory_estimate;
#[cfg(test)] pub(crate) use super::blocks_scan::next_line_byte;

/// 两字符串从前向后相同的字节数（公共前缀长度）。
///
/// 大文档增量扫描几乎总是「只有一小段编辑，前后都是整段相等的字节」，因此按
/// 4KB 分块比较（`&[u8]` 的 `==` 落到底层 memcmp / 向量化），命中同块直接跳过，
/// 只在首个相异的块内逐字节定位——相对于逐字节迭代器（`zip().take_while()`）在
/// debug 下慢 5~10 倍、release 下数倍。量级仍是 O(n)（要读全串找差异），但把
/// 「背全文」的常数压到内存带宽级。
///
/// 生产路径已全部切到 [`scan_blocks_incremental_rope`]（按 dirty 区域切片，不再
/// 全串比对）；本函数与 [`common_suffix_bytes`]、[`scan_blocks_incremental`] 仅
/// 保留给测试/基准复用（str 路径的正确性安全网）。
#[cfg(test)]
fn common_prefix_bytes(a: &str, b: &str) -> usize {
    const BLK: usize = 4096;
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let n = aa.len().min(bb.len());
    let mut i = 0;
    while i + BLK <= n {
        if &aa[i..i + BLK] != &bb[i..i + BLK] {
            break;
        }
        i += BLK;
    }
    while i < n && aa[i] == bb[i] {
        i += 1;
    }
    i
}

/// 两字符串从后向前相同的字节数（公共后缀长度）。分块策略同上。测试/基准复用。
#[cfg(test)]
fn common_suffix_bytes(a: &str, b: &str) -> usize {
    const BLK: usize = 4096;
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let n = aa.len().min(bb.len());
    let (la, lb) = (aa.len(), bb.len());
    let mut i = 0;
    while i + BLK <= n {
        let sa = &aa[la - i - BLK..la - i];
        let sb = &bb[lb - i - BLK..lb - i];
        if sa != sb {
            break;
        }
        i += BLK;
    }
    while i < n && aa[la - 1 - i] == bb[lb - 1 - i] {
        i += 1;
    }
    i
}

/// **预览增量解析**：在上一次块索引基础上，只重扫「变化区及其所在块、直到与旧索引
/// 尾部重新对齐」的部分，跳过未变行的重复扫描。
///
/// `dirty`：编辑器侧通道给出的「当前文本中与 `prev_src` 不同的字节区间」（打字路径
/// 按 `InputEdit` 累积，O(k)），提供后**跳过 `common_prefix/_suffix` 两次 O(n) 全串
/// 比对**——打字同步的主成本从 O(n) 落到 O(k)（k=编辑字节数）。
/// `None` 时回退到全串比对定位变化区（快照 diff 无编辑信息的下界，作为兜底/测试安全网）。
///
/// 正确性保证与 `scan_blocks` 完全一致（复用同一套切块判定）；增量只在「尾部字节与
/// 旧文本相同、且当前位置恰是旧索引的块边界」时截断并复用旧尾部（块结束必为 Normal
/// 态、同字节 → 同分类，故复用安全）；对不上（如围栏边界跨变化区）就一直重扫到文末，
/// 结果与全量 `scan_blocks` 逐字节一致——这是**正确性兜底**。变化区过大直接全扫。
///
/// 生产路径已改用 rope 版 [`scan_blocks_incremental_rope`]（快照 Rope::clone O(1) +
/// 按 dirty 区域切片，不再对全串 split）；本 `&str` 版保留给测试/基准做正确性参照。
#[cfg(test)]
pub fn scan_blocks_incremental(
    prev_src: &str,
    prev: &BlockIndex,
    new_src: &str,
    dirty: Option<Range<usize>>,
) -> BlockIndex {
    let old_len = prev_src.len();
    let new_len = new_src.len();
    let new_lines: Vec<&str> = new_src.split('\n').collect();
    let (tail_start_new, changed, b_first) = match dirty {
        // 已知变化区间（new_src 坐标）→ 直接定位，跳过前后缀全串比对。
        // 首处变化之前字节未变，故 b_first 在 prev_src 中的偏移相同。
        Some(r) => {
            let dlo = r.start.min(new_len);
            let dhi = r.end.min(new_len).max(dlo);
            (dhi, dhi - dlo, dlo)
        }
        None => {
            let cp = common_prefix_bytes(prev_src, new_src);
            let cs = common_suffix_bytes(prev_src, new_src);
            let tail_start_new = new_len.saturating_sub(cs);
            let changed = new_len.saturating_sub(cp + cs);
            if new_len == old_len && changed == 0 {
                // 文本未变（同长且公共前后缀覆盖全串）
                return prev.clone();
            }
            (tail_start_new, changed, cp)
        }
    };
    if new_len == old_len && changed == 0 {
        return prev.clone();
    }
    // 变化区过大 → 全扫兜底（增量没赚头）。注意纯删除 changed 可能低估，
    // 但仍交给增量循环处理（正确性不依赖该阈值）。
    if changed >= old_len.max(new_len) / 2 {
        return scan_blocks(new_src);
    }

    let delta = new_len as i64 - old_len as i64; // 总字节增量（可负）

    // 锚点 = 旧索引中含首处变化字节的块（首处变化之前字节/行号两边一致）。
    // 用块的 `byte` 二分定位：变化前的行字节偏移在两帧相同。
    let bi = prev
        .blocks
        .partition_point(|b| b.byte as usize <= b_first)
        .saturating_sub(1);
    let anchor = prev.blocks.get(bi).map(|b| b.src_line as usize).unwrap_or(0);
    // 当前行的起始字节：锚点行起始 < b_first → 未变，直接复用旧索引的 byte。
    let anchor_byte = prev.blocks.get(bi).map(|b| b.byte as usize).unwrap_or(0);

    // 未变前缀直接沿用（分段共享，O(1) 不再整段 to_vec memcpy）；toc 同样取前缀。
    // 注意 toc 行号是**源码行**坐标，不能用块下标 `bi` 比较（围栏/表格是一块对应
    // 多行源码，两类坐标会漂移）：保留所有「行号 < 重扫起点行号 anchor」的旧标题
    // （重扫会重新产出其后的标题）。
    let prefix_segs = prev.blocks.shares_prefix(bi);
    let mut blocks: Vec<BlockMeta> = Vec::new(); // 新扫描的变化中段（通常极小）
    let mut suffix_segs: Vec<BlocksSeg> = Vec::new();
    let mut toc: Vec<(usize, u8, String)> = prev
        .toc
        .iter()
        .filter(|(r, _, _)| *r < anchor)
        .cloned()
        .collect();

    let mut idx = anchor;
    let mut cur_byte = anchor_byte as u32; // 当前扫描行的起始字节（new 坐标）
    let mut reused = false;
    while idx < new_lines.len() {
        // —— 复用旧尾部的机会：游标已进入「与旧文本字节相同」的尾部，且对应旧位置恰是块边界 ——
        //    prev_off = new_byte - delta 映射到旧坐标；旧块的 `byte` 即其行首偏移，
        //    能命中块边界（块 byte 严格递增，二分可定位）。
        let new_byte = cur_byte as usize;
        if new_byte >= tail_start_new {
            let prev_off = new_byte as i64 - delta;
            if (0..=old_len as i64).contains(&prev_off) {
                let prev_off = prev_off as usize;
                let pi = prev.blocks.partition_point(|b| (b.byte as usize) < prev_off);
                if let Some(pb) = prev.blocks.get(pi) {
                    if pb.byte as usize == prev_off {
                        // 同字节尾部 + 块首 Normal 态 → 旧尾部块逐字有效，行号与字节
                        // 同时平移（byte 平移 = 总字节增量 delta，源后缀整体平移）。
                        // 分段持久化：O(1) 共享 + 读取时施加 (delta, line_shift)。
                        let prev_line = pb.src_line as usize;
                        let line_shift = idx as i64 - prev_line as i64;
                        suffix_segs = prev.blocks.shares_suffix(pi, delta, line_shift);
                        // toc 尾部：与 `scan_blocks` 一致的「源码行」坐标 → 随行号偏移
                        for (r, lv, t) in prev.toc.iter().filter(|(r, _, _)| *r as usize >= prev_line) {
                            toc.push((( *r as i64 + line_shift).max(0) as usize, *lv, t.clone()));
                        }
                        reused = true;
                        break;
                    }
                }
            }
        }

        // —— 否则与 `scan_blocks` 完全相同的切块逻辑（逐块消费 `idx`）——
        let row_start = idx;
        let line = new_lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        let raw_len = line.len() as u32;
        let blk_byte = cur_byte;
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = 0u32;
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            while idx < new_lines.len() {
                let l = new_lines[idx].trim_end_matches('\r');
                if let Some((c, _)) = fence_len(l.trim_start()) {
                    if c == fc {
                        idx += 1;
                        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
                        break;
                    }
                }
                code_lines += 1;
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Code,
                raw_len,
                aux: code_lines,
            });
            continue;
        }
        if t.is_empty() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Blank,
                raw_len: 0,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let text = inline_strip(t[hashes..].trim_start()).to_string();
            toc.push((row_start, hashes as u8, text));
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Heading(hashes as u8),
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if t.starts_with('>') {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Quote,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if list_prefix(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Bullet,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if is_rule(t) {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Rule,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if parse_image_line(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Image,
                raw_len,
                aux: 1,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if let Some((table, consumed)) = try_parse_table(&new_lines, idx) {
            for _ in 0..consumed {
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Table,
                raw_len,
                aux: table.rows.len() as u32,
            });
            continue;
        }
        blocks.push(BlockMeta {
            src_line: row_start as u32,
            byte: blk_byte,
            kind: RowKind::Paragraph,
            raw_len,
            aux: 0,
        });
        idx += 1;
        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
    }
    debug_assert!(
        reused || idx >= new_lines.len(),
        "增量扫描要么对齐复用旧尾部、要么扫完新文本"
    );
    BlockIndex {
        blocks: Blocks::assemble(prefix_segs, blocks, suffix_segs),
        toc,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_fences_lists() {
        let src = "# 一级标题\n```rs\nlet a = 1;\n```\n- item\n\n> quote\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Heading(1));
        assert_eq!(rows[0].text.as_ref(), "一级标题");
        assert_eq!(rows[1].kind, RowKind::Code);
        assert_eq!(rows[1].code_language(), Some("rs"), "围栏语言应被捕获");
        assert_eq!(rows[1].text.as_ref(), "let a = 1;");
        assert_eq!(rows[2].kind, RowKind::Bullet);
        assert_eq!(rows[2].text.as_ref(), "item");
        assert_eq!(
            rows[2].prefix.as_ref().map(|s| s.as_ref()),
            Some("•"),
            "无序列表应带前缀"
        );
        assert_eq!(rows[4].kind, RowKind::Quote);
    }

    #[test]
    fn code_fence_language_variants() {
        // 无语言标记 / 含信息串 / `{lang}` 旧式包裹 / 波浪号围栏
        let rows = parse_to_rows("```\nplain\n```\n\n```rust, ignore\nlet x = 1;\n```\n\n~~~{python}\nx = 1\n~~~\n");
        assert_eq!(rows[0].kind, RowKind::Code);
        assert_eq!(rows[0].code_language(), None, "纯 ``` 围栏无语言");
        assert_eq!(rows[2].kind, RowKind::Code);
        assert_eq!(rows[2].code_language(), Some("rust"), "取信息串首词");
        assert_eq!(rows[4].kind, RowKind::Code);
        assert_eq!(rows[4].code_language(), Some("python"), "`{{lang}}` 旧式包裹应剥掉花括号");
    }

    #[test]
    fn strips_inline_markers() {
        let rows = parse_to_rows("**加粗** 和 `code` 和 *斜体*\n");
        let r = &rows[0];
        assert_eq!(r.text.as_ref(), "加粗 和 code 和 斜体");
        assert_eq!(r.runs.len(), 3);
        assert!(matches!(
            r.runs[0].1,
            RunKind::Bold
        ));
    }

    #[test]
    fn strips_strikethrough_links_images() {
        let rows = parse_to_rows("a ~~删除~~ [链接文字](https://x.y) ![alt text](img.png)\n");
        let r = &rows[0];
        assert_eq!(r.text.as_ref(), "a 删除 链接文字 alt text");
        assert_eq!(r.runs.len(), 3);
        assert!(matches!(r.runs[0].1, RunKind::Strikethrough));
        assert!(matches!(r.runs[1].1, RunKind::Link));
        assert!(matches!(r.runs[2].1, RunKind::Image));
    }

    #[test]
    fn inline_mapped_fast_path_equals_slow_for_no_markers() {
        // 无内联记号起始字节（` * ~ [）→ 快路径：直拷 + 恒等映射，与慢路径零匹配
        // 完全相同；含记号起始字节但无法匹配（孤立的 `[`）也走慢路径并保持一致。
        for s in [
            "alpha beta gamma",
            "中文段落 数字 123，标点。",
            "混合 alpha 中文 42",
            "with [ bracket",
            "just a backslash \\ and zero",
        ] {
            let (t, runs, map) = inline_style_mapped(s);
            assert_eq!(t.as_ref(), s, "无匹配原文应原样保留: {s:?}");
            assert!(runs.is_empty(), "{s:?} 不应有样式段");
            assert_eq!(map.len(), s.len() + 1, "{s:?} 映射长度 = len+1");
            for (r, &m) in map.iter().enumerate() {
                assert_eq!(r, m, "{s:?} 映射应恒等");
            }
        }
        // 有标记的行走慢路径（防快路径误伤）：`**x**` / `` `c` `` / `[a](u)`。
        for s in ["**bold**", "`code`", "[link](https://a.b)", "a *em* b", "~~del~~", "![alt](x.png)"] {
            let (t, runs, map) = inline_style_mapped(s);
            assert_ne!(t.as_ref(), s, "有记号应被清理: {s:?}");
            assert!(!runs.is_empty(), "{s:?} 应有样式段");
            assert_eq!(map.len(), t.len() + 1);
        }
    }

    #[test]
    fn parses_task_lists() {
        let rows = parse_to_rows("- [x] 已完成\n- [ ] 未完成\n");
        let done = &rows[0];
        let todo = &rows[1];
        assert_eq!(done.kind, RowKind::Bullet);
        assert_eq!(
            done.prefix.as_ref().map(|s| s.as_ref()),
            Some("☑"),
            "勾选任务列表应带 ☑"
        );
        assert_eq!(done.text.as_ref(), "已完成");
        assert_eq!(
            todo.prefix.as_ref().map(|s| s.as_ref()),
            Some("☐"),
            "未勾选任务列表应带 ☐"
        );
    }

    #[test]
    fn parses_gfm_pipe_table() {
        let src = "| 型号 | 尺寸 |\n| :--- | ---: |\n| A | 10 |\n| B | 20 |\n";
        let rows = parse_to_rows(src);
        let table = rows[0].table().expect("应为表格行");
        assert_eq!(rows.len(), 2, "表格折叠成 1 行 + 末尾空行");
        assert_eq!(table.header.len(), 2);
        assert_eq!(table.header[0].as_ref(), "型号");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][1].as_ref(), "10");
        assert_eq!(table.aligns, vec![TableAlign::Left, TableAlign::Right]);
    }

    #[test]
    fn table_supports_no_outer_pipes_and_escaped_pipe() {
        let src = "a | b\n--- | ---\n1 | x\\|y\n";
        let rows = parse_to_rows(src);
        let table = rows[0].table().expect("应为表格行");
        assert_eq!(table.header[0].as_ref(), "a");
        assert_eq!(table.rows[0][1].as_ref(), "x|y", "\\| 应解义为字面竖线");
    }

    #[test]
    fn pipe_without_delimiter_stays_paragraph() {
        // 只有 `|` 而没有分隔行 → 不是表格，保持普通段落
        let rows = parse_to_rows("just | a line\nnext\n");
        assert_eq!(rows.len(), 3, "两段 + 末尾空行");
        assert_eq!(rows[0].kind, RowKind::Paragraph);
        assert_eq!(rows[0].text.as_ref(), "just | a line");
        assert_eq!(rows[1].kind, RowKind::Paragraph);
    }

    #[test]
    fn table_breaks_at_blank_or_non_pipe_line() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n\n后文\n| 3 | 4 |\n|---|---|\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Table);
        assert_eq!(rows[1].kind, RowKind::Blank, "表格在空行处断开");
        assert_eq!(rows[2].kind, RowKind::Paragraph, "空行后普通段落");
        assert_eq!(rows[3].kind, RowKind::Table, "新表格再次识别");
    }

    #[test]
    fn delimiter_row_single_dash_is_valid() {
        // GFM 允许分隔格只有 1 个短横（`-`）
        let src = "姓名|年龄\n-|-\n张三|30\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Table);
    }

    #[test]
    fn split_table_cells_mapped_round_trips_to_source() {
        // `| A | B |`：单元格内容 A 的源区间 [2,3)（`|`/空格编号 -> A@2）。
        let line = "| A | B |";
        let cells = split_table_cells_mapped(line);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].0, 2..3, "A 的源区间");
        assert_eq!(cells[0].1.as_ref(), "A");
        assert_eq!(cells[0].2, vec![2, 3], "渲染 A ↔ 源 2..3");
        assert_eq!(cells[1].1.as_ref(), "B");
        assert_eq!(cells[1].2, vec![6, 7]);
        // 转义竖线：`a\|b` 渲染 `a|b`，仍是一格。
        let cells2 = split_table_cells_mapped("| a\\|b | c |");
        assert_eq!(cells2.len(), 2);
        assert_eq!(cells2[0].1.as_ref(), "a|b", "转义竖线渲染为字面 |");
        assert_eq!(cells2[0].0, 2..6, "内容源区间含转义 4 字节 a\\|b");
        assert_eq!(cells2[0].2, vec![2, 4, 5, 6], "渲染 a|b ↔ 源 a\\|b（'|' 映到字面源位）");
        // 行内粗体：`**x**` 渲染 `x`，映射跳过记号。
        let cells3 = split_table_cells_mapped("| **x** |");
        assert_eq!(cells3[0].1.as_ref(), "x");
        assert_eq!(cells3[0].0, 2..7, "内容源区间 = `**x**` 五字节");
        assert_eq!(cells3[0].2, vec![4, 7], "渲染 x ↔ ** 之间的 x（源 4..5，哨兵 7）");
        // 无首尾外竖线（raw_cells 风格）+ 尾随空格。
        let cells4 = split_table_cells_mapped("A | B ");
        assert_eq!(cells4[0].0, 0..1);
        assert_eq!(cells4[0].1.as_ref(), "A");
        assert_eq!(cells4[1].0, 4..5, "B 在索引 4（跳过 `A | `）");
    }

    #[test]
    fn full_line_image_becomes_image_row() {
        let src = "前文\n\n![插图](images/a.jpeg)\n\n后文\n";
        let rows = parse_to_rows(src);
        let img = rows.iter().find(|r| r.kind == RowKind::Image).expect("应有图片行");
        assert_eq!(img.text.as_ref(), "插图", "图片行文本应为 alt");
        assert_eq!(
            img.image().map(|i| i.src.as_ref()),
            Some("images/a.jpeg"),
            "图片行 src 应保留"
        );
        // 非整行的行内图片仍是段落
        let src2 = "文字 ![内嵌](x.png) 尾部\n";
        let rows2 = parse_to_rows(src2);
        assert_eq!(rows2[0].kind, RowKind::Paragraph, "行内图片不作独立图片行");
    }

    /// 关键正确性：`scan_blocks` 的块边界/类型/aux 必须与全量 `parse_to_rows`
    /// 完全一致；`parse_block_window` 重解析出的行必须与全量行的同下标切片逐字节一致。
    /// 这是「窗口级懒解析」索引不漂移的根基测试。
    #[test]
    fn block_index_and_window_match_full_parse() {
        // 覆盖：围栏 / 表格 / 标题 / 列表 / 引述 / 图片 / 空行 / 普通段落混排
        let cases: Vec<String> = {
            let mut v = Vec::new();
            v.push(
                "# 一级标题\n\n正文段落，`code` 与 **bold**\n\n- 列表一\n- 列表二\n\n> 引述\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 型号 | 尺寸 |\n| :--- | ---: |\n| A | 10 |\n| B | 20 |\n\n![插图](images/x.png)\n\n后文\n"
                    .to_string(),
            );
            // EPUB 排版指令 / 脚注标记（对齐指令 + 正文缩进「　　」 + `[^n]` 注记）。
            v.push(
                "%%ALIGN:RIGHT%%落款行\n　　正文[^1]段落。\n正文[^2]第二段。\n".to_string(),
            );
            // 真实（若有）+ benchdata
            for name in ["SKILL.md", "大文档测试.md", "bench_20k.md", "bench_160k.md"] {
                if let Ok(s) = std::fs::read_to_string(format!("target/{name}")) {
                    v.push(s);
                }
                if let Ok(s) = std::fs::read_to_string(format!("target/benchdata/{name}")) {
                    v.push(s);
                }
            }
            v
        };

        for src in &cases {
            let rows = parse_to_rows(src);
            let idx = scan_blocks(src);
            assert_eq!(
                idx.blocks.len(),
                rows.len(),
                "块数应等于渲染行数 (源 {} B)",
                src.len()
            );
            for (i, (m, r)) in idx.blocks.iter().zip(&rows).enumerate() {
                assert_eq!(m.src_line as usize, r.src_line, "block {i} src_line");
                assert_eq!(m.kind, r.kind, "block {i} kind");
                let aux = match r.kind {
                    RowKind::Code => r.code_lines().len() as u32,
                    RowKind::Table => r.table().map_or(0, |t| t.rows.len() as u32),
                    RowKind::Image => 1,
                    _ => 0,
                };
                assert_eq!(m.aux, aux, "block {i} aux");
            }

            // 目录：标题行号/级别/文本一致
            assert_eq!(idx.toc.len(), rows.iter().filter(|r| matches!(r.kind, RowKind::Heading(_))).count());
            for ((li, lv, tx), r) in idx.toc.iter().zip(rows.iter().filter(|r| matches!(r.kind, RowKind::Heading(_)))) {
                assert_eq!(*li, r.src_line);
                assert_eq!(*lv as u8, match r.kind { RowKind::Heading(n) => n, _ => 0 });
                assert_eq!(tx.as_str(), r.text.as_ref());
            }

            // 窗口重解析 == 全量切片（开头 / 中部 / 末尾）
            let n = idx.blocks.len();
            assert!(n > 0);
            for (lo, hi) in [
                (0, (5.min(n) + 0) as usize),
                (n / 2 - 2, n / 2 + 3),
                (n.saturating_sub(6), n - 1),
            ] {
                if lo > hi || hi >= n {
                    continue;
                }
                let win = parse_block_window(src, &idx, lo, hi);
                assert_eq!(win.len(), hi - lo + 1, "窗口 {lo}..={hi}");
                for (j, r) in win.iter().enumerate() {
                    let full = &rows[lo + j];
                    assert_eq!(r.src_line, full.src_line, "win hit {lo}+{j}");
                    assert_eq!(r.kind, full.kind);
                    assert_eq!(r.text.as_ref(), full.text.as_ref());
                }
            }
        }
    }

    /// rope 版窗口重解析（[`parse_block_window_rope`]）必须与 str 版逐行一致——
    /// 覆盖窗口模式的「从 Rope 按块字节边界切片」路径（视图 src_buf 改持 Rope 后
    /// 滚动重解析的全部走这里）。
    #[test]
    fn rope_block_window_matches_str_window() {
        let block = "# 一级标题\n\n正文段落，`code` 与 **bold**\n\n- 列表一\n- 列表二\n\n> 引述\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 型号 | 尺寸 |\n| :--- | ---: |\n| A | 10 |\n| B | 20 |\n\n![插图](images/x.png)\n\n后文\n";
        let mut src = String::from(block);
        for i in 0..60 {
            src.push_str(&format!("# 章节 {i}\n\n段落与 `code` [链接](https://x)\n\n"));
        }
        let rope = Rope::from(src.as_str());
        let idx_str = scan_blocks(&src);
        let idx_rope = scan_blocks_rope(&rope);
        assert_eq!(idx_rope.blocks, idx_str.blocks, "rope 全量索引与 str 一致");
        assert_eq!(idx_rope.toc, idx_str.toc);

        let n = idx_str.blocks.len();
        for (lo, hi) in [
            (0, 4_usize),
            (n / 2 - 2, n / 2 + 3),
            (n.saturating_sub(5), n - 1),
        ] {
            if lo > hi || hi >= n {
                continue;
            }
            let ws = parse_block_window(&src, &idx_str, lo, hi);
            let wr = parse_block_window_rope(&rope, &idx_rope, lo, hi);
            assert_eq!(wr.len(), ws.len(), "rope 窗口 {lo}..={hi} 行数");
            for (r, s) in wr.iter().zip(&ws) {
                assert_eq!(r.src_line, s.src_line);
                assert_eq!(r.kind, s.kind);
                assert_eq!(r.text.as_ref(), s.text.as_ref());
                assert_eq!(
                    r.code_lines().len(),
                    s.code_lines().len(),
                    "rope 窗口 {lo}..={hi} code 行数"
                );
            }
        }
    }

    /// 基准：解析结果的驻留内存放大倍数（打印，不构成断言；`--ignored` 运行）。
    ///
    /// 运行：`cargo test --release -- --ignored bench_row_parse_memory --nocapture`
    /// 真实小文档（SKILL 风格）+ 接近 `PREVIEW_MAX_DOC_LINES` 的大文档各测一遍，
    /// 直观看出「解析成 Row 把源码放大了多少倍」。
    /// 当前进程**私有字节**（`GetProcessMemoryInfo` 的 `PrivateUsage`），
    /// 直接反映本进程堆上被该次解析真正占住的内存（含分配器锯齿/碎片）。
    /// Windows 上用 Win32 API 读取；非 Windows 无法取数返回 0（基准仅内网 Windows 用）。
    #[cfg(windows)]
    fn process_private_bytes() -> usize {
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_nonpaged_pool_usage: usize,
            quota_nonpaged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            #[allow(clashing_extern_declarations)]
            fn K32GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                counters: *mut ProcessMemoryCounters,
                size: u32,
            ) -> i32;
        }
        unsafe {
            let mut m: ProcessMemoryCounters = std::mem::zeroed();
            m.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) != 0 {
                m.private_usage
            } else {
                0
            }
        }
    }

    #[cfg(not(windows))]
    fn process_private_bytes() -> usize {
        0
    }

    #[test]
    #[ignore]
    fn bench_row_parse_memory() {
        use std::time::Instant;

        // 项目 target 目录下的真实 SKILL 文档（与本讨论同款，143 行小文档）；
        // 以及 16 万行大文档（超过 PREVIEW_MAX_DOC_LINES=20000 自动预览拦截线）。
        // 跑 --ignored 且文件存在才打印，避免路径依赖。
        let mut cases: Vec<(String, String)> = Vec::new();
        for name in ["SKILL.md", "SKILL.txt", "大文档测试.md", "大文档测试.txt"] {
            if let Ok(s) = std::fs::read_to_string(format!("target/{name}")) {
                cases.push((format!("target/{name}"), s));
            }
        }
        // 自动扫描 target/benchdata/*.md（生成脚本同目录即可自动纳入对比）
        if let Ok(dir) = std::fs::read_dir("target/benchdata") {
            let mut paths: Vec<_> = dir
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
                .collect();
            paths.sort();
            for p in paths {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    cases.push((format!("{}", p.display()), s));
                }
            }
        }

        // 大文档：标题/段落/列表/表格/代码围栏混合 ≈ 1.4 万行，贴近自动预览上限
        let block = "# 章节标题\n\n正文段落，包含 `行内代码` 与 **加粗** 和 [链接](https://x.y)。\n\n- 列表项一\n- 列表项二\n\n| 列A | 列B |\n| --- | --- |\n| 值1 | 值2 |\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n";
        let mut big = String::with_capacity(block.len() * 700);
        for _ in 0..700 {
            big.push_str(block);
        }
        cases.push(("合成大文档（≈1.4 万行）".to_string(), big));

        use std::mem::size_of;
        println!(
            "Row={}B | RowKind={}B RunKind={}B SharedString={}B Vec<SharedString>={}B (Range,RunKind)={}B TableData={}B Option<TableData>={}B Option<Box<TableData>>={}B Option<SharedString>={}B Option<Arc<Image>>={}B Option<u32>={}B",
            size_of::<Row>(),
            size_of::<RowKind>(),
            size_of::<RunKind>(),
            size_of::<SharedString>(),
            size_of::<Vec<SharedString>>(),
            size_of::<(Range<usize>, RunKind)>(),
            size_of::<TableData>(),
            size_of::<Option<TableData>>(),
            size_of::<Option<Box<TableData>>>(),
            size_of::<Option<SharedString>>(),
            size_of::<Option<std::sync::Arc<Image>>>(),
            size_of::<Option<u32>>(),
        );

        for (name, src) in &cases {
            let t = Instant::now();
            let before = process_private_bytes();
            let rows = parse_to_rows(src);
            let after = process_private_bytes();
            let (n, bytes) = rows_memory_estimate(&rows);
            let bytes = bytes.max(1);
            let delta_kb = after.saturating_sub(before) / 1024;
            println!(
                "{name}: 源 {} B / {} 行 → {} 渲染行, 估算驻留 {} KB (= 源×{:.1}), 进程私有字节增量 {} KB, Row {} B, 解析 {} ms",
                src.len(),
                src.lines().count(),
                n,
                bytes / 1024,
                bytes as f64 / src.len().max(1) as f64,
                delta_kb,
                std::mem::size_of::<Row>(),
                t.elapsed().as_millis(),
            );
        }

        // —— 大文档：窗口级懒解析 vs 全量驻留 ——
        println!("--- 窗口级懒解析对比（索引 + 原文 + 顶部 40 块窗口） ---");
        for (name, src) in &cases {
            let idx = scan_blocks(src);
            if idx.blocks.len() <= 4_000 {
                continue;
            }
            let n = idx.blocks.len();
            let index_b = n * std::mem::size_of::<BlockMeta>()
                + idx
                    .toc
                    .iter()
                    .map(|(_, _, t)| t.len() + 32)
                    .sum::<usize>();
            let src_b = src.len();
            let win = parse_block_window(src, &idx, 0, 40);
            let (_, win_b) = rows_memory_estimate(&win);
            let windowed_b = (index_b + src_b + win_b).max(1);
            let (full_n, full_b) = rows_memory_estimate(&parse_to_rows(src));
            let pct = 100.0 * (1.0 - windowed_b as f64 / full_b.max(1) as f64);
            println!(
                "{name}: 全量 {full_n} 行 ≈ {} KB ↔ 窗口模式 ≈ {} KB（省 {pct:.0}%）",
                full_b / 1024,
                windowed_b / 1024,
            );
        }
    }

    /// 增量索引必须与全量 `scan_blocks` 逐字节一致（blocks / toc）——
    /// 覆盖插入、删行、拆行、合并、围栏内改、围栏新开、表格改、标题改。
    #[test]
    fn incremental_equals_full_scan_across_edits() {
        let base = "# 标题\n> 引用\n- 列表\n\n```rust\nfn f() {}\nlet s = \"x\";\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n末段 `code` 与[链接](https://x)\n";
        let mut src = base.to_string();
        let mut prev = scan_blocks(&src);
        let apply = |src: &mut String, prev: &mut BlockIndex, start: usize, end: usize, ins: &str| {
            let (s, e) = (start.min(src.len()), end.min(src.len()).max(start));
            let mut new = String::with_capacity(src.len() + ins.len() - (e - s));
            new.push_str(&src[..s]);
            new.push_str(ins);
            new.push_str(&src[e..]);
            let inc = scan_blocks_incremental(src, prev, &new, None);
            let full = scan_blocks(&new);
            assert!(
                inc.blocks == full.blocks && inc.toc == full.toc,
                "编辑后不一致\n新文本头≈:\n{:?}\ninc_blocks={:?}\n\nfull_blocks={:?}",
                &new[..new.len().min(160)],
                inc.blocks,
                full.blocks,
            );
            *src = new;
            *prev = inc;
        };

        // 行内插入
        let at = src.find("let s").unwrap() + 3;
        apply(&mut src, &mut prev, at, at, "ux ");
        // 删除引用行
        let q = "> 引用";
        let at = src.find(q).unwrap();
        apply(&mut src, &mut prev, at, at + q.len(), "");
        // 拆行（列表后插入换行+新项）
        let at = src.find("- 列表").unwrap() + "- 列表".len();
        apply(&mut src, &mut prev, at, at, "- 新项\n");
        // 合并行（删除末段前换行）
        let at = src.find("末段").unwrap() - 1;
        apply(&mut src, &mut prev, at, at + 1, " ");
        // 围栏内新增一行（前一步把 "let s" 改成了 "letux"）
        let at = src.find("letux").unwrap();
        apply(&mut src, &mut prev, at, at, "    let t2 = 2;\n");
        // 中段新开一个围栏
        let at = src.find("末段").unwrap();
        apply(&mut src, &mut prev, at, at, "```\n中间代码\n```\n\n");
        // 编辑表头（表格行坐标变化；全 ASCII 区间避免切在 UTF-8 中间）
        let at = src.find("| a ").unwrap();
        apply(&mut src, &mut prev, at, at + 4, "| ah");
        // 编辑标题（toc 变化）："#" → "## "（0..2 全 ASCII）
        apply(&mut src, &mut prev, 0, 2, "## ");
    }

    /// 未变文本 → 增量直接返回旧索引（零重扫）。
    #[test]
    fn incremental_noop_returns_prev() {
        let src = "# 标题\n正文\n";
        let prev = scan_blocks(src);
        let inc = scan_blocks_incremental(src, &prev, src, None);
        assert_eq!(inc.blocks, prev.blocks);
        assert_eq!(inc.toc, prev.toc);
    }

    /// dirty 侧通道路径：给 `scan_blocks_incremental` 提供「变化字节区间」时，
    /// 结果必须与全量 `scan_blocks` 逐字节一致。覆盖各种编辑 + 用真实的前后缀
    /// 定位精确 dirty，以及模拟 tabs 侧 bounding box 的「膨胀」dirty。
    #[test]
    fn incremental_with_dirty_equals_full_scan() {
        let base = String::from("# 标题\n> 引用\n- 列表\n\n```rust\nfn f() {}\nlet s = \"x\";\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n末段与[链接](https://x) 结尾\n");
        // 每个场景：构造 prev_src（前一帧）→ new_src（当前帧），dirty 用前后缀定位的
        // 精确变化区 + 一个「前后膨胀 6B」的 bounding box 版各验一遍。
        let run = |name: &str, prev_src: String, new_src: String| {
            let prev = scan_blocks(&prev_src);
            let cp = common_prefix_bytes(&prev_src, &new_src);
            let cs = common_suffix_bytes(&prev_src, &new_src);
            let exact = cp..(new_src.len() - cs);
            for (tag, dirty) in [
                ("精确", exact.clone()),
                ("膨胀", (exact.start.saturating_sub(6))..(exact.end + 6).min(new_src.len())),
            ] {
                let inc = scan_blocks_incremental(&prev_src, &prev, &new_src, Some(dirty));
                let full = scan_blocks(&new_src);
                assert!(
                    inc.blocks == full.blocks && inc.toc == full.toc,
                    "{name}/{tag} 不一致\nprev_len={} new_len={} dirty={:?}\ninc_blocks={:?}\nfull_blocks={:?}",
                    prev_src.len(),
                    new_src.len(),
                    exact,
                    inc.blocks,
                    full.blocks,
                );
            }
        };

        // —— 连续两次编辑构造两帧（去抖窗口内多次编辑的真实场景）——
        let mut frame1 = base.clone();
        let mut frame2 = base.clone();
        let a = frame1.find("fn f").unwrap() + 4;
        frame1.insert_str(a, "g");
        let a = frame2.find("fn f").unwrap() + 4;
        frame2.insert_str(a, "g");
        frame2.insert_str(frame2.find("结尾").unwrap(), "XYZ");
        run("两帧累积", frame1, frame2);

        // —— 同帧多处编辑（dirty 须覆盖全部变化点）——
        run("两处编辑", base.clone(), {
            let mut v = base.clone();
            v.insert_str(v.find("| a").unwrap() + 2, "XX");
            v.insert_str(v.find("结尾").unwrap(), "YY");
            v
        });

        // —— 单编辑场景 ——
        run("文末追加", base.clone(), { let mut v = base.clone(); v.push_str("\n尾行"); v });
        run("中部删字", base.clone(), { let mut v = base.clone(); v.remove(v.find("结尾").unwrap()); v });
        run("中部插字", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "XYZ"); v });
        run("插整段", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n\n新段落\n"); v });
        run("拆行", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n"); v });
        run("合并行", base.clone(), { let mut v = base.clone(); let at = v.rfind('\n').unwrap(); v.remove(at); v });
        run("表头改", base.clone(), { let mut v = base.clone(); let at = v.find("| a ").unwrap(); v.replace_range(at..at + 4, "| ah"); v });
        run("围栏内改", base.clone(), { let mut v = base.clone(); v.replace_range(v.find("fn f").unwrap() + 4..v.find("fn f").unwrap() + 7, "g()"); v });
        run("去围栏", base.clone(), { let mut v = base.clone(); let at = v.find("```rust").unwrap(); v.replace_range(at..at + 3, "kok"); v });
    }

    /// ropey `Rope` 必须 `Send + Sync`（快照交后台线程扫描的前提），且 `clone` 为
    /// O(1) COW（不物化全文）。编译期 + 运行期断言。
    #[test]
    fn rope_is_send_sync_clone_cheap() {
        fn require_send_sync<T: Send + Sync>() {}
        require_send_sync::<Rope>();
        let a = Rope::from("hello 中文，多行\n第二行\n");
        let b = a.clone();
        assert_eq!(a.to_string(), b.to_string(), "COW 克隆内容一致");
        assert_eq!(a.len(), b.len());
    }

    /// rope 版增量索引必须与 rope 全量扫描逐字节一致（blocks / toc）——这是
    /// 「按 dirty 区域切片 + 尾部对齐复用」路径的正确性安全网。复现 str 版的
    /// 全部编辑场景（含跨帧累积、同帧多处编辑、精确/膨胀 dirty）。
    #[test]
    fn incremental_rope_with_dirty_equals_full_scan() {
        let base = String::from("# 标题\n> 引用\n- 列表\n\n```rust\nfn f() {}\nlet s = \"x\";\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n末段与[链接](https://x) 结尾\n");
        let run = |name: &str, prev_src: String, new_src: String| {
            let prev_rope = Rope::from(prev_src.as_str());
            let prev = scan_blocks_rope(&prev_rope);
            let new_rope = Rope::from(new_src.as_str());
            let cp = common_prefix_bytes(&prev_src, &new_src);
            let cs = common_suffix_bytes(&prev_src, &new_src);
            let exact = cp..(new_src.len() - cs);
            for (tag, dirty) in [
                ("精确", exact.clone()),
                ("膨胀", (exact.start.saturating_sub(6))..(exact.end + 6).min(new_src.len())),
            ] {
                let inc = scan_blocks_incremental_rope(&prev_rope, &prev, &new_rope, Some(dirty));
                let full = scan_blocks_rope(&new_rope);
                assert!(
                    inc.blocks == full.blocks && inc.toc == full.toc,
                    "{name}/{tag} 不一致\nprev_len={} new_len={} dirty={:?}\ninc_blocks={:?}\nfull_blocks={:?}",
                    prev_src.len(),
                    new_src.len(),
                    exact,
                    inc.blocks,
                    full.blocks,
                );
            }
        };

        // 连续两次编辑构造两帧（去抖窗口内多次编辑的真实场景）
        let mut frame1 = base.clone();
        let mut frame2 = base.clone();
        let a = frame1.find("fn f").unwrap() + 4;
        frame1.insert_str(a, "g");
        let a = frame2.find("fn f").unwrap() + 4;
        frame2.insert_str(a, "g");
        frame2.insert_str(frame2.find("结尾").unwrap(), "XYZ");
        run("两帧累积", frame1, frame2);

        // 同帧多处编辑（dirty 须覆盖全部变化点）
        run("两处编辑", base.clone(), {
            let mut v = base.clone();
            v.insert_str(v.find("| a").unwrap() + 2, "XX");
            v.insert_str(v.find("结尾").unwrap(), "YY");
            v
        });

        // 单编辑场景
        run("文末追加", base.clone(), { let mut v = base.clone(); v.push_str("\n尾行"); v });
        run("中部删字", base.clone(), { let mut v = base.clone(); v.remove(v.find("结尾").unwrap()); v });
        run("中部插字", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "XYZ"); v });
        run("插整段", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n\n新段落\n"); v });
        run("拆行", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n"); v });
        run("合并行", base.clone(), { let mut v = base.clone(); let at = v.rfind('\n').unwrap(); v.remove(at); v });
        run("表头改", base.clone(), { let mut v = base.clone(); let at = v.find("| a ").unwrap(); v.replace_range(at..at + 4, "| ah"); v });
        run("围栏内改", base.clone(), { let mut v = base.clone(); v.replace_range(v.find("fn f").unwrap() + 4..v.find("fn f").unwrap() + 7, "g()"); v });
        run("去围栏", base.clone(), { let mut v = base.clone(); let at = v.find("```rust").unwrap(); v.replace_range(at..at + 3, "kok"); v });
    }

    /// rope 版 `dirty=None` 的快照 diff 兜底路径 + 「未变文本零重扫」——
    /// 与 str 版行为一致。
    #[test]
    fn incremental_rope_noop_and_snapshot_diff_fallback() {
        // 未变 → 直接返回旧索引
        let src = Rope::from("# 标题\n正文\n");
        let prev = scan_blocks_rope(&src);
        let inc = scan_blocks_incremental_rope(&src, &prev, &src, None);
        assert_eq!(inc.blocks, prev.blocks);
        assert_eq!(inc.toc, prev.toc);

        // dirty=None 兜底：真实编辑，靠前后缀 diff 定位变化区
        let prev_src = Rope::from("# 标题\n> 引用\n普通段落与 `code` [链接](https://x) 结尾\n");
        let prev = scan_blocks_rope(&prev_src);
        let mut text = prev_src.to_string();
        let at = text.find("普通段落").unwrap() + 3;
        text.insert_str(at, "ABC");
        let new = Rope::from(text.as_str());
        let inc = scan_blocks_incremental_rope(&prev_src, &prev, &new, None);
        let full = scan_blocks_rope(&new);
        assert!(
            inc.blocks == full.blocks && inc.toc == full.toc,
            "dirty=None 兜底不一致\ninc_blocks={:?}\nfull_blocks={:?}",
            inc.blocks,
            full.blocks,
        );
    }

    /// 基准（--ignored）：rope 化「打字同步每轮」成本对比——旧 str 路径（`value()`
    /// 物化 6.4MB + 增量扫描对全串 split）vs 新 rope 路径（Rope::clone O(1) + 按 dirty
    /// 区域切片）。与 handoff 3.1 同量级的大文档（≈6MB/14 万行），打印不断言。
    #[test]
    #[ignore]
    fn bench_rope_incremental_vs_old_str_snapshot() {
        use std::time::Instant;
        let block = "# 章节标题\n\n正文段落，包含 `行内代码` 与 **加粗** 和 [链接](https://x.y)。\n\n- 列表项一\n- 列表项二\n\n| 列A | 列B |\n| --- | --- |\n| 值1 | 值2 |\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n";
        let mut text = String::new();
        while text.len() < 6_000_000 {
            text.push_str(block);
        }
        let n_lines = text.lines().count();
        let prev_str = text.clone();
        let prev_rope = Rope::from(prev_str.as_str());
        let prev = scan_blocks(&prev_str);

        // 每次在文档中部插 6 字节（模拟打字），dirty 形如 (at..at+6)；
        // 插入点取字节边界（真实 InputEdit 的 dirty 区间恒为字符边界）。
        let at = text.find("，包含").unwrap();
        let mut new = text.clone();
        new.insert_str(at, "ABCXYZ");
        let rope = Rope::from(new.as_str());
        let dirty_range = at..(at + 6);

        // 快照成本：旧 `value()`（String 物化/克隆）vs 新 `Rope::clone`（O(1) COW）
        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..50 {
            let s = prev_str.to_string();
            sink += s.len();
        }
        let value_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;
        let t = Instant::now();
        for _ in 0..50 {
            let r = prev_rope.clone();
            sink += r.len();
        }
        let rope_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;

        // 增量扫描：旧 str（全串 split 建行表）vs 新 rope（只切 dirty 区域）
        let t = Instant::now();
        let mut sum_blocks = 0usize;
        for _ in 0..50 {
            let idx = scan_blocks_incremental(&prev_str, &prev, &new, Some(dirty_range.clone()));
            sum_blocks += idx.blocks.len();
        }
        let str_inc_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;
        let t = Instant::now();
        for _ in 0..50 {
            let idx =
                scan_blocks_incremental_rope(&prev_rope, &prev, &rope, Some(dirty_range.clone()));
            sum_blocks += idx.blocks.len();
        }
        let rope_inc_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;

        println!(
            "大文档 {} 行 / {:.1} MB：快照 value()/String 物化 {:.3} ms vs Rope::clone {:.3} ms（{:.0}x）；增量 str(全串split) {:.3} ms vs rope(区域切片) {:.3} ms；每轮合计 str {:.3} ms vs rope {:.3} ms（{:.1}x）",
            n_lines,
            text.len() as f64 / 1e6,
            value_ms,
            rope_ms,
            value_ms / rope_ms.max(1e-9),
            str_inc_ms,
            rope_inc_ms,
            str_inc_ms + value_ms,
            rope_inc_ms + rope_ms,
            (str_inc_ms + value_ms) / (rope_inc_ms + rope_ms).max(1e-9),
        );
        assert!(sum_blocks > 0, "sink {}", sink);
    }

    /// 回归：增量 toc 前缀过滤曾用「块下标 `bi`」去比较**源码行号**。围栏（一块对
    /// 应多行源码）会让两类坐标漂移：文档中部某标题的源码行号 > 它前方的块数。
    /// 末尾追加时，如果该标题在重扫起点之前（应保留），旧过滤器却把它误丢 →
    /// 增量 toc 与全量不一致（str 与 rope 两条增量路径都曾受影响）。
    #[test]
    fn toc_prefix_uses_source_line_not_block_index() {
        // 500 行围栏（1 块）→「前置标题」（行号很大、块下标很小）→ 若干单行段落块
        let mut src = String::from("```rust\n");
        for i in 0..500 {
            src.push_str(&format!("// 第 {i} 行\n"));
        }
        src.push_str("```\n");
        src.push_str("# 前置标题\n");
        for i in 0..500 {
            src.push_str(&format!("正文行 {i}\n"));
        }
        src.push('\n');
        let base = scan_blocks(&src);
        assert!(
            base.toc.iter().any(|(_, _, t)| t == "前置标题"),
            "前置标题应在 toc"
        );

        // 末尾追加新标题：锚点 = 末尾空块，其块下标 ≈ 502 > 前置标题行号 502 附近
        let mut new = src.clone();
        new.push_str("# 追加标题\n正文追加\n");
        let dirty = src.len()..new.len();
        let full = scan_blocks(&new);

        let str_prev = base.clone();
        let inc = scan_blocks_incremental(&src, &str_prev, &new, Some(dirty.clone()));
        assert!(
            inc.blocks == full.blocks && inc.toc == full.toc,
            "str 版 toc 前缀过滤不应丢标题\ninc_toc={:?}\nfull_toc={:?}",
            inc.toc,
            full.toc,
        );

        let prev_rope = Rope::from(src.as_str());
        let prev = scan_blocks_rope(&prev_rope);
        let new_rope = Rope::from(new.as_str());
        let inc = scan_blocks_incremental_rope(&prev_rope, &prev, &new_rope, Some(dirty));
        assert!(
            inc.blocks == full.blocks && inc.toc == full.toc,
            "rope 版 toc 前缀过滤不应丢标题\ninc_toc={:?}\nfull_toc={:?}",
            inc.toc,
            full.toc,
        );
    }

    /// #5 持久段：连续编辑后 `blocks` 的段数被 amortize 压在上限内（读取保持 O(段数)），
    /// 且每轮结果与全量扫描一致（中段的 `分割/平移` 合成正确）。
    #[test]
    fn blocks_segments_stay_bounded_across_edits() {
        let block = "# 章节\n\n正文 `code` [链接](https://x)\n\n- 项一\n- 项二\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n";
        let mut text = String::new();
        for _ in 0..1500 {
            text.push_str(block);
        }
        let mut cur = Rope::from(text.as_str());
        let mut prev = scan_blocks_rope(&cur);
        // 中部 ASCII 锚点「| a |」（纯 ASCII → 其字节位置必为字符边界），供中段编辑用
        let bytes = text.as_bytes();
        let half = text.len() / 2;
        let at_mid = (half..text.len().saturating_sub(5))
            .find(|&i| bytes[i] == b'|' && &bytes[i..i + 4] == b"| a ")
            .unwrap_or(text.len() - 1);
        let mut max_segs = 0usize;
        for i in 0..120 {
            // 文末追加（段数会累积 → 触发合并），每 3 轮夹一次中部编辑（触发前缀/后缀
            // 共享与平移合成）。
            let at = if i % 3 == 2 { at_mid } else { cur.len() };
            let ins = "\n# 追加\n正文\n";
            let mut next = cur.clone();
            next.insert(at, ins);
            let dirty = at..at + ins.len();
            let inc = scan_blocks_incremental_rope(&cur, &prev, &next, Some(dirty));
            max_segs = max_segs.max(inc.blocks.seg_count());
            let full = scan_blocks_rope(&next);
            assert!(
                inc.blocks == full.blocks && inc.toc == full.toc,
                "第 {i} 轮增量应与全量一致"
            );
            cur = next;
            prev = inc;
        }
        assert!(
            max_segs <= BLOCKS_MAX_SEGS,
            "段数应被 amortize 压在上限内: max={max_segs} 上限={BLOCKS_MAX_SEGS}"
        );
    }

    /// 基准（--ignored）：复刻 handoff §3.1 的方法——对参考文件 `target/大文档测试.md`
    /// （6.4MB / ≈17 万行）连续做 **1000 次文末插入**，对比 str 路径（`value()`
    /// String 物化 + 增量扫描全串 split）与新 rope 路径（Rope::clone O(1) + 增量
    /// 扫描区域切片），每轮取 dirty，最终与全量扫描逐字节校验。打印、不断言。
    #[test]
    #[ignore]
    fn bench_1000_inserts_on_reference_doc() {
        use std::time::Instant;
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/大文档测试.md"
        ));
        if !path.exists() {
            eprintln!("跳过：缺少参考文件 {path:?}");
            return;
        }
        let text = std::fs::read_to_string(path).expect("参考文件读取失败");
        let n_inserts = 1000usize;

        // —— 旧 str 路径：每轮 value()（String 物化）+ 增量扫描（全串 split）——
        let mut cur = text.clone();
        let mut prev = scan_blocks(&cur);
        let mut str_total = std::time::Duration::ZERO;
        let t0 = Instant::now();
        for i in 0..n_inserts {
            let old_len = cur.len();
            let mut next = cur.clone();
            next.push_str(&format!("\n# 追加基准段 {i}\n正文 `code` 与[链接](https://x)\n"));
            let dirty = old_len..next.len();
            let t = Instant::now();
            let inc = scan_blocks_incremental(&cur, &prev, &next, Some(dirty));
            str_total += t.elapsed();
            if i % 100 == 99 {
                let full = scan_blocks(&next);
                assert!(inc.blocks == full.blocks && inc.toc == full.toc, "str 第 {i} 轮不一致");
            }
            cur = next;
            prev = inc;
        }
        println!(
            "旧 str（value+split+增量）: {} 次文末插入共 {:.1} s，扫描累计 {:.1} ms/次",
            n_inserts,
            t0.elapsed().as_secs_f64(),
            str_total.as_secs_f64() * 1000.0 / n_inserts as f64,
        );

        // —— 新 rope 路径：每轮 Rope::clone（O(1)）+ 增量扫描（区域切片）——
        let mut cur = Rope::from(text.as_str());
        let mut prev = scan_blocks_rope(&cur);
        let mut rope_total = std::time::Duration::ZERO;
        let t0 = Instant::now();
        for i in 0..n_inserts {
            let old_len = cur.len();
            let mut next = cur.clone();
            next.insert(old_len, &format!("\n# 追加基准段 {i}\n正文 `code` 与[链接](https://x)\n"));
            let dirty = old_len..next.len();
            let t = Instant::now();
            let inc = scan_blocks_incremental_rope(&cur, &prev, &next, Some(dirty));
            rope_total += t.elapsed();
            if i % 100 == 99 {
                let full = scan_blocks_rope(&next);
                assert!(inc.blocks == full.blocks && inc.toc == full.toc, "rope 第 {i} 轮不一致");
            }
            cur = next;
            prev = inc;
        }
        println!(
            "新 rope（clone+区域切片+增量）: {} 次文末插入共 {:.1} s，扫描累计 {:.1} ms/次；末尾 blocks 段数 {}（≤ {} 才保持 O(1) 共享）",
            n_inserts,
            t0.elapsed().as_secs_f64(),
            rope_total.as_secs_f64() * 1000.0 / n_inserts as f64,
            prev.blocks.seg_count(),
            BLOCKS_MAX_SEGS,
        );

        // —— 中部编辑（前后缀都要共享/平移）：持久分段的典型场景 ——
        let mid = text.len() / 2;
        let mut cur = Rope::from(text.as_str());
        let mut prev = scan_blocks_rope(&cur);
        let mut mid_total = std::time::Duration::ZERO;
        let ins = "\n# 中段追加行\n正文\n";
        let t0 = Instant::now();
        for i in 0..200 {
            let at = mid + (i * 3) % 8000;
            let mut next = cur.clone();
            next.insert(at, ins);
            let dirty = at..at + ins.len();
            let t = Instant::now();
            let inc = scan_blocks_incremental_rope(&cur, &prev, &next, Some(dirty));
            mid_total += t.elapsed();
            if i % 100 == 99 {
                let full = scan_blocks_rope(&next);
                assert!(inc.blocks == full.blocks && inc.toc == full.toc, "rope 中段第 {i} 轮不一致");
            }
            cur = next;
            prev = inc;
        }
        println!(
            "rope 中部编辑（前后缀持久共享）: 200 次共 {:.1} s，扫描累计 {:.1} ms/次；段数 {}",
            t0.elapsed().as_secs_f64(),
            mid_total.as_secs_f64() * 1000.0 / 200.0,
            prev.blocks.seg_count(),
        );
    }

    /// 基准（--ignored）：大文档小编辑时，增量（dirty 侧通道 vs 快照 diff）vs
    /// 全量扫描耗时对比（打印、不断言）。
    #[test]
    #[ignore]
    fn bench_incremental_vs_full_scan() {
        use std::time::Instant;
        let mut src = String::new();
        for i in 0..1600 {
            src.push_str(&format!("# 标题段 {i}\n> 引用\n- 列表项\n\n```rust\nfn f() {{}}\nlet s = \"x\";\n```\n\n普通段落与 `code` [链接](https://x)\n\n"));
        }
        let prev = scan_blocks(&src);
        // 中部小编辑：给某段插几个字
        let at = src.find("普通段落").unwrap() + 6;
        let mut new = src.clone();
        new.insert_str(at, "ABC");
        let dirty = at..(at + 3);
        let t0 = Instant::now();
        let _inc_fast = scan_blocks_incremental(&src, &prev, &new, Some(dirty));
        let fast_ms = t0.elapsed();
        let t0 = Instant::now();
        let _inc = scan_blocks_incremental(&src, &prev, &new, None);
        let inc_ms = t0.elapsed();
        let t1 = Instant::now();
        let _full = scan_blocks(&new);
        let full_ms = t1.elapsed();
        println!(
            "{} 行：增量(dirty) {:.1} ms vs 增量(快照diff) {:.1} ms vs 全量 {:.1} ms",
            prev.blocks.len(),
            fast_ms.as_secs_f64() * 1000.0,
            inc_ms.as_secs_f64() * 1000.0,
            full_ms.as_secs_f64() * 1000.0,
        );
    }

    #[test]
    fn epub_align_directive_and_notes_parse() {
        let src = "%%ALIGN:RIGHT%%落款第一行\n%%ALIGN:LEFT%%浮段\n　　正文[^1]带注段。\n正文[^2]再一段。";
        let rows = parse_to_rows(src);
        assert_eq!(rows.len(), 4, "指令行不产生额外渲染行");
        // 右对齐行
        assert_eq!(rows[0].kind, RowKind::Paragraph);
        assert_eq!(rows[0].align, Some(RowAlign::Right), "RIGHT 指令应生效");
        assert_eq!(rows[0].text.as_ref(), "落款第一行", "指令前缀应被剥掉");
        // 左对齐行
        assert_eq!(rows[1].align, Some(RowAlign::Left));
        assert_eq!(rows[1].text.as_ref(), "浮段");
        // 普通正文：无对齐、`[^1]` → `[1]` + 注记区间（含前置「　　」缩进）
        assert_eq!(rows[2].align, None);
        assert_eq!(rows[2].text.as_ref(), "　　正文[1]带注段。", "注记 ^ 应剥掉");
        assert_eq!(rows[2].notes.len(), 1, "应有 1 条注记");
        let (r, n) = &rows[2].notes[0];
        assert_eq!(*n, 1);
        assert_eq!(&rows[2].text[r.clone()], "[1]", "注记区间应对应 [1]");
        // 无指令行保持默认
        assert_eq!(rows[3].align, None);
        // 窗口重解析（块边界切片）也要恢复对齐：从第 2 块起的切片应含指令行。
        let src2 = "前缀段\n%%ALIGN:RIGHT%%落款行";
        let idx = scan_blocks(src2);
        let mut rb = Vec::new();
        for i in 0..idx.blocks.len() {
            rb.extend(parse_block_window(src2, &idx, i, i));
        }
        assert_eq!(rb.len(), idx.blocks.len());
        assert_eq!(rb[1].align, Some(RowAlign::Right), "窗口重解析应恢复对齐指令");
    }

    #[test]
    fn epub_directive_inside_quote_and_bullet_is_stripped() {
        // 引述块/列表项里被包裹的对齐指令（如 `> %%ALIGN:RIGHT%%…`）会漏到行首判断
        // 之外，必须从内容里也剥掉，不能作为可见文本渲染。
        let rows = parse_to_rows("> %%ALIGN:RIGHT%%引述落款\n- %%ALIGN:LEFT%%列表项");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, RowKind::Quote);
        assert_eq!(rows[0].text.as_ref(), "引述落款", "引述里的指令应剥掉，实际 {}", rows[0].text);
        assert_eq!(rows[0].align, Some(RowAlign::Right));
        assert!(!rows[0].text.contains("%%ALIGN"), "不得泄漏指令串");
        assert_eq!(rows[1].kind, RowKind::Bullet);
        assert_eq!(rows[1].text.as_ref(), "列表项", "列表项里的指令应剥掉，实际 {}", rows[1].text);
        assert_eq!(rows[1].align, Some(RowAlign::Left));
        assert!(!rows[1].text.contains("%%ALIGN"), "不得泄漏指令串");
        // 扫描器一致性不受影响（指令行仍是同一块）。
        let idx = scan_blocks("> %%ALIGN:RIGHT%%引述落款\n- %%ALIGN:LEFT%%列表项");
        assert_eq!(idx.blocks.len(), 2);
    }

    #[test]
    fn epub_note_def_marker_parses() {
        // 注释定义标记 `[!n]` → Row.note_defs（逆跳用），显示成 `[n]`。
        let rows = parse_to_rows("正文[^2]引用。\n[!2]　注释定义内容。\n[!33]　另一条。");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].notes.len(), 1);
        assert_eq!(rows[0].note_defs.len(), 0);
        assert_eq!(rows[1].notes.len(), 0);
        assert_eq!(rows[1].note_defs.len(), 1, "定义标记应进 note_defs");
        let (r, n) = &rows[1].note_defs[0];
        assert_eq!(*n, 2);
        assert_eq!(&rows[1].text[r.clone()], "[2]");
        assert_eq!(rows[1].text.starts_with("[2]"), true);
        assert_eq!(rows[2].note_defs[0].1, 33);
    }

    #[test]
    fn citation_index_from_rows_maps_refs_and_defs() {
        // 全量路径：注记号 → (渲染行, 显示文本字节区间) + 详情正文。
        let rows = parse_to_rows("一句话[^1]引用。\n[!1]　定义1。\n[!2]　定义2（无文内对端）。");
        let idx = citation_index_from_rows(&rows);
        // 区间是**字节**坐标（CJK 每字 3 字节）：「一句话」=9B → [^1] 显示成 [1] 在 [9..12)。
        assert_eq!(idx.refs.get(&1), Some(&(0, 9..12)), "文内 [1] 应在第 0 行、区间 [9..12)");
        assert_eq!(idx.defs.get(&1), Some(&(1, 0..3)), "定义 [1] 应在第 1 行、区间 [0..3)");
        assert_eq!(idx.defs.get(&2), Some(&(2, 0..3)), "定义 [2] 应在第 2 行");
        assert_eq!(idx.refs.get(&2), None, "孤立定义不应进 refs");
        assert_eq!(idx.details.get(&1).map(|s| s.as_ref()), Some("定义1。"), "详情应剥掉 [1] 标记");
        assert_eq!(
            idx.details.get(&2).map(|s| s.as_ref()),
            Some("定义2（无文内对端）。")
        );
    }

    #[test]
    fn scan_citations_matches_row_indexes_and_skips_code() {
        // 窗口模式懒扫描：与 parse_to_rows 同块结构，行坐标（BlockIndex 下标）一致；
        // 代码块里的 `[^n]` 不是引用，不得计入。
        let src = "# 标题[^1]\n\n正文[^2]。\n\n```\n[^9] 代码里的方括号不是引用\n```\n\n[!1]　定1。\n[!2]　定2。";
        let idx = scan_citations(src);
        // 块：0=标题,1=空行,2=正文,3=空行,4=代码块,5=空行,6=定1,7=定2
        // （字节区间：「标题」=6B → [1] 在 [6..9)；「正文」=6B → [2] 在 [6..9)。）
        assert_eq!(idx.refs.get(&1), Some(&(0, 6..9)), "文内 [1] 在第 0 块");
        assert_eq!(idx.refs.get(&2), Some(&(2, 6..9)), "文内 [2] 在第 2 块");
        assert_eq!(idx.refs.contains_key(&9), false, "代码块里的 [^9] 不是引用");
        assert_eq!(idx.defs.get(&1), Some(&(6, 0..3)), "定义 [1] 在第 6 块");
        assert_eq!(idx.defs.get(&2), Some(&(7, 0..3)), "定义 [2] 在第 7 块");
        assert_eq!(idx.details.get(&1).map(|s| s.as_ref()), Some("定1。"));
        assert_eq!(idx.details.get(&2).map(|s| s.as_ref()), Some("定2。"));
        // 与全量路径产出一致（同结构 → 同名块坐标一致）。
        let rows = parse_to_rows(src);
        let from_rows = citation_index_from_rows(&rows);
        assert_eq!(idx, from_rows, "scan_citations 应与全量行推导一致");
    }
}
