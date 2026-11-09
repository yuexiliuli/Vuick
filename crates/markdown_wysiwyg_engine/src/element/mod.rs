//! WYSIWYG 元素引擎的**纯类型核心**：一次布局后的行网格（[`Frame`]）、全篇基础布局分块/绳
//! （[`BlockLayout`]）、折叠/重排计划（[`FoldPlan`]/[`FoldPlanSets`]/[`RevealPlan`]）、
//! 行/表格/卡片类型（[`ShapedRow`]/[`ShapedTable`]/[`RowExtra`]/[`BlockCard`] 等）与不依赖
//! 编辑器实体/窗口的纯几何（[`geom`]）与常量。
//!
//! 自 `markdown_wysiwyg` 的 `wysiwyg/element/mod.rs` 纯类型部分 git mv 拆出；`WyEditorElement`
//! （gpui `Element` 实现）与 `paint`/`hit`/`frame`/`shape` 留宿主。宿主 `markdown_wysiwyg`
//! 经 `pub(crate) use markdown_wysiwyg_engine::element::*` + `use geom::*`（连同 `hit::*`/
//! `paint::*`/`shape::*` 等重导出）保持 `crate::markdown::wysiwyg::element::*` 全部既有路径不变。
//!
//! **窗口化 shaping**：元素是 auto-grow（宿主滚动），整篇 shape 会在大文档上每帧
//! 卡死。这里复刻 `live` 的带机制——每帧只真正 shape **可视带**（`prepaint` 由
//! `bounds.origin.y` + `viewport_size` 推算并发布），带外块用 [`BlockMeta`] 的
//! `raw_len`/`aux` 估算高度占位（不 shape、不物化行），帧缓存按
//! `(content_gen, width, band)` 命中后整帧跳过重建。

use std::ops::Range;

use gpui::{Pixels, SharedString};

use markdown::ir::IrKind;
use markdown::parse::inline_style::MathAlign;
use markdown::parse::recognition::{AlertKind, LinkHit};
use markdown_wysiwyg_math::InlineMathText;
use markdown_wysiwyg_mermaid::MmLayout;

/// 坐标 / 几何纯函数（`source_to_display` / 代码块、表格块行组与单元格几何换算）。
/// 宿主经 `markdown_wysiwyg_engine::element::geom::X`（宿主 `element` 再 `use geom::*`）
/// 扁平消费；本 crate 不做 `pub use geom::*` 平铺——避免宿主 glob 链三层嵌套（glob 透传
/// 只到两层，见宿主 element shim 的注释）。
pub mod geom;

/// 行首 gutter 几何常量（paint 与 hit-test 共用，不得分开改）。
pub const GUTTER_MARK_X: f32 = 2.;
/// 警报行正文缩进宽（= shape 期 `x_off`）——gutter 点击区即 [padding, padding+此宽]。
pub const ALERT_GUTTER_W: f32 = 84.;
/// 标题折叠箭头 gutter 宽（= shape 期标题行 `x_off`）——箭头画在左缘、正文右移，
/// gutter 点击区即 [padding, padding+此宽]；与 `heading_chevron_hit`/`paint_heading_chevron` 共用。
pub const HEADING_GUTTER_W: f32 = 20.;

/// 一段排好的行布局是否含 `.notdef`（glyph id=0，元字形＝豆腐块）。Windows 冷启动时
/// 主字体缺、需走回退字体才有的字形（❗⚠✅ 等）首次解析可能直接给 `.notdef`（空心
/// 矩形）；含它的行若被 `RowShapeCache`（宿主）钉住则会整次运行不消失——Pass2/热身的命中
/// 判定与入缓存前都据此剔除，把该块转交宿主的 `NotdefRetry`「冷却一帧 → 新鲜平台
/// 重排」自愈（见宿主 `wysiwyg/cache.rs` 中该类型的文档：扰动 wrap-width 已被证伪，勿再用）。
pub fn layout_has_notdef(layout: &gpui::LineLayout) -> bool {
    layout.runs.iter().any(|r| r.glyphs.iter().any(|g| g.id.0 == 0))
}

/// 一帧后的表格渲染数据（shape 期算好列宽/全量单元格文本，paint 直接画网格）。
/// 与预览一致：列宽按内容自适应（不缩放、不省略），整表自然宽，超出由宿主裁剪。
#[derive(Clone)]
pub struct ShapedTable {
    pub widths: Vec<f32>,
    pub total_w: f32,
    pub row_h: Pixels,
    pub header_texts: Vec<gpui::SharedString>,
    pub rows_texts: Vec<Vec<gpui::SharedString>>,
}

/// 一帧后的图片渲染数据（显示框 + 源；题注文本经 `ShapedRow` 的 `wrapped`/`byte_map`
/// 走普通文本路径——可编辑，源即 alt，不再重复驻留）。
#[derive(Clone)]
pub struct ShapedImage {
    pub src: gpui::SharedString,
    pub w: f32,
    pub h: f32,
}

/// 表格单元格的编辑排版（shape 期算了对齐位移与排好版的单行，命中/光标列↔x 换算用）。
#[derive(Clone)]
pub struct TableCellLayout {
    /// 源码内容区间（相对该行行首）。
    pub src: Range<usize>,
    /// 单元格内文本起始 x（对齐后，相对单元格左缘）。
    pub text_x: f32,
    /// 渲染字节 → 源字节（相对该行行首）；长度 = 渲染文本长 + 1。
    pub map: std::rc::Rc<Vec<usize>>,
    /// 排好的单元格单行（`closest_index_for_x`/`x_for_index`）。
    pub line: gpui::ShapedLine,
}

/// 表格可编辑行的排版（表头 + 各数据行，与 `ShapedTable.rows_texts` 同构对齐）。
#[derive(Clone)]
pub struct TableRowCellsLayout {
    pub src_line: usize,
    pub cells: Vec<TableCellLayout>,
}

/// 表格块整块的单元格排版（`ShapedRow.table_edit`）。
#[derive(Clone)]
pub struct TableCellsLayout {
    pub rows: Vec<TableRowCellsLayout>,
}

/// 行首装饰（列表项目符号 / 任务框 / 警报标签）——paint 画在内容区左缘 gutter。
#[derive(Clone, Debug)]
pub enum RowGutter {
    /// 无序列表 `•`。
    Bullet,
    /// 有序列表数字（如 `1.`）。
    Ordered(String),
    /// 任务框（`- [x]` → `Some(true)` 已勾选）。
    Check { checked: bool },
    /// GitHub 警报（`> [!NOTE]` 等）；`fold` = 可折叠状态（`Some(true)` = 已折叠）。
    Alert { kind: AlertKind, fold: Option<bool> },
}

/// 行内 `![alt](src)`：源 src + 显示区（alt 文本覆盖），paint 加载后画缩略图。
#[derive(Clone, Debug)]
pub struct InlineImageHit {
    pub display_off: usize,
    pub len: usize,
    pub src: SharedString,
}

/// 可点击链接命中（显示字节区间 + 目标；hand 光标 + 点击导航）。
#[derive(Clone, Debug)]
pub struct RowLink {
    pub disp: Range<usize>,
    pub target: LinkHit,
}

/// 脚注标记（`[n]`）：显示字节区间 + 注记号。P4-2 注记迁入 WYSIWYG——paint（悬浮
/// 高亮/命中色）、命中、跳转共用同一几何（同 [`RowLink`] 模式）。
#[derive(Clone, Debug)]
pub struct NoteMarker {
    /// 显示字节区间（`[n]` 显示文本，含括号）。
    pub disp: Range<usize>,
    /// 注记号（`[^n]` / `[!n]` 里的数字）。
    pub n: usize,
    /// 是否末尾定义标记（`[!n]`）；false = 文内引用 `[^n]`。
    pub is_def: bool,
}

/// 块级卡片（数学块 / Mermaid 图）——把整块折叠成一张自绘卡片。
#[derive(Clone, Debug)]
pub enum BlockCard {
    Math { align: MathAlign, latex: String },
    /// mermaid 图卡片：源码不驻留（paint 只消费 `ShapedRow.mm` 的已排布局与标签）。
    Mermaid,
}

/// 每行的增强装饰/内嵌数据（默认全空；文本行按类型填充）。
#[derive(Clone, Default)]
pub struct RowExtra {
    pub gutter: Option<RowGutter>,
    pub links: Vec<RowLink>,
    pub note_markers: Vec<NoteMarker>,
    pub inline_maths: Vec<InlineMathText>,
    pub inline_images: Vec<InlineImageHit>,
    /// 非空 = 本块画成一张卡片（不再画文本），并跳过普通 paint。
    pub card: Option<BlockCard>,
}

/// 一次布局后的行网格（measure→prepaint→paint 跨阶段复用）。
///
/// **大文档窗口化**：`rows` 只含**带内**（可视带 + 缓冲）的整形行；带外块不物化
/// `ShapedRow`，靠 `base`（全篇基础布局，跨档持久）+ `deltas`（本帧动态覆盖）
/// 定位（命中、光标、hscroll 只需带内行；带外光标走 `src_line_top` 兜底）。
/// 滚动/带移动不重算全篇前缀和（Fenwick 点更新），7MB/15.9 万块 doc 从每档 ~950ms
/// 降到 ~10ms（带内整形成本）。
pub struct Frame {
    pub padding: Pixels,
    pub line_h: Pixels,
    /// 仅带内的整形行（含块级卡片：数学/Mermaid；折叠 body 0 高不物化）。`top` 已填有效值。
    pub rows: Vec<ShapedRow>,
    /// 全篇基础布局（内容/宽度代缓存，跨档持久；带内整形真高经 `set` 写回）。
    pub base: std::rc::Rc<std::cell::RefCell<BlockLayout>>,
    /// 本帧动态高度覆盖：`(块下标, 相对基础高的增量)`，按下标升序。
    /// 折叠 body → −base（0 高）；数学卡片 → 卡高−base。滚动期间内容代不变即恒定。
    pub deltas: std::rc::Rc<[(usize, f32)]>,
    pub total: Pixels,
    /// 本帧可视带（元素局部 y）；帧缓存键之一。
    pub band: (f32, f32),
    /// 本帧 shaping 宽（px）；帧缓存键之一。
    pub width: Pixels,
    /// 本帧内容代数；帧缓存键之一。
    pub rgen: u64,
    /// 本帧图片加载代数；帧缓存键之一（占位→真实尺寸后必须重建）。
    pub igen: u64,
    /// 本帧数学排版代数；帧缓存键之一（Loading→就绪后卡片高变大，必须重建）。
    pub mgen: u64,
    /// 本帧 mermaid 缩放代数；帧缓存键之一（滚轮缩放后卡高/几何按新比例重建）。
    pub zgen: u64,
}

impl Frame {
    /// `i` 左侧动态增量之和（按升序 `(块下标,增量)` 稀疏表二分）。
    fn delta_prefix(&self, i: usize) -> f32 {
        let d = &self.deltas;
        let k = d.partition_point(|&(ix, _)| ix < i);
        d[..k].iter().map(|&(_, v)| v).sum()
    }
    /// 下标恰为 `i` 的动态增量（无则 0）。
    fn delta_at(&self, i: usize) -> f32 {
        self.deltas.iter().find(|(ix, _)| *ix == i).map(|&(_, v)| v).unwrap_or(0.)
    }
    /// 全篇块总数。（供测试；生产用 `base.n()`。）
    #[allow(dead_code)]
    pub fn block_count(&self) -> usize {
        self.base.borrow().n()
    }
    /// 第 `i` 块有效顶 y（元素局部 px）= 基础前缀 + 动态增量前缀。
    /// （供测试断言带外 top 收敛；生产光标走 `src_line_top`。）
    #[allow(dead_code)]
    pub fn block_top(&self, i: usize) -> f32 {
        let base = self.base.borrow();
        base.top(i) + self.delta_prefix(i)
    }

    /// `[lo_px, hi_px)`（元素局部 y）内的块下标区间。有效 top = 基础前缀 + 增量前缀
    /// 且单调 → 二分（与宿主 `build_frame` 的 `eff_top`/`count_true` 同构；但用本帧已建好
    /// 的 `base` + `deltas`，不重建）——供宿主**滚动预热**定位「下一量子带」的块。
    pub fn band_block_range(&self, lo_px: f32, hi_px: f32) -> (usize, usize) {
        let n = self.base.borrow().n();
        let count_true = |pred: &dyn Fn(usize) -> bool| -> usize {
            let (mut l, mut r) = (0usize, n + 1);
            while l < r {
                let m = (l + r) / 2;
                if pred(m) {
                    l = m + 1;
                } else {
                    r = m;
                }
            }
            l
        };
        let i0 = count_true(&|k| self.block_top(k) <= lo_px).saturating_sub(1);
        let i1 = count_true(&|k| self.block_top(k) < hi_px).min(n);
        (i0, i1)
    }
    /// 第 `i` 块有效高（px）= 基础高 + 动态增量。（供测试断言带外高沿用收敛真值。）
    #[allow(dead_code)]
    pub fn block_height(&self, i: usize) -> f32 {
        let base = self.base.borrow();
        base.height(i) + self.delta_at(i)
    }
    /// 源码行 → 所在块下标（块按 `src_line` 升序连续覆盖源码行；越界归到首/末块）。
    pub fn block_index_for_src_line(&self, line: usize) -> Option<usize> {
        self.base.borrow().block_index_for_src_line(line)
    }
    /// 源码行所在块的有效顶 y（元素局部 px）——带外光标/命中（未物化行）的定位兜底。
    pub fn src_line_top(&self, line: usize) -> f32 {
        match self.block_index_for_src_line(line) {
            Some(i) => self.block_top(i),
            None => 0.,
        }
    }
}

/// 全篇块**基础布局**（按「内容/图片/数学/宽度」代每档重建一次，档内滚动/带移动
/// **持久复用**）：**分块/绳**——块切分成定长 `Chunk`（S2 pass1 绳化），中段结构编辑
/// 只动编辑边框块，尾块整块复用（只平移行偏移 `off`）+ 块级 Fenwick 前缀聚合 → 每键
/// O(#编辑块 + C) 而非平铺数组的 O(n) 整表重建（6.4MB/~13 万块 Enter 的 Fenwick+复制
/// ~7ms → 只走编辑邻域）。
///
/// 动态高度（折叠 body→0、数学卡片→卡高）不走这个基表——由 [`Frame::deltas`] 稀疏
/// 增量覆盖（按块下标升序），`block_top`/`block_height` 合并计算。`set(i,h)` 点更新
/// O(log C + 块长)，`top/total` 查询 O(log C)。
pub struct BlockLayout {
    /// 块切块（全局块下标 = 前缀块计数累计）。
    chunks: Vec<Chunk>,
    /// 块级计数 Fenwick（len = C+1）：定位全局块下标 → 所在 Chunk。
    cnt_fen: Vec<usize>,
    /// 块级高和 Fenwick（len = C+1）：`top/total` 前缀。
    sum_fen: Vec<f64>,
    /// 全局块总数。
    n: usize,
}

/// 单个块 Chunk：稠密块数组（高 + 起始源行）+ 行偏移（结构编辑尾部平移累加，读时
/// `s[i] + off`）+ 本地高前缀（`top` 的 O(1) 本地段）。`h/s/p` 用 `Rc` 背板：分块增量
/// 重建时尾部 Chunk **浅克隆共享**（每 Chunk O(1)，不复制数组），旧布局帧末释放后
/// 独享再被 `set` 就地写（COW）。
#[derive(Clone)]
struct Chunk {
    h: std::rc::Rc<[f32]>,
    s: std::rc::Rc<[u32]>,
    off: i64,
    p: std::rc::Rc<[f64]>,
}

impl Chunk {
    fn from_owned(h: Vec<f32>, s: Vec<u32>, off: i64) -> Chunk {
        let mut p = Vec::with_capacity(h.len() + 1);
        let mut acc = 0f64;
        p.push(0.0);
        for &v in &h {
            acc += v as f64;
            p.push(acc);
        }
        Chunk { h: h.into(), s: s.into(), off, p: p.into() }
    }
    fn from_slices(h: &[f32], s: &[u32], off: i64) -> Chunk {
        Chunk::from_owned(h.to_vec(), s.to_vec(), off)
    }
    fn eff_first(&self) -> usize {
        (self.s.first().copied().unwrap_or(0) as i64 + self.off).max(0) as usize
    }
    fn len(&self) -> usize {
        self.h.len()
    }
}

/// 默认块 Chunk 容量（≥1：空块不落 Chunk，避免零长）。
const CHUNK: usize = 128;

impl BlockLayout {
    /// 空布局（O(1)）：给「即将被 Pass1 重建」的草稿占位。
    pub fn empty() -> Self {
        Self { chunks: Vec::new(), cnt_fen: Vec::new(), sum_fen: Vec::new(), n: 0 }
    }
    /// 由整块高度 + 起始源码行建（首建/测试/宽度变化回退用；O(n) 打包成 Chunk）。
    pub fn new(heights: Vec<f32>, block_src_lines: Vec<u32>) -> Self {
        debug_assert_eq!(heights.len(), block_src_lines.len());
        let entries: Vec<(f32, u32)> = heights
            .iter()
            .zip(block_src_lines.iter())
            .map(|(&h, &s)| (h, s))
            .collect();
        let mut b = Self::empty();
        b.chunks = Self::pack(&entries);
        b.rebuild_index();
        b
    }
    pub fn n(&self) -> usize {
        self.n
    }
    /// 整表重建（首建/宽度变化/尾部对齐回退；O(n) 平铺 → Chunk 打包）。
    pub fn rebuild_from_flat(&mut self, heights: Vec<f32>, block_src_lines: Vec<u32>) {
        *self = BlockLayout::new(heights, block_src_lines);
    }
    /// 物化 `[lo, hi)` 块的 (高度, 有效起始源行) 平铺（回退/测试用；O(hi-lo)）。
    pub fn flat_range(&self, lo: usize, hi: usize) -> (Vec<f32>, Vec<u32>) {
        let lo = lo.min(self.n);
        let hi = hi.min(self.n).max(lo);
        let mut h = Vec::with_capacity(hi - lo);
        let mut s = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            h.push(self.height(i));
            s.push(self.src_line(i));
        }
        (h, s)
    }
    pub fn height(&self, i: usize) -> f32 {
        if i >= self.n {
            return 0.;
        }
        let (c, l) = self.chunk_index_of(i);
        self.chunks[c].h[l]
    }
    /// 第 `i` 块起始源行（块 s 原始值 + Chunk 行偏移）。
    pub fn src_line(&self, i: usize) -> u32 {
        if i >= self.n {
            return 0;
        }
        let (c, l) = self.chunk_index_of(i);
        (self.chunks[c].s[l] as i64 + self.chunks[c].off).max(0) as u32
    }
    /// 点更新第 `i` 块高（增量改块级高和 Fenwick + 本地前缀；差 ~0 跳过；COW：共享的
    /// 尾部 Chunk 首次写才克隆背板，之后就地）。
    pub fn set(&mut self, i: usize, h: f32) {
        if i >= self.n {
            return;
        }
        let (c, l) = self.chunk_index_of(i);
        let ch = &mut self.chunks[c];
        let old = ch.h[l];
        let d = h - old;
        if d.abs() < 1e-6 {
            return;
        }
        let hs = std::rc::Rc::make_mut(&mut ch.h);
        hs[l] = h;
        let ps = std::rc::Rc::make_mut(&mut ch.p);
        for t in l..hs.len() {
            ps[t + 1] = ps[t] + hs[t] as f64;
        }
        let mut j = c + 1;
        let dd = d as f64;
        let len = self.sum_fen.len();
        while j < len {
            self.sum_fen[j] += dd;
            j += j & (!j + 1);
        }
    }
    /// 第 `i` 块 top（= 前 i 块高之和，px）。
    pub fn top(&self, i: usize) -> f32 {
        if i >= self.n || i == 0 {
            return self.top_total(i);
        }
        let (c, l) = self.chunk_index_of(i);
        (self.sum_prefix(c) + self.chunks[c].p[l]) as f32
    }
    fn top_total(&self, i: usize) -> f32 {
        // i == 0 → 0；i >= n → total（块级高和 Fenwick 全量）。
        if i == 0 {
            0.
        } else {
            self.sum_prefix(self.chunks.len()) as f32
        }
    }
    /// 总高（px）。
    pub fn total(&self) -> f32 {
        self.sum_prefix(self.chunks.len()) as f32
    }
    /// 含源行 `line` 的块下标（分块：按 Chunk 首行二分 + 块内二分；越界归到首/末块）。
    pub fn block_for_line(&self, line: usize) -> usize {
        self.block_index_for_src_line(line).unwrap_or(0)
    }
    /// 源码行 → 所在块（同上；空布局 None）。
    pub fn block_index_for_src_line(&self, line: usize) -> Option<usize> {
        if self.chunks.is_empty() {
            return None;
        }
        let c = {
            let mut lo = 0usize;
            let mut hi = self.chunks.len();
            while lo < hi {
                let m = (lo + hi) / 2;
                if self.chunks[m].eff_first() <= line {
                    lo = m + 1;
                } else {
                    hi = m;
                }
            }
            lo.saturating_sub(1)
        };
        let cs = &self.chunks[c];
        let li = cs
            .s
            .partition_point(|&raw| (raw as i64 + cs.off).max(0) as usize <= line)
            .saturating_sub(1)
            .min(cs.s.len().saturating_sub(1));
        Some(self.cnt_prefix(c) + li)
    }

    /// **结构编辑**：`self`（重建草稿，通常为空）← 旧[0..keep)(浅克隆) ++ middle(打包)
    /// ++ 旧[j..)(尾部浅克隆 + Chunk 级 `off += line_delta`，O(1)/Chunk，不重排平铺数组)。
    /// `n_exp` = 预期新块数（debug 断言）。只做前缀 [0..keep)、尾部 [j..) 的**结构级浅拷贝**
    /// 与编辑边框块的 O(CHUNK) 切片——中段编辑 O(#编辑块 + C) 而非平铺 O(n)。
    pub fn apply_structural_edit(
        &mut self,
        old: &BlockLayout,
        keep: usize,
        middle: &[(f32, u32)],
        j: usize,
        line_delta: i64,
        n_exp: usize,
    ) {
        let n_old = old.n;
        debug_assert!(
            keep <= n_old && j <= n_old && keep <= j,
            "apply_structural_edit pre: keep={keep} j={j} n_old={n_old} n_exp={n_exp} mid={}",
            middle.len()
        );
        let mut chunks = old.chunks.clone();
        let (ci, local) = if keep >= n_old {
            (chunks.len(), 0)
        } else {
            let (cc, loc) = old.chunk_index_of(keep);
            if loc == 0 {
                (cc, 0)
            } else {
                (cc, loc)
            }
        };
        // 前缀 [0..keep)：Chunk 浅克隆（Rc 背板共享，O(C) 小结构体）；编辑边框块切两半。
        let mut tail = chunks.split_off(ci);
        let mut pre = chunks;
        if local > 0 {
            let c = tail.remove(0);
            pre.push(Chunk::from_slices(&c.h[..local], &c.s[..local], c.off));
            let right = Chunk::from_slices(&c.h[local..], &c.s[local..], c.off);
            tail.insert(0, right);
        }
        // 丢弃旧 [keep..j)（编辑区旧块），保留旧 [j..) 为尾部。
        let mut drop = j.saturating_sub(keep);
        let mut ti = 0usize;
        while drop > 0 && ti < tail.len() {
            let len = tail[ti].len();
            if len > drop {
                let (h, s, off) = {
                    let c = &tail[ti];
                    (c.h[drop..].to_vec(), c.s[drop..].to_vec(), c.off)
                };
                tail[ti] = Chunk::from_owned(h, s, off);
                break;
            }
            drop -= len;
            ti += 1;
        }
        if ti > 0 {
            tail.drain(..ti);
        }
        // 尾部整块行号平移：只动旧尾部 Chunk 的 `off`（middle 是新块 src，不得平移）。
        if line_delta != 0 {
            for c in &mut tail {
                c.off += line_delta;
            }
        }
        // middle 插在尾部之前（新 [keep..k) 块，src 已是新文档行号）。
        if !middle.is_empty() {
            let mid = Self::pack(middle);
            tail.splice(0..0, mid);
        }
        self.chunks = pre;
        self.chunks.extend(tail);
        self.rebalance();
        self.rebuild_index();
        debug_assert_eq!(self.n, n_exp);
    }

    fn pack(entries: &[(f32, u32)]) -> Vec<Chunk> {
        let mut out = Vec::new();
        let mut h = Vec::with_capacity(CHUNK);
        let mut s = Vec::with_capacity(CHUNK);
        for &(hh, ss) in entries {
            h.push(hh);
            s.push(ss);
            if h.len() == CHUNK {
                out.push(Chunk::from_owned(std::mem::take(&mut h), std::mem::take(&mut s), 0));
            }
        }
        if !h.is_empty() {
            out.push(Chunk::from_owned(h, s, 0));
        }
        out
    }
    /// 碎片化护栏：Chunk 数超过 `2*(n/CHUNK)+8`（多次中段 split 累积）→ 物化全部偏移后
    /// 重新整包（摊还 O(CHUNK)/次编辑，保 C = O(n/CHUNK)，定位/前缀仍 O(log C)）。
    fn rebalance(&mut self) {
        let n = self.chunks.iter().map(|c| c.h.len()).sum::<usize>();
        let cap = 2 * (n / CHUNK).max(1) + 8;
        if self.chunks.len() <= cap {
            return;
        }
        let mut entries = Vec::with_capacity(n);
        for c in &self.chunks {
            for i in 0..c.h.len() {
                entries.push((c.h[i], (c.s[i] as i64 + c.off).max(0) as u32));
            }
        }
        self.chunks = Self::pack(&entries);
    }
    /// 块级索引重建（O(C)）：计数/高和 Fenwick + 全局块数。
    fn rebuild_index(&mut self) {
        let c = self.chunks.len();
        let mut cnt_fen = vec![0usize; c + 1];
        let mut sum_fen = vec![0f64; c + 1];
        let mut n = 0usize;
        for (i, ch) in self.chunks.iter().enumerate() {
            let sz = ch.h.len();
            let hs = ch.p.last().copied().unwrap_or(0.0);
            n += sz;
            let ti = i + 1;
            cnt_fen[ti] += sz;
            sum_fen[ti] += hs;
            let parent = ti + (ti & ti.wrapping_neg());
            if parent <= c {
                cnt_fen[parent] += cnt_fen[ti];
                sum_fen[parent] += sum_fen[ti];
            }
        }
        self.cnt_fen = cnt_fen;
        self.sum_fen = sum_fen;
        self.n = n;
    }
    /// Chunk 的块计数前缀：chunks[0..m) 的块数（m ∈ [0, C]）。
    fn cnt_prefix(&self, m: usize) -> usize {
        let mut s = 0usize;
        let mut j = m.min(self.cnt_fen.len());
        while j > 0 {
            s += self.cnt_fen[j];
            j -= j & (!j + 1);
        }
        s
    }
    /// Chunk 的高和前缀：chunks[0..m) 的高之和（m ∈ [0, C]）。
    fn sum_prefix(&self, m: usize) -> f64 {
        let mut s = 0f64;
        let mut j = m.min(self.sum_fen.len());
        while j > 0 {
            s += self.sum_fen[j];
            j -= j & (!j + 1);
        }
        s
    }
    /// 全局块下标 → (Chunk 下标, 块内偏移)。Fenwick **二进制抬升**（select）O(log C)——
    /// 替代二分×Fenwick 查询的 O(log² C)（pass2/plan 逐块热路径的回归源）。
    fn chunk_index_of(&self, i: usize) -> (usize, usize) {
        let i = i.min(self.n.saturating_sub(1));
        let len = self.cnt_fen.len(); // C+1
        let mut pos = 0usize;
        let mut k = i;
        let mut bit = len.next_power_of_two() >> 1;
        while bit > 0 {
            let t = pos + bit;
            if t < len && self.cnt_fen[t] <= k {
                k -= self.cnt_fen[t];
                pos = t;
            }
            bit >>= 1;
        }
        // pos = 满足 cnt_prefix(pos) ≤ 原 i 的最大块计数位；块 i 落在 chunk pos。
        let c = pos;
        let base = self.cnt_prefix(c);
        (c, i.saturating_sub(base))
    }
}


/// 折叠计划的 **marker 集合**（行索引结构）：`math_start` / `math_body` / `fold_lines` /
/// `fold_bodies` / `fold_split`。只依赖 region/折叠**结构**（`regions_ver` / 行号平移 /
/// `fold_ver`），**与布局 epoch / mgen 无关**——同 `Rc` 被多个 `FoldPlan` 共享（M5-B3-1
/// 子项 2：布局换代/公式光栅就绪只重建 deltas，marker 集合跨计划复用，免每次 O(#区域)
/// 哈希重建）。不可变：构建后任何一行号/折叠态变化都会因缓存键（含行号平移版本）miss
/// 而重建整份。
pub struct FoldPlanSets {
    /// 数学块首源码行 → `math_regions` 下标（全部块；reveal 由 `RevealPlan` 覆盖）。
    pub math_start: rustc_hash::FxHashMap<usize, usize>,
    /// 数学块 body 行（折叠成 0 高）。
    pub math_body: rustc_hash::FxHashSet<usize>,
    /// 折叠 body 行（callout / 标题；折叠成 0 高）。
    pub fold_lines: rustc_hash::FxHashSet<usize>,
    /// 各折叠体的 body 行区间（reveal 找「caret 所在折叠体」用）。
    pub fold_bodies: Vec<Range<usize>>,
    /// `fold_bodies` 前缀中 alert 折叠体条数：`..fold_split` 为 alert 组、`fold_split..`
    /// 为标题组（各自升序且互不重叠、跨组可重叠）——`fold_reveal` 据此分组二分。
    pub fold_split: usize,
}

/// 折叠/重排计划：与**可视带无关且不含 caret**（内容、数学就绪、宽度、结构版本决定，
/// 键 = `(epoch, regions_ver, mgen, fold_ver)`），滚动/带移动/**光标移动/纯文本键入**都
/// 复用——旧实现把 caret 纳入键，每次光标动/每键都整算折叠增量（结构密集大文档
/// ~40-100ms/次 → 输入/方向键「巨大延迟」）。caret 的 reveal 由 `fold_reveal` 按带价
/// 优惠叠加（最多一个数学块 + 一个折叠体）。`sets` 为可共享的 marker 集合（见
/// [`FoldPlanSets`]）：布局/公式就绪重建只换 `deltas`，`sets` 跨计划复用。
pub struct FoldPlan {
    /// marker 集合（与可视带、caret、布局、公式就绪均无关，跨计划共享）。
    pub sets: std::rc::Rc<FoldPlanSets>,
    /// 排序的 `(块下标, Δ)`：折叠→−基础高、数学卡片→卡高−基础高。
    pub deltas: std::rc::Rc<[(usize, f32)]>,
    /// `deltas` 的前缀和表：`delta_pref[p]` = 前 p 项增量之和（p ∈ [0, len]）。
    /// **跨带/跨帧缓存**（与 `deltas` 同生命周期，构建时一次算好）：宿主 `build_frame` 的
    /// `find_band_range` 每帧用它做二分定位带区间——旧实现每帧新建 `Vec<f32>` 做 O(k)
    /// 分配与填充（带内 3000 条增量时 ~2.8ms/帧），滚动帧现直接只读复用本表。
    pub delta_pref: std::rc::Rc<[f32]>,
    /// `deltas` 求和（total = base.total + sum(deltas) - reveal；免每档 O(k) 重加）。
    pub delta_sum: f32,
}

/// 由**有序**增量表构建前缀和表：`[0, Δ0, Δ0+Δ1, …]`（长 = len+1）。`FoldPlan` 构建
/// 时算好（`delta_pref` 字段）跨带缓存；宿主 `find_band_range` 每帧只读复用（免 O(k) 分配）。
/// `frame` 里 reveal 覆盖产生的过滤增量表也用它就地重建前缀（reveal 帧罕见且表小）。
pub fn build_delta_pref(deltas: &[(usize, f32)]) -> std::rc::Rc<[f32]> {
    let mut v = Vec::with_capacity(deltas.len() + 1);
    let mut s = 0f32;
    v.push(0.);
    for &(_, d) in deltas.iter() {
        s += d;
        v.push(s);
    }
    v.into_boxed_slice().into()
}

/// caret 所在区域被「展开显示源码」（reveal-on-caret）：由 `fold_plan` 的全折叠基准
/// 上**按带现算**（最多一个数学块 + 一个折叠体，区域二分 O(log #区域) + 少量 block 查）。
pub struct RevealPlan {
    /// 被 reveal 的源码行区间（≤2 个：数学块区间 + 折叠体区间）。
    pub ranges: Vec<Range<usize>>,
    /// 块下标 → Δ 归 0 的块（reveal 的那块不再折叠成 0 / 卡片）。
    pub zeroed: Vec<usize>,
    /// 被归零的静态 Δ 之和（total 减去）。
    pub reveal_delta_sum: f32,
}

impl RevealPlan {
    pub fn empty() -> Self {
        Self { ranges: Vec::new(), zeroed: Vec::new(), reveal_delta_sum: 0. }
    }
    /// 该源码行是否被 reveal（不再折叠 / 不装卡片）。
    pub fn contains(&self, line: usize) -> bool {
        self.ranges.iter().any(|r| r.contains(&line))
    }
    pub fn is_empty(&self) -> bool {
        self.zeroed.is_empty()
    }
}


#[derive(Clone)]
pub struct ShapedRow {
    pub src_line: usize,
    pub src_lines: Range<usize>,
    /// 该块的可视行（`shape_text` 结果；一个块 = 一个或多个 `WrappedLine`）。
    /// 带外块为空（未 shape，paint 跳过）。**`Rc`**：行整形缓存跨帧共享整形结果
    /// （`WrappedLine` 本身不可 Clone；Rc 浅克隆即共享字形数据，滚动复用零重排）。
    pub wrapped: std::rc::Rc<Vec<gpui::WrappedLine>>,
    /// 渲染文本是否含非 ASCII 字节（回退字形候选）。纯 ASCII 行命中缓存时 `notdef`
    /// 扫描可直接短路 → 新增的命中检查对大多数文档零开销。
    pub has_non_ascii: bool,
    pub sub_heights: Vec<Pixels>,
    /// 编辑映射：渲染字节 → 源字节偏移（语义见 `DocRow::byte_map`）。
    /// 文本行为**相对该行**的局部映射；代码块为**相对块首**的块级映射。
    pub byte_map: std::rc::Rc<Vec<usize>>,
    pub editable: bool,
    /// （仅多源码行可编辑块，当前为代码块）每源码行在其整块渲染文本
    /// （代码行以 `\n` 连接）中的**起始渲染字节偏移**；长度 = 源码行数。
    /// 空 = 普通单源码行文本块（段落等折行仍只映射一行，用 `byte_map` 全宽映射）。
    pub multi_offsets: Vec<usize>,
    /// （仅文本行；已按 [`markdown::layout::expand_bg_over_whitespace`] 扩展空白）行内代码
    /// （反引号）在渲染文本中的字节区间——paint 前自绘圆角 pill 底。空 = 无行内代码。
    pub code_ranges: Vec<Range<usize>>,
    /// 该行实际行高（正文=para_line、标题=heading_line、代码=code_line 等）。
    pub lh: Pixels,
    /// 文本在内容区内的**额外左缩进**（padding 之外；引述=边条+内衬）。编辑可点行
    /// 的 caret/hit-test/paint 都要用它对齐，否则文本缩进但光标/点击仍在原处。
    pub x_off: Pixels,
    /// 行对齐（EPUB `%%ALIGN:X%%` / Heading 居中）的**逐可视行**水平移量（px；
    /// 与 `wrapped[i]` 对齐，`i` 超出取 0）：平日在内容盒内按此行宽右/中推。空 =
    /// 无对齐（普通行，恒左对齐，行为与改造前逐位一致）。
    pub align_offsets: Vec<Pixels>,
    pub height: Pixels,
    pub top: Pixels,
    /// 块语义（paint 按类型分派：代码面板/表格网格/图片）。
    pub kind: IrKind,
    /// Table 块的网格数据（带外块为 None）。
    pub table: Option<ShapedTable>,
    /// Table 块的单元格编辑排版（带外块为 None；表格可编辑靠它，`byte_map` 仍空）。
    pub table_edit: Option<TableCellsLayout>,
    /// Image 块的显示数据（带外块 / 加载失败仍 Some 显示 alt）。
    pub image: Option<ShapedImage>,
    /// mermaid 卡片的已排布局（卡片相对坐标；shape 期算好，paint 直接画，免每帧重排）。
    pub mm: Option<std::rc::Rc<MmLayout>>,
    /// 每行的增强装饰/内嵌数据（列表 gutter、链接、行内数学/图片、块级卡片）。
    pub extra: RowExtra,
}

/// 整行整形结果是否含 `.notdef`（扫描全部折行布局的 glyph id=0）。命中缓存的行与
/// 新整形入缓存的行都过此检查——含 notdef 的坏行不入缓存并进入重试（见宿主
/// `WyEditor` 的 notdef 自愈状态机：冷却一帧 → 新鲜平台重排），回退解析成功
/// 后的新鲜重排会自然取代它。
/// 纯 ASCII 行（`has_non_ascii == false`）直接短路——命中检查对主流文档零开销。
pub fn shaped_row_has_notdef(row: &ShapedRow) -> bool {
    if !row.has_non_ascii {
        return false;
    }
    row.wrapped
        .iter()
        .any(|wl| layout_has_notdef(&wl.unwrapped_layout))
}
