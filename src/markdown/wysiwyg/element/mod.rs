//! 新管线 WYSIWYG 自绘元素：把 [`Doc`] 统一的 `DocRow` 行排版成可编辑视图。
//! 数据完全来自本项目 `Doc`（`rows_in_block_window` + `byte_map`），零 zorite 依赖。
//!
//! **窗口化 shaping**：元素是 auto-grow（宿主滚动），整篇 shape 会在大文档上每帧
//! 卡死。这里复刻 `live` 的带机制——每帧只真正 shape **可视带**（`prepaint` 由
//! `bounds.origin.y` + `viewport_size` 推算并发布），带外块用 [`BlockMeta`] 的
//! `raw_len`/`aux` 估算高度占位（不 shape、不物化行），帧缓存按
//! `(content_gen, width, band)` 命中后整帧跳过重建。

use std::ops::Range;

use gpui::{
    App, Bounds, Corners, CursorStyle, DispatchPhase, Edges, Element, ElementId,
    ElementInputHandler, Entity, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, MouseMoveEvent, Pixels, ScrollWheelEvent, SharedString, TextAlign,
    Window, fill, point, px, relative, size,
};

/// 行首 gutter 几何常量（paint 与 hit-test 共用，不得分开改）。
pub(crate) const GUTTER_MARK_X: f32 = 2.;
/// 警报行正文缩进宽（= shape 期 `x_off`）——gutter 点击区即 [padding, padding+此宽]。
pub(crate) const ALERT_GUTTER_W: f32 = 84.;
/// 标题折叠箭头 gutter 宽（= shape 期标题行 `x_off`）——箭头画在左缘、正文右移，
/// gutter 点击区即 [padding, padding+此宽]；与 `heading_chevron_hit`/`paint_heading_chevron` 共用。
pub(crate) const HEADING_GUTTER_W: f32 = 20.;

pub(crate) use super::frame::{build_frame, prewarm_band, warm_emoji_rasterizer};
pub(crate) use super::math::*;
pub(crate) use super::mermaid::*;
pub(crate) use super::shape::*;
use super::{CaretVis, WyEditor, WyTheme};
use crate::markdown::ir::IrKind;
use crate::markdown::parse::recognition::{AlertKind, LinkHit};
use crate::markdown::parse::inline_style::MathAlign;

mod paint;
mod hit;
mod geom;
pub(crate) use geom::*;
pub(crate) use paint::*;
pub(crate) use hit::*;

/// 单行文本的显示宽（px，f32）——表格列宽测量用。
pub(crate) fn shaped_text_width(
    window: &mut Window,
    text: &str,
    font_size: Pixels,
    font: &gpui::Font,
    color: gpui::Hsla,
) -> f32 {
    let run = gpui::TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        ..Default::default()
    };
    window
        .text_system()
        .shape_line(gpui::SharedString::from(text), font_size, &[run], None)
        .width()
        .into()
}

/// 一段排好的行布局是否含 `.notdef`（glyph id=0，元字形＝豆腐块）。Windows 冷启动时
/// 主字体缺、需走回退字体才有的字形（❗⚠✅ 等）首次解析可能直接给 `.notdef`（空心
/// 矩形）；含它的行若被 `RowShapeCache` 钉住则会整次运行不消失——Pass2/热身的命中
/// 判定与入缓存前都据此剔除，把该块转交 [`super::NotdefRetry`]「冷却一帧 → 新鲜平台
/// 重排」自愈（参见 `mod.rs` 中该类型的文档：扰动 wrap-width 已被证伪，勿再用）。
pub(crate) fn layout_has_notdef(layout: &gpui::LineLayout) -> bool {
    layout.runs.iter().any(|r| r.glyphs.iter().any(|g| g.id.0 == 0))
}

/// 一帧后的表格渲染数据（shape 期算好列宽/全量单元格文本，paint 直接画网格）。
/// 与预览一致：列宽按内容自适应（不缩放、不省略），整表自然宽，超出由宿主裁剪。
#[derive(Clone)]
pub(crate) struct ShapedTable {
    pub widths: Vec<f32>,
    pub total_w: f32,
    pub row_h: Pixels,
    pub header_texts: Vec<gpui::SharedString>,
    pub rows_texts: Vec<Vec<gpui::SharedString>>,
}

/// 一帧后的图片渲染数据（显示框 + 源；题注文本经 `ShapedRow` 的 `wrapped`/`byte_map`
/// 走普通文本路径——可编辑，源即 alt，不再重复驻留）。
#[derive(Clone)]
pub(crate) struct ShapedImage {
    pub src: gpui::SharedString,
    pub w: f32,
    pub h: f32,
}

/// 表格单元格的编辑排版（shape 期算了对齐位移与排好版的单行，命中/光标列↔x 换算用）。
#[derive(Clone)]
pub(crate) struct TableCellLayout {
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
pub(crate) struct TableRowCellsLayout {
    pub src_line: usize,
    pub cells: Vec<TableCellLayout>,
}

/// 表格块整块的单元格排版（`ShapedRow.table_edit`）。
#[derive(Clone)]
pub(crate) struct TableCellsLayout {
    pub rows: Vec<TableRowCellsLayout>,
}

/// 行首装饰（列表项目符号 / 任务框 / 警报标签）——paint 画在内容区左缘 gutter。
#[derive(Clone, Debug)]
pub(crate) enum RowGutter {
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
pub(crate) struct InlineImageHit {
    pub display_off: usize,
    pub len: usize,
    pub src: SharedString,
}

/// 可点击链接命中（显示字节区间 + 目标；hand 光标 + 点击导航）。
#[derive(Clone, Debug)]
pub(crate) struct RowLink {
    pub disp: Range<usize>,
    pub target: LinkHit,
}

/// 块级卡片（数学块 / Mermaid 图）——把整块折叠成一张自绘卡片。
#[derive(Clone, Debug)]
pub(crate) enum BlockCard {
    Math { align: MathAlign, latex: String },
    /// mermaid 图卡片：源码不驻留（paint 只消费 `ShapedRow.mm` 的已排布局与标签）。
    Mermaid,
}

/// 每行的增强装饰/内嵌数据（默认全空；文本行按类型填充）。
#[derive(Clone, Default)]
pub(crate) struct RowExtra {
    pub gutter: Option<RowGutter>,
    pub links: Vec<RowLink>,
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
pub(crate) struct Frame {
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
    pub(crate) fn block_count(&self) -> usize {
        self.base.borrow().n()
    }
    /// 第 `i` 块有效顶 y（元素局部 px）= 基础前缀 + 动态增量前缀。
    /// （供测试断言带外 top 收敛；生产光标走 `src_line_top`。）
    #[allow(dead_code)]
    pub(crate) fn block_top(&self, i: usize) -> f32 {
        let base = self.base.borrow();
        base.top(i) + self.delta_prefix(i)
    }

    /// `[lo_px, hi_px)`（元素局部 y）内的块下标区间。有效 top = 基础前缀 + 增量前缀
    /// 且单调 → 二分（与 `build_frame` 的 `eff_top`/`count_true` 同构；但用本帧已建好
    /// 的 `base` + `deltas`，不重建）——供**滚动预热**定位「下一量子带」的块。
    pub(crate) fn band_block_range(&self, lo_px: f32, hi_px: f32) -> (usize, usize) {
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
    pub(crate) fn block_height(&self, i: usize) -> f32 {
        let base = self.base.borrow();
        base.height(i) + self.delta_at(i)
    }
    /// 源码行 → 所在块下标（块按 `src_line` 升序连续覆盖源码行；越界归到首/末块）。
    pub(crate) fn block_index_for_src_line(&self, line: usize) -> Option<usize> {
        self.base.borrow().block_index_for_src_line(line)
    }
    /// 源码行所在块的有效顶 y（元素局部 px）——带外光标/命中（未物化行）的定位兜底。
    pub(crate) fn src_line_top(&self, line: usize) -> f32 {
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
pub(crate) struct BlockLayout {
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
    pub(crate) fn empty() -> Self {
        Self { chunks: Vec::new(), cnt_fen: Vec::new(), sum_fen: Vec::new(), n: 0 }
    }
    /// 由整块高度 + 起始源码行建（首建/测试/宽度变化回退用；O(n) 打包成 Chunk）。
    pub(crate) fn new(heights: Vec<f32>, block_src_lines: Vec<u32>) -> Self {
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
    pub(crate) fn n(&self) -> usize {
        self.n
    }
    /// 整表重建（首建/宽度变化/尾部对齐回退；O(n) 平铺 → Chunk 打包）。
    pub(crate) fn rebuild_from_flat(&mut self, heights: Vec<f32>, block_src_lines: Vec<u32>) {
        *self = BlockLayout::new(heights, block_src_lines);
    }
    /// 物化 `[lo, hi)` 块的 (高度, 有效起始源行) 平铺（回退/测试用；O(hi-lo)）。
    pub(crate) fn flat_range(&self, lo: usize, hi: usize) -> (Vec<f32>, Vec<u32>) {
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
    pub(crate) fn height(&self, i: usize) -> f32 {
        if i >= self.n {
            return 0.;
        }
        let (c, l) = self.chunk_index_of(i);
        self.chunks[c].h[l]
    }
    /// 第 `i` 块起始源行（块 s 原始值 + Chunk 行偏移）。
    pub(crate) fn src_line(&self, i: usize) -> u32 {
        if i >= self.n {
            return 0;
        }
        let (c, l) = self.chunk_index_of(i);
        (self.chunks[c].s[l] as i64 + self.chunks[c].off).max(0) as u32
    }
    /// 点更新第 `i` 块高（增量改块级高和 Fenwick + 本地前缀；差 ~0 跳过；COW：共享的
    /// 尾部 Chunk 首次写才克隆背板，之后就地）。
    pub(crate) fn set(&mut self, i: usize, h: f32) {
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
    pub(crate) fn top(&self, i: usize) -> f32 {
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
    pub(crate) fn total(&self) -> f32 {
        self.sum_prefix(self.chunks.len()) as f32
    }
    /// 含源行 `line` 的块下标（分块：按 Chunk 首行二分 + 块内二分；越界归到首/末块）。
    pub(crate) fn block_for_line(&self, line: usize) -> usize {
        self.block_index_for_src_line(line).unwrap_or(0)
    }
    /// 源码行 → 所在块（同上；空布局 None）。
    pub(crate) fn block_index_for_src_line(&self, line: usize) -> Option<usize> {
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
    pub(crate) fn apply_structural_edit(
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


/// 折叠/重排计划：与**可视带无关且不含 caret**（内容、数学就绪、宽度、结构版本决定，
/// 键 = `(epoch, regions_ver, mgen, fold_ver)`），滚动/带移动/**光标移动/纯文本键入**都
/// 复用——旧实现把 caret 纳入键，每次光标动/每键都整算折叠增量（结构密集大文档
/// ~40-100ms/次 → 输入/方向键「巨大延迟」）。caret 的 reveal 由 `fold_reveal` 按带价
/// 优惠叠加（最多一个数学块 + 一个折叠体）。
pub(crate) struct FoldPlan {
    /// 数学块首源码行 → `math_regions` 下标（全部块；reveal 由 `RevealPlan` 覆盖）。
    pub math_start: rustc_hash::FxHashMap<usize, usize>,
    /// 数学块 body 行（折叠成 0 高）。
    pub math_body: rustc_hash::FxHashSet<usize>,
    /// 折叠 body 行（callout / 标题；折叠成 0 高）。
    pub fold_lines: rustc_hash::FxHashSet<usize>,
    /// 各折叠体的 body 行区间（reveal 找「caret 所在折叠体」用）。
    pub fold_bodies: Vec<Range<usize>>,
    /// 排序的 `(块下标, Δ)`：折叠→−基础高、数学卡片→卡高−基础高。
    pub deltas: std::rc::Rc<[(usize, f32)]>,
    /// `deltas` 求和（total = base.total + sum(deltas) - reveal；免每档 O(k) 重加）。
    pub delta_sum: f32,
}

/// caret 所在区域被「展开显示源码」（reveal-on-caret）：由 `fold_plan` 的全折叠基准
/// 上**按带现算**（最多一个数学块 + 一个折叠体，O(区域数) 线性扫 + 少量 block 查）。
pub(crate) struct RevealPlan {
    /// 被 reveal 的源码行区间（≤2 个：数学块区间 + 折叠体区间）。
    pub(crate) ranges: Vec<Range<usize>>,
    /// 块下标 → Δ 归 0 的块（reveal 的那块不再折叠成 0 / 卡片）。
    pub(crate) zeroed: Vec<usize>,
    /// 被归零的静态 Δ 之和（total 减去）。
    pub(crate) reveal_delta_sum: f32,
}

impl RevealPlan {
    pub(crate) fn empty() -> Self {
        Self { ranges: Vec::new(), zeroed: Vec::new(), reveal_delta_sum: 0. }
    }
    /// 该源码行是否被 reveal（不再折叠 / 不装卡片）。
    pub(crate) fn contains(&self, line: usize) -> bool {
        self.ranges.iter().any(|r| r.contains(&line))
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.zeroed.is_empty()
    }
}


#[derive(Clone)]
pub(crate) struct ShapedRow {
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
    /// （仅文本行；已按 [`crate::markdown::view::expand_bg_over_whitespace`] 扩展空白）行内代码
    /// （反引号）在渲染文本中的字节区间——paint 前自绘圆角 pill 底。空 = 无行内代码。
    pub code_ranges: Vec<Range<usize>>,
    /// 该行实际行高（正文=para_line、标题=heading_line、代码=code_line 等）。
    pub lh: Pixels,
    /// 文本在内容区内的**额外左缩进**（padding 之外；引述=边条+内衬）。编辑可点行
    /// 的 caret/hit-test/paint 都要用它对齐，否则文本缩进但光标/点击仍在原处。
    pub x_off: Pixels,
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
/// 新整形入缓存的行都过此检查——含 notdef 的坏行不入缓存并进入重试（见
/// [`super::WyEditor`] 的 notdef 自愈状态机：冷却一帧 → 新鲜平台重排），回退解析成功
/// 后的新鲜重排会自然取代它。
/// 纯 ASCII 行（`has_non_ascii == false`）直接短路——命中检查对主流文档零开销。
pub(crate) fn shaped_row_has_notdef(row: &ShapedRow) -> bool {
    if !row.has_non_ascii {
        return false;
    }
    row.wrapped
        .iter()
        .any(|wl| layout_has_notdef(&wl.unwrapped_layout))
}

/// 元素：auto-grow、宿主滚动。
pub(crate) struct WyEditorElement {
    pub editor: Entity<WyEditor>,
}

/// 元素一帧的 prepaint 产物：可点击链接的 hand 光标 hitbox（paint 期设光标样式）。
#[derive(Default)]
pub(crate) struct WyPrepaint {
    link_grips: Vec<Hitbox>,
    /// 图片调整条（右中边缘）的 hitbox：既触发 hit_test 变化（进/出该区即刷新光标），
    /// 也让 paint 用其 bounds 判定「指针落在调整区」→ 发窗口级 resize 光标。
    resize_grips: Vec<Hitbox>,
}

impl IntoElement for WyEditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WyEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = WyPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let editor = self.editor.clone();
        let mut style = gpui::Style::default();
        style.size.width = relative(1.).into();
        let id = window.request_measured_layout(style, move |known, available, window, cx| {
            let e = editor.read(cx);
            let width_definite = matches!(
                available.width,
                gpui::AvailableSpace::Definite(w)
                    if f32::from(w).is_finite() && f32::from(w) > 1.
            );
            let width = match available.width {
                gpui::AvailableSpace::Definite(w) => w,
                _ => known.width.unwrap_or(px(800.)),
            };
            let width = if f32::from(width).is_finite() && f32::from(width) > 1. {
                width
            } else {
                px(800.)
            };
            // 非确定宽度（Indefinite / 探测性 / `Definite(0px)` 等占位）一律不建帧：
            // 复用现成帧总高估算，只让真正的正 definite 宽度趟建帧。否则 0px/探测趟会
            // 以 `known`/800 兜底建一帧，下一趟 definite 真实宽又重建——帧缓存永不命中
            // （右栏 md_wy 宿主每键 ×N 次 build_frame，debug ~100-500ms/键，即 A2 引入
            // 的卡顿根因；实测根治后每键降到单次重建）。旧版仅在 `known` 为空时才跳过，
            // known 非空（如上一趟 800 兜底）时 0px 趟仍会建占位帧。
            let probe = !width_definite;
            // 帧缓存：内容/宽度/可视带/图片代数都没变 → 直接复用总高，不再整篇重建
            // （大文档的每帧“卡死”就是这里：每次 notify 都重排全 16 万行）。
            let rgen = e.content_gen();
            let igen = e.images_gen();
            let mgen = e.math_gen();
            let zgen = e.mermaid_zoom_gen();
            let band = e.shape_band().unwrap_or((0., 2048.));
            let cached_total = e
                .frame
                .borrow()
                .as_ref()
                .filter(|f| {
                    f.rgen == rgen
                        && f.igen == igen
                        && f.mgen == mgen
                        && f.zgen == zgen
                        && f.width == width
                        && f.band == band
                })
                .map(|f| f.total);
            let total = match cached_total {
                Some(t) => t,
                // 探测性测量：不建占位帧，直接返回现成总高 / 行数估算。
                None if probe => e
                    .frame
                    .borrow()
                    .as_ref()
                    .map(|f| f.total)
                    .unwrap_or(px((e.doc.line_count() as f32 * 18.).max(400.))),
                None => {
                    // 诊断：本次 measure 缓存未命中的原因（排查双重建/换宽；仅 WY_TRACE）。
                    if crate::prof::trace_active() {
                        let old = e.frame.borrow().clone();
                        let reason = old
                            .as_ref()
                            .map(|f| {
                                let mut v = Vec::new();
                                if f.rgen != rgen {
                                    v.push("rgen".into());
                                }
                                if f.igen != igen {
                                    v.push("igen".into());
                                }
                                if f.mgen != mgen {
                                    v.push("mgen".into());
                                }
                                if f.zgen != zgen {
                                    v.push("zgen".into());
                                }
                                if f.width != width {
                                    v.push(format!(
                                        "width:{:.0}!={:.0}",
                                        f32::from(f.width),
                                        f32::from(width)
                                    ));
                                }
                                if f.band != band {
                                    v.push(format!("band:{band:?}"));
                                }
                                v.join(",")
                            })
                            .unwrap_or_else(|| "no-frame".into());
                        crate::prof::op_detail_append("mmiss", &reason);
                    }
                    // 操作级实时计时：首帧（frame 从未建成）在此登记 open，让这次
                    // measure→prepaint 的整帧构建都计进同一次操作（输入类操作已在
                    // 事件入口 op_start，这里只补 frame 为空的初始构建）。
                    if std::env::var_os("WY_TRACE").is_some()
                        && !crate::prof::op_pending()
                        && e.frame.borrow().is_none()
                    {
                        crate::prof::op_start_detail(
                            "open",
                            &format!("band={band:?} width={:.0}", f32::from(width)),
                        );
                    }
                    let _t_bf = crate::prof::begin();
                    let frame = build_frame(e, window, width, band);
                    crate::prof::end("wy.measure.bf", _t_bf);
                    let total = frame.total;
                    let _ = e;
                    editor.update(cx, |er, _| {
                        *er.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
                    });
                    total
                }
            };
            size(width.max(px(1.)), total)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> WyPrepaint {
        let _t_pp = crate::prof::begin();
        // 可视带（窗口坐标 → 元素局部 y）：元素在滚动容器内的原点即滚动偏移；
        // 量程化使小滚动不动带（measure→prepaint 的带保持一致）。
        let vh = f32::from(window.viewport_size().height).max(1.);
        let top = (-f32::from(bounds.origin.y)).max(0.);
        let q = 512.;
        let band = (
            (((top - vh) / q).floor() * q).max(0.),
            ((top + 2. * vh) / q).ceil() * q,
        );
        self.editor.update(cx, |e, _| e.set_shape_band(Some(band)));

        // 带移动 / 内容 / 宽度 / 图片/数学/mermaid缩放 变化 → 按新状态重建窗口化帧。
        let rgen = self.editor.read(cx).content_gen();
        let igen = self.editor.read(cx).images_gen();
        let mgen = self.editor.read(cx).math_gen();
        let zgen = self.editor.read(cx).mermaid_zoom_gen();
        let width = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map(|f| f.width).unwrap_or(px(800.))
        };
        let stale = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().is_none_or(|f| {
                f.rgen != rgen
                    || f.igen != igen
                    || f.mgen != mgen
                    || f.zgen != zgen
                    || f.width != width
                    || f.band != band
            })
        };
        if stale {
            // 诊断：本次重建的诱因（排查 measure/prepaint 双重建；仅 WY_TRACE）。
            if crate::prof::trace_active() {
                let f = self.editor.read(cx).frame.borrow();
                let reason = Self::frame_stale_reason(
                    f.as_ref().map(|v| &**v),
                    rgen,
                    igen,
                    mgen,
                    zgen,
                    width,
                    band,
                );
                crate::prof::op_detail_append("stale", &reason);
            }
            // 操作级实时计时：无输入类操作在途时，按重建诱因标记 scroll/resize。
            // 输入类操作已在事件处理入口 `op_start`（type/newline/backspace…）；
            // 首帧（frame None）已在 measure 登记 open——这里只补纯滚动/尺寸帧。
            if !crate::prof::op_pending() {
                let old = self.editor.read(cx).frame.borrow().clone();
                let kind = match old.as_ref() {
                    Some(f) if f.rgen == rgen && f.band != band => "scroll",
                    _ => "resize",
                };
                crate::prof::op_start_detail(kind, &format!("band={band:?} top={top:.0}"));
            }
            let _t_bb = crate::prof::begin();
            let frame = build_frame(self.editor.read(cx), window, width, band);
            crate::prof::end("wy.prepaint.bf", _t_bb);
            self.editor.update(cx, |e, _| {
                *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
            });
        }

        // 字号缩放**视口锚定**（WyEditor 自驱动）：字号变化后 frame 逐帧把「估计高」
        // 细化为「整形真值」，`block_top` 缓慢变动——这里在 WY 自己的每帧 prepaint 里
        // 重钉视口顶，直到恢复目标收敛才消费锚点（对齐 `markdown/view`、`editor` 的
        // Ctrl+滚轮缩放不滚动）。`done` 前 `window.refresh()` 保证被钉的目标逐帧生效；
        // 收敛（`done`）后不再请求帧，避免空转。
        if let Some((target, done)) = self.editor.read(cx).advance_zoom_restore()
            && let Some((vh, pad_y)) = self.editor.read(cx).host_scroll() {
                vh.set_offset(gpui::point(gpui::px(0.), gpui::px(-(target + pad_y))));
                if !done {
                    window.refresh();
                }
            }

        // 编辑区⇄预览滚动同步（WY 自驱动落地）：宿主把目标行写进 `scroll_target`，
        // 在此每帧经 `advance_pending_scroll` 重钉宿主偏移直至稳住（仿缩放恢复收敛，
        // 规避「单次写宿主偏移被后续布局复位」）。目标在或刚清除都要刷下一帧。
        if self.editor.read(cx).scroll_target_active() {
            self.editor.update(cx, |e, _| e.advance_pending_scroll(window));
            window.refresh();
        }

        // 光标可视位置（元素局部）→ IME bounds。
        let frame = {
            let e = self.editor.read(cx);
            e.frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        if let Some(f) = frame.as_ref() {
            let editor = self.editor.read(cx);
            let (on, x, ctop) = caret_geometry(editor, f);
            super::set_caret_vis(CaretVis {
                x,
                top: if on { ctop } else { px(0.) },
                line_height: f.line_h,
            });
        }

        // C5: 行内链接的 hand 光标 hitbox（paint 期 set_cursor_style）。几何与
        // `link_hit_at` 一致（wrapped 首行的字形几何头/尾两段）。
        let mut link_grips = Vec::new();
        if let Some(f) = frame.as_ref() {
            link_grips = Self::link_hit_grips(f, bounds, window);
        }
        // D1：滚动预热——带穿越前把下一量子带块提前 shape 进缓存（预算内摊平突发）。
        if let Some(f) = frame.as_ref() {
            prewarm_band(self.editor.read(cx), f, window);
        }
        // 图片调整条（右中边缘）hitbox：可编辑模式对所有可见图片行登记。作用有二：
        // (a) 指针进/出该区时 `dispatch_mouse_event` 的 hit_test 变化 → 触发光标刷新；
        // (b) paint 用其 bounds 判定「指针在调整区」→ 发窗口级 `ResizeLeftRight` 光标。
        let mut resize_grips = Vec::new();
        if self.editor.read(cx).is_editable()
            && let Some(f) = frame.as_ref()
        {
            for row in &f.rows {
                if row.kind != crate::markdown::ir::IrKind::Image {
                    continue;
                }
                let Some(bb) = super::image_box_local(f, row) else {
                    continue;
                };
                let hb = super::image_resize_handle_window(bounds.origin, &bb);
                resize_grips.push(window.insert_hitbox(hb, HitboxBehavior::Normal));
            }
        }
        crate::prof::end("wy.prepaint", _t_pp);
        WyPrepaint {
            link_grips,
            resize_grips,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut WyPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _t_paint = crate::prof::begin();
        crate::prof::paint_mark();
        let (focus, theme) = {
            let e = self.editor.read(cx);
            (e.focus_handle(), e.theme.clone())
        };
        let frame = {
            let e = self.editor.read(cx);
            e.frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        let Some(f) = frame.as_ref() else {
            return;
        };
        // 记录本帧元素在窗口中的 bounds：`MouseDownEvent.position` 是窗口坐标，
        // `source_offset_at` 需用它把命中换算回元素局部坐标。
        self.editor
            .update(cx, |e, _| e.set_last_bounds(Some(bounds)));
        // 图片编辑卡片锚点：以**本帧** `bounds.origin` + 当前帧几何**同代配对**算出并
        // 缓存（render 只读缓存摆卡片）。⚠️ 不能由 render 现算 frame——`end_selection`
        // 清帧后 `ensure_frame_for_input` 用兜底 800 宽重建的临时帧与真实帧宽不同，
        // render 期每次读到不同宽度 → 卡片横向左右跳一帧。
        self.editor
            .update(cx, |e, _| {
                e.set_image_edit_anchor(
                    frame.as_ref().and_then(|f| e.image_edit_anchor_at(bounds.origin, f)),
                );
            });
        // 数学排版同步（可见带懒光栅）：只对**当前带内**公式发起排版（对齐
        // view/editor 只渲染可见区；不再开篇全量光栅 1.3 万公式占满后台 CPU）。
        let _t_sm = crate::prof::begin();
        self.editor
            .update(cx, |e, ecx| e.sync_math(ecx, f));
        crate::prof::end("wy.paint.synmath", _t_sm);
        // 代码块高亮后台解析（可见带懒加载）：消费本帧登记的 `request_code_parse`，
        // 后台跑 syntect 整块解析；就绪后定向作废该块整形缓存 → 下帧高亮行重建该块。
        let _t_sp = crate::prof::begin();
        self.editor
            .update(cx, |e, ecx| e.sync_code_parse(ecx));
        crate::prof::end("wy.paint.syncparse", _t_sp);
        let origin = bounds.origin;
        // 首帧光栅预热（一次/进程）：先打热 DWrite 彩字管线再画真实行（见
        // [`warm_emoji_rasterizer`]——冷进程首次 COLR 光栅失败 → emoji 变空白/方框）。
        let _t_warm = crate::prof::begin();
        warm_emoji_rasterizer(window);
        crate::prof::end("wy.paint.warmemoji", _t_warm);
        // 背景层：先给所有可见引述行画灰底带 + 左指示条（`py_1` 上下各越 4px，相邻带连片）。
        // ⚠️ 必须**先画全部背景再画全部文字**——若逐行「带 + 文字」连画，后一行的带会盖住
        // 前一行文字底部（引述第一行被第二行背景盖掉的 bug）。
        // callout 区域（marker + body 行）的左条统一用警报色，普通引述用灰条。
        // `alert_region_kinds` 是内容纯函数 → 走 derived 缓存（滚动/每帧不重扫全篇）。
        let derived = {
            let editor = self.editor.read(cx);
            editor.derived()
        };
        let alert_kinds = &derived.borrow().alert_region_kinds;
        self.paint_band_rows(origin, &theme, f, alert_kinds, window, cx);
        // C5: 悬浮在可点击链接上 → hand 光标。
        for hb in &prepaint.link_grips {
            window.set_cursor_style(CursorStyle::PointingHand, hb);
        }
        // 图片调整条：指针落在任一可见图片的右中边缘调整区（或正在拖拽）→
        // **窗口级** `ew-resize`（←||→）光标。⚠️ 不用 per-hitbox `set_cursor_style`：
        // hitbox 光标会被宿主 `#live-vscrollbar-overlay` 等顶层叠层 hitbox 压住而失效，
        // 窗口级 `set_window_cursor_style` 优先级最高；且只在命中此区时才发，其余位置
        // 沿用根元素的 IBeam。hitbox（`resize_grips`）负责触发进/出时的光标刷新。
        let mouse = window.mouse_position();
        let over_resize = self.editor.read(cx).image_resizing()
            || prepaint
                .resize_grips
                .iter()
                .any(|hb| hb.bounds.contains(&mouse));
        if over_resize {
            window.set_window_cursor_style(CursorStyle::ResizeLeftRight);
        }
        // 选区高亮（可编辑行与源选区相交的部分，按字形几何逐个 WrappedLine 画）。
        // 搜索跳转高亮（预览独有能力，搬入统一组件）：命中 → 以命中色复用同一几何。
        let hl = crate::markdown::view::selection_color(cx);
        if let Some(range) = editor_selection(&self.editor, cx) {
            paint_selection(&self.editor, origin, &theme, f, &range, hl, window, cx);
        }
        paint_search_highlight(&self.editor, origin, &theme, f, window, cx);
        // 光标几何每帧按 caret 现算（帧缓存不含 caret，改 caret 无需整帧重建）。
        // A1 只读门控：`editable=false` 不画光标（光标移动/选区仍工作，只是无竖条）。
        let (caret_on, caret_x, caret_top) = {
            let e = self.editor.read(cx);
            caret_geometry(e, f)
        };
        if self.editor.read(cx).is_editable() && caret_on && focus.is_focused(window) {
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(
                    point(origin.x + caret_x, origin.y + caret_top),
                    size(px(2.), f.line_h),
                ),
                corner_radii: gpui::Corners::default(),
                background: theme.text.into(),
                border_widths: gpui::Edges::default(),
                border_color: gpui::Hsla::default(),
                border_style: gpui::BorderStyle::Solid,
            });
        }
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        // 滚轮：**在宿主 zoom_overlay / 滚动容器之前**（bubble）拦截——光标悬停在某张
        // mermaid 卡片上时，普通滚轮=整卡缩放（`WyEditor::on_scroll_wheel` 返回 true →
        // `stop_propagation`，宿主不滚动）；其余位置放行（Ctrl 归全局字号缩放；
        // 非 Ctrl 归宿主滚动）。必须走 `window.on_mouse_event`：元素在宿主 `zoom_overlay`
        // 顶层兄弟之下，元素级 `.on_scroll_wheel` 会被遮罩吞掉（对比 `editor/view/render.rs`）。
        let wzeditor = self.editor.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if !matches!(phase, DispatchPhase::Bubble) {
                return;
            }
            let consumed = wzeditor.update(cx, |e, c| e.on_scroll_wheel(event, window, c));
            if consumed {
                cx.stop_propagation();
            }
        });
        // 图片悬浮态（编辑按钮亮显）：也走窗口级 mouse-move——元素在宿主
        // `#live-vscrollbar-overlay`（inset_0）等顶层叠层之下，元素级 `.on_mouse_move`
        // 会被遮罩吞掉（与上方滚轮同因）。Bubble 阶段读 `last_bounds` 判命中后更新
        // `hover_image`。拖选中（鼠标按下）由元素级拖选事件驱动，这里跳过。
        let wzed_hover = self.editor.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if !matches!(phase, DispatchPhase::Bubble) {
                return;
            }
            wzed_hover.update(cx, |e, c| e.on_window_mouse_move(event, c));
        });
        crate::prof::end("wy.paint", _t_paint);
        // 操作级实时计时结算：本帧若含未结算操作（type/backspace/scroll/open…），
        // 在此以「开始→绘制完成」的墙钟 + 帧内阶段 breakdown 落地一行 OP 记录。
        crate::prof::op_finish_after_paint();
    }
}

impl WyEditorElement {
    /// 本帧带重建的诱因（WY_TRACE 诊断）：frame `None` → `"no-frame"`，否则以逗号连接
    /// 变化的代标签（rgen/igen/mgen/zgen/width/band）。
    #[allow(clippy::too_many_arguments)] // 各代数标签 + 宽 + 带，纯只读诊断
    fn frame_stale_reason(
        old: Option<&Frame>,
        rgen: u64,
        igen: u64,
        mgen: u64,
        zgen: u64,
        width: gpui::Pixels,
        band: (f32, f32),
    ) -> String {
        old.map_or("no-frame".into(), |f| {
            let mut v = Vec::new();
            if f.rgen != rgen {
                v.push("rgen");
            }
            if f.igen != igen {
                v.push("igen");
            }
            if f.mgen != mgen {
                v.push("mgen");
            }
            if f.zgen != zgen {
                v.push("zgen");
            }
            if f.width != width {
                v.push("width");
            }
            if f.band != band {
                v.push("band");
            }
            v.join(",")
        })
    }

    /// 行内链接的 hand 光标 hitbox（paint 期 `set_cursor_style`）。几何与 `link_hit_at`
    /// 一致（wrapped 首行的字形几何头/尾两段）——折行时头段到行尾、尾段从行首起。
    fn link_hit_grips(
        f: &Frame,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
    ) -> Vec<gpui::Hitbox> {
        let mut link_grips = Vec::new();
        for row in &f.rows {
            if row.extra.links.is_empty() || row.wrapped.is_empty() {
                continue;
            }
            let Some(w0) = row.wrapped.first() else {
                continue;
            };
            let ox = bounds.origin.x + f.padding + row.x_off;
            let oy = bounds.origin.y + row.top + px(text_row_shift(row));
            for lk in &row.extra.links {
                let (Some(p0), Some(p1)) = (
                    w0.position_for_index(lk.disp.start, row.lh),
                    w0.position_for_index(lk.disp.end, row.lh),
                ) else {
                    continue;
                };
                if p0.y == p1.y {
                    let hit = Bounds::new(
                        point(ox + p0.x, oy + p0.y),
                        size((p1.x - p0.x).max(px(2.)), row.lh),
                    );
                    link_grips.push(window.insert_hitbox(hit, HitboxBehavior::Normal));
                } else {
                    // 折行链接：头段到行尾、尾段从行首起。
                    let head = Bounds::new(
                        point(ox + p0.x, oy + p0.y),
                        size((w0.width() - p0.x).max(px(2.)), row.lh),
                    );
                    let tail = Bounds::new(point(ox, oy + p1.y), size(p1.x.max(px(2.)), row.lh));
                    link_grips.push(window.insert_hitbox(head, HitboxBehavior::Normal));
                    link_grips.push(window.insert_hitbox(tail, HitboxBehavior::Normal));
                }
            }
        }
        link_grips
    }

    /// 逐行绘制：先给所有可见引述行画灰底带 + 左指示条，再逐行按类型自绘。
    /// ⚠️ 必须**先画全部背景再画全部文字**——若逐行「带 + 文字」连画，后一行的带会盖住
    /// 前一行文字底部（引述第一行被第二行背景盖掉的 bug）。
    /// callout 区域（marker + body 行）的左条统一用警报色，普通引述用灰条。
    #[allow(clippy::too_many_arguments)] // 原点 + 主题 + 帧 + 警报表 + 窗口 + ctx，自绘管线固有
    fn paint_band_rows(
        &self,
        origin: gpui::Point<gpui::Pixels>,
        theme: &WyTheme,
        f: &Frame,
        alert_kinds: &rustc_hash::FxHashMap<
            usize,
            crate::markdown::parse::recognition::AlertKind,
        >,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _t_row = crate::prof::begin();
        for row in &f.rows {
            if row.kind == IrKind::Quote && !row.wrapped.is_empty() {
                paint_quote_band(origin, theme, f, row, alert_kinds, window);
            }
        }
        for row in &f.rows {
            // 块级卡片（数学块 / Mermaid 图）优先自绘，跳过普通文本/代码路径。
            if let Some(card) = &row.extra.card {
                paint_card(&self.editor, origin, theme, f, row, card, window, cx);
                continue;
            }
            // 非文本块按类型自绘：代码面板 / 表格网格 / 图片。
            match row.kind {
                IrKind::Code => {
                    // 带外（未 shape）代码块不画（占位无内容）。
                    if row.wrapped.is_empty() {
                        continue;
                    }
                    paint_code_row(&self.editor, origin, theme, f, row, window, cx);
                    continue;
                }
                IrKind::Table => {
                    if let Some(t) = &row.table {
                        paint_table_row(&self.editor, origin, theme, f, row, t, window, cx);
                    }
                    continue;
                }
                IrKind::Image => {
                    if let Some(img) = &row.image {
                        paint_image_row(&self.editor, origin, theme, f, row, img, window, cx);
                    }
                    continue;
                }
                _ => {}
            }
            // 带外（未 shape）行不可见：跳过文本与背景，避免为整篇占位块画屏。
            if row.wrapped.is_empty() {
                continue;
            }
            // 行首装饰（列表符号 / 任务框 / 有序号）画在 gutter；警报 chrome 稍后叠画。
            paint_row_gutter(origin, theme, f, row, window, cx);
            // 引述块背景已在上面预画；这里只画文字 + 警报 chrome（标签/箭头叠在文字上）。
            if row.kind == IrKind::Quote {
                paint_text_row(origin, theme, f, row, window, cx);
                paint_alert_chrome(origin, theme, f, row, window, cx);
            } else {
                paint_text_row(origin, theme, f, row, window, cx);
                // 标题折叠箭头（标题保留行右缘 muted 小三角；点击翻转视图本地折叠态）。
                if matches!(row.kind, IrKind::Heading(_)) {
                    paint_heading_chevron(&self.editor, origin, theme, f, row, window, cx);
                }
            }
            // 行内数学 / 行内图片叠加（B1/B2）。
            paint_inline_maths(&self.editor, origin, theme, f, row, window, cx);
            paint_inline_images(&self.editor, origin, f, row, window, cx);
        }
        crate::prof::end("wy.paint.rowloop", _t_row);
    }
}

#[cfg(test)]
mod tests;




