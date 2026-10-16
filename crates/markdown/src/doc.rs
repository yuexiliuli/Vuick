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
    parse_block_window, parse_block_window_rope, BlockIndex, BlockMeta, Row, RowAlign, RowKind,
    RunKind, TableData,
};
use super::parse;

/// 块级渲染负载（非纯文本块的渲染数据，统一行 [`DocRow`] 的伴生字段）。
///
/// Code/Table/Image 等块在预览侧重于 `Row.payload`（代码全部行 / 表格网格 / 图片源），
/// WYSIWYG 也需要这些才画得出来——统一行把它们一并带过来，避免渲染分支丢数据。
#[derive(Debug, Clone, Default, PartialEq)]
pub enum DocPayload {
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
pub struct TableRowMap {
    /// 该行源码行号。
    pub src_line: usize,
    /// 逐单元格。
    pub cells: Vec<TableCellMap>,
}

/// 单个表格单元格的编辑映射。
#[derive(Debug, Clone, PartialEq)]
pub struct TableCellMap {
    /// 源码内容区间（**相对该行行首**；`\|` 转义按源字节计入，哨兵指到内容物理尾）。
    pub src: Range<usize>,
    /// 渲染文本（与预览单元格同源，已解义/行内清理）。
    pub text: SharedString,
    /// 渲染字节 → 源字节（相对该行行首）；长度 = `text.len()+1`。
    pub map: Vec<usize>,
}

/// 统一渲染行（块粒度：Code/Table 等一个块 = 一个 DocRow，跨多行）。
#[derive(Debug, Clone, PartialEq)]
pub struct DocRow {
    /// 语义（IR 金标准）。
    pub kind: IrKind,
    /// 该块首个源码行（0 基）。
    pub src_line: usize,
    /// 覆盖的源码行区间（多行块 = 块区间；单行块 = src_line..src_line+1）。
    pub lines: Range<usize>,
    /// 渲染文本（与预览 `parse_to_rows` 逐字节一致）。
    pub text: SharedString,
    /// 行内样式段（与预览同源）。
    pub runs: Vec<(Range<usize>, RunKind)>,
    /// 编辑映射：渲染字节 → **源字节偏移**。
    /// - 文本行（段落/标题/列表/引述）：相对**该行**行首，长度 = `text.len()+1`，
    ///   末项为哨兵 = 源段尾；由 `parse::text_segment` + `parse::inline_style_mapped`
    ///   装配（与预览同一段派生）。
    /// - 代码块（围栏内内容行可编辑）：相对**块首**（`src_line` 围栏行起始），
    ///   渲染文本 = 各代码行以 `\n` 连接（围栏标记本身只读不渲染）。
    /// - 其它非文本行（表格/图片/规则/空行）为空。
    pub byte_map: Vec<usize>,
    /// 块级渲染负载（非文本块的渲染数据）。
    pub payload: DocPayload,
    /// 行对齐（EPUB 排版指令 `%%ALIGN:X%%`，来自 [`super::parse::Row::align`]；
    /// `None` = 跟随默认左对齐）。WYSIWYG 只读 EPUB 宿主用它做右/中对齐单源。
    pub align: Option<RowAlign>,
    /// 文内脚注引用标记 `[^n]`（显示区间 = **相对本行渲染文本**的字节范围，见
    /// [`super::parse::extract_notes`] 的 `[^n] → [n]` 剥记号语义；由 M5-B5 P4-2
    /// 「注记迁入 WYSIWYG」透传，A 侧 `MarkdownPreview` 删除后由 WyEditor 承载
    /// 悬浮卡/双向跳转）。
    pub notes: Vec<(Range<usize>, usize)>,
    /// 脚注**定义**标记 `[!n]`（同 `notes` 区间语义；`[!n] → [n]` 显示）。
    pub note_defs: Vec<(Range<usize>, usize)>,
    /// 表格块的逐行/单元格编辑映射（仅 [`IrKind::Table`] 行非 `Some`；代码块用
    /// `byte_map`，表格因单元格多列需要更细粒度）。`Rc` 压低克隆成本。
    pub table_rows: Option<std::rc::Rc<Vec<TableRowMap>>>,
}

/// 文档源：`Str`（小文档）或 `Rope`（大文档，COW，与编辑器共享底层树）。
enum DocSrc {
    Str(SharedString),
    Rope(Rope),
}

/// 按行装配 [`DocRow`]（`assemble_docrow`）所需的**行索引 / 行文本访问接口**。
///
/// [`Doc`] 与预览快照（`MarkdownPreview` 的 `PreviewSrc` + 行起始表 / ropey 内建
/// 行索引）都实现它：让「`Doc::rows_in_block_window` 的真实 `DocRow`」与「预览窗口 /
/// 全量物化的 `DocRow`」共用同一份装配逻辑（M5-B5 P3-1a 预览渲染行源切共享 `DocRow`、
/// `byte_map` 保真）。语义口径与 [`Doc::line_starts`] / [`Doc::line_str`] 一致：
/// `line_start(ix)` = 第 ix 行起始字节偏移（越界 = `src_len`）、`line_count()` = 总行数
/// （含末尾换行的幻影空行）、`line_str(ix)` = 第 ix 行源码文本（剥 `\r\n`/`\n` 行尾）。
pub(crate) trait DocLineView {
    fn line_start(&self, ix: usize) -> usize;
    fn line_count(&self) -> usize;
    fn line_str(&self, ix: usize) -> String;
}

impl DocLineView for Doc {
    fn line_start(&self, ix: usize) -> usize {
        self.line_starts.get(ix).unwrap_or_else(|| self.src_len())
    }
    fn line_count(&self) -> usize {
        self.line_starts.len()
    }
    fn line_str(&self, ix: usize) -> String {
        self.line_str(ix)
    }
}

/// 行起始表（`line_starts` / `line_utf16_starts` 共用）的**分段持久化**。
///
/// 与 [`parse::Blocks`] 同构：增量编辑只重建变化中段，未变前缀/后缀用 `Arc` 共享、
/// 读取时叠加常量平移 `shift`（不逐条复制）。背景：Enter/换行路径的旧实现在每键
/// 「前缀拷贝 + 尾部逐条平移」两个 O(n) 段（139k 行 ×2 索引实测 3.2~3.7ms/键），
/// 分段后只需重扫变化区（通常极小），前后缀 O(1) 共享；段数超限合并且 amortize。
#[derive(Clone, Debug)]
/// 行起始表（`LineStarts`）——经 [`Doc::line_starts`] 跨 crate（markdown-wysiwyg）消费，
/// `get`/`partition_point`/`len` 随拆 crate 放宽为 pub。
pub struct LineStarts {
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
    pub fn len(&self) -> usize {
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
    pub fn get(&self, i: usize) -> Option<usize> {
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
    pub fn partition_point<P: FnMut(&usize) -> bool>(&self, mut pred: P) -> usize {
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
pub struct Doc {
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
    /// `text_shared` 被调次数（跨 crate 测试探针，无条件存在）——B 步断言
    /// `EntityInputHandler` 不再物化全文。markdown-wysiwyg 测试跨 crate 观测。
    text_calls: std::cell::Cell<u64>,
}

/// `Doc` 的 **O(1) 共享克隆**：克隆即共享、编辑即 COW 分离。
///
/// 全部重字段都是一次性只读构建的「持久分段 + Arc / ropey COW」：`index`
/// （`Blocks/Toc` 的 `Arc<[..]>` 段）、`line_starts`/`line_utf16_starts`（`LineSeg`
/// 的 `Arc<[..]>` 段）、`rope`（ropey 拓扑 COW，与宿主编辑器共享底层树）——这里逐项
/// `Arc` 克隆，不复制任何数据。唯一跳过分叉的是 `text_cache`：fork 缓存本就该空
/// （`edit` 会失效它；同源共享下 fork 各自首读全文时各自物化一次并缓存，无正确性
/// 影响）、`text_calls` 归零（每份独立观测）。
///
/// 编辑分离语义：`Doc::edit` 只替换**自己**这份克隆的字段——rope remove/insert 出新树、
/// 分段重建叠出「旧前缀 Arc + 新中段 + 旧后缀 Arc」——原 Doc 的字段逐字节不动（与
/// `open_rope_edit_does_not_pollute_host_rope` 同款不污染语义，该测试已锁 ropey chunk
/// 边界的 COW 前提）。宿主用途：右栏只读预览 `md_wy` 与左面板 live2 各自持 `Doc`——
/// toggle 切 live2 时 `md_wy.doc().clone()` O(1) 复用同一份已解析结构（消双解析），
/// live2 首编辑即与该份不可变共享分离。
impl Clone for Doc {
    fn clone(&self) -> Self {
        Self {
            src: match &self.src {
                DocSrc::Str(s) => DocSrc::Str(s.clone()),
                DocSrc::Rope(r) => DocSrc::Rope(r.clone()),
            },
            index: self.index.clone(),
            rope: self.rope.clone(),
            line_starts: self.line_starts.clone(),
            line_utf16_starts: self.line_utf16_starts.clone(),
            char_count: self.char_count,
            total_utf16: self.total_utf16,
            edit_ver: self.edit_ver,
            text_cache: RefCell::new(None),
            text_calls: std::cell::Cell::new(0),
        }
    }
}

impl Doc {
    /// 打开小/中文档（`Str` 源）。
    pub fn open(src: SharedString) -> Self {
        let _t = crate::prof::begin();
        // 融合一趟字节扫描：行起始（u32）+ 行粒度 UTF-16 前缀 + 字符数 + 全文 UTF-16
        // 单元数，替代旧四趟独立 O(n)（`build_line_starts_u32` / `chars().count()` /
        // 逐行 `encode_utf16().count()` / 全文 `encode_utf16().count()`）。
        let (ls32, ls16, char_count, total_utf16) =
            parse::build_line_stats_utf16(src.as_bytes());
        crate::prof::end("doc.open.stats", _t);
        let _t = crate::prof::begin();
        // 块分类复用上述行起始表——不再整篇第二遍 `build_line_starts_u32`。
        // blocks-only（toc 跳过：WYSIWYG Doc 层从不消费 index.toc，侧边栏目录由
        // `WyEditor::toc_entries()` 独立构建；见 `scan_blocks_with_line_starts` 注释）。
        let lines: Vec<&str> = src.split('\n').collect();
        let index = parse::scan_blocks_with_line_starts(&lines, &ls32, false);
        crate::prof::end("doc.open.scan_blocks", _t);
        let _t = crate::prof::begin();
        let rope = Rope::from(&*src);
        crate::prof::end("doc.open.rope", _t);
        let line_starts: Vec<usize> = ls32.into_iter().map(|b| b as usize).collect();
        Self {
            src: DocSrc::Str(src),
            index,
            rope,
            line_starts: LineStarts::from_vec(line_starts),
            line_utf16_starts: LineStarts::from_vec(ls16),
            char_count,
            total_utf16,
            edit_ver: 0,
            text_cache: RefCell::new(None),
            text_calls: std::cell::Cell::new(0),
        }
    }

    /// 打开大文档（`Rope` 源，COW）：块索引走 rope 原生全量 **blocks-only** 扫描
    /// （跳过 toc——WYSIWYG 从不消费 `index.toc`）；行切片按 chunk 装配（跨 chunk
    /// 分裂的行物化为局部 `String`，余下零拷贝借 chunk `&str`，**不整份物化 rope**）；
    /// 四行统计一趟 chunk 融合扫描（[`build_line_stats_utf16_rope`]，与 Str 分支
    /// [`parse::build_line_stats_utf16`] 口径一致）。`DocSrc::Rope` 持 COW 快照，
    /// [`Self::edit`] 不污染宿主编辑区共享的 rope 原树（有回归测试锁）。
    pub fn open_rope(rope: Rope) -> Self {
        let (ls32, ls16, char_count, total_utf16) = build_line_stats_utf16_rope(&rope);
        let n = ls32.len();
        let rope_len = rope.len();
        // 行 k 的源码字节区间（含尾 `\n` 剔除；末行/幻影尾行到文末）。
        let line_content = |k: usize| -> (usize, usize) {
            let s = ls32[k] as usize;
            let e = if k + 1 < n {
                (ls32[k + 1] as usize).saturating_sub(1)
            } else {
                rope_len
            };
            (s, e)
        };
        // 两阶段：先物化跨 chunk 分裂的行（`owned` 冻结），再装配行引用——规避
        // `Vec<&str>` 同时借 rope chunk 与本地 `owned` 的借用冲突。
        let mut owned: Vec<String> = Vec::new();
        for k in 0..n {
            let (s, e) = line_content(k);
            if s < e {
                let (chunk, off) = rope.chunk(s);
                if e > off + chunk.len() {
                    owned.push(rope.slice(s..e).to_string());
                }
            }
        }
        let mut lines: Vec<&str> = Vec::with_capacity(n);
        let mut oi = 0usize;
        for k in 0..n {
            let (s, e) = line_content(k);
            if s == e {
                lines.push("");
            } else {
                let (chunk, off) = rope.chunk(s);
                let rel = s - off;
                if e <= off + chunk.len() {
                    lines.push(&chunk[rel..rel + (e - s)]);
                } else {
                    lines.push(owned[oi].as_str());
                    oi += 1;
                }
            }
        }
        debug_assert_eq!(oi, owned.len(), "跨 chunk 行物化数与装配数应一致");
        let index = parse::scan_blocks_with_line_starts(&lines, &ls32, false);
        let src = DocSrc::Rope(rope.clone());
        Self {
            src,
            index,
            rope,
            line_starts: LineStarts::from_vec(ls32.into_iter().map(|b| b as usize).collect()),
            line_utf16_starts: LineStarts::from_vec(ls16),
            char_count,
            total_utf16,
            edit_ver: 0,
            text_cache: RefCell::new(None),
            text_calls: std::cell::Cell::new(0),
        }
    }

    /// 原地编辑（字节区间替换）：rope COW remove+insert + 增量块扫描 + 增量行起始。
    /// 替代旧 `reopen`（需先拼好**全新整串**再全量索引）——编辑后 rope 为唯一增量源
    /// `src`，O(log n + 变化区)，删掉每键 O(n) 整串复制分配（6MB 文档每键省一次
    /// 6MB memcpy）。
    pub fn edit(&mut self, range: Range<usize>, text: &str) {
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
    pub fn block_count(&self) -> usize {
        self.index.blocks.len()
    }

    /// 源码行 → 包含它的块下标（块按 `src_line` 升序连续覆盖；越界归到首/末块）。
    /// 编辑后块下标的稳定性判定（`pending_patch_blocks`）依赖它。
    pub fn block_index_for_line(&self, line: usize) -> usize {
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
    pub fn line_starts(&self) -> &LineStarts {
        &self.line_starts
    }

    /// 显示语义行数（`str::lines()` 口径，与状态栏一致）：`line_starts` 含**末尾换行符
    /// 产生的幻影尾行**（`split('\n')` 语义），`lines().count()` 不计它——去掉即一致。
    /// O(1)，不物化全文。
    pub fn line_count(&self) -> usize {
        let n = self.line_starts.len();
        if n > 0 && self.line_starts.get(n - 1) == Some(self.len()) {
            n - 1
        } else {
            n
        }
    }

    /// 标量字符数（`chars().count()` 口径），`edit` 增量维护，O(1)。
    pub fn char_count(&self) -> usize {
        self.char_count
    }

    /// 全文 UTF-16 单元总数（IME `text_length_utf16` 用），`edit` 增量维护，O(1)。
    pub fn total_utf16(&self) -> usize {
        self.total_utf16
    }

    /// 行粒度 UTF-16 前缀索引（与 `line_starts` 平行）：第 i 项 = `line_starts[i]` 前累计
    /// UTF-16 单元数。仅测试消费（parity 对比）。
    #[cfg(test)]
    pub(crate) fn line_utf16_starts(&self) -> &LineStarts {
        &self.line_utf16_starts
    }

    /// 字节偏移 → 前累计 UTF-16 单元数（IME 协议层换算；O(log n + 行长)：行表二分
    /// + 行内切片计数，不触碰全文）。
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
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
    pub fn utf16_to_byte(&self, utf16: usize) -> usize {
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
    pub fn block_meta(&self, i: usize) -> Option<BlockMeta> {
        self.index.blocks.get(i)
    }

    /// 第 `ix` 源行的 `(字符数, 是否全 ASCII)`——窗口化 shaping 的块高估算用：
    /// 只读字节切片数 chars，不物化 `Row`、不 shape，O(行长)。
    pub fn line_chars_ascii(&self, ix: usize) -> (usize, bool) {
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
    pub fn rows_in_block_window(&self, lo: usize, hi: usize) -> Vec<DocRow> {
        let rows = match &self.src {
            DocSrc::Str(s) => parse_block_window(s.as_ref(), &self.index, lo, hi),
            DocSrc::Rope(r) => parse_block_window_rope(r, &self.index, lo, hi),
        };
        rows.into_iter()
            .enumerate()
            .map(|(j, r)| assemble_docrow(self, &self.index, lo + j, r))
            .collect()
    }

    /// 按源码行区间找覆盖的块窗口并物化（虚拟化视口色调）。仅测试消费（parity 对比）。
    #[cfg(test)]
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

    /// 第 `ix` 行的源码文本（仅该行切片，O(行长)）。
    pub fn line_str(&self, ix: usize) -> String {
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
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// 全文字节长（编辑/光标边界钳位用；rope 单真源，O(1)）。
    pub fn len(&self) -> usize {
        self.rope.len()
    }

    /// 全文（宿主镜像/测试/派生全量扫描用）。Str 源（load 后）O(1) 克隆；rope 源
    /// 走**版本化缓存**（键 `edit_ver`，edit 后失效）——保底任何遗漏调用点不再重复
    /// O(n) 拷贝（输入热路径已改走 UTF-16 索引/切片，见 `EntityInputHandler`）。
    pub fn text_shared(&self) -> SharedString {
        // 测试探针计数无条件自增（markdown-wysiwyg 测试跨 crate 观测；非热路径）。
        self.text_calls.set(self.text_calls.get() + 1);
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

    /// `text_shared` 被调次数（跨 crate 测试探针）——B 步断言
    /// `EntityInputHandler` 不再物化全文。markdown-wysiwyg 测试跨 crate 消费 → 无条件 pub。
    pub fn text_shared_calls(&self) -> u64 {
        self.text_calls.get()
    }

    /// 字节区间切片（O(区间长)；边界钳到 [0, len]，rope 单切一次）。
    pub fn slice_str(&self, range: Range<usize>) -> String {
        let r = range.start.min(self.len())..range.end.min(self.len());
        if r.end <= r.start {
            return String::new();
        }
        self.rope.slice(r).to_string()
    }
}

/// rope 的行统计融合一趟 chunk 扫描：与 `parse::build_line_stats_utf16`（Str 分支）
/// 同口径产出四值（`line_starts` u32 / 行粒度 UTF-16 前缀 / 标量字符数 / 全文
/// UTF-16 单元数），逐 chunk 推进绝对字节游标 `base`、跨 chunk 进位 `run_u16` /
/// `char_count`——rope 的 chunk 边界落在字符边界上（chunk 是合法 `&str`），逐引导
/// 字节折算与单缓冲逐值一致（有对拍测试背书）。`Doc::open_rope` 生产路径用。
fn build_line_stats_utf16_rope(rope: &Rope) -> (Vec<u32>, Vec<usize>, usize, usize) {
    let cap = rope.len() / 8 + 2;
    let mut line_starts = Vec::with_capacity(cap);
    let mut line_utf16_starts = Vec::with_capacity(cap);
    line_starts.push(0u32);
    line_utf16_starts.push(0usize);
    let mut run_u16 = 0usize;
    let mut char_count = 0usize;
    let mut base = 0usize; // 当前 chunk 的文档绝对字节起始
    for chunk in rope.chunks() {
        let bytes = chunk.as_bytes();
        let m = bytes.len();
        let mut j = 0usize;
        while j < m {
            let b = bytes[j];
            if b < 0x80 {
                char_count += 1;
                run_u16 += 1;
                if b == b'\n' {
                    line_starts.push((base + j + 1) as u32);
                    line_utf16_starts.push(run_u16);
                }
                j += 1;
            } else {
                // 非 ASCII：按 UTF-8 引导字节折算（同 Str 分支口径；chunk 边界在
                // 字符边界上，无跨 chunk 的半字符）。
                let l = if b < 0xC0 {
                    1
                } else if b < 0xE0 {
                    2
                } else if b < 0xF0 {
                    3
                } else {
                    4
                };
                char_count += 1;
                run_u16 += if l == 4 { 2 } else { 1 };
                j += l;
            }
        }
        base += m;
    }
    (line_starts, line_utf16_starts, char_count, run_u16)
}

/// 行粒度 UTF-16 前缀索引构建：第 i 项 = `line_starts[i]`（含幻影尾行）前的累计
/// UTF-16 单元数。`u16_of_range` 统计单行字节区间的 UTF-16 单元数。O(n)。
/// 仅**对拍测试**（`line_stats_fused_matches_old_multi_pass` 的旧四趟参考）消费；
/// `Doc::open` 走融合一趟 [`parse::build_line_stats_utf16`]、`open_rope` 走
/// [`build_line_stats_utf16_rope`]。
#[cfg(test)]
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

/// 逐块装配统一行 [`DocRow`]（`Doc::rows_in_block_window` 与预览窗口/全量 DocRow
/// 物化共用；M5-B5 P3-1a 渲染行源切到共享 DocRow 的装配核心）。
///
/// - kind 用 `row_kind`（与 `ir` 金标准同式，parity 钉死）；
/// - 行区间用**索引自身**（块 k 覆盖 `[src_line, 下一块 src_line)`；末块到
///   `line_count()` = `Doc::line_starts.len()` 口径）；
/// - `byte_map`/`table_rows` 语义 = [`Doc`] 编辑映射（与 WYSIWYG 同一份）。
pub(crate) fn assemble_docrow<V: DocLineView>(
    view: &V,
    index: &BlockIndex,
    k: usize,
    r: Row,
) -> DocRow {
    let kind = row_kind(r.kind);
    let n = index.blocks.len();
    let lines = if k + 1 < n {
        let next = index
            .blocks
            .get(k + 1)
            .map(|b| b.src_line as usize)
            .unwrap_or(r.src_line + 1);
        r.src_line..next.max(r.src_line + 1)
    } else {
        r.src_line..view.line_count().max(r.src_line + 1)
    };
    let code_lines = r.code_lines().to_vec();
    let byte_map = match r.kind {
        RowKind::Paragraph | RowKind::Heading(_) | RowKind::Bullet | RowKind::Quote => {
            let line = view.line_str(r.src_line);
            parse::text_segment(&line, r.kind)
                .map(|(off, seg)| {
                    parse::inline_style_mapped(seg).2.into_iter().map(|b| off + b).collect()
                })
                .unwrap_or_default()
        }
        RowKind::Code => code_byte_map(view, r.src_line, &code_lines),
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
            table_row_maps(view, r.src_line, n_data)
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
        align: r.align,
        notes: r.notes.clone(),
        note_defs: r.note_defs.clone(),
    }
}

/// 表格块：逐可见行（表头 + 各数据行，跳过分隔行）用
/// [`parse::split_table_cells_mapped`] 装配单元格编辑映射。
fn table_row_maps<V: DocLineView>(
    view: &V,
    src_line: usize,
    n_data: usize,
) -> Option<std::rc::Rc<Vec<TableRowMap>>> {
    let mut out = Vec::with_capacity(1 + n_data);
    for k in 0..=n_data {
        // k=0 表头（src_line）；数据行 i（0 基，k=i+1）源行 = src_line + 1 + k。
        let line_ix = if k == 0 { src_line } else { src_line + 1 + k };
        let line = view.line_str(line_ix);
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
fn code_byte_map<V: DocLineView>(
    view: &V,
    src_line: usize,
    code_lines: &[SharedString],
) -> Vec<usize> {
    let block_start = view.line_start(src_line);
    let mut map = Vec::new();
    for (k, line) in code_lines.iter().enumerate() {
        // 行与行之间的渲染 `\n`（首行前无分隔符）。
        if k > 0 {
            let prev_nl = view.line_start(src_line + 1 + k).saturating_sub(1);
            map.push(prev_nl.saturating_sub(block_start));
        }
        let line_start = view.line_start(src_line + 1 + k);
        for (i, _) in line.as_bytes().iter().enumerate() {
            map.push(line_start.saturating_sub(block_start) + i);
        }
    }
    let sentinel = view
        .line_start(src_line + 1 + code_lines.len())
        .saturating_sub(block_start);
    map.push(sentinel);
    map
}

/// 预览 `RowKind` → IR 语义（对齐子集上恒等；parity 保证二者一致）。
pub fn row_kind(k: RowKind) -> IrKind {
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

    /// 四条行统计「新旧口径对拍」：融合一趟 `parse::build_line_stats_utf16` 与旧的
    /// 四趟独立算法（`build_line_starts_u32` + 逐行 `encode_utf16().count()` +
    /// `chars().count()` + `encode_utf16().count()`），在多样本（ASCII / 中文 / CRLF /
    /// BOM / 空行 / 结尾换行 / 4 字节 emoji）与真实 6.4MB 文档上逐值相等。
    #[test]
    fn line_stats_fused_matches_old_multi_pass() {
        let samples = [
            "",
            "\n",
            "a",
            "ab\ncd",
            "ab\ncd\n",
            "line one\nline two\r\nline three\r\n",
            "中文文档第一行\n第二行 更多内容\n",
            "\u{feff}BOM 开头\n正文",
            "end with newline\n",
            "emoji 😀🚀 后续\n中 end\r\n",
            "a\r\n\r\nb\r\n",
            "\r\n",
            "x\nx\nx\ntail",
        ];
        for s in &samples {
            let (ls32, ls16, cc, total) =
                crate::markdown::parse::build_line_stats_utf16(s.as_bytes());
            let want_ls: Vec<usize> = crate::markdown::parse::build_line_starts_u32(s)
                .into_iter()
                .map(|b| b as usize)
                .collect();
            let want_ls16 = build_line_utf16_starts(
                |lo, hi| s[lo..hi].encode_utf16().count(),
                &want_ls,
                s.len(),
            );
            assert_eq!(
                ls32.into_iter().map(|b| b as usize).collect::<Vec<_>>(),
                want_ls,
                "line_starts {s:?}"
            );
            assert_eq!(ls16, want_ls16, "line_utf16_starts {s:?}");
            assert_eq!(cc, s.chars().count(), "char_count {s:?}");
            assert_eq!(total, s.encode_utf16().count(), "total_utf16 {s:?}");
        }
        // 真实 6.4MB 文档（fixtures 缺失时跳过）：融合一趟 vs 旧四趟 vs rope 全量算法。
        let p = r"fixtures/大文档测试2.md";
        if let Ok(s) = std::fs::read_to_string(p) {
            let (ls32, ls16, cc, total) =
                crate::markdown::parse::build_line_stats_utf16(s.as_bytes());
            let ls: Vec<usize> = ls32.into_iter().map(|b| b as usize).collect();
            let want_ls = crate::markdown::parse::build_line_starts_u32(&s)
                .into_iter()
                .map(|b| b as usize)
                .collect::<Vec<_>>();
            let want_ls16 = build_line_utf16_starts(
                |lo, hi| s[lo..hi].encode_utf16().count(),
                &want_ls,
                s.len(),
            );
            assert_eq!(ls, want_ls, "realfile line_starts");
            assert_eq!(ls16, want_ls16, "realfile line_utf16_starts");
            assert_eq!(cc, s.chars().count(), "realfile char_count");
            assert_eq!(total, s.encode_utf16().count(), "realfile total_utf16");
            // rope 全量算法（`open_rope`）逐值对拍。
            let rd = Doc::open_rope(ropey::Rope::from(s.as_str()));
            assert_eq!(rd.line_starts().to_vec(), ls, "realfile rope line_starts");
            assert_eq!(rd.line_utf16_starts().to_vec(), ls16, "realfile rope utf16");
            assert_eq!(rd.char_count(), cc, "realfile rope char_count");
            assert_eq!(rd.total_utf16(), total, "realfile rope total_utf16");
        }
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

    /// COW 语义回归：`Doc::open_rope` 持宿主 rope 的 COW 快照——`Doc::edit` 只改动
    /// 自己的趟副本（`new_rope = self.rope.clone()`，ropey 拓扑 COW），宿主共享的
    /// rope 原树逐字节不变；再对同一宿主 rope `open_rope` 得到独立新 Doc，互不串扰。
    /// （锁「open_rope 后编辑不污染共享 rope 源」的 doc.rs:467 同款语义。）
    #[test]
    fn open_rope_edit_does_not_pollute_host_rope() {
        let src = "# 标题\n段落 **加粗** `代码`\n\n```rust\nlet a = 1;\n```\n\ntail\n";
        let host = ropey::Rope::from(src);
        let mut doc = Doc::open_rope(host.clone());
        // 行内编辑（no-'\n'，尾部平移路径）+ 结构编辑（含 '\n'，区域重建路径）。
        let at = src.find("let a = 1;").unwrap();
        doc.edit(at..at + "let a = 1;".len(), "let b = 2;");
        let nl = src.find("\n\n```rust").unwrap() + 1;
        doc.edit(nl..nl, "插入行\n");
        // 宿主 rope 快照未被污染（COW）：内容与起始完全一致。
        assert_eq!(host.to_string(), src, "宿主 rope 必须保持原状（COW）");
        // 唯一真源读回：Doc 反映两次编辑。
        let want = src
            .replacen("let a = 1;", "let b = 2;", 1)
            .replace("\n\n```rust", "\n插入行\n\n```rust");
        assert_eq!(doc.text_shared(), SharedString::from(want.as_str()));
        // 再开同一宿主 rope → 独立新 Doc，不受上一 Doc 编辑影响。
        let doc2 = Doc::open_rope(host.clone());
        assert_eq!(doc2.text_shared(), SharedString::from(src));
        assert_eq!(doc2.len(), src.len());
    }

    /// `Doc::clone` 的「克隆即共享、编辑即 COW 分离」回归（共享复用右栏已解析 Doc 的
    /// 语义地基）：克隆一份 Doc，编辑克隆（行内 + 结构各一，覆盖无换行/含换行两路径），
    /// 原 Doc 逐字节不变；再对已编辑克隆二次克隆并编辑，三者互不串扰。Rope 源与 Str 源
    /// 各验一遍（Rope 源 = 大文档复用路径）。
    #[test]
    fn clone_edit_does_not_pollute_original() {
        let src = "# 标题\n段落 **加粗** `代码`\n\n```rust\nlet a = 1;\n```\n\ntail\n";
        let check = |doc: Doc, tag: &str| {
            let mut fork = doc.clone();
            // 行内编辑（no-'\n'）+ 结构编辑（含 '\n'），同 COW 测试的双路径。
            let at = src.find("let a = 1;").unwrap();
            fork.edit(at..at + "let a = 1;".len(), "let b = 2;");
            let nl = src.find("\n\n```rust").unwrap() + 1;
            fork.edit(nl..nl, "插入行\n");
            let want = src
                .replacen("let a = 1;", "let b = 2;", 1)
                .replace("\n\n```rust", "\n插入行\n\n```rust");
            // 原 Doc（克隆源）未受编辑影响；fork 反映两次编辑。
            assert_eq!(
                doc.text_shared(),
                SharedString::from(src),
                "{tag}: 原 Doc 必须保持原状（克隆共享、编辑分离）"
            );
            assert_eq!(
                fork.text_shared(),
                SharedString::from(want.as_str()),
                "{tag}: fork 应反映编辑"
            );
            assert_eq!(doc.len(), src.len(), "{tag}: 原 Doc len 不变");
            // 多代共享：对已编辑 fork 再克隆并编辑，前两代都不串扰。
            let mut fork2 = fork.clone();
            fork2.edit(0..0, "# 前缀\n");
            assert_eq!(
                fork.text_shared(),
                SharedString::from(want.as_str()),
                "{tag}: fork 不受 fork2 编辑影响"
            );
            assert_eq!(
                fork2.text_shared(),
                SharedString::from(format!("# 前缀\n{want}")),
                "{tag}: fork2 反映其编辑"
            );
            assert_eq!(
                doc.text_shared(),
                SharedString::from(src),
                "{tag}: 原 Doc 仍不受任何代影响"
            );
        };
        check(Doc::open(SharedString::from(src)), "Str 源");
        let host = ropey::Rope::from(src);
        check(Doc::open_rope(host.clone()), "Rope 源");
        assert_eq!(host.to_string(), src, "宿主 rope 未被克隆编辑污染（COW）");
    }

    /// Rope/Str 两构造路径逐项对拍（冷开 blocks-only 与 Str 口一致的抽取面）：
    /// 行统计 / 块序列 / `line_str` / `line_chars_ascii` 两分支逐值相等；
    /// 并含**跨 chunk 分裂行**（`RopeBuilder` 构造多叶 rope，长行跨叶边界）——
    /// `open_rope` 的行装配走物化跨叶行路径，须与 Str 分支逐字一致。
    #[test]
    fn open_rope_matches_str_doc_incl_cross_chunk_lines() {
        // 单叶样本：基础结构（标题/围栏/表格/引用/图片/CRLF/中文/emoji）。
        let samples = [
            "",
            "# 标题\r\n正文 **粗**\r\n\r\n```rust\r\nlet a = 1;\r\n```\r\n\r\n| A | B |\r\n|---|---|\r\n| 1 | 2 |\r\n\r\n> 引用\r\n\r\n![图](x.png)\r\n",
            "行零\n行一\n普通段落\nAB\nCD\n末尾行\n",
            "emoji 😀🚀 后\n中\r\n",
        ];
        // 多叶 rope：>1024B 无换行长行直接成叶在 1024 处截断 → 首行跨叶边界
        // （其余行在第二叶内）；再补一段普通文本（未跟踪的换行归属也在叶边界附近）。
        let mut builder = ropey::RopeBuilder::new();
        builder.append(&"x".repeat(2000));
        builder.append("\n第二行\n");
        let multi_leaf_rope = builder.finish();
        let multi_leaf_src = format!("{}\n第二行\n", "x".repeat(2000));
        assert_eq!(multi_leaf_rope.to_string(), multi_leaf_src);
        // 断言确实构造出跨叶行：首叶只含第一行前缀（无 '\n'）。
        assert!(!multi_leaf_rope.chunk(0).0.contains('\n'));
        let check = |str_doc: &Doc, rope_doc: &Doc, tag: &str| {
            assert_eq!(
                rope_doc.line_starts().to_vec(),
                str_doc.line_starts().to_vec(),
                "{tag}: line_starts"
            );
            assert_eq!(
                rope_doc.line_utf16_starts().to_vec(),
                str_doc.line_utf16_starts().to_vec(),
                "{tag}: line_utf16_starts"
            );
            assert_eq!(rope_doc.char_count(), str_doc.char_count(), "{tag}: char_count");
            assert_eq!(rope_doc.total_utf16(), str_doc.total_utf16(), "{tag}: total_utf16");
            assert_eq!(rope_doc.block_count(), str_doc.block_count(), "{tag}: block_count");
            let n = rope_doc.block_count();
            for i in 0..n {
                assert_eq!(
                    rope_doc.block_meta(i),
                    str_doc.block_meta(i),
                    "{tag}: block_meta[{i}]"
                );
            }
            let n_lines = rope_doc.line_count();
            for i in 0..n_lines {
                assert_eq!(
                    rope_doc.line_str(i),
                    str_doc.line_str(i),
                    "{tag}: line_str[{i}]"
                );
                assert_eq!(
                    rope_doc.line_chars_ascii(i),
                    str_doc.line_chars_ascii(i),
                    "{tag}: line_chars_ascii[{i}]"
                );
            }
        };
        for s in &samples {
            let str_doc = Doc::open(SharedString::from(s.to_string()));
            let rope_doc = Doc::open_rope(ropey::Rope::from(*s));
            check(&str_doc, &rope_doc, &format!("sample {s:?}"));
        }
        {
            let str_doc = Doc::open(SharedString::from(multi_leaf_src.as_str()));
            let rope_doc = Doc::open_rope(multi_leaf_rope);
            check(&str_doc, &rope_doc, "cross-chunk multi-leaf");
        }
    }
}
