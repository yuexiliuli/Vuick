//! 文档级轻量**块索引**：一次遍历产出整篇 `BlockMeta` 列表（≈20B/行，不复制文本）。
//!
//! 从 `parser`（行扫描器 + 块索引 + 引用索引 + 增量分析）拆出的「块索引结构 +
//! 全量扫描」部分（2026-09-24 重构）：依赖 `rows`（行解析的切块判定函数）；
//! 增量扫描 / 窗口物化见 `parser`（`blocks_scan`，经门面重导出）。

use gpui::SharedString;

use super::inline_style::blockquote_prefix;
use super::rows::{
    fence_len, inline_strip, is_rule, list_prefix, parse_image_line, strip_align_directive,
    try_parse_table, RowKind,
};

/// 一个渲染行的**轻量元数据**（只存行号/类型/几个长度，不复制文本）。
///
/// 供大文档「窗口级懒解析」使用：行号索引（二分定位源码行→渲染行）、
/// 平均行高估算、目录扫描都只依赖它，不需要物化整篇 [`Row`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMeta {
    /// 该渲染行覆盖的源码起始行（0 基，与 [`parse_to_rows`] 产出的 `Row.src_line` 一致）。
    pub src_line: u32,
    /// 块首行的**字节起始偏移**（原文坐标）：`parse_block_window` 直接用它切片到
    /// 块边界；增量扫描用它做尾部对齐与锚点定位——替代过去整篇 `line_starts` 数组
    /// （省内存，且增量扫描不再为建数组而全串扫一遍）。
    pub byte: u32,
    /// 块类型（与 [`parse_to_rows`] 产出的 `Row.kind` 一致）。
    pub kind: RowKind,
    /// 块首行的原始字节长度（估算用；表格/代码块按 `aux` 计数）。
    pub raw_len: u32,
    /// Code：代码行数；Table：数据行数；Image：1；其余：0。
    pub aux: u32,
}

/// 整篇文档的轻量块索引（≈20B/行，160k 行 ≈ 3.2MB，而整篇 `Vec<Row>` ≈ 25MB）。
#[derive(Debug, Clone)]
pub struct BlockIndex {
    /// 每个渲染行一个条目（按行序，`src_line`/`byte` 严格递增）。**分段持久化**：
    /// 增量扫描 O(1) 共享未变前后缀，只重建变化段（见 [`Blocks`]）。
    pub blocks: Blocks,
    /// 目录：(渲染行号, 标题级别, 标题文本)。窗口模式下照常全量可用。
    /// **分段持久化**（见 [`Toc`]）：增量扫描 O(1) 共享未变前缀/后缀、只重建变化
    /// 中段，后缀行号随编辑整体平移——不再每键重建扁平表（6.4MB/1.6 万标题实测
    /// ~1.7ms/键）；标题文本 `SharedString`（`Arc<str>`）克隆 O(1)。
    pub toc: Toc,
}

/// 分段持久化的轻量块列表。
///
/// 背景：增量扫描每轮都要产出「旧前缀 + 新中段 + 旧尾部(平移)」——用扁平 `Vec` 时
/// 前后缀每轮整体 memcpy（16 万块 ≈ 3.2MB ≈ 一两 ms，是 rope 化后每轮唯一 O(n)）。
/// 这里把 `blocks` 拆成**只读段**：段内是 `Arc<[BlockMeta]>`（不可变共享），段携带
/// 子区间与常量坐标平移，读写只发生在段边界：
/// - 前缀复用 = 对前段做 O(1) 子区间裁剪，零拷贝；
/// - 尾部复用 = 子区间裁剪 + 叠加 (byte_delta, line_shift) 平移（各块统一平移，可在读取时算）；
/// - 只重建「变化中段」（通常极小）。
///
/// 段数超 [`BLOCKS_MAX_SEGS`] 时合并且 amortize：合并是 O(n)，但每数轮才一次。
/// 用 `Arc`（非 `Rc`）→ `BlockIndex` 仍 `Send`，可整份交给后台线程继续增量（顺带把
/// 同步循环里 `Rc<BlockIndex> → 后台` 的整份拷贝从 O(n) 降到 O(1) 引用共享）。
#[derive(Clone, Debug)]
pub struct Blocks {
    segs: std::sync::Arc<Vec<BlocksSeg>>,
    len: usize,
}

/// 一段不可变块元数据（`[start,end)` 于 `data`），读取时施加常量坐标平移。
#[derive(Clone, Debug)]
pub struct BlocksSeg {
    data: std::sync::Arc<[BlockMeta]>,
    start: usize,
    end: usize,
    byte_delta: i64,
    line_shift: i64,
}

/// 段数上限：超过即合并成一段（amortize，读取仍是 O(段数)，段数很小）。
pub(crate) const BLOCKS_MAX_SEGS: usize = 8;

impl BlocksSeg {
    fn new(data: std::sync::Arc<[BlockMeta]>) -> Self {
        let len = data.len();
        Self { data, start: 0, end: len, byte_delta: 0, line_shift: 0 }
    }
    #[inline]
    fn len(&self) -> usize {
        self.end - self.start
    }
    /// 段内第 `i` 个块的**当前值**（已施加平移）。
    #[inline]
    fn at(&self, i: usize) -> BlockMeta {
        let mut b = self.data[self.start + i];
        b.byte = (b.byte as i64 + self.byte_delta).max(0) as u32;
        b.src_line = (b.src_line as i64 + self.line_shift).max(0) as u32;
        b
    }
    /// 段内子区间 `[s,e)`（O(1)，保持原平移）。
    fn sub(&self, s: usize, e: usize) -> Self {
        debug_assert!(s <= e && e <= self.len());
        Self { data: self.data.clone(), start: self.start + s, end: self.start + e, byte_delta: self.byte_delta, line_shift: self.line_shift }
    }
    /// 叠加常量平移（与已有平移合成：`(data.byte + 旧δ) + 新δ = data.byte + (旧δ+新δ)`）。
    fn shifted(&self, byte_delta: i64, line_shift: i64) -> Self {
        let mut seg = self.clone();
        seg.byte_delta += byte_delta;
        seg.line_shift += line_shift;
        seg
    }
    fn iter(&self) -> impl Iterator<Item = BlockMeta> + '_ {
        (0..self.len()).map(move |i| self.at(i))
    }
}

impl Blocks {
    /// 由完整 `Vec` 建立（全量扫描 / 合并）：原地转成单个共享段，之后裁剪零拷贝。
    pub fn from_vec(v: Vec<BlockMeta>) -> Self {
        let len = v.len();
        Self { segs: std::sync::Arc::new(vec![BlocksSeg::new(v.into())]), len }
    }

    /// 段数（读取路径的遍历代价，通常 ≤ [`BLOCKS_MAX_SEGS`]）。测试/基准观测用。
    #[cfg(test)]
    pub fn seg_count(&self) -> usize {
        self.segs.len()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 第 `i` 个块的当前值（O(段数) ≤ [`BLOCKS_MAX_SEGS`]）。
    #[inline]
    pub fn at(&self, mut i: usize) -> BlockMeta {
        debug_assert!(i < self.len, "blocks.at({i}) 越界 len={}", self.len);
        for seg in self.segs.iter() {
            let l = seg.len();
            if i < l {
                return seg.at(i);
            }
            i -= l;
        }
        unreachable!("blocks.at({i}) 越界 len={}", self.len)
    }

    pub fn get(&self, i: usize) -> Option<BlockMeta> {
        if i >= self.len {
            return None;
        }
        Some(self.at(i))
    }

    pub fn iter(&self) -> BlocksIter<'_> {
        BlocksIter { blocks: self, seg_i: 0, seg_off: 0 }
    }

    /// 二分定位：`blocks[..n]` 中最后一个满足 `pred` 的块下标 + 1（块 `byte` 严格递增，
    /// 平移保持单调，等价于原 `slice::partition_point`）。
    pub fn partition_point<P: Fn(&BlockMeta) -> bool>(&self, pred: P) -> usize {
        let mut lo = 0usize;
        let mut hi = self.len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let b = self.at(mid);
            if pred(&b) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// 增量组装：前缀（共享段）+ 中段（新扫描的 owned）+ 后缀（共享段）。段数超限合并且。
    pub(crate) fn assemble(prefix: Vec<BlocksSeg>, middle: Vec<BlockMeta>, suffix: Vec<BlocksSeg>) -> Self {
        let mut segs = prefix;
        if !middle.is_empty() {
            segs.push(BlocksSeg::new(middle.into()));
        }
        segs.extend(suffix);
        segs.retain(|s| s.len() > 0);
        let len = segs.iter().map(|s| s.len()).sum();
        if segs.len() > BLOCKS_MAX_SEGS {
            // amortize：合并成单一共享段（O(n) 一次，之后数轮裁剪零拷贝）
            let mut flat = Vec::with_capacity(len);
            for s in &segs {
                flat.extend(s.iter());
            }
            return Self { segs: std::sync::Arc::new(vec![BlocksSeg::new(flat.into())]), len };
        }
        Self { segs: std::sync::Arc::new(segs), len }
    }

    /// 取出前 `k` 个块的共享段（O(段数)）——增量扫描复用未变前缀，零拷贝。
    pub(crate) fn shares_prefix(&self, k: usize) -> Vec<BlocksSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            if rem == 0 {
                break;
            }
            let l = seg.len();
            if rem >= l {
                out.push(seg.clone());
                rem -= l;
            } else {
                out.push(seg.sub(0, rem));
                rem = 0;
            }
        }
        out
    }

    /// 取出从第 `k` 块起的共享段并叠加平移 `(byte_delta, line_shift)`——复用旧尾部。
    pub(crate) fn shares_suffix(&self, k: usize, byte_delta: i64, line_shift: i64) -> Vec<BlocksSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            let l = seg.len();
            if rem >= l {
                rem -= l;
                continue;
            }
            let seg = seg.sub(rem, l).shifted(byte_delta, line_shift);
            out.push(seg);
            rem = 0;
        }
        out
    }
}

impl PartialEq for Blocks {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}
impl Eq for Blocks {}

/// 按序产出每个块的当前值（值语义：平移段需即时计算，无法按引用借出）。
pub struct BlocksIter<'a> {
    blocks: &'a Blocks,
    seg_i: usize,
    seg_off: usize,
}
impl<'a> Iterator for BlocksIter<'a> {
    type Item = BlockMeta;
    fn next(&mut self) -> Option<BlockMeta> {
        while self.seg_i < self.blocks.segs.len() {
            let seg = &self.blocks.segs[self.seg_i];
            if self.seg_off < seg.len() {
                let b = seg.at(self.seg_off);
                self.seg_off += 1;
                return Some(b);
            }
            self.seg_i += 1;
            self.seg_off = 0;
        }
        None
    }
}

/// 目录（标题表）**分段持久化**：与 [`Blocks`] 同构——增量扫描只重建变化中段，
/// 未变前缀 O(1) 共享、后缀 O(1) 共享并叠加随编辑整体平移的 `line_shift`（读取时
/// 施加，不逐条复制）。标题文本用 [`SharedString`]（`Arc<str>`），经段共享后无需
/// 每键深拷。
///
/// 背景：预览每键 `scan_blocks_incremental_rope` 若重建扁平表，需逐条深拷贝并在
/// 行号上平移 1.6 万条标题（实测 ~0.85ms/键）——分段后每键只扫变化区（O(k)），
/// 前后缀零拷贝，预热成本平摊到 `assemble` 的合并（段数超限才 O(toc)）。目录行号
/// 按源码行升序严格递增（每个标题占一行），二分定位不依赖全表物化。
#[derive(Clone, Debug)]
pub struct Toc {
    segs: std::sync::Arc<Vec<TocSeg>>,
    len: usize,
}

/// 一段不可变标题元数据（`[start,end)` 于 `data`），读取时施加常量 `line_shift`。
#[derive(Clone, Debug)]
pub struct TocSeg {
    data: std::sync::Arc<[(usize, u8, SharedString)]>,
    start: usize,
    end: usize,
    line_shift: i64,
}

/// 目录段数上限（与 `blocks` 同一摊销策略：超限合并成单段，之后若干轮只裁剪）。
pub(crate) const TOC_MAX_SEGS: usize = 8;

impl TocSeg {
    fn new(data: std::sync::Arc<[(usize, u8, SharedString)]>) -> Self {
        let len = data.len();
        Self { data, start: 0, end: len, line_shift: 0 }
    }
    #[inline]
    fn len(&self) -> usize {
        self.end - self.start
    }
    /// 段内第 `i` 条标题的**当前值**（已施加 `line_shift`）。
    #[inline]
    fn at(&self, i: usize) -> (usize, u8, SharedString) {
        let (r, lv, t) = &self.data[self.start + i];
        ((*r as i64 + self.line_shift).max(0) as usize, *lv, t.clone())
    }
    fn sub(&self, s: usize, e: usize) -> Self {
        debug_assert!(s <= e && e <= self.len());
        Self { data: self.data.clone(), start: self.start + s, end: self.start + e, line_shift: self.line_shift }
    }
    fn shifted(&self, line_shift: i64) -> Self {
        let mut seg = self.clone();
        seg.line_shift += line_shift;
        seg
    }
}

impl Toc {
    /// 由完整条目建立（全量扫描 / 合并）：原地转成单个共享段，之后裁剪零拷贝。
    pub fn from_vec(v: Vec<(usize, u8, SharedString)>) -> Self {
        let len = v.len();
        Self { segs: std::sync::Arc::new(vec![TocSeg::new(v.into())]), len }
    }

    /// 目录行号按源码行升序（标题各占一行）：统计 `row < line` 的条目数（二分，
    /// 只读 `data` 原始行号 + 段内平移，不物化）。增量扫描用它按**行坐标**切前缀/
    /// 后缀（与旧扁平 `filter(r < anchor)` / `filter(r >= prev_line)` 等价）。
    pub(crate) fn count_before_line(&self, line: usize) -> usize {
        let mut total = 0usize;
        for seg in self.segs.iter() {
            let mut lo = seg.start;
            let mut hi = seg.end;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let (r, _, _) = &seg.data[mid];
                if (*r as i64 + seg.line_shift) < line as i64 {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            total += lo - seg.start;
        }
        total
    }

    #[allow(dead_code)] // 目录长度/物化面：预览引擎已删，仅 parse 测试消费
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> TocIter<'_> {
        TocIter { toc: self, seg_i: 0, seg_off: 0 }
    }

    /// 物化整表（消费方按需：侧边栏目录渲染时用，非每键路径）。
    #[allow(dead_code)] // 预览引擎已删，仅 parse 测试消费
    pub fn to_vec(&self) -> Vec<(usize, u8, SharedString)> {
        let mut v = Vec::with_capacity(self.len());
        for s in self.segs.iter() {
            for i in 0..s.len() {
                v.push(s.at(i));
            }
        }
        v
    }

    /// 取出前 `k` 条的共享段（O(段数)）——增量扫描复用未变前缀，零拷贝。
    pub(crate) fn shares_prefix(&self, k: usize) -> Vec<TocSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            if rem == 0 {
                break;
            }
            let l = seg.len();
            if rem >= l {
                out.push(seg.clone());
                rem -= l;
            } else {
                out.push(seg.sub(0, rem));
                rem = 0;
            }
        }
        out
    }

    /// 取出从第 `k` 条起的共享段并叠加平移 `line_shift`——复用旧后缀。
    pub(crate) fn shares_suffix(&self, k: usize, line_shift: i64) -> Vec<TocSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            let l = seg.len();
            if rem >= l {
                rem -= l;
                continue;
            }
            out.push(seg.sub(rem, l).shifted(line_shift));
            rem = 0;
        }
        out
    }

    /// 增量组装：前缀（共享段）+ 中段（新扫描的 owned）+ 后缀（共享段）。段数超限合并且。
    pub(crate) fn assemble(
        prefix: Vec<TocSeg>,
        middle: Vec<(usize, u8, SharedString)>,
        suffix: Vec<TocSeg>,
    ) -> Self {
        let mut segs = prefix;
        if !middle.is_empty() {
            segs.push(TocSeg::new(middle.into()));
        }
        segs.extend(suffix);
        segs.retain(|s| s.len() > 0);
        let len = segs.iter().map(|s| s.len()).sum();
        if segs.len() > TOC_MAX_SEGS {
            let mut flat = Vec::with_capacity(len);
            for s in &segs {
                for i in 0..s.len() {
                    flat.push(s.at(i));
                }
            }
            return Self { segs: std::sync::Arc::new(vec![TocSeg::new(flat.into())]), len };
        }
        Self { segs: std::sync::Arc::new(segs), len }
    }
}

impl Default for Toc {
    fn default() -> Self {
        Self { segs: std::sync::Arc::new(Vec::new()), len: 0 }
    }
}

impl PartialEq for Toc {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}
impl Eq for Toc {}

/// 按序产出每条标题的当前值（值语义：平移段需即时计算，无法按引用借出）。
pub struct TocIter<'a> {
    toc: &'a Toc,
    seg_i: usize,
    seg_off: usize,
}
impl<'a> Iterator for TocIter<'a> {
    type Item = (usize, u8, SharedString);
    fn next(&mut self) -> Option<(usize, u8, SharedString)> {
        while self.seg_i < self.toc.segs.len() {
            let seg = &self.toc.segs[self.seg_i];
            if self.seg_off < seg.len() {
                let t = seg.at(self.seg_off);
                self.seg_off += 1;
                return Some(t);
            }
            self.seg_i += 1;
            self.seg_off = 0;
        }
        None
    }
}

/// 行 → 字节起始偏移表（与 `build_line_starts(&str)` 语义相同，但存 u32 省内存）。
/// 每行起始字节偏移（n 行 → n 项；`split('\n')` 语义，含末段）。
/// `pub(crate)`：底座 `markdown::doc` 用它定位任意行的源字节区间。
pub(crate) fn build_line_starts_u32(src: &str) -> Vec<u32> {
    let mut starts = Vec::with_capacity(src.len() / 8 + 2);
    starts.push(0u32);
    for (i, &b) in src.as_bytes().iter().enumerate() {
        if b == b'\n' {
            starts.push((i + 1) as u32);
        }
    }
    starts
}

/// 一趟字节扫描同时产出 `Doc::open` 所需的四条行统计（替代旧四趟独立 O(n)）：
/// - `line_starts`（u32，`split('\n')` 语义，与 [`build_line_starts_u32`] 逐值相同）；
/// - `line_utf16_starts`（第 i 项 = `line_starts[i]` 前累计 UTF-16 单元数，含该行尾 `\n`）；
/// - `char_count`（`chars().count()` 口径）；
/// - `total_utf16`（`encode_utf16().count()` 口径）。
///
/// UTF-16 单元按 UTF-8 引导字节折算：1–3 字节字符 = 1 单元、4 字节字符 = 2 单元
/// （= `char::len_utf16`），ASCII 逐字节同长。BOM（3 字节）、`\r\n` 都按普通字符
/// 计入，与旧口径逐值一致（对拍测试 [`Doc`] 侧 `line_stats_fused_matches_old_multi_pass`）。
pub(crate) fn build_line_stats_utf16(bytes: &[u8]) -> (Vec<u32>, Vec<usize>, usize, usize) {
    let cap = bytes.len() / 8 + 2;
    let mut line_starts = Vec::with_capacity(cap);
    let mut line_utf16_starts = Vec::with_capacity(cap);
    line_starts.push(0u32);
    line_utf16_starts.push(0usize);
    let mut run_u16 = 0usize;
    let mut char_count = 0usize;
    let mut i = 0usize;
    let n = bytes.len();
    while i < n {
        let b = bytes[i];
        if b < 0x80 {
            char_count += 1;
            run_u16 += 1;
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
                line_utf16_starts.push(run_u16);
            }
            i += 1;
        } else {
            // 非 ASCII：按 UTF-8 引导字节推进（`&str` 保证合法 UTF-8；连续字节
            // 0x80..0xC0 不出现于字符首，按 1 推进防死循环）。
            let len = if b < 0xC0 {
                1
            } else if b < 0xE0 {
                2
            } else if b < 0xF0 {
                3
            } else {
                4
            };
            char_count += 1;
            run_u16 += if len == 4 { 2 } else { 1 };
            i += len;
        }
    }
    (line_starts, line_utf16_starts, char_count, run_u16)
}

/// 一个源码块的**切块判定**结果：类型 + 消费行数 + 辅助计数。
///
/// “切块判定”指「当前行属于哪种块、吃几行源码」：围栏 / 空行 / 标题 / 引述 /
/// 列表 / 分隔线 / 独立图片 / 表格 / 段落，判定顺序与 [`parse_to_rows`] 完全一致。
/// [`scan_blocks`] 与 `blocks_scan::scan_blocks_incremental_rope` 共用本函数，
/// 免去两份“与 scan_blocks 完全相同”的重复切块逻辑漂移；各调用方自行负责行级
/// 前置处理（EPUB `%%ALIGN:` 剥离与否）与字节游标推进。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockClass {
    pub(crate) kind: RowKind,
    /// 该块消费的源码行数（围栏含开门/关门行；表格含表头+分隔+数据行）。
    pub(crate) consumed: usize,
    /// Code：代码行数；Table：数据行数；Image：1；其余：0。
    pub(crate) aux: u32,
    /// 标题级别（非标题为 0），供调用方据此构建目录文本。
    pub(crate) hashes: u8,
}

pub(crate) fn classify_block(lines: &[&str], idx: usize, t: &str) -> BlockClass {
    if let Some((fc, _)) = fence_len(t) {
        let mut code_lines = 0u32;
        let mut j = idx + 1;
        while j < lines.len() {
            let l = lines[j].trim_end_matches('\r');
            if let Some((c, _)) = fence_len(l.trim_start())
                && c == fc {
                j += 1;
                break;
            }
            code_lines += 1;
            j += 1;
        }
        return BlockClass {
            kind: RowKind::Code,
            consumed: j - idx,
            aux: code_lines,
            hashes: 0,
        };
    }
    if t.is_empty() {
        return BlockClass {
            kind: RowKind::Blank,
            consumed: 1,
            aux: 0,
            hashes: 0,
        };
    }
    let hashes = t.chars().take_while(|&c| c == '#').count() as u8;
    if (1..=6).contains(&hashes) && t.len() > hashes as usize {
        return BlockClass {
            kind: RowKind::Heading(hashes),
            consumed: 1,
            aux: 0,
            hashes,
        };
    }
    if t.starts_with('>') {
        return classify_quote_region(lines, idx, t);
    }
    if list_prefix(t).is_some() {
        return BlockClass { kind: RowKind::Bullet, consumed: 1, aux: 0, hashes: 0 };
    }
    if is_rule(t) {
        return BlockClass { kind: RowKind::Rule, consumed: 1, aux: 0, hashes: 0 };
    }
    if parse_image_line(t).is_some() {
        return BlockClass { kind: RowKind::Image, consumed: 1, aux: 1, hashes: 0 };
    }
    if let Some((table, consumed)) = try_parse_table(lines, idx) {
        return BlockClass {
            kind: RowKind::Table,
            consumed,
            aux: table.rows.len() as u32,
            hashes: 0,
        };
    }
    BlockClass { kind: RowKind::Paragraph, consumed: 1, aux: 0, hashes: 0 }
}

/// 引号区：收集从 `idx` 起连续 `>` 行，逐行剥前缀+对齐指令+前导空白成 body
/// （与 [`super::rows::parse_quote_region`] 同口径）。返回（body 行切片，区长度）。
/// 供块索引**全量/增量扫描**一次收集、逐子块发射用（避免逐子块重扫整段 → O(L²)，
/// 2026-11-13 性能评审修复）。
pub(crate) fn quote_region_body<'a>(
    lines: &'a [&'a str],
    idx: usize,
) -> (Vec<&'a str>, usize) {
    let mut j = idx;
    while j < lines.len()
        && lines[j].trim_end_matches('\r').trim_start().starts_with('>')
    {
        j += 1;
    }
    let mut body: Vec<&str> = Vec::with_capacity(j - idx);
    for raw in &lines[idx..j] {
        let raw = raw.trim_end_matches('\r');
        match blockquote_prefix(raw) {
            Some(p) => {
                let b = raw[p..].trim_start();
                let (_, b) = strip_align_directive(b);
                body.push(b);
            }
            None => body.push(raw),
        }
    }
    (body, j - idx)
}

/// 引号区内 `body[k]` 起单个（子）块的分类（对齐 [`classify_quote_region`] 首块
/// 语义 + TOC 写入）：残留 `>` 按单行引述；段落/空行降级 [`RowKind::Quote`]；
/// 标题把目录文本写入 `toc`（body 已剥 `>` 前缀）。
pub(crate) fn classify_quote_line(
    body: &[&str],
    k: usize,
    build_toc: bool,
    row_start: usize,
    toc: &mut Vec<(usize, u8, SharedString)>,
) -> BlockClass {
    let Some(bk) = body.get(k) else {
        return BlockClass { kind: RowKind::Quote, consumed: 1, aux: 0, hashes: 0 };
    };
    let t0 = bk.trim_start();
    // 残留 `>`（双空格剥后仍以 `>` 开头）：按单行引述
    if t0.starts_with('>') {
        return BlockClass { kind: RowKind::Quote, consumed: 1, aux: 0, hashes: 0 };
    }
    let mut c = classify_block(body, k, t0);
    // 引号区内：段落/空行降级为 Quote（保持引号灰带连续，与 rows 侧一致）
    match c.kind {
        RowKind::Paragraph | RowKind::Blank => {
            c.kind = RowKind::Quote;
            c.consumed = 1;
            c.aux = 0;
            c.hashes = 0;
        }
        _ => {}
    }
    if build_toc
        && let RowKind::Heading(h) = c.kind
    {
        let text = inline_strip(t0[h as usize..].trim_start());
        toc.push((row_start, h, text));
    }
    c
}

/// 引号区：收集连续 `>` 行，剥前缀后对**首个子块**分类（并列 [`classify_block`]）。
/// 与 [`super::rows::parse_quote_region`] 的块边界 / consumed 严格一致（同款
/// 判定顺序与剥前缀逻辑），保证窗口物化不变式 `rows.len() == hi-lo+1` 与
/// 增量/全量一致（2026-11-13）。
fn classify_quote_region(lines: &[&str], idx: usize, _t: &str) -> BlockClass {
    let (body, _) = quote_region_body(lines, idx);
    if body.is_empty() {
        return BlockClass { kind: RowKind::Quote, consumed: 1, aux: 0, hashes: 0 };
    }
    let mut dummy_toc = Vec::new();
    classify_quote_line(&body, 0, false, idx, &mut dummy_toc)
}

/// 一次遍历产出整篇轻量块索引（不复制文本，不建 [`Row`]）。
///
/// 判定顺序与 [`parse_to_rows`] 完全一致（围栏 / 空行 / 标题 / 引述 / 列表 /
/// 分隔线 / 独立图片 / 表格 / 段落），并复用同一切块判定函数（`fence_len`、
/// `list_prefix`、`is_rule`、`parse_image_line`、`try_parse_table`），
/// 因此块边界与消耗行数与全量解析天然一致（有测试背书）。
pub fn scan_blocks(src: &str) -> BlockIndex {
    let _t = crate::prof::begin();
    let line_starts = build_line_starts_u32(src);
    crate::prof::end("scan.blocks.line_starts", _t);
    let _t = crate::prof::begin();
    let lines: Vec<&str> = src.split('\n').collect();
    crate::prof::end("scan.blocks.split", _t);
    scan_blocks_with_line_starts(&lines, &line_starts, true)
}

/// [`scan_blocks`] 的复用行起始版：块判定逻辑完全一致，仅不重复计算行起始表与
/// `split`——输入是已按 `split('\n')` 切好的行切片（`lines[i]` 不含尾 `\n`，
/// `line_starts[i]` 为该行字节起始）。字符串源由 [`scan_blocks`] 切好传入；rope 源
///（WYSIWYG `Doc::open_rope` 冷开，零整份拷贝）由调用方按 chunk 装配行切片后传入。
///
/// `build_toc=false` 时目录恒空：WYSIWYG `Doc` 层从不消费 `index.toc`（侧边栏目录
/// 由预览侧 `WyEditor::toc_entries()` 独立构建），整表目录重建是大文档打开的真热点，
/// 跳过归零；需要正确目录的调用方（parse 测试 / 未来目录消费）传 `true`。
pub(crate) fn scan_blocks_with_line_starts(
    lines: &[&str],
    line_starts: &[u32],
    build_toc: bool,
) -> BlockIndex {
    let _t = crate::prof::begin();
    let mut blocks: Vec<BlockMeta> = Vec::with_capacity(lines.len());
    let mut toc: Vec<(usize, u8, SharedString)> = Vec::new();
    let mut idx = 0usize;
    while idx < lines.len() {
        let row_start = idx;
        let raw_line = lines[idx].trim_end_matches('\r');
        let raw_len = raw_line.len() as u32;
        let byte = line_starts[row_start];
        // 与 parse_to_rows 一致的 EPUB 对齐指令剥离（kind 判定对同一份内容做）。
        let (_, line) = strip_align_directive(raw_line);
        let t = line.trim_start();
        if t.starts_with('>') {
            // 引号区：一次收集 body、逐子块发射，idx 直接跳过整区
            //（避免单行子块每次重扫整段 → O(L²)，2026-11-13 性能评审修复）。
            let (body, region_len) = quote_region_body(lines, idx);
            let mut k = 0usize;
            while k < body.len() {
                let sub_row = row_start + k;
                let c = classify_quote_line(&body, k, build_toc, sub_row, &mut toc);
                let sub_raw = lines[sub_row].trim_end_matches('\r');
                blocks.push(BlockMeta {
                    src_line: sub_row as u32,
                    byte: line_starts[sub_row],
                    kind: c.kind,
                    raw_len: if c.kind == RowKind::Blank {
                        0
                    } else {
                        sub_raw.len() as u32
                    },
                    aux: c.aux,
                });
                k += c.consumed.max(1);
            }
            idx += region_len;
            continue;
        }
        let c = classify_block(lines, idx, t);
        if build_toc
            && let RowKind::Heading(h) = c.kind
        {
            // 引号区内标题：目录文本取剥前缀后的 body（`>` 前缀不落入 TOC）。
            let tt: &str = if t.starts_with('>') {
                blockquote_prefix(t).map(|p| t[p..].trim_start()).unwrap_or(t)
            } else {
                t
            };
            let _t = crate::prof::begin();
            let text = inline_strip(tt[h as usize..].trim_start());
            toc.push((row_start, h, text));
            crate::prof::end("scan.blocks.toc", _t);
        }
        blocks.push(BlockMeta {
            src_line: row_start as u32,
            byte,
            kind: c.kind,
            raw_len: if c.kind == RowKind::Blank { 0 } else { raw_len },
            aux: c.aux,
        });
        idx += c.consumed;
    }
    crate::prof::end("scan.blocks.classify", _t);
    BlockIndex {
        blocks: Blocks::from_vec(blocks),
        toc: Toc::from_vec(toc),
    }
}