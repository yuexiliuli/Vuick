//! `BlockIndex` 在 **Rope / 字符串** 上的「全量 / 增量」扫描与**窗口物化**引擎。
//!
//! 从 `parser`（行扫描器 + 块索引 + 引用索引 + 增量分析）拆出的「增量（rope）扫描 +
//! 窗口物化」部分（2026-09-24 重构）：依赖 `rows`（行解析）与 `blocks`（块索引结构）；
//! `#[cfg(test)]` 的 str 版正确性参照（`common_prefix_bytes` / `scan_blocks_incremental`）
//! 保留在 `parser`（测试基础设施），本模块只保留生产路径。

use std::ops::Range;

use gpui::{Image, SharedString};
use gpui_component::input::Rope;

use crate::markdown::blocks::{scan_blocks, BlockIndex, BlockMeta, Blocks, BlocksSeg};
use crate::markdown::rows::{
    fence_len, inline_strip, is_rule, list_prefix, parse_image_line, try_parse_table,
    parse_to_rows, Row, RowKind, RunKind, TableAlign, TableData,
};

/// 全量扫描一个 **ropey Rope** 文本（打开/首同步/大变化兜底用）。
///
/// 全量扫描本身就是 O(n)，因此内部物化一次 `String` 后复用 [`scan_blocks`]；
/// 单次 O(n) 物化只发生在「打开文档 / 预览首次同步 / 变化区过大」这类低频路径，
/// 打字同步的热路径走的是下面的 [`scan_blocks_incremental_rope`]（按 dirty 区域
/// 切片，O(k)）。传 `rope` 共享底层的快照更便宜（见 ropey COW 语义）。
pub fn scan_blocks_rope(rope: &Rope) -> BlockIndex {
    let text = rope.to_string();
    scan_blocks(&text)
}

/// 两个 rope 的公共前缀字节数。只在 `dirty=None` 的快照 diff 兜底路径调用（O(n)，
/// 与旧 `&str` 全串比对同量级），用 rope 的逐字节迭代器避免物化全文。
fn common_prefix_bytes_rope(a: &Rope, b: &Rope) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    let mut ia = a.bytes();
    let mut ib = b.bytes();
    while i < n {
        match (ia.next(), ib.next()) {
            (Some(x), Some(y)) if x == y => i += 1,
            _ => break,
        }
    }
    i
}

/// 两个 rope 的公共后缀字节数（方向反转的逐字节迭代）。
fn common_suffix_bytes_rope(a: &Rope, b: &Rope) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    let mut ia = a.bytes().reversed();
    let mut ib = b.bytes().reversed();
    while i < n {
        match (ia.next(), ib.next()) {
            (Some(x), Some(y)) if x == y => i += 1,
            _ => break,
        }
    }
    i
}

/// **预览增量解析（rope 版）**：语义与 `scan_blocks_incremental`（str 版，测试参照）
/// 完全一致，但输入为 ropey `Rope` —— 从而 UI 线程快照可 `Rope::clone`（O(1) COW，
/// 不物化全文），后台扫描也**不再对全串 split 建行表**：按 dirty 区/尾部对齐点从
/// rope `slice` 出变化区域（O(k)）再对区域切片扫描。
///
/// 定位尾部对齐点的关键不变量（与 str 版逐行对齐等价）：
/// dirty 之外新文本字节与旧文本相同，因此变化区之后的「第一条旧块边界」在旧坐标
/// `ob ≥ tail_start_new - delta` 处，对应新坐标行首 `ob + delta`——增量扫描从锚点
/// 行扫到该行首即可安全复用旧尾部 [`prev.blocks[pi..]`]（行号/字节整体平移）。
/// 对齐假设不命中（罕见，如变化区横跨围栏导致边界漂移）则整轮回退全量扫描兜底。
pub fn scan_blocks_incremental_rope(
    prev_src: &Rope,
    prev: &BlockIndex,
    new_src: &Rope,
    dirty: Option<Range<usize>>,
) -> BlockIndex {
    let old_len = prev_src.len();
    let new_len = new_src.len();
    let (tail_start_new, changed, b_first) = match dirty {
        // 已知变化区间（new_src 坐标）→ 直接定位，跳过前后缀全串比对。
        Some(r) => {
            let dlo = r.start.min(new_len);
            let dhi = r.end.min(new_len).max(dlo);
            (dhi, dhi - dlo, dlo)
        }
        None => {
            let cp = common_prefix_bytes_rope(prev_src, new_src);
            let cs = common_suffix_bytes_rope(prev_src, new_src);
            let tail_start_new = new_len.saturating_sub(cs);
            let changed = new_len.saturating_sub(cp + cs);
            if new_len == old_len && changed == 0 {
                return prev.clone();
            }
            (tail_start_new, changed, cp)
        }
    };
    if new_len == old_len && changed == 0 {
        return prev.clone();
    }
    if changed >= old_len.max(new_len) / 2 {
        return scan_blocks_rope(new_src);
    }

    let delta = new_len as i64 - old_len as i64;

    // 锚点 = 旧索引中含首处变化字节的块（变化前字节/行号两边一致）。
    let bi = prev
        .blocks
        .partition_point(|b| b.byte as usize <= b_first)
        .saturating_sub(1);
    let anchor = prev.blocks.get(bi).map(|b| b.src_line as usize).unwrap_or(0);
    let anchor_byte = prev.blocks.get(bi).map(|b| b.byte as usize).unwrap_or(0);

    // 未变前缀直接沿用；toc 同样取前缀（与 str 版相同的「源码行 vs 块下标」坐标
    // 修正：按重扫起点 `anchor` 行号过滤，而非块下标 `bi`）。
    let prefix_segs = prev.blocks.shares_prefix(bi);
    let mut blocks: Vec<BlockMeta> = Vec::new(); // 新扫描的变化中段（通常极小）
    let mut suffix_segs: Vec<BlocksSeg> = Vec::new();
    let mut toc: Vec<(usize, u8, String)> = prev
        .toc
        .iter()
        .filter(|(r, _, _)| *r < anchor)
        .cloned()
        .collect();

    // —— 只从 rope 切出「锚点到尾部对齐点」的变化区域（O(k)），不对全串 split ——
    let tail_old = tail_start_new as i64 - delta; // 尾部起点映射到旧坐标
    let pi = if tail_old <= old_len as i64 {
        prev.blocks.partition_point(|b| (b.byte as i64) < tail_old)
    } else {
        prev.blocks.len()
    };
    let region_end = if pi < prev.blocks.len() {
        // 变化区可能跨过围栏/表格边界使「首条旧块边界」与其后内容发生合并/分裂漂移，
        // 直接把尾部对齐点定在这条边界上常命中不了（然后全量回退——6MB 文档
        // `scan_blocks_rope` debug ~620ms）。向后放宽若干旧块边界做对齐检测区：
        // 扫描经过的旧块边界越多，命中「编辑后仍存在的旧块边界」概率越高，把
        // 全量回退变成小区域增量。放宽窗口外的尾部仍按字节精确对齐拼接。
        let lo = (pi + 16).min(prev.blocks.len());
        if lo < prev.blocks.len() {
            // 对齐检测区终点的旧块边界（新坐标行首）
            ((prev.blocks.at(lo).byte as i64 + delta).max(anchor_byte as i64))
                .min(new_len as i64) as usize
        } else {
            // 尾部不足 16 条边界 → 直接放宽到文末，由扫描自行收尾（不复用也无回退）。
            new_len
        }
    } else {
        // 尾部没有可用块边界 → 扫到文末（不复用，等价全量重扫）
        new_len
    };
    let region = if region_end > anchor_byte {
        new_src.slice(anchor_byte..region_end).to_string()
    } else {
        String::new()
    };
    let new_lines: Vec<&str> = region.split('\n').collect();
    let region_bytes = region.len();

    let mut idx = 0usize;
    let mut cur_byte = 0u32; // 区域内（相对锚点）当前行起始字节
    let mut reused = false;
    while idx < new_lines.len() {
        // —— 复用旧尾部的机会（absolute 坐标）——
        let new_byte = anchor_byte + cur_byte as usize;
        if new_byte >= tail_start_new {
            let prev_off = new_byte as i64 - delta;
            if (0..=old_len as i64).contains(&prev_off) {
                let prev_off = prev_off as usize;
                let pii = prev.blocks.partition_point(|b| (b.byte as usize) < prev_off);
                if let Some(pb) = prev.blocks.get(pii) {
                    if pb.byte as usize == prev_off {
                        let prev_line = pb.src_line as usize;
                        let line_shift = (anchor + idx) as i64 - prev_line as i64;
                        suffix_segs = prev.blocks.shares_suffix(pii, delta, line_shift);
                        for (r, lv, t) in prev.toc.iter().filter(|(r, _, _)| *r as usize >= prev_line) {
                            toc.push(((*r as i64 + line_shift).max(0) as usize, *lv, t.clone()));
                        }
                        reused = true;
                        break;
                    }
                }
            }
        }

        // —— 否则与 `scan_blocks` 完全相同的切块逻辑（逐块消费 `idx`）——
        let row_start = anchor + idx; // 文档绝对行号
        let line = new_lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        let raw_len = line.len() as u32;
        let blk_byte = (anchor_byte as u32).wrapping_add(cur_byte); // 文档绝对字节
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = 0u32;
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            while idx < new_lines.len() {
                let l = new_lines[idx].trim_end_matches('\r');
                if let Some((c, _)) = fence_len(l.trim_start()) {
                    if c == fc {
                        idx += 1;
                        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
                        break;
                    }
                }
                code_lines += 1;
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        if let Some((table, consumed)) = try_parse_table(&new_lines, idx) {
            for _ in 0..consumed {
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
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
        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
    }
    debug_assert!(
        reused || idx >= new_lines.len(),
        "增量扫描要么对齐复用旧尾部、要么扫完新文本"
    );
    if !reused && region_end < new_len {
        // 对齐假设未命中（如变化区横跨围栏导致边界漂移）→ 全量兜底保证与
        // [`scan_blocks`] 逐字节一致（区域切片只覆盖到假设对齐点，不能丢）。
        return scan_blocks_rope(new_src);
    }
    BlockIndex {
        blocks: Blocks::assemble(prefix_segs, blocks, suffix_segs),
        toc,
    }
}

/// 从行 `i` 的起始字节推进到下一行的起始字节（+行长+换行符；末行无换行时
/// clamp 到 `new_len`，哨兵多算的 1 字节只会让尾部复用更早就触发、不会越界）。
pub(crate) fn next_line_byte(new_lines: &[&str], cur: u32, i: usize, new_len: usize) -> u32 {
    cur.saturating_add(new_lines[i].len() as u32)
        .saturating_add(1)
        .min(new_len as u32)
}

/// 物化块区间 `[lo, hi]`（闭区间）的渲染行。
///
/// 关键正确性前提：`parse_to_rows` 是**逐块无状态**的（每轮 while 迭代自包含），
/// 且 `lo/hi` 是块边界。因此从 `lo` 行的字节起始切片重新解析，
/// 产出的行与全量解析第 `lo..=hi` 个 [`Row`] **逐字节一致**——索引不会漂移。
/// 缓存只保留这块窗口，渲染随滚动窗口移动，整篇驻留不随文档增长。
pub fn parse_block_window(src: &str, index: &BlockIndex, lo: usize, hi: usize) -> Vec<Row> {
    if index.blocks.is_empty() {
        return Vec::new();
    }
    let n = index.blocks.len();
    let lo = lo.min(n - 1);
    let hi = hi.min(n - 1);
    if lo > hi {
        return Vec::new();
    }
    let start = index.blocks.at(lo).byte as usize;
    let end0 = if hi + 1 < n {
        index.blocks.at(hi + 1).byte as usize
    } else {
        src.len()
    };
    // 切片到"下一块首行"处会带上上一块的换行符：`split('\n')` 会多造一个代表
    // "下一行"的空元素 → 多出幻影空行。裁掉末尾的 '\n' 即精确到块边界。
    // 唯一例外：**末块本身就是 EOF 幻影空行**（`byte == src.len()`，即文档以
    // `\r?\n` 结尾时扫描器给最后那个空串造的 Blank 块）——它靠这 1 个 '\n'
    // 才存在，裁掉会把窗口内的一块弄丢（全档窗口 70 vs 69 的实测来源）。
    let mut end = end0;
    let last_is_phantom = hi + 1 >= n && index.blocks.at(hi).byte as usize == src.len();
    if !last_is_phantom && end > start && src.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    let rows = parse_to_rows(&src[start..end]);
    debug_assert_eq!(rows.len(), hi - lo + 1, "窗口重解析行数必须与块数一致");
    // 窗口内 parse 的行号是"切片相对"的，平移到文档坐标（与块索引一致）
    let base_line = index.blocks.at(lo).src_line as usize;
    if base_line != 0 {
        let mut rows = rows;
        for r in &mut rows {
            r.src_line += base_line;
        }
        rows
    } else {
        rows
    }
}

/// rope 版 [`parse_block_window`]：块边界/行号/字节语义完全一致，只是输入为
/// ropey `Rope`——窗口只 `slice`（O(log n)）到可见区间（≤ 过扫块数），再 `to_string`
/// 物化该小块交给 [`parse_to_rows`]。窗口级懒解析路径不必再驻留整篇 6.4MB 的
/// `SharedString`（src_buf 改持 `Rope`，与编辑器 COW 共享底层，clone O(1)）。
pub fn parse_block_window_rope(rope: &Rope, index: &BlockIndex, lo: usize, hi: usize) -> Vec<Row> {
    if index.blocks.is_empty() {
        return Vec::new();
    }
    let n = index.blocks.len();
    let lo = lo.min(n - 1);
    let hi = hi.min(n - 1);
    if lo > hi {
        return Vec::new();
    }
    let start = index.blocks.at(lo).byte as usize;
    let end0 = if hi + 1 < n {
        index.blocks.at(hi + 1).byte as usize
    } else {
        rope.len()
    };
    // 与 `parse_block_window` 同式：裁掉窗口尾部的 '\n'（含末块 EOF 情形）——
    // 除非末块本身是 EOF 幻影空行（`byte == src.len()`），靠它保留那 1 个 '\n'。
    let mut end = end0;
    let last_is_phantom = hi + 1 >= n && index.blocks.at(hi).byte as usize == rope.len();
    if !last_is_phantom && end > start && rope.get_byte(end - 1) == Some(b'\n') {
        end -= 1;
    }
    let region = rope.slice(start..end).to_string();
    let rows = parse_to_rows(&region);
    debug_assert_eq!(rows.len(), hi - lo + 1, "窗口重解析行数必须与块数一致");
    let base_line = index.blocks.at(lo).src_line as usize;
    if base_line != 0 {
        let mut rows = rows;
        for r in &mut rows {
            r.src_line += base_line;
        }
        rows
    } else {
        rows
    }
}

/// 估算渲染行列表的**驻留内存**（近似）：返回（行数，约多少字节）。
///
/// 口径说明：
/// - 计入 `Row` 结构体本身（`size_of::<Row>()`）；
/// - 计入每个 `SharedString` 的**内容**（`len()` 字节）外加每块独立堆分配的
///   分配器开销（这里按 `+32 B/块` 粗估，实际 16~64 B 视分配器而定）；
/// - 计入内联 `runs` / `code_lines` / 表格 cell 的 Vec 容量与内容。
///
/// 不包含：`toc` / `code_scroll` / `table_scroll` 等辅助结构、文本整形/字体缓存。
/// 用于定位“解析结果把文档放大多少倍”（15 万行 ≈ 30MB+ 的量级来自此处）。
pub(crate) fn rows_memory_estimate(rows: &[Row]) -> (usize, usize) {
    /// 每个独立堆分配的近似分配器开销（head + 对齐 + 页头分摊）。
    const ALLOC_OVERHEAD: usize = 32;
    fn string_bytes(s: &SharedString) -> usize {
        ALLOC_OVERHEAD + s.len()
    }
    fn string_vec_bytes(v: &[SharedString]) -> usize {
        v.len() * std::mem::size_of::<SharedString>()
            + v.iter().map(string_bytes).sum::<usize>()
    }

    let mut bytes = 0usize;
    for r in rows {
        bytes += std::mem::size_of::<Row>();
        bytes += string_bytes(&r.text);
        if let Some(p) = &r.prefix {
            bytes += string_bytes(p);
        }
        bytes += r.runs.capacity() * std::mem::size_of::<(Range<usize>, RunKind)>();
        bytes += string_vec_bytes(r.code_lines());
        bytes += r
            .code_language()
            .map(|l| ALLOC_OVERHEAD + l.len())
            .unwrap_or(0);
        if let Some(t) = r.table() {
            bytes += std::mem::size_of::<TableData>();
            for h in &t.header {
                bytes += string_bytes(h);
            }
            for row in &t.rows {
                bytes += string_vec_bytes(row);
            }
            bytes += t.aligns.capacity() * std::mem::size_of::<TableAlign>();
        }
        if let Some(img) = r.image() {
            bytes += string_bytes(img.src);
            if img.img_data.is_some() {
                bytes += std::mem::size_of::<std::sync::Arc<Image>>();
            }
        }
    }
    (rows.len(), bytes)
}