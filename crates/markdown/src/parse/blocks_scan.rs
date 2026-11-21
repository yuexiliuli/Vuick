//! `BlockIndex` 在 **Rope / 字符串** 上的「全量 / 增量」扫描与**窗口物化**引擎。
//!
//! 从 `parser`（行扫描器 + 块索引 + 引用索引 + 增量分析）拆出的「增量（rope）扫描 +
//! 窗口物化」部分（2026-09-24 重构）：依赖 `rows`（行解析）与 `blocks`（块索引结构）；
//! `#[cfg(test)]` 的 str 版正确性参照（`common_prefix_bytes` / `scan_blocks_incremental`）
//! 保留在 `parser`（测试基础设施），本模块只保留生产路径。

// 块扫描内核逐游标传参 → 放行
#![allow(clippy::too_many_arguments)]

use std::ops::Range;

use gpui::{Image, SharedString};
use ropey::{LineType, Rope};

use super::blocks::{
    classify_block, classify_quote_line, quote_region_body, scan_blocks, BlockIndex, BlockMeta,
    Blocks, BlocksSeg, Toc, TocSeg,
};
use super::rows::{
    fence_len, inline_strip, parse_to_rows, Row, RowKind, RunKind, TableAlign, TableData,
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

/// —— S2 巨代码块 in-place 增量 ——
///
/// 通用增量路径锚点在「块首」：围栏是单个 `Code` 块，编辑点落在围栏内部时，锚点仍
/// 是开门行 → region 从块首切片到尾部对齐点（整段≤文末）→ 物化 + split + 重 classify
/// 一遍 6MB 围栏（debug 实测 ~37ms/键）。这里给出**围栏内编辑的专用捷径**：锚点块
/// 是 Code 围栏且编辑点所在变化区不扰动围栏结构时，只按「编辑点净行差」平移
/// 围栏块的 `aux`，前缀/后缀块与目录 O(段数) 共享平移——整段不再物化、不再重 classify。
///
/// 判定（任一不满足 → `None` 走通用慢速路径，正确性由全量对拍兜底）：
/// 1. 锚点块 [RowKind::Code] 且其开门行确为围栏（取开门围栏字符 `open_char`）；
/// 2. 变化字节区间（新旧两侧，各自扩到覆盖完整行）内**不存在**与开门**同字符**的
///    ≥3 围栏行——`classify_block` 的关门判定只比围栏字符、不比长度（开门 3 反引号
///    会被 4 反引号行提前关闭），故开门/关门行被编辑、内部新成/毁围栏、删除换行导致
///    合并行成围栏，都会被这里的整行检查拒掉（保守回退）。此条件等价于「开门到关门
///    之间第一个同字符围栏行（即关门行）的相对位置不变」：变化区外所有行字节相同 ⇒
///    围栏结构逐块不变。
///
/// 命中时产物与全量扫描逐块一致（有对拍回归测试）：围栏块仅 `aux` 平移净行差，
/// 后缀块/目录整体平移 `(delta, delta_lines)`（与通用路径尾部复用同式）。
/// `#[cfg(test)]` 计数供回归观察：巨块内编辑应命中本路径而非整段重扫（不断言时间）。
#[cfg(test)]
pub(crate) static INPLACE_CODE_SCAN_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// 字节区间 `[lo, hi)` 覆盖的「整行」字节跨度（`[s, e)`，s/e 都是行首字节）。
/// 空区间（lo == hi，如纯删除/纯插入的另一侧）也覆盖 lo 所在的一行——检查合并/分裂
/// 点本身；lo 已到 EOF（无内容）时返回空（`[n, n)`）。
fn whole_line_byte_span(rope: &Rope, lo: usize, hi: usize) -> (usize, usize) {
    let n = rope.len();
    let lo = lo.min(n);
    let hi = hi.min(n).max(lo);
    if lo >= n {
        return (n, n);
    }
    let line_lo = rope.byte_to_line_idx(lo, LineType::LF);
    let last = if hi > lo { hi - 1 } else { lo };
    let line_last = rope.byte_to_line_idx(last, LineType::LF);
    let s = rope.line_to_byte_idx(line_lo, LineType::LF);
    let e = rope.line_to_byte_idx(line_last + 1, LineType::LF);
    (s, e)
}

/// `[s, e)`（已对齐整行）：任一行 `fence_len(trim_start)` 的围栏字符与开门 `open_char`
/// 相同即「可能改变围栏结构」——判定口径与 `classify_block` 的关门判定逐字一致：
/// 关门**只比围栏字符** `~`/`` ` ``、**不比长度**（开门 3 反引号的围栏会被 4 反引号行
/// 提前关闭），因此任何 ≥3 同字符围栏行都必须拒掉，保守回退通用路径。
fn span_has_fc_fence_line(rope: &Rope, s: usize, e: usize, open_char: char) -> bool {
    if s >= e || s >= rope.len() {
        return false;
    }
    let text = rope.slice(s..e).to_string();
    for raw in text.split('\n') {
        let t = raw.trim_end_matches('\r').trim_start();
        if let Some((c, _)) = fence_len(t)
            && c == open_char
        {
            return true;
        }
    }
    false
}

/// S2 巨代码块 in-place 扫描：见模块头注释。返回 `None` 表示判定不命中（走通用路径）。
fn try_code_block_inplace_scan(
    prev_src: &Rope,
    prev: &BlockIndex,
    new_src: &Rope,
    b_first: usize,
    tail_start_new: usize,
    delta: i64,
    bi: usize,
    rebuild_toc: bool,
) -> Option<BlockIndex> {
    let block = prev.blocks.at(bi);
    if block.kind != RowKind::Code {
        return None;
    }
    let open_byte = block.byte as usize;
    let open_line = block.src_line as usize;
    // 开门行文本（读旧 rope；变化前字节逐位相同，与新版一致）。
    let open_line_no = prev_src.byte_to_line_idx(open_byte, LineType::LF);
    let open_end = prev_src.line_to_byte_idx(open_line_no + 1, LineType::LF);
    let open_text = prev_src.slice(open_byte..open_end).to_string();
    let (open_char, _) = fence_len(open_text.trim_end_matches('\r').trim_start())?;

    // 变化字节区间（旧/新坐标系）：前缀/后缀字节相同 ⇒ 区间 [b_first, tail) 精确覆盖
    // 被改内容（删/插/替换统一；`tail_start_new - delta` 把尾部映射回旧坐标）。
    // 端点一律裁到字符边界（真实 dirty 通道恒为字符边界；膨胀 dirty 兜底防 slice 越界）
    // ——裁入的整字符来自未变内容，不改变围栏行判定结果。
    let old_lo = prev_src.floor_char_boundary(b_first.min(prev_src.len()));
    let new_lo = new_src.floor_char_boundary(b_first.min(new_src.len()));
    let old_hi = prev_src
        .ceil_char_boundary(((tail_start_new as i64 - delta).max(0) as usize).min(prev_src.len()))
        .max(old_lo);
    let new_hi = new_src
        .ceil_char_boundary(tail_start_new.min(new_src.len()))
        .max(new_lo);

    // 围栏结构不变量：两侧「变化区覆盖整行」都不得出现开门同字符围栏行（见模块头）。
    let (os, oe) = whole_line_byte_span(prev_src, old_lo, old_hi);
    let (ns, ne) = whole_line_byte_span(new_src, new_lo, new_hi);
    if span_has_fc_fence_line(prev_src, os, oe, open_char)
        || span_has_fc_fence_line(new_src, ns, ne, open_char)
    {
        return None;
    }

    // 行数平移量（围栏块 aux 与后缀/目录行号都按它平）：「未变后缀起点」的新旧行号差
    // ——即使 dirty 尾部被「尾随相同字节」（如共用尾 `\n`）裁掉，行号差仍精确等于编辑
    // 引入/移除的净行数（区域换行差会被共用尾 `\n` 漏算，已实测 + 对拍修正）。
    let old_tail = ((tail_start_new as i64 - delta).max(0) as usize).min(prev_src.len());
    let new_tail = tail_start_new.min(new_src.len());
    let delta_lines = new_src.byte_to_line_idx(new_tail, LineType::LF) as i64
        - prev_src.byte_to_line_idx(old_tail, LineType::LF) as i64;

    // 锚点块：仅 aux 平移；byte/src_line/kind/raw_len 皆不变（开门行在变化前、未动）。
    let mut mid = prev.blocks.at(bi);
    mid.aux = (mid.aux as i64 + delta_lines).max(0) as u32;

    // 前缀块共享段不变；后缀块（围栏之后）整体平移 (delta, delta_lines)。
    let prefix = prev.blocks.shares_prefix(bi);
    let suffix = prev.blocks.shares_suffix(bi + 1, delta, delta_lines);

    // 目录：标题不可能出现在围栏内部——前缀取开门行之前；后缀经「关门行后第一行」
    // 切出（未闭围栏无后缀块 ⇒ 目录后缀也空）。
    let toc = if rebuild_toc {
        let prefix_n = prev.toc.count_before_line(open_line);
        let suffix_n = if bi + 1 < prev.blocks.len() {
            prev.toc.count_before_line(open_line + block.aux as usize + 2)
        } else {
            prev.toc.len()
        };
        let tp = prev.toc.shares_prefix(prefix_n);
        let ts = prev.toc.shares_suffix(suffix_n, delta_lines);
        Toc::assemble(tp, Vec::new(), ts)
    } else {
        Toc::default()
    };

    Some(BlockIndex {
        blocks: Blocks::assemble(prefix, vec![mid], suffix),
        toc,
    })
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
#[allow(dead_code)] // 增量窗口扫描面：预览引擎已删，仅 parse 测试消费
pub fn scan_blocks_incremental_rope(
    prev_src: &Rope,
    prev: &BlockIndex,
    new_src: &Rope,
    dirty: Option<Range<usize>>,
) -> BlockIndex {
    scan_blocks_incremental_rope_impl(prev_src, prev, new_src, dirty, true)
}

/// [`scan_blocks_incremental_rope`] 的裁减版（WYSIWYG `Doc::edit` 每键专用）：
/// 只重扫/装配 `blocks`，**跳过整表目录重建**。
///
/// WYSIWYG 编辑路径从不消费 `index.toc`（目录/侧边栏由预览侧独立索引提供），而
/// 目录重建是全表逐条深克隆（6.4MB/1.6 万标题文档实测 ~1.7ms/键；文本 `SharedString`
/// 化后仍 ~0.7ms/键）——跳过后 `doc.scanblocks` 从 ~1.7ms 落到 ~0.03ms（仅区域切块）。
/// 返回的 `BlockIndex.toc` 为**空**（消费方不读）；需要正确目录的业务必须走完整版。
/// 对齐假设与兜底逻辑与完整版一致（兜底全量扫描会连目录一起重建，保证正确性）。
pub(crate) fn scan_blocks_incremental_rope_blocks_only(
    prev_src: &Rope,
    prev: &BlockIndex,
    new_src: &Rope,
    dirty: Option<Range<usize>>,
) -> BlockIndex {
    scan_blocks_incremental_rope_impl(prev_src, prev, new_src, dirty, false)
}

fn scan_blocks_incremental_rope_impl(
    prev_src: &Rope,
    prev: &BlockIndex,
    new_src: &Rope,
    dirty: Option<Range<usize>>,
    rebuild_toc: bool,
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

    // —— S2 巨代码块 in-place：锚点块是 Code 围栏且编辑点不扰动围栏结构时，只按
    // 编辑点净行差平移 `aux`，前缀/后缀共享段零拷贝，不再「块首→文末」整段
    // 物化 + 重 classify（6MB 单围栏实测 ~37ms/键 → 亚毫秒）。不命中则继续通用路径。
    if let Some(idx) = try_code_block_inplace_scan(
        prev_src,
        prev,
        new_src,
        b_first,
        tail_start_new,
        delta,
        bi,
        rebuild_toc,
    ) {
        #[cfg(test)]
        INPLACE_CODE_SCAN_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return idx;
    }

    // 未变前缀直接沿用；toc 同样取前缀（与 str 版相同的「源码行 vs 块下标」坐标
    // 修正：按重扫起点 `anchor` 行号过滤，而非块下标 `bi`）。
    let prefix_segs = prev.blocks.shares_prefix(bi);
    let mut blocks: Vec<BlockMeta> = Vec::new(); // 新扫描的变化中段（通常极小）
    let mut suffix_segs: Vec<BlocksSeg> = Vec::new();
    // 目录分段：未变前缀共享（行号 < 重扫起点 `anchor`）；变化中段由区域扫描产出；
    // 后缀在复用旧尾部时按 `prev_line` 切出并叠加 `line_shift`（读取时施加）。相比
    // 原先整表深克隆 + 逐条平移（1.6 万标题 ≈0.85ms/键），此时 O(段数) 共享零拷贝。
    let _t_tp = crate::prof::begin();
    let toc_prefix_segs = if rebuild_toc {
        prev.toc.shares_prefix(prev.toc.count_before_line(anchor))
    } else {
        Vec::new()
    };
    let mut toc_mid: Vec<(usize, u8, SharedString)> = Vec::new();
    let mut toc_suffix_segs: Vec<TocSeg> = Vec::new();
    crate::prof::end("ss.toc.prefix", _t_tp);

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
    // 尾部对齐点先 floor 到字符边界再切片（真实 dirty 通道恒为字符边界，但膨胀/兜底
    // 路径可能落入多字节字符中间；与 `try_code_block_inplace_scan` 同姿势，P1-12）。
    let region_end = new_src.floor_char_boundary(region_end);
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
                if let Some(pb) = prev.blocks.get(pii)
                    && pb.byte as usize == prev_off {
                        let prev_line = pb.src_line as usize;
                        let line_shift = (anchor + idx) as i64 - prev_line as i64;
                        suffix_segs = prev.blocks.shares_suffix(pii, delta, line_shift);
                        let _t_ts = crate::prof::begin();
                        if rebuild_toc {
                            toc_suffix_segs =
                                prev.toc.shares_suffix(prev.toc.count_before_line(prev_line), line_shift);
                        }
                        crate::prof::end("ss.toc.suffix", _t_ts);
                        reused = true;
                        break;
                    }
            }
        }

        // —— 否则与 `scan_blocks` 相同的切块逻辑（逐块消费 `idx`）——
        let row_start = anchor + idx; // 文档绝对行号
        let line = new_lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        let raw_len = line.len() as u32;
        let blk_byte = (anchor_byte as u32).wrapping_add(cur_byte); // 文档绝对字节
        if t.starts_with('>') {
            // 引号区：一次收集、逐子块发射（O(L) 而非 O(L²)，2026-11-13 性能评审
            // 修复；语义与 `scan_blocks_with_line_starts` 引号分支一致）。
            let (body, region_len) = quote_region_body(&new_lines, idx);
            let mut k = 0usize;
            let mut sub_cur = cur_byte;
            while k < body.len() {
                let sub_row = row_start + k;
                let c = classify_quote_line(&body, k, rebuild_toc, sub_row, &mut toc_mid);
                let sub_line = new_lines[idx + k].trim_end_matches('\r');
                let sub_blk_byte = (anchor_byte as u32).wrapping_add(sub_cur);
                blocks.push(BlockMeta {
                    src_line: sub_row as u32,
                    byte: sub_blk_byte,
                    kind: c.kind,
                    raw_len: if c.kind == RowKind::Blank {
                        0
                    } else {
                        sub_line.len() as u32
                    },
                    aux: c.aux,
                });
                let consumed = c.consumed.max(1);
                for kk in 0..consumed {
                    sub_cur = next_line_byte(&new_lines, sub_cur, idx + k + kk, region_bytes);
                }
                k += consumed;
            }
            for kk in idx..idx + region_len {
                cur_byte = next_line_byte(&new_lines, cur_byte, kk, region_bytes);
            }
            idx += region_len;
            continue;
        }
        let c = classify_block(&new_lines, idx, t);
        if rebuild_toc
            && let RowKind::Heading(h) = c.kind
        {
            let text = inline_strip(t[h as usize..].trim_start());
            toc_mid.push((row_start, h, text));
        }
        blocks.push(BlockMeta {
            src_line: row_start as u32,
            byte: blk_byte,
            kind: c.kind,
            raw_len: if c.kind == RowKind::Blank { 0 } else { raw_len },
            aux: c.aux,
        });
        for k in idx..idx + c.consumed {
            cur_byte = next_line_byte(&new_lines, cur_byte, k, region_bytes);
        }
        idx += c.consumed;
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
    let toc = if rebuild_toc {
        Toc::assemble(toc_prefix_segs, toc_mid, toc_suffix_segs)
    } else {
        Toc::default()
    };
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
/// `#[allow(dead_code)]`：preview 迁移到 `PreviewRow`（M5-B5 P3-1a）后仅本模块测试消费
/// （`view` 用其 `PreviewRow` 同口径变体 `preview_rows_memory_estimate`）。
#[allow(dead_code)]
pub(crate) fn rows_memory_estimate(rows: &[Row]) -> (usize, usize) {
    /// 每个独立堆分配的近似分配器开销（head + 对齐 + 页头分摊）。
    const ALLOC_OVERHEAD: usize = 32;
    fn string_bytes(s: &SharedString) -> usize {
        ALLOC_OVERHEAD + s.len()
    }
    fn string_vec_bytes(v: &[SharedString]) -> usize {
        std::mem::size_of_val(v)
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