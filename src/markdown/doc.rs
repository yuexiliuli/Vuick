//! 「强大底座」第一层：**共享文档模型**（markdown::doc）。
//!
//! 目标（用户路线）：以本项目的 Markdown 预览底座（行级虚拟化、窗口化块索引、恒定内存）
//! 为基干，抽象出一个 preview 与 live（WYSIWYG）都从其上衍生的统一文档层：
//! - **语义** = [`ir::IrKind`]（`markdown::ir`，金标准）；
//! - **结构索引** = [`parse::BlockIndex`]（`scan_blocks`/`scan_blocks_rope`，
//!   O(1) 打开、窗口物化，与预览同一增量机制）；
//! - **行物化** = [`parse::parse_block_window`(_rope)]（与预览**同一份**
//!   文本/行内段，保证两分支渲染一致）。
//!
//! [`DocRow`] 是两分支的**统一行**：合并「IR 语义 + 预览文本 + 行号 + 行区间」。
//! 源支持 `Str`（小文档/测试）与 `Rope`（大文档，与编辑器 COW 共享底层，窗口化
//! 物化不整份持有——预览的性能底座）。后续阶段叠加编辑映射并让 preview/live
//! 迁移到本层消费；一致性由 `markdown::parity` 钉死。

use std::cell::RefCell;
use std::ops::Range;

use gpui::SharedString;
use ropey::Rope;

use super::ir::IrKind;
use super::parse::{
    parse_block_window, parse_block_window_rope, BlockMeta, Row, RowKind, RunKind, TableData,
};
use super::parse;

/// 块级渲染负载（非纯文本块的渲染数据，统一行 [`DocRow`] 的伴生字段）。
///
/// Code/Table/Image 等块在预览侧重于 `Row.payload`（代码全部行 / 表格网格 / 图片源），
/// WYSIWYG 也需要这些才画得出来——统一行把它们一并带过来，避免渲染分支丢数据。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum DocPayload {
    #[default]
    Text,
    /// 代码围栏：全部代码行 + 语言标记（用于等宽排版/背景面板）。
    Code {
        code_lines: Vec<SharedString>,
        language: Option<SharedString>,
    },
    /// 独立图片行：图片源。
    Image { src: SharedString },
    /// GFM 表格：网格数据。
    Table(TableData),
}

/// 表格可编辑行的源码映射（逐可见行：表头 + 各数据行；分隔行不渲染不编辑）。
/// 供元素层命中/光标/选区把「单元格渲染位置 ↔ 源字节」换算。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableRowMap {
    /// 该行源码行号。
    pub src_line: usize,
    /// 逐单元格。
    pub cells: Vec<TableCellMap>,
}

/// 单个表格单元格的编辑映射。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableCellMap {
    /// 源码内容区间（**相对该行行首**；`\|` 转义按源字节计入，哨兵指到内容物理尾）。
    pub src: Range<usize>,
    /// 渲染文本（与预览单元格同源，已解义/行内清理）。
    pub text: SharedString,
    /// 渲染字节 → 源字节（相对该行行首）；长度 = `text.len()+1`。
    pub map: Vec<usize>,
}

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
    /// 编辑映射：渲染字节 → **源字节偏移**。
    /// - 文本行（段落/标题/列表/引述）：相对**该行**行首，长度 = `text.len()+1`，
    ///   末项为哨兵 = 源段尾；由 `parse::text_segment` + `parse::inline_style_mapped`
    ///   装配（与预览同一段派生）。
    /// - 代码块（围栏内内容行可编辑）：相对**块首**（`src_line` 围栏行起始），
    ///   渲染文本 = 各代码行以 `\n` 连接（围栏标记本身只读不渲染）。
    /// - 其它非文本行（表格/图片/规则/空行）为空。
    pub(crate) byte_map: Vec<usize>,
    /// 块级渲染负载（非文本块的渲染数据）。
    pub(crate) payload: DocPayload,
    /// 表格块的逐行/单元格编辑映射（仅 [`IrKind::Table`] 行非 `Some`；代码块用
    /// `byte_map`，表格因单元格多列需要更细粒度）。`Rc` 压低克隆成本。
    pub(crate) table_rows: Option<std::rc::Rc<Vec<TableRowMap>>>,
}

/// 文档源：`Str`（小文档）或 `Rope`（大文档，COW，与编辑器共享底层树）。
enum DocSrc {
    Str(SharedString),
    Rope(Rope),
}

/// 行起始表（`line_starts` / `line_utf16_starts` 共用）的**分段持久化**。
///
/// 与 [`parse::Blocks`] 同构：增量编辑只重建变化中段，未变前缀/后缀用 `Arc` 共享、
/// 读取时叠加常量平移 `shift`（不逐条复制）。背景：Enter/换行路径的旧实现在每键
/// 「前缀拷贝 + 尾部逐条平移」两个 O(n) 段（139k 行 ×2 索引实测 3.2~3.7ms/键），
/// 分段后只需重扫变化区（通常极小），前后缀 O(1) 共享；段数超限合并且 amortize。
#[derive(Clone, Debug)]
pub(crate) struct LineStarts {
    segs: std::sync::Arc<Vec<LineSeg>>,
    len: usize,
}

/// 一段不可变行起始（`[start,end)` 于 `data`），读取时施加常量坐标平移。
#[derive(Clone, Debug)]
pub(crate) struct LineSeg {
    data: std::sync::Arc<[usize]>,
    start: usize,
    end: usize,
    shift: i64,
}

/// 段数上限：超过即合并成一段（amortize，读取仍是 O(段数)，段数很小）。
pub(crate) const LINE_STARTS_MAX_SEGS: usize = 8;

impl LineSeg {
    fn new(data: std::sync::Arc<[usize]>) -> Self {
        let len = data.len();
        Self { data, start: 0, end: len, shift: 0 }
    }
    #[inline]
    fn len(&self) -> usize {
        self.end - self.start
    }
    /// 段内第 `i` 个行起始的**当前值**（已施加平移）。
    #[inline]
    fn at(&self, i: usize) -> usize {
        ((self.data[self.start + i] as i64 + self.shift).max(0)) as usize
    }
    /// 段内子区间 `[s,e)`（O(1)，保持原平移）。
    fn sub(&self, s: usize, e: usize) -> Self {
        debug_assert!(s <= e && e <= self.len());
        Self {
            data: self.data.clone(),
            start: self.start + s,
            end: self.start + e,
            shift: self.shift,
        }
    }
    /// 叠加常量平移（与已有平移合成：旧δ + 新δ）。
    fn shifted(&self, delta: i64) -> Self {
        let mut seg = self.clone();
        seg.shift += delta;
        seg
    }
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len()).map(move |i| self.at(i))
    }
}

impl LineStarts {
    /// 由完整 `Vec` 建立（全量扫描 / 合并）：原地转成单个共享段，之后裁剪零拷贝。
    fn from_vec(v: Vec<usize>) -> Self {
        let len = v.len();
        Self { segs: std::sync::Arc::new(vec![LineSeg::new(v.into())]), len }
    }

    /// 段数（读取路径的遍历代价，通常 ≤ [`LINE_STARTS_MAX_SEGS`]）。测试观测用。
    #[cfg(test)]
    pub(crate) fn seg_count(&self) -> usize {
        self.segs.len()
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline]
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 第 `i` 个行起始（O(段数) ≤ [`LINE_STARTS_MAX_SEGS`]）。
    #[inline]
    fn at(&self, mut i: usize) -> usize {
        debug_assert!(i < self.len, "line_starts.at({i}) 越界 len={}", self.len);
        for seg in self.segs.iter() {
            let l = seg.len();
            if i < l {
                return seg.at(i);
            }
            i -= l;
        }
        unreachable!("line_starts.at({i}) 越界 len={}", self.len)
    }

    #[inline]
    pub(crate) fn get(&self, i: usize) -> Option<usize> {
        if i >= self.len {
            return None;
        }
        Some(self.at(i))
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.segs.iter().flat_map(|s| s.iter())
    }

    /// 物化成平坦 `Vec`（测试/一致性校验用）。
    #[cfg(test)]
    pub(crate) fn to_vec(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(self.len);
        v.extend(self.iter());
        v
    }

    /// 二分定位：`[..n]` 中最后一个满足 `pred` 的下标 + 1（行起始升序、平移保持单调，
    /// 等价于原 `slice::partition_point`）。
    pub(crate) fn partition_point<P: FnMut(&usize) -> bool>(&self, mut pred: P) -> usize {
        let mut lo = 0usize;
        let mut hi = self.len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let v = self.at(mid);
            if pred(&v) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// 增量组装：前缀（共享段）+ 中段（新扫描的 owned）+ 后缀（共享段）。段数超限合并且。
    fn assemble(prefix: Vec<LineSeg>, middle: Vec<usize>, suffix: Vec<LineSeg>) -> Self {
        let mut segs = prefix;
        if !middle.is_empty() {
            segs.push(LineSeg::new(middle.into()));
        }
        segs.extend(suffix);
        segs.retain(|s| s.len() > 0);
        let len = segs.iter().map(|s| s.len()).sum();
        if segs.len() > LINE_STARTS_MAX_SEGS {
            // amortize：合并成单一共享段（O(n) 一次，之后数轮裁剪零拷贝）
            let mut flat = Vec::with_capacity(len);
            for s in &segs {
                flat.extend(s.iter());
            }
            return Self { segs: std::sync::Arc::new(vec![LineSeg::new(flat.into())]), len };
        }
        Self { segs: std::sync::Arc::new(segs), len }
    }

    /// 取出前 `k` 项的共享段（O(段数)）——编辑复用未变前缀，零拷贝。
    fn shares_prefix(&self, k: usize) -> Vec<LineSeg> {
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

    /// 取出从第 `k` 项起的共享段并叠加平移 `delta`——复用旧尾部。
    fn shares_suffix(&self, k: usize, delta: i64) -> Vec<LineSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            let l = seg.len();
            if rem >= l {
                rem -= l;
                continue;
            }
            let seg = seg.sub(rem, l).shifted(delta);
            out.push(seg);
            rem = 0;
        }
        out
    }
}

impl PartialEq for LineStarts {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}
impl Eq for LineStarts {}

/// 共享文档：BlockIndex（结构索引，含逐块字节/行号） + 源（窗口化物化）。
///
/// 不存 `ir`（仅 kind/行区间，可从索引 + `row_kind` 同式推出；`build_ir` 只留在
/// `ir`/`parity` 作金标准与 diff 司机）——省掉每次打开/编辑对全篇 `build_ir` 的一次
/// O(n) 扫描（7MB/15.9 万块 ~800ms）。
pub(crate) struct Doc {
    src: DocSrc,
    index: parse::BlockIndex,
    /// 与 `src` 同步的 rope 快照（COW）——`reopen` 用
    /// `scan_blocks_incremental_rope`（mid-line 插入 0.3ms，替代全量 `scan_blocks`
    /// 的 ~1.2s）与自维护行起始。
    rope: Rope,
    /// 每行起始字节偏移（n 行 → n 项；`split('\n')` 语义）——行源文本定位用。
    /// 分段持久化（见 [`LineStarts`]）：Enter/换行增量不再整表重建，前后缀 Arc 共享 +
    /// 读取时平移。
    line_starts: LineStarts,
    /// 与 `line_starts` 平行、同长的**行粒度 UTF-16 前缀索引**：第 i 项 = `line_starts[i]`
    /// 前的累计 UTF-16 单元数。IME/`EntityInputHandler` 的 UTF-16↔字节换算靠它
    /// （O(log n + 行长)，替代全文扫描），`edit` 增量维护。
    line_utf16_starts: LineStarts,
    /// 标量字符数（`chars().count()` 口径，状态栏显示用）——`edit` 增量维护，O(1)。
    char_count: usize,
    /// 全文 UTF-16 单元总数（`EntityInputHandler::text_length_utf16` 用，O(1)）——
    /// `edit` 增量维护。
    total_utf16: usize,
    /// 文本版本号：`text_shared` 版本化缓存键，`edit` 后 +1 失效。
    edit_ver: u64,
    /// `text_shared` 的全文缓存（防漏网调用点重复 O(n) 拷贝；edit 后失效清空）。
    text_cache: RefCell<Option<(u64, SharedString)>>,
    /// （仅测试）`text_shared` 被调次数——B 步断言 `EntityInputHandler` 不再物化全文。
    #[cfg(test)]
    text_shared_calls: std::cell::Cell<u64>,
}

impl Doc {
    /// 打开小/中文档（`Str` 源）。
    pub(crate) fn open(src: SharedString) -> Self {
        let _t = crate::prof::begin();
        let index = parse::scan_blocks(src.as_ref());
        crate::prof::end("doc.open.scan_blocks", _t);
        let _t = crate::prof::begin();
        let line_starts: Vec<usize> = parse::build_line_starts_u32(src.as_ref())
            .into_iter()
            .map(|b| b as usize)
            .collect();
        crate::prof::end("doc.open.line_starts", _t);
        let _t = crate::prof::begin();
        let rope = Rope::from(&*src);
        crate::prof::end("doc.open.rope", _t);
        let _t = crate::prof::begin();
        let char_count = src.chars().count();
        let line_utf16_starts = build_line_utf16_starts(
            |s, e| src.as_ref()[s..e].encode_utf16().count(),
            &line_starts,
            src.len(),
        );
        let total_utf16 = src.encode_utf16().count();
        crate::prof::end("doc.open.counts_utf16", _t);
        Self {
            src: DocSrc::Str(src),
            index,
            rope,
            line_starts: LineStarts::from_vec(line_starts),
            line_utf16_starts: LineStarts::from_vec(line_utf16_starts),
            char_count,
            total_utf16,
            edit_ver: 0,
            text_cache: RefCell::new(None),
            #[cfg(test)]
            text_shared_calls: std::cell::Cell::new(0),
        }
    }

    /// 打开大文档（`Rope` 源，COW）：块索引全量一次（O(n)），**窗口化物化行**走 rope
    /// 切片，不整份持有字符串——与预览一致。`line_starts` 用 chunks 单遍扫（O(n)，
    /// 替代旧实现逐字节 `get_byte` 的 O(n log n) 树下降）。
    #[allow(dead_code)]
    pub(crate) fn open_rope(rope: Rope) -> Self {
        let index = parse::scan_blocks_rope(&rope);
        let line_starts = build_line_starts_rope(&rope);
        let char_count = rope.chars().count();
        let line_utf16_starts = build_line_utf16_starts(
            |s, e| rope.slice(s..e).chars().map(|c| c.len_utf16()).sum(),
            &line_starts,
            rope.len(),
        );
        let total_utf16: usize = rope
            .chars()
            .map(|c| c.len_utf16())
            .sum::<usize>();
        let src = DocSrc::Rope(rope.clone());
        Self {
            src,
            index,
            rope,
            line_starts: LineStarts::from_vec(line_starts),
            line_utf16_starts: LineStarts::from_vec(line_utf16_starts),
            char_count,
            total_utf16,
            edit_ver: 0,
            text_cache: RefCell::new(None),
            #[cfg(test)]
            text_shared_calls: std::cell::Cell::new(0),
        }
    }

    /// 原地编辑（字节区间替换）：rope COW remove+insert + 增量块扫描 + 增量行起始。
    /// 替代旧 `reopen`（需先拼好**全新整串**再全量索引）——编辑后 rope 为唯一增量源
    /// `src`，O(log n + 变化区)，删掉每键 O(n) 整串复制分配（6MB 文档每键省一次
    /// 6MB memcpy）。
    pub(crate) fn edit(&mut self, range: Range<usize>, text: &str) {
        let _t_doc = crate::prof::begin();
        let old_len = self.len();
        let start = range.start.min(old_len);
        let end = range.end.min(old_len);
        let inserted_len = text.len();
        // 原子符数（标量口径）增量：被删段 / 插入串都是小编辑，O(范围长)。
        let removed_chars = if end > start {
            self.rope.slice(start..end).chars().count()
        } else {
            0
        };
        let inserted_chars = text.chars().count();
        // UTF-16 单元增量（行前缀/总数索引共用）：同样只扫小编辑串。
        let removed_u16: usize = if end > start {
            self.rope
                .slice(start..end)
                .chars()
                .map(|c| c.len_utf16())
                .sum::<usize>()
        } else {
            0
        };
        let inserted_u16 = text.encode_utf16().count();
        let delta_u16 = inserted_u16 as i64 - removed_u16 as i64;
        let mut new_rope = self.rope.clone();
        if end > start {
            new_rope.remove(start..end);
        }
        if inserted_len > 0 {
            new_rope.insert(start, text);
        }
        let dirty = start..start + inserted_len;
        let _t_sc = crate::prof::begin();
        // blocks-only 增量（toc 跳过：WYSIWYG 从不消费 index.toc，整表目录重建是
        // 每键 ~1.7ms 的真热点；预览/侧边栏目录由预览侧独立索引维护，不经本层）。
        let new_index = parse::scan_blocks_incremental_rope_blocks_only(
            &self.rope,
            &self.index,
            &new_rope,
            Some(dirty),
        );
        crate::prof::end("doc.scanblocks", _t_sc);
        let _t_ul = crate::prof::begin();
        // 纯行内编辑（insert/remove 都不含 '\n'）：行数/行首集合不变，只有「编辑点之后
        // 的尾部」整体平移 `delta_bytes`/`delta_u16`。分段持久化后连尾部都不复制
        // （共享后缀段 + 读取时平移），O(段数)——旧实现在此仍逐条平移尾部（139k 行）。
        // 含 '\n'（Enter/换行粘贴/跨行删）走区域重建（[`Self::line_starts_after_newline`]）。
        let removed_has_nl = end > start && self.rope.slice(start..end).chars().any(|c| c == '\n');
        let has_nl = removed_has_nl || text.contains('\n');
        let delta_bytes = inserted_len as i64 - (end - start) as i64;
        if !has_nl {
            // 受影响起始行号：首个行首 > 编辑起点（无 '\n' ⇒ (start, end] 内无行首）。
            // 字节/UTF-16 两条索引同形（行数未变）→ 同一 `keep` 适用。
            let keep = self.line_starts.partition_point(|&x| x <= start);
            if delta_bytes != 0 {
                let prefix = self.line_starts.shares_prefix(keep);
                let suffix = self.line_starts.shares_suffix(keep, delta_bytes);
                self.line_starts = LineStarts::assemble(prefix, Vec::new(), suffix);
            }
            if delta_u16 != 0 {
                let prefix = self.line_utf16_starts.shares_prefix(keep);
                let suffix = self.line_utf16_starts.shares_suffix(keep, delta_u16);
                self.line_utf16_starts = LineStarts::assemble(prefix, Vec::new(), suffix);
            }
        } else {
            let (new_ls, new_ls16) = Self::line_starts_after_newline(
                &self.line_starts,
                &self.line_utf16_starts,
                &new_rope,
                old_len,
                range.clone(),
                delta_bytes,
                delta_u16,
            );
            self.line_starts = new_ls;
            self.line_utf16_starts = new_ls16;
        }
        crate::prof::end("doc.linestarts", _t_ul);
        self.rope = new_rope;
        self.index = new_index;
        self.total_utf16 = (self.total_utf16 as i64 + delta_u16).max(0) as usize;
        self.char_count =
            (self.char_count as i64 + inserted_chars as i64 - removed_chars as i64).max(0) as usize;
        // 全文副本缓存失效。
        self.edit_ver = self.edit_ver.wrapping_add(1);
        *self.text_cache.borrow_mut() = None;
        // rope 为唯一源；原 Str 快路径（load 后首次编辑起）不再保留整份 SharedString。
        self.src = DocSrc::Rope(self.rope.clone());
        crate::prof::end("doc.edit", _t_doc);
    }

    /// 换行编辑（含 `\n`）后的两条行索引（字节/UTF-16 前缀）增量重建——分段版。
    ///
    /// 与旧 flat `Vec` 版同三段式与边界语义（前缀沿用 + 变化区重扫 + 尾部平移），但
    /// 前缀/后缀改为共享段（[`LineStarts::shares_prefix`] / [`LineStarts::shares_suffix`]），
    /// 只真正物化「变化中段」，同一次区域遍历同时产出字节与 UTF-16 起始。旧版前缀
    /// 拷贝 + 尾部逐条平移两个 O(n) 段（139k 行实测 3.2~3.7ms/键→O(变化区)）。
    #[allow(clippy::too_many_arguments)]
    fn line_starts_after_newline(
        old: &LineStarts,
        old_u16: &LineStarts,
        new: &Rope,
        old_len: usize,
        edit_old: Range<usize>,
        delta_bytes: i64,
        delta_u16: i64,
    ) -> (LineStarts, LineStarts) {
        let new_len = new.len();
        // 前缀：所有 start ≤ 编辑起点 的行（其行首字节不变）。
        let keep = old.partition_point(|&s| s <= edit_old.start);
        // 对齐点 = 首个旧 start > 编辑终点（新坐标 +delta）。
        let j = old.partition_point(|&s| s <= edit_old.end.min(old_len));
        let old_lines = old.len();
        let region_start = old.get(keep.saturating_sub(1)).unwrap_or(0);
        let region_end = if j < old_lines {
            (old.at(j) as i64 + delta_bytes).clamp(region_start as i64, new_len as i64) as usize
        } else {
            new_len.max(region_start)
        };
        // 变化区起点基线 = 上一前缀行行首前的累计 UTF-16 数。
        let base_u16 = if keep > 0 { old_u16.get(keep - 1).unwrap_or(0) } else { 0 };
        // 变化区：单遍扫出新行起始（字节 + UTF-16 前缀，同一行集），只从 rope 切成
        // 小窗口（单行/变化规模）。边界处理同旧：区域尾部对齐点的行起始由「后缀共享
        // 段 + 平移」产出（= region_end），区域扫描要跳过恰落在 region_end 的 '\n' 行
        // 起始（否则重复一遍）；仅当没有尾部（编辑近文末）时才保留它（延续
        // `build_line_starts` 对末尾空行的语义）。
        let region = new.slice(region_start..region_end).to_string();
        let no_suffix = j >= old_lines;
        let mut byte_starts: Vec<usize> = Vec::new();
        let mut u16_starts: Vec<usize> = Vec::new();
        let mut run_u16 = base_u16 as i64;
        for (i, c) in region.char_indices() {
            run_u16 += c.len_utf16() as i64;
            if c == '\n' {
                let s = region_start + i + 1;
                if s < region_end || no_suffix {
                    byte_starts.push(s);
                    u16_starts.push(run_u16 as usize);
                }
            }
        }
        let prefix = old.shares_prefix(keep);
        let suffix = old.shares_suffix(j, delta_bytes);
        let prefix16 = old_u16.shares_prefix(keep);
        let suffix16 = old_u16.shares_suffix(j, delta_u16);
        (
            LineStarts::assemble(prefix, byte_starts, suffix),
            LineStarts::assemble(prefix16, u16_starts, suffix16),
        )
    }

    /// 块总数（虚拟化滚动条量程用，O(1)）。
    pub(crate) fn block_count(&self) -> usize {
        self.index.blocks.len()
    }

    /// 源码行 → 包含它的块下标（块按 `src_line` 升序连续覆盖；越界归到首/末块）。
    /// 编辑后块下标的稳定性判定（`pending_patch_blocks`）依赖它。
    pub(crate) fn block_index_for_line(&self, line: usize) -> usize {
        let b = &self.index.blocks;
        if b.is_empty() {
            return 0;
        }
        b.partition_point(|m| (m.src_line as usize) <= line)
            .saturating_sub(1)
            .min(b.len() - 1)
    }

    /// 每行起始字节偏移（`split('\n')` 语义，长度 = 行数）——`row_col`/`line_start`/
    /// `line_end` 的 O(log n) 二分索引（替代每帧对整篇源文本的线性扫描）。
    pub(crate) fn line_starts(&self) -> &LineStarts {
        &self.line_starts
    }

    /// 显示语义行数（`str::lines()` 口径，与状态栏一致）：`line_starts` 含**末尾换行符
    /// 产生的幻影尾行**（`split('\n')` 语义），`lines().count()` 不计它——去掉即一致。
    /// O(1)，不物化全文。
    pub(crate) fn line_count(&self) -> usize {
        let n = self.line_starts.len();
        if n > 0 && self.line_starts.get(n - 1) == Some(self.len()) {
            n - 1
        } else {
            n
        }
    }

    /// 标量字符数（`chars().count()` 口径），`edit` 增量维护，O(1)。
    pub(crate) fn char_count(&self) -> usize {
        self.char_count
    }

    /// 全文 UTF-16 单元总数（IME `text_length_utf16` 用），`edit` 增量维护，O(1)。
    pub(crate) fn total_utf16(&self) -> usize {
        self.total_utf16
    }

    /// 行粒度 UTF-16 前缀索引（与 `line_starts` 平行）：第 i 项 = `line_starts[i]` 前累计
    /// UTF-16 单元数。
    #[allow(dead_code)]
    pub(crate) fn line_utf16_starts(&self) -> &LineStarts {
        &self.line_utf16_starts
    }

    /// 字节偏移 → 前累计 UTF-16 单元数（IME 协议层换算；O(log n + 行长)：行表二分
    /// + 行内切片计数，不触碰全文）。
    pub(crate) fn byte_to_utf16(&self, byte: usize) -> usize {
        let byte = byte.min(self.len());
        let i = self
            .line_starts
            .partition_point(|&s| s <= byte)
            .saturating_sub(1);
        let line_start = self.line_starts.get(i).unwrap_or(0);
        let base_u16 = self.line_utf16_starts.get(i).unwrap_or(0);
        if byte <= line_start {
            return base_u16;
        }
        base_u16
            + self
                .rope
                .slice(line_start..byte)
                .chars()
                .map(|c| c.len_utf16())
                .sum::<usize>()
    }

    /// UTF-16 单元数 → 字节偏移（IME 协议层换算；O(log n + 行长)，越界钳到行尾/文末）。
    /// 与全量 `s.char_indices()` 版 `offset_from_utf16` 语义一致：返回「累计 ≥ 目标」的
    /// 第一个字符起始字节。
    pub(crate) fn utf16_to_byte(&self, utf16: usize) -> usize {
        let i = self
            .line_utf16_starts
            .partition_point(|&v| v <= utf16)
            .saturating_sub(1);
        let line_start = self.line_starts.get(i).unwrap_or(0);
        let base_u16 = self.line_utf16_starts.get(i).unwrap_or(0);
        let line_end = self.line_starts.get(i + 1).unwrap_or(self.len());
        let mut acc = base_u16;
        let mut byte = line_start;
        for c in self.rope.slice(line_start..line_end).chars() {
            if acc >= utf16 {
                return byte;
            }
            acc += c.len_utf16();
            byte += c.len_utf8();
        }
        line_end
    }

    /// 第 `i` 块的轻量元数据（`BlockMeta`，≈20B，O(段数)）——窗口化 shaping 的
    /// 块高估算/带内判定只依赖它，**不物化行**。
    pub(crate) fn block_meta(&self, i: usize) -> Option<BlockMeta> {
        self.index.blocks.get(i)
    }

    /// 第 `ix` 源行的 `(字符数, 是否全 ASCII)`——窗口化 shaping 的块高估算用：
    /// 只读字节切片数 chars，不物化 `Row`、不 shape，O(行长)。
    pub(crate) fn line_chars_ascii(&self, ix: usize) -> (usize, bool) {
        let s = self.line_starts.get(ix).unwrap_or(0);
        let e = self
            .line_starts
            .get(ix + 1)
            .unwrap_or_else(|| self.src_len());
        match &self.src {
            DocSrc::Str(str) => {
                let mut e2 = e;
                if e2 > s {
                    let b = str.as_bytes();
                    if b[e2 - 1] == b'\n' {
                        e2 -= 1;
                    }
                    if e2 > s && b[e2 - 1] == b'\r' {
                        e2 -= 1;
                    }
                }
                let mut ascii = true;
                let mut n = 0usize;
                for c in str[s..e2].chars() {
                    if c.len_utf8() > 1 {
                        ascii = false;
                    }
                    n += 1;
                }
                (n, ascii)
            }
            DocSrc::Rope(r) => {
                // rope 单切出 `RopeSlice`（不复制）+ 逐字符数——与 Str 分支同语义。
                // 避免每块 `slice().to_string()` 的堆分配：Pass1 逐块 `line_chars_ascii`
                // 时 Rope 每次切串分配（debug 6 万块 ~190ms）vs `RopeSlice::chars`
                // 零分配遍历（chunk 迭代）；`\r/\n` 处理同 Str 分支。
                let mut ascii = true;
                let mut n = 0usize;
                let e = e.min(r.len());
                if e > s {
                    for c in r.slice(s..e).chars() {
                        if c == '\n' {
                            break;
                        }
                        if c == '\r' {
                            continue;
                        }
                        if c.len_utf8() > 1 {
                            ascii = false;
                        }
                        n += 1;
                    }
                }
                (n, ascii)
            }
        }
    }

    /// 按块窗口物化统一行（`[lo_block, hi_block]`，含两端）。
    pub(crate) fn rows_in_block_window(&self, lo: usize, hi: usize) -> Vec<DocRow> {
        let rows = match &self.src {
            DocSrc::Str(s) => parse_block_window(s.as_ref(), &self.index, lo, hi),
            DocSrc::Rope(r) => parse_block_window_rope(r, &self.index, lo, hi),
        };
        rows.into_iter().enumerate().map(|(j, r)| self.assemble(lo + j, r)).collect()
    }

    /// 按源码行区间找覆盖的块窗口并物化（虚拟化视口色调）。
    #[allow(dead_code)]
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

    /// 把预览侧一个块行装成统一行：kind 用 `row_kind`（与 `ir` 金标准同式，parity 钉死），
    /// 行区间用**索引自身**（块 k 覆盖 `[src_line, 下一块 src_line)`，与窗口切片同源——
    /// 不再为 kind/行区间在整篇 IR 上二分/全扫）。
    fn assemble(&self, k: usize, r: Row) -> DocRow {
        let kind = row_kind(r.kind);
        let n = self.index.blocks.len();
        let lines = if k + 1 < n {
            let next = self.index.blocks.get(k + 1).map(|b| b.src_line as usize).unwrap_or(r.src_line + 1);
            r.src_line..next.max(r.src_line + 1)
        } else {
            r.src_line..self.line_starts.len().max(r.src_line + 1)
        };
        let code_lines = r.code_lines().to_vec();
        let byte_map = match r.kind {
            RowKind::Paragraph
            | RowKind::Heading(_)
            | RowKind::Bullet
            | RowKind::Quote => {
                let line = self.line_str(r.src_line);
                parse::text_segment(&line, r.kind)
                    .map(|(off, seg)| {
                        parse::inline_style_mapped(seg).2.into_iter().map(|b| off + b).collect()
                    })
                    .unwrap_or_default()
            }
            RowKind::Code => self.code_byte_map(r.src_line, &code_lines),
            RowKind::Image => {
                // 独立图片行 `![alt](src)`：alt 文本与源字节 1:1 连续（`!`/`[`
                // 各占 1 字节，alt 自字节 2 起；末项哨兵 = 闭合 `]` 前 = 2+alt.len()）。
                // 让图片题注成为与普通文本行同构的可编辑段（编辑正确映射回源）。
                (0..=r.text.len()).map(|i| 2 + i).collect()
            }
            _ => Vec::new(),
        };
        let payload = match r.kind {
            RowKind::Code => DocPayload::Code {
                code_lines,
                language: r.code_language().map(Into::into),
            },
            RowKind::Image => DocPayload::Image {
                src: r.image().map(|i| i.src.clone()).unwrap_or_default(),
            },
            RowKind::Table => r.table().cloned().map(DocPayload::Table).unwrap_or_default(),
            _ => DocPayload::Text,
        };
        let table_rows = match r.kind {
            RowKind::Table => {
                let n_data = r.table().map_or(0, |t| t.rows.len());
                self.table_row_maps(r.src_line, n_data)
            }
            _ => None,
        };
        DocRow {
            kind,
            src_line: r.src_line,
            lines,
            text: r.text,
            runs: r.runs,
            byte_map,
            payload,
            table_rows,
        }
    }

    /// 表格块：逐可见行（表头 + 各数据行，跳过分隔行）用
    /// [`parse::split_table_cells_mapped`] 装配单元格编辑映射。
    fn table_row_maps(
        &self,
        src_line: usize,
        n_data: usize,
    ) -> Option<std::rc::Rc<Vec<TableRowMap>>> {
        let mut out = Vec::with_capacity(1 + n_data);
        for k in 0..=n_data {
            // k=0 表头（src_line）；数据行 i（0 基，k=i+1）源行 = src_line + 1 + k。
            let line_ix = if k == 0 { src_line } else { src_line + 1 + k };
            let line = self.line_str(line_ix);
            let cells = parse::split_table_cells_mapped(&line)
                .into_iter()
                .map(|(src, text, map)| TableCellMap { src, text, map })
                .collect();
            out.push(TableRowMap { src_line: line_ix, cells });
        }
        Some(std::rc::Rc::new(out))
    }

    /// 代码块（围栏内内容行可编辑）的块级编辑映射：
    /// `byte_map[i]` = 整块渲染文本（代码行以 `\n` 连接，围栏标记本身只读不渲染）
    /// 第 `i` 个渲染字节对应的**块首源偏移**（相对 `src_line` 围栏行起始），使元素层
    /// 可对围栏内任意内容行做「渲染位置 ↔ 源字节」换算。末项哨兵 = 末内容行行尾
    /// （= 闭合围栏行起始）。`CRLF` 下分隔符映射到上一行的 `\n`（`line_starts` 已跳过 `\r\n`）。
    fn code_byte_map(&self, src_line: usize, code_lines: &[SharedString]) -> Vec<usize> {
        let block_start = self.line_starts.get(src_line).unwrap_or(0);
        let mut map = Vec::new();
        for (k, line) in code_lines.iter().enumerate() {
            // 行与行之间的渲染 `\n`（首行前无分隔符）。
            if k > 0 {
                let prev_nl = self
                    .line_starts
                    .get(src_line + 1 + k)
                    .unwrap_or(block_start)
                    .saturating_sub(1);
                map.push(prev_nl.saturating_sub(block_start));
            }
            let line_start = self
                .line_starts
                .get(src_line + 1 + k)
                .unwrap_or(block_start);
            for (i, _) in line.as_bytes().iter().enumerate() {
                map.push(line_start.saturating_sub(block_start) + i);
            }
        }
        let sentinel = self
            .line_starts
            .get(src_line + 1 + code_lines.len())
            .unwrap_or_else(|| self.src_len())
            .saturating_sub(block_start);
        map.push(sentinel);
        map
    }

    /// 第 `ix` 行的源码文本（仅该行切片，O(行长)）。
    pub(crate) fn line_str(&self, ix: usize) -> String {
        let s = self.line_starts.get(ix).unwrap_or(0);
        let e = self
            .line_starts
            .get(ix + 1)
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

    /// 底层 rope（快照 / COW 编辑用）。
    pub(crate) fn rope(&self) -> &Rope {
        &self.rope
    }

    /// 全文字节长（编辑/光标边界钳位用；rope 单真源，O(1)）。
    pub(crate) fn len(&self) -> usize {
        self.rope.len()
    }

    /// 全文（宿主镜像/测试/派生全量扫描用）。Str 源（load 后）O(1) 克隆；rope 源
    /// 走**版本化缓存**（键 `edit_ver`，edit 后失效）——保底任何遗漏调用点不再重复
    /// O(n) 拷贝（输入热路径已改走 UTF-16 索引/切片，见 `EntityInputHandler`）。
    pub(crate) fn text_shared(&self) -> SharedString {
        #[cfg(test)]
        self.text_shared_calls.set(self.text_shared_calls.get() + 1);
        if let Some((ver, s)) = &*self.text_cache.borrow()
            && *ver == self.edit_ver {
                return s.clone();
            }
        let s = match &self.src {
            DocSrc::Str(s) => s.clone(),
            DocSrc::Rope(r) => SharedString::from(r.to_string()),
        };
        *self.text_cache.borrow_mut() = Some((self.edit_ver, s.clone()));
        s
    }

    /// （仅测试）`text_shared` 被调次数——B 步断言 `EntityInputHandler` 不再物化全文。
    #[cfg(test)]
    pub(crate) fn text_shared_calls(&self) -> u64 {
        self.text_shared_calls.get()
    }

    /// 字节区间切片（O(区间长)；边界钳到 [0, len]，rope 单切一次）。
    pub(crate) fn slice_str(&self, range: Range<usize>) -> String {
        let r = range.start.min(self.len())..range.end.min(self.len());
        if r.end <= r.start {
            return String::new();
        }
        self.rope.slice(r).to_string()
    }
}

/// rope 的行起始字节偏移表（`split('\n')` 语义）：chunks 单遍扫（每字节一次，O(n)），
/// 替代旧实现逐字节 `get_byte` 的 O(n log n) 树下降。
fn build_line_starts_rope(rope: &Rope) -> Vec<usize> {
    let mut v = vec![0usize];
    let mut pos = 0usize;
    for chunk in rope.chunks() {
        for (i, b) in chunk.bytes().enumerate() {
            if b == b'\n' {
                v.push(pos + i + 1);
            }
        }
        pos += chunk.len();
    }
    v
}

/// 行粒度 UTF-16 前缀索引构建：第 i 项 = `line_starts[i]`（含幻影尾行）前的累计
/// UTF-16 单元数。`u16_of_range` 统计单行字节区间的 UTF-16 单元数（Str 走
/// `encode_utf16().count()`，rope 走 `chars()` 累加）。O(n)。
fn build_line_utf16_starts(
    u16_of_range: impl Fn(usize, usize) -> usize,
    line_starts: &[usize],
    len: usize,
) -> Vec<usize> {
    let mut v = Vec::with_capacity(line_starts.len());
    let mut run = 0usize;
    for i in 0..line_starts.len() {
        v.push(run);
        let s = line_starts[i];
        let e = line_starts.get(i + 1).copied().unwrap_or(len);
        run += u16_of_range(s, e);
    }
    v
}

/// 预览 `RowKind` → IR 语义（对齐子集上恒等；parity 保证二者一致）。
pub(crate) fn row_kind(k: RowKind) -> IrKind {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_matches_full_rebuild(d: &Doc, ctx: &str) -> (usize, usize) {
        let want: Vec<usize> =
            crate::markdown::parse::build_line_starts_u32(&d.rope().to_string())
                .into_iter()
                .map(|b| b as usize)
                .collect();
        assert_eq!(
            d.line_starts().to_vec(),
            want,
            "line_starts 与全量重建不一致（{ctx}）"
        );
        (d.line_starts().seg_count(), d.line_starts().len())
    }

    /// 分段行起始的摊销护栏 + 一致性：连续打字（no-'\n'）/ Enter（has-'\n'）/
    /// 跨行替换后段数不超限，且逐值恒等于全量重建。
    #[test]
    fn line_starts_segment_amortize_and_consistency() {
        let base = "行零\n行一\n普通段落正文\nAB\nCD\n末尾行\n";
        let mut doc = Doc::open(SharedString::from(base));

        let at = base.find("AB").unwrap() + 1;
        for i in 0..40 {
            doc.edit(at..at, "x"); // 打字：no-'\n'，只平移后缀
            let (segs, _) = assert_matches_full_rebuild(&doc, &format!("打字 i={i}"));
            assert!(segs <= LINE_STARTS_MAX_SEGS, "打字段数超限：{segs}");
            doc.edit(at..at + 1, "\n"); // Enter：has-'\n'，区域重建
            let (segs, _) = assert_matches_full_rebuild(&doc, &format!("Enter i={i}"));
            assert!(segs <= LINE_STARTS_MAX_SEGS, "Enter 段数超限：{segs}");
        }

        // 跨行替换 / 删除压力（含中文换行插入、整行删除）。
        for _ in 0..20 {
            // 随机字节位先回退到字符边界（rope 编辑要求边界对齐）。
            let len = doc.len();
            let rope = doc.rope();
            let seek = (at + len / 3) % len.max(1);
            let a = rope.char_to_byte_idx(rope.byte_to_char_idx(seek));
            let b = rope.char_to_byte_idx(rope.byte_to_char_idx((a + 3).min(len)));
            doc.edit(a..b, "中\n文");
            let (segs, _) = assert_matches_full_rebuild(&doc, "替换");
            assert!(segs <= LINE_STARTS_MAX_SEGS, "替换段数超限：{segs}");
            let len = doc.len();
            let rope = doc.rope();
            let e = rope.char_to_byte_idx(rope.byte_to_char_idx((a + 2).min(len)));
            if e > a {
                doc.edit(a..e, "");
                let (segs, _) = assert_matches_full_rebuild(&doc, "删除");
                assert!(segs <= LINE_STARTS_MAX_SEGS, "删除段数超限：{segs}");
            }
        }

        // 文末追加（无后缀段 + 幻影尾行的语义路径）。
        for _ in 0..12 {
            let len = doc.len();
            doc.edit(len..len, "\n# 尾部\n正文");
            let (segs, _) = assert_matches_full_rebuild(&doc, "文末追加");
            assert!(segs <= LINE_STARTS_MAX_SEGS, "文末追加段数超限：{segs}");
        }
    }
}

