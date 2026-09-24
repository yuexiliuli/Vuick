//! 新管线 WYSIWYG 自绘元素：把 [`Doc`] 统一的 `DocRow` 行排版成可编辑视图。
//! 数据完全来自本项目 `Doc`（`rows_in_block_window` + `byte_map`），零 zorite 依赖。
//!
//! **窗口化 shaping**：元素是 auto-grow（宿主滚动），整篇 shape 会在大文档上每帧
//! 卡死。这里复刻 `live` 的带机制——每帧只真正 shape **可视带**（`prepaint` 由
//! `bounds.origin.y` + `viewport_size` 推算并发布），带外块用 [`BlockMeta`] 的
//! `raw_len`/`aux` 估算高度占位（不 shape、不物化行），帧缓存按
//! `(content_gen, width, band)` 命中后整帧跳过重建。

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, Bounds, Corners, CursorStyle, Edges, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    Pixels, Point, RenderImage, SharedString, TextAlign, TextRun, Window, fill, point, px, relative,
    size,
};

/// 行首 gutter 几何常量（paint 与 hit-test 共用，不得分开改）。
const GUTTER_MARK_X: f32 = 2.;
/// 警报行正文缩进宽（= shape 期 `x_off`）——gutter 点击区即 [padding, padding+此宽]。
const ALERT_GUTTER_W: f32 = 84.;
/// 标题折叠箭头 gutter 宽（= shape 期标题行 `x_off`）——箭头画在左缘、正文右移，
/// gutter 点击区即 [padding, padding+此宽]；与 `heading_chevron_hit`/`paint_heading_chevron` 共用。
const HEADING_GUTTER_W: f32 = 20.;

use super::{notdef_debug, CaretVis, WyEditor, WyTheme};
use crate::markdown::doc::{DocPayload, DocRow};
use crate::markdown::ir::IrKind;
use crate::markdown::recognition::{self, AlertKind, LinkHit};
use crate::markdown::inline_style::{self, MathAlign, MathRegion};
use crate::markdown::parser::{BlockMeta, RowKind, RunKind, TableAlign};
use crate::syntax::{github_theme_cached, parse_code_lines, resolve_line_styles_with};

#[path = "paint.rs"]
mod paint;
#[path = "hit.rs"]
mod hit;
pub(crate) use paint::*;
pub(crate) use hit::*;

/// 单行文本的显示宽（px，f32）——表格列宽测量用。
fn shaped_text_width(
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

/// 一帧后的图片渲染数据（显示框 + 源 + alt 兜底）。
#[derive(Clone)]
pub(crate) struct ShapedImage {
    pub src: gpui::SharedString,
    pub alt: gpui::SharedString,
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

/// 行内 `$…$` 数学：splice 后的 spacer 位置 + 要绘制的 LaTeX 文本。
/// （不 derive Debug：`RenderImage` 未实现 `Debug`。）
#[derive(Clone)]
pub(crate) struct InlineMathText {
    /// spacer 起始渲染字节。
    pub display_off: usize,
    /// spacer 字节数（= 空格数；双击命中盒宽度用它）。
    pub len: usize,
    /// 该公式在**源文档中的字节区间**（含定界符 `$…$`）——双击编辑时把光标塞进它、
    /// 并在 shape 期 reveal（reveal-on-caret 只对源区间内的共享，放智能回源）。
    pub source: Range<usize>,
    /// 去定界符后的 LaTeX（paint 以斜体排版绘制）。
    pub latex: SharedString,
    /// **排版槽键**（`inline:` + 去定界符 LaTeX）——`sync_math`（可见带懒光栅）按此
    /// claim 占槽/光栅；`latex` 是 Unicode 展示态、不可当键用。
    pub key: SharedString,
    /// 已就绪的排版位图 + 逻辑（typeset em）px 尺寸；`Some` 时 paint 画 `RenderImage`
    /// 覆盖 spacer（参考 zorite `shape_inline_math`），`None` = 排版中/失败 → 画斜体 Unicode。
    /// `w`/`h` 是 22px 排版下的原始尺寸，画图按 `inline_math_fit_h` 再缩放。
    pub ready: Option<(Arc<RenderImage>, f32, f32)>,
    /// 所在 `row.wrapped` 的 BlockLine 索引（`shape_text_row` 撑高后填，paint 定位用）。
    pub line: usize,
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
        let base = self.base.borrow();
        let sl = base.src_lines();
        if sl.is_empty() {
            return None;
        }
        let i = sl
            .partition_point(|&s| (s as usize) <= line)
            .saturating_sub(1)
            .min(sl.len() - 1);
        Some(i)
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
/// **持久复用**）：`heights` 每块基础高（估算或整形真值），Fenwick 维护前缀和。
///
/// 动态高度（折叠 body→0、数学卡片→卡高）不走这个基表——由 [`Frame::deltas`] 稀疏
/// 增量覆盖（按块下标升序），`block_top`/`block_height` 合并计算。好处：滚动/带移动
/// **不再对整个 O(n) 高度数组重算前缀和**（旧实现 7MB/15.9 万块 ~40ms/档）。
/// `set(i,h)` 点更新 O(log n)（新块整形真值收敛），`top/total` 查询 O(log n)。
pub(crate) struct BlockLayout {
    heights: Vec<f32>,
    fen: Vec<f64>,
    block_src_lines: Vec<u32>,
    /// **稠密行→块索引**：`line_block[line]` = 含该源行的块下标（O(1) 查）。
    /// 结构编辑（Pass1 rebuild）时随块表一次填充（O(行数)），跨结构编辑的 band 移动/
    /// fold_plan 重建 / fold_reveal 全部复用——替代按查询行做 `partition_point`
    /// O(log n)（9000 查询 × 17 步 ≈ Enter 时 fp.dyn 的 6-10ms 主成本，改为
    /// 9000×O(1) + 一次 O(n) 填充 ≈ <1ms）。
    line_block: Vec<u32>,
}

impl BlockLayout {
    /// 由整块高度 + 起始源码行建（**O(n)** Fenwick 建表：每个元素只做一次祖先上推，
    /// 替代每元素 while 爬整条祖先链的 O(n log n)——10.7 万块每次 Pass1 重建 ~40ms→~1ms）。
    /// `doc_lines`：源文档总行数（稠密 `line_block` 的范围）。
    pub(crate) fn new(heights: Vec<f32>, block_src_lines: Vec<u32>, doc_lines: usize) -> Self {
        let n = heights.len();
        // 标准 O(n) 建树：tree[i] 累计 a[i-1] 后把部分和上推到父节点。
        let mut fen = vec![0f64; n + 1];
        for (i, &h) in heights.iter().enumerate() {
            let ti = i + 1;
            fen[ti] += h as f64;
            let parent = ti + (ti & ti.wrapping_neg()); // lowbit(ti)，与查询/点更新的 `j & (!j+1)` 同构
            if parent <= n {
                fen[parent] += fen[ti];
            }
        }
        // 稠密行→块：块按 src_line 升序连续覆盖，游标一次填满。
        let mut line_block = vec![0u32; doc_lines.max(1)];
        let mut blk = 0usize;
        for l in 0..doc_lines.min(line_block.len()) {
            while blk + 1 < block_src_lines.len() && (block_src_lines[blk + 1] as usize) <= l {
                blk += 1;
            }
            line_block[l] = blk as u32;
        }
        Self {
            heights,
            fen,
            block_src_lines,
            line_block,
        }
    }
    /// 整表重建（同代内容变更/首次打开用；O(n)）。
    pub(crate) fn rebuild(
        &mut self,
        heights: Vec<f32>,
        block_src_lines: Vec<u32>,
        doc_lines: usize,
    ) {
        *self = BlockLayout::new(heights, block_src_lines, doc_lines);
    }
    /// 含源行 `line` 的块下标（O(1) 稠密查；越界归到首/末块）。
    pub(crate) fn block_for_line(&self, line: usize) -> usize {
        let blk = self.line_block.get(line).copied().unwrap_or(0) as usize;
        blk.min(self.heights.len().saturating_sub(1))
    }
    /// 点更新第 `i` 块高（增量改前缀和；差 ~0 跳过）。
    pub(crate) fn set(&mut self, i: usize, h: f32) {
        let old = self.heights[i];
        let d = h - old;
        if d.abs() < 1e-6 {
            return;
        }
        self.heights[i] = h;
        let mut j = i + 1;
        let dd = d as f64;
        while j < self.fen.len() {
            self.fen[j] += dd;
            j += j & (!j + 1);
        }
    }
    /// 第 `i` 块 top（= 前 i 块高之和，px）。
    pub(crate) fn top(&self, i: usize) -> f32 {
        let mut s = 0f64;
        let mut j = i.min(self.heights.len());
        while j > 0 {
            s += self.fen[j];
            j -= j & (!j + 1);
        }
        s as f32
    }
    /// 总高（px）。
    pub(crate) fn total(&self) -> f32 {
        self.top(self.heights.len())
    }
    pub(crate) fn height(&self, i: usize) -> f32 {
        self.heights.get(i).copied().unwrap_or(0.)
    }
    pub(crate) fn heights(&self) -> &[f32] {
        &self.heights
    }
    pub(crate) fn n(&self) -> usize {
        self.heights.len()
    }
    pub(crate) fn src_lines(&self) -> &[u32] {
        &self.block_src_lines
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
    pub mm: Option<std::rc::Rc<(Vec<MmPlaced>, Vec<(usize, usize, Option<String>)>)>>,
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

/// 该行是否用到了**回退字体**：同时存在「应用解析过的 run（`get_font_for_id` 有值，
/// 正文/粗斜体/等宽）」与「平台内部新建的 run（查不到，即 DWrite 回退字形）」。
/// 冷启动首次 shape 回退字形（❗⚠✅ 等）时 DWrite 可能给「错误回退字体的非零占位字形」
/// （glyph id≠0，逃过 `.notdef` 检测）→ 渲染方框且会被直接缓存（caret 行正常 / 非
/// caret 行方框的翻转就是它）。用它识别这些行 → 先校验重排再入缓存。纯 CJK（全 Some）、
/// 测试 mock（全 None，其 layout 恒 FontId(0)）与纯 emoji 行不触发。
fn row_uses_fallback(ts: &gpui::WindowTextSystem, _base: &gpui::Font, srow: &ShapedRow) -> bool {
    if !srow.has_non_ascii {
        return false;
    }
    let mut known = false;
    let mut unknown = false;
    for wl in srow.wrapped.iter() {
        for r in &wl.unwrapped_layout.runs {
            if ts.get_font_for_id(r.font_id).is_some() {
                known = true;
            } else {
                unknown = true;
            }
            if known && unknown {
                return true;
            }
        }
    }
    false
}

/// 诊断：`WY_NOTDEF_DEBUG=1` 时输出含非 ASCII 字形的行的真实整形结果（每 run 的
/// 字体名/font_id/is_emoji/glyph id）。只在「字形签名变化」时打印一次，配合点击复现
/// 「caret 行正常 / 非 caret 行方框」的翻转——方框行与正常行的 run 字体/glyph 差异会
/// 直接暴露（例如冷启动错误回退字体 vs 正确的 Segoe UI Emoji，glyph id 都非 0）。
static LAST_SPECIAL_ROW: std::sync::Mutex<Option<(usize, String)>> =
    std::sync::Mutex::new(None);
fn debug_special_row(
    editor: &WyEditor,
    window: &Window,
    rl: usize,
    srow: &ShapedRow,
    base_font: &gpui::Font,
) {
    if std::env::var_os("WY_NOTDEF_DEBUG").is_none() {
        return;
    }
    let txt = editor.doc().line_str(rl);
    if txt.bytes().all(|b| b.is_ascii()) {
        return;
    }
    let ts = window.text_system();
    let base_id = ts.resolve_font(base_font);
    let mut sig = String::new();
    let mut parts = Vec::new();
    for wl in srow.wrapped.iter() {
        for run in &wl.unwrapped_layout.runs {
            let fam = ts
                .get_font_for_id(run.font_id)
                .map(|f| f.family.to_string())
                .unwrap_or_else(|| format!("#{}", run.font_id.0));
            let fb = if run.font_id != base_id { "*FB" } else { "" };
            let mut glyphs = String::new();
            let mut last_idx = None;
            for g in &run.glyphs {
                if last_idx == Some(g.index) {
                    continue;
                }
                last_idx = Some(g.index);
                if !glyphs.is_empty() {
                    glyphs.push(',');
                }
                glyphs.push_str(&format!("{}:{}", g.index, g.id.0));
            }
            sig.push_str(&format!("|{}:{}:{}", run.font_id.0, fam, glyphs));
            parts.push(format!(
                "{} id={} emo={}{} [{}]",
                fam,
                run.font_id.0,
                run.glyphs.iter().any(|g| g.is_emoji),
                fb,
                glyphs
            ));
        }
    }
    {
        let mut last = LAST_SPECIAL_ROW.lock().unwrap();
        if last.as_ref().is_some_and(|(lr, ls)| *lr == rl && *ls == sig) {
            return;
        }
        *last = Some((rl, sig));
    }
    let sample: String = txt.chars().take(40).collect();
    notdef_debug(format_args!(
        "row rl={rl} base=#{} {}",
        base_id.0,
        parts.join(" | ")
    ));
    notdef_debug(format_args!("  text={sample:?}"));
}

/// `DocRow.runs` → gpui `TextRun`（`RunKind` → 视觉样式）。
/// 行内代码：等宽字体 + `code_inline_fg` 文字色（**无 run 背景**——圆角 pill 由
/// [`paint_text_row`] 在 paint 阶段按像素几何自绘，与预览 [`TextTrack::paint_code_ranges`]
/// 同式；`code_inline_*` 单一源 [`super::WyTheme`] ← [`StyleParams`]）。
fn runs_to_text_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    color: gpui::Hsla,
    mono: &gpui::Font,
    _accent: gpui::Hsla,
    link: gpui::Hsla,
    code_inline_fg: gpui::Hsla,
) -> Vec<gpui::TextRun> {
    use gpui::{StrikethroughStyle, TextRun, UnderlineStyle};
    let len = text.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    for (range, kind) in runs {
        let s = range.start.min(len);
        let e = range.end.min(len);
        if s > i {
            out.push(TextRun {
                len: s - i,
                font: base.clone(),
                color,
                ..Default::default()
            });
        }
        let mut tr = TextRun {
            len: e.saturating_sub(s),
            font: base.clone(),
            color,
            ..Default::default()
        };
        match kind {
            RunKind::Bold => tr.font.weight = gpui::FontWeight::BOLD,
            RunKind::Italic => tr.font.style = gpui::FontStyle::Italic,
            RunKind::InlineCode => {
                tr.font = mono.clone();
                tr.color = code_inline_fg;
            }
            RunKind::Strikethrough => {
                tr.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                });
            }
            RunKind::Link => {
                tr.color = link;
                tr.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(link),
                    wavy: false,
                });
            }
            // 行内图片：alt 只是加载完成前的占位文字——正文色、无下划线（不冒充链接）。
            RunKind::Image => {}
        }
        out.push(tr);
        i = e;
    }
    if i < len {
        out.push(TextRun {
            len: len - i,
            font: base.clone(),
            color,
            ..Default::default()
        });
    }
    out
}

/// 表格块的总高（px）：与预览**实际渲染**同式 `(1 + 数据行数) × 行高 + 行分隔数 + 2`
/// （行高 = 正文行高 `para_line` + 12，外边框 2px；见 `view::table::render_table`）。
/// `estimate_height` 与 `shape_docrow` 用同一公式 → 带内/带外高度一致（滚动不跳）。
fn table_block_height(theme: &WyTheme, n_rows: usize) -> Pixels {
    let row_h = f32::from(theme.para_line()) + 12.;
    px((1. + n_rows as f32) * row_h + n_rows as f32 + 2.)
}

/// 表格单行高（表头/数据行同高）＝正文行高 + 12（预览 `render_table` 的 `row_h`）。
fn table_row_h(theme: &WyTheme) -> Pixels {
    px(f32::from(theme.para_line()) + 12.)
}

/// 带外（且从未整形过）块的高度估算：用**真实字数**（`Doc::line_chars_ascii`）
/// 与近似字符宽（ASCII ≈ 0.5 字高、含 CJK ≈ 0.95 字高）估可视行数，尽量贴合真实
/// shape，使带外/带内高度差 ≈ 0（滚动带边界不再跳「字体大小」）；代码用
/// `(行数+2) × code_line`（与预览同式）、表格用 [`table_block_height`]、
/// 标题用各级行高 + 底部留白；图片用占位高度；空行/分隔线固定一行。
/// 只作兜底——整形过的块走缓存真值。
fn estimate_height(
    meta: &BlockMeta,
    chars: usize,
    ascii: bool,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> Pixels {
    let est_char_w = |ascii: bool, fs: Pixels| {
        if ascii {
            f32::from(fs) * 0.5
        } else {
            f32::from(fs) * 0.95
        }
    };
    let est_rows = |cpl: f32, chars: usize| -> f32 { (chars as f32 / cpl.max(1.)).ceil().max(1.) };
    match meta.kind {
        RowKind::Code => theme.code_line() * (meta.aux.max(1) as f32 + 2.),
        // 与 `shape_table_row` 同式：网格高 + 底部滚动条预留带（`table_hscroll` 用，
        // 恒定常数，带内外一致 → 滚动不跳）。
        RowKind::Table => {
            table_block_height(theme, meta.aux.max(1) as usize)
                + px(crate::config::markdown::TABLE_SCROLLBAR_RESERVE_PX)
        }
        RowKind::Image => px(crate::config::markdown::IMAGE_ROW_DEFAULT_H),
        RowKind::Heading(n) => {
            let fs = theme.heading_fs(n);
            let cpl = avail / est_char_w(ascii, fs).max(1.);
            theme.heading_line(n) * est_rows(cpl, chars) + px(8.)
        }
        RowKind::Blank | RowKind::Rule => line_h,
        _ => {
            let cpl = avail / est_char_w(ascii, theme.font_size).max(1.);
            line_h * est_rows(cpl, chars)
        }
    }
}

/// 文本行（段落/标题/引述/列表）的行样式：字号/行高/基准色/加粗随类型（对齐预览）。
struct RowTextStyle {
    fs: Pixels,
    lh: Pixels,
    color: gpui::Hsla,
    bold: bool,
    /// 块底部额外留白（标题 py_1）。
    pad: Pixels,
}

fn text_style_for(kind: IrKind, theme: &WyTheme) -> RowTextStyle {
    match kind {
        IrKind::Heading(n) => RowTextStyle {
            fs: theme.heading_fs(n),
            lh: theme.heading_line(n),
            color: theme.text,
            bold: true,
            pad: px(8.),
        },
        // 引述用次要文字色（与预览一致）。
        IrKind::Quote => RowTextStyle {
            fs: theme.font_size,
            lh: theme.para_line(),
            color: theme.muted,
            bold: false,
            pad: px(0.),
        },
        _ => RowTextStyle {
            fs: theme.font_size,
            lh: theme.para_line(),
            color: theme.text,
            bold: false,
            pad: px(0.),
        },
    }
}

/// 行内 `$…$` 数学：把 display 文本里的公式 splice 成空格 spacer（宽度≈排版后的
/// latex 宽，`splice_inline_math` 把 spacer 映射回公式源位），paint 期再在 spacer 处
/// 画排版位图（已就绪）/ 斜体 latex（排版中或失败）。返回重写后的
/// `(display, gpui runs, byte_map)` + 每处公式位置。
///
/// `caret_disp` = 本行内光标的**渲染列**（`Some` = 光标在该行；`None` = 他行 / 无）。
/// reveal-on-caret：光标落进 `$…$` 的源区间内（含定界符 `$`）→ 该公式不 splice，
/// 显示原始 `$…$` 文本（可编辑）——用户双击公式把光标放进去即可直接改 LaTeX。
fn splice_math_text(
    editor: &WyEditor,
    window: &mut Window,
    src_line: usize,
    caret_disp: Option<usize>,
    rtext: &str,
    runs: Vec<TextRun>,
    map: Vec<usize>,
    fs: Pixels,
    lh: Pixels,
    base_font: &gpui::Font,
    math_font: &gpui::Font,
    math_color: Hsla,
    code_ranges: &[Range<usize>],
) -> (String, Vec<TextRun>, Vec<usize>, Vec<InlineMathText>) {
    let spans = inline_style::inline_math_spans(rtext);
    if spans.is_empty() {
        return (rtext.to_string(), runs, map, Vec::new());
    }
    let line_start = editor.line_start(src_line);
    // 跳过完全落在行内代码里的 `$…$`（反引号内是字面量，不是数学）。
    let in_code = |r: &Range<usize>| code_ranges.iter().any(|c| c.start < r.end && r.start < c.end);
    let space_w = {
        let run = [TextRun { len: 1, font: base_font.clone(), color: math_color, ..Default::default() }];
        f32::from(window.text_system().shape_line(" ".into(), fs, &run, None).width()).max(1.)
    };
    let slots = editor.math_slots();
    let mut formulas: Vec<(Range<usize>, usize)> = Vec::new();
    let mut maths: Vec<InlineMathText> = Vec::new();
    for span in spans {
        if in_code(&span) {
            continue;
        }
        // reveal-on-caret：光标严格落在 `$…$` 内（不碰定界符不算数，与块公式一致的
        // “单击停在边上、双击进去编辑”语义）→ 不 splice，显示可编辑的原始源码。
        if let Some(c) = caret_disp {
            if c > span.start && c < span.end {
                continue;
            }
        }
        let latex = inline_style::inline_math_latex(rtext, &span);
        if latex.trim().is_empty() {
            continue;
        }
        // 排版已就绪：spacer 宽 = 位图逻辑宽（按正文行字号缩放；store 按 FONT_SIZE 排）。
        // 排版中/失败：回退 Unicode 排版宽（paint 照旧画斜体 Unicode 文本）。
        let ready: Option<(Arc<RenderImage>, f32, f32)> = match slots
            .borrow()
            .get(super::inline_math_key(latex.trim()).as_ref())
            .cloned()
        {
            Some(super::MathSlot::Ready(img, w, h)) => Some((img, w, h)),
            _ => None,
        };
        let shown = math_to_unicode(latex);
        let shown = shown.replace('\n', " ");
        let (_w, n, paint_ready) = match ready {
            Some((img, w, h)) => {
                // spacer 宽 = 显示宽（`inline_math_fit_h` 缩放，供 paint 同式画图）。
                let fit = inline_math_fit_h(fs, lh, h);
                let scale = if h > 0.0 {
                    fit / h
                } else {
                    f32::from(fs) / crate::config::math::MATH_FONT_SIZE
                };
                let lw = (w * scale).max(1.);
                (lw.max(1.), (lw / space_w).ceil().max(1.0) as usize, Some((img, w, h)))
            }
            None => {
                if shown.is_empty() {
                    continue;
                }
                let run = [TextRun { len: shown.len(), font: math_font.clone(), color: math_color, ..Default::default() }];
                let w = f32::from(window.text_system().shape_line(shown.clone().into(), fs, &run, None).width());
                let n = (w / space_w).ceil() as usize;
                if n == 0 {
                    continue;
                }
                (w, n.max(1), None)
            }
        };
        if n > 0 {
            let n = n.max(1);
            formulas.push((span.clone(), n));
            // 源区间（绝对）：span 渲染列 → 经 byte_map 反查源列 → 加行首。
            let src_lo = line_start + display_to_source(&map, span.start);
            let src_hi = line_start
                + display_to_source(&map, span.end.saturating_sub(1).max(span.start));
            maths.push(InlineMathText {
                display_off: 0,
                len: n,
                source: src_lo..src_hi + 1,
                latex: shown.into(),
                key: super::inline_math_key(latex.trim()),
                ready: paint_ready,
                line: 0,
            });
        }
    }
    if formulas.is_empty() {
        return (rtext.to_string(), runs, map, Vec::new());
    }
    let gap = TextRun { len: 0, font: base_font.clone(), color: math_color, ..Default::default() };
    let (nd, nr, nm, places) =
        inline_style::splice_inline_math(rtext, &runs, &map, &formulas, &gap);
    let maths = places
        .into_iter()
        .zip(maths)
        .map(|(pl, mut m)| {
            m.display_off = pl.display_off;
            m
        })
        .collect();
    (nd, nr, nm, maths)
}

/// 行内公式的显示高（px）：原生按正文行字号缩放（`fs/MATH_FONT_SIZE × 图高`），但 ≤
/// `INLINE_MATH_BAND × 行高`（只压超高算子公式，其余保持字号缩放、不撑行）。shape
/// （spacer/行高）与 paint（画图）都用它——宽/高永远一致。参数单一源
/// = `crate::config::math`。
fn inline_math_fit_h(fs: Pixels, lh: Pixels, h: f32) -> f32 {
    let base = f32::from(fs) / crate::config::math::MATH_FONT_SIZE;
    let cap = f32::from(lh) * crate::config::math::INLINE_MATH_BAND;
    (base * h).clamp(1.0, cap)
}

/// display_off 落在 `wrapped` 里的第几个 BlockLine。文本块一源码行 = 一个
/// BlockLine（行内数学/折行都在其内，spacer 属于首个）→ 恒 0。
fn visual_block_line_for_index(wrapped: &[gpui::WrappedLine], _off: usize) -> usize {
    let _ = wrapped;
    0
}

/// 文本块（段落/标题/引述/列表/分隔线/空行）：按类型行样式形状成折行可视行。
fn shape_text_row(
    editor: &WyEditor,
    r: DocRow,
    window: &mut Window,
    width: Pixels,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    let kind = r.kind;
    let st = text_style_for(kind, theme);
    let mut base = base_font.clone();
    if st.bold {
        base.weight = gpui::FontWeight::BOLD;
    }
    // 行内代码（反引号）区间（已扩展空白）→ paint 自绘圆角 pill（与预览同式）；
    // 在数学 splice 之前算（数学公式不落入行内代码）。
    let code_ranges: Vec<Range<usize>> = r
        .runs
        .iter()
        .filter(|(_, k)| *k == RunKind::InlineCode)
        .map(|(rg, _)| crate::markdown::view::expand_bg_over_whitespace(r.text.as_ref(), rg))
        .collect();
    let runs = runs_to_text_runs(
        r.text.as_ref(),
        &r.runs,
        &base,
        st.color,
        &theme.mono,
        theme.accent,
        theme.link,
        theme.code_inline_fg,
    );
    // 行内数学 `$…$`：斜体 math 字体排版宽度 → splice 成空格 spacer（B2）。
    let mut math_font = theme.font.clone();
    math_font.style = gpui::FontStyle::Italic;
    // 光标若在该行（源列），告知 splice：光标落到 `$…$` 内 → reveal（显示可编辑的
    // 原始 LaTeX）；他行/无 → 照常 splice 成 spacer。
    let caret_disp = {
        let (cl, col) = editor.row_col(editor.caret());
        if cl == r.src_line {
            Some(col)
        } else {
            None
        }
    };
    let (rtext, runs2, map2, mut inline_maths) = splice_math_text(
        editor,
        window,
        r.src_line,
        caret_disp,
        r.text.as_ref(),
        runs,
        r.byte_map.clone(),
        st.fs,
        st.lh,
        &base,
        &math_font,
        super::math_color(theme),
        &code_ranges,
    );
    let wrapped = window
        .text_system()
        .shape_text(rtext.clone().into(), st.fs, &runs2, Some(width), None)
        .map(|ls| ls.into_vec())
        .unwrap_or_default();
    let mut height = st.pad;
    let mut sub_heights = Vec::with_capacity(wrapped.len());
    for wl in &wrapped {
        let sh = st.lh * (wl.wrap_boundaries().len() + 1) as f32;
        height += sh;
        sub_heights.push(sh);
    }
    if height <= st.pad {
        height = st.lh + st.pad;
        sub_heights.push(st.lh);
    }
    // 行内公式超行高：撑高所在 BlockLine（公式保持可读，不再压扁），paint 期按撑高后
    // 的 `sub_heights` 定位文本与公式——否则 `\int_0^1 x^2 dx` 会盖住上一行/或被缩太小。
    for im in &mut inline_maths {
        let Some((_, _, h)) = im.ready else { continue };
        let li = visual_block_line_for_index(&wrapped, im.display_off);
        im.line = li;
        let fit = inline_math_fit_h(st.fs, st.lh, h);
        let need = fit.max(f32::from(sub_heights[li]));
        let old = f32::from(sub_heights[li]);
        if need > old {
            height += px(need - old);
            sub_heights[li] = px(need);
        }
    }
    let editable = !map2.is_empty();
    // 引述文本缩进 = 内容边距(12) + 左侧指示条(3) + 带内左内衬(12) − 内容边距(12)。
    let x_off = if kind == IrKind::Quote { px(15.) } else { px(0.) };
    let has_non_ascii = wrapped.iter().any(|wl| !wl.text.is_ascii());
    let wrapped = std::rc::Rc::new(wrapped);
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped,
        has_non_ascii,
        sub_heights,
        byte_map: std::rc::Rc::new(map2),
        editable,
        multi_offsets: Vec::new(),
        code_ranges,
        lh: st.lh,
        x_off,
        height,
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: None,
        mm: None,
        extra: RowExtra { inline_maths, ..Default::default() },
    }
}

/// 文本行的装饰化入口（在 [`shape_text_row`] 之上叠加：警报标记剥除、列表/任务
/// gutter、可点击链接、行内图片）。需要源文本以识别构造，故把 `editor` 穿进来。
fn shape_text_row_decorated(
    mut r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    let raw = editor.doc().line_str(r.src_line);
    // C5: 让所有可点击链接（wiki/#tag/裸 URL）与 `[text](url)` 同色 + 下划线，
    // 与预览/阅读器视觉一致（在 alert 剥标记与数学 splice 之前合并进 runs）。
    style_links_in_runs(&mut r, &raw);
    let mut gutter: Option<RowGutter> = None;
    // C3: 警报（callout）——剥掉 `[!KIND]` 标记，paint 画彩条 + 粗体标签 + 折叠箭头。
    if r.kind == IrKind::Quote
        && let Some(p) = inline_style::blockquote_prefix(&raw)
    {
        // 对「剥掉 `>` 后的 body」再去首空白匹配（`r.text` 也已是 trim 后的主体），
        // 这样 `>  [!NOTE]` 这类多空格变体也能精确剥掉标记。
        let body = &raw[p..];
        let trimmed = body.trim_start();
        if let Some((akind, consumed, fold)) = inline_style::alert_prefix(trimmed) {
            let n = consumed.min(r.text.len());
            strip_prefix_bytes(&mut r, n);
            gutter = Some(RowGutter::Alert { kind: akind, fold });
        }
    }
    let mut srow = shape_text_row(editor, r.clone(), window, width, theme, base_font);
    // C1: 列表/任务行首装饰 + 正文缩进（与预览一致：正文色符号 + 缩进）。
    if r.kind == IrKind::Bullet {
        if let Some(g) = bullet_gutter(&raw) {
            let is_check = matches!(g, RowGutter::Check { .. });
            srow.extra.gutter = Some(g);
            srow.x_off = if is_check { px(24.) } else { px(18.) };
        }
    }
    if let Some(g @ RowGutter::Alert { .. }) = &gutter {
        srow.extra.gutter = Some(g.clone());
        srow.x_off = px(84.);
    }
    // 标题折叠：为左缘折叠箭头预留 gutter（正文右移 `HEADING_GUTTER_W`；无折叠箭头时
    // 也统一留位，避免命中/光标在「有无箭头」间抖动）。
    if matches!(r.kind, IrKind::Heading(_)) {
        srow.x_off = px(HEADING_GUTTER_W);
    }
    // C5: 行内链接命中（hand 光标 + 单键点击导航）。
    if srow.editable {
        srow.extra.links = link_hits(&raw, srow.byte_map.as_slice());
    }
    // B1: 行内图片 `![alt](src)` → 在 alt 文本（显示区）上方画缩略图。
    srow.extra.inline_images = inline_image_hits(&r, &srow, &raw);
    srow
}

/// 把渲染文本/样式/映射的前 `n` 个字节剥掉（警报标记行用；`n` 是剥掉后的字节数）。
fn strip_prefix_bytes(r: &mut DocRow, n: usize) {
    let n = n.min(r.text.len());
    if n == 0 {
        return;
    }
    r.text = r.text.as_ref()[n..].into();
    r.runs = r
        .runs
        .iter()
        .filter_map(|(rng, k)| {
            let s = rng.start.saturating_sub(n);
            let e = rng.end.saturating_sub(n);
            (s < e).then_some((s..e, *k))
        })
        .collect();
    let bm = &mut r.byte_map;
    if bm.len() > n {
        bm.drain(..n);
    } else {
        bm.clear();
        bm.push(0);
    }
}

/// 列表行首装饰：任务框（`- [ ]` / `- [x]`）优先于普通项目符号 / 有序号码。
fn bullet_gutter(raw: &str) -> Option<RowGutter> {
    let t = raw.trim_start();
    for b in ["- ", "* ", "+ "] {
        for (mark, checked) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
            if let Some(rest) = t.strip_prefix(&format!("{b}{mark}")) {
                if !rest.is_empty() {
                    return Some(RowGutter::Check { checked });
                }
            }
        }
    }
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return Some(RowGutter::Bullet);
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..4).contains(&digits) && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
        return Some(RowGutter::Ordered(format!("{}.", &t[..digits])));
    }
    None
}

const LATEX_SYMBOLS: &[(&str, char)] = &[
    ("pi", 'π'), ("infty", '∞'), ("int", '∫'), ("iint", '∬'), ("oint", '∮'),
    ("sum", '∑'), ("prod", '∏'), ("cdot", '·'), ("times", '×'), ("div", '÷'),
    ("pm", '±'), ("mp", '∓'), ("neq", '≠'), ("leq", '≤'), ("geq", '≥'),
    ("le", '≤'), ("ge", '≥'), ("ne", '≠'), ("approx", '≈'), ("equiv", '≡'),
    ("sim", '∼'), ("partial", '∂'), ("nabla", '∇'),
    ("alpha", 'α'), ("beta", 'β'), ("gamma", 'γ'), ("delta", 'δ'),
    ("epsilon", 'ε'), ("zeta", 'ζ'), ("eta", 'η'), ("theta", 'θ'),
    ("lambda", 'λ'), ("mu", 'μ'), ("nu", 'ν'), ("xi", 'ξ'),
    ("rho", 'ρ'), ("sigma", 'σ'), ("tau", 'τ'), ("phi", 'φ'), ("chi", 'χ'),
    ("psi", 'ψ'), ("omega", 'ω'), ("Gamma", 'Γ'), ("Delta", 'Δ'),
    ("Theta", 'Θ'), ("Lambda", 'Λ'), ("Sigma", 'Σ'), ("Phi", 'Φ'),
    ("Omega", 'Ω'), ("rightarrow", '→'), ("leftarrow", '←'),
    ("leftrightarrow", '↔'), ("to", '→'), ("mapsto", '↦'),
    ("Rightarrow", '⇒'), ("Leftrightarrow", '⇔'),
    ("ldots", '…'), ("cdots", '⋯'), ("dots", '…'), ("in", '∈'), ("notin", '∉'),
    ("subset", '⊂'), ("subseteq", '⊆'), ("cup", '∪'), ("cap", '∩'),
    ("forall", '∀'), ("exists", '∃'), ("nexists", '∄'), ("propto", '∝'),
    ("perp", '⊥'), ("parallel", '∥'), ("angle", '∠'), ("circ", '∘'),
    ("degree", '°'), ("deg", '°'), ("oplus", '⊕'), ("otimes", '⊗'),
    ("emptyset", '∅'), ("ell", 'ℓ'), ("hbar", 'ℏ'), ("bullet", '•'),
    ("uparrow", '↑'), ("downarrow", '↓'), ("ast", '∗'), ("star", '⋆'),
    ("dag", '†'), ("ddag", '‡'),
];

/// 把一个字符上标化（Unicode 上标），不可映射返回 `None`。
fn sup_map(c: char) -> Option<char> {
    Some(match c {
        '0'..='9' => ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'][c as usize - '0' as usize],
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        _ => return None,
    })
}

/// 把一个字符下标化（Unicode 下标），不可映射返回 `None`。
fn sub_map(c: char) -> Option<char> {
    Some(match c {
        '0'..='9' => ['₀', '₁', '₂', '₃', '₄', '₅', '₆', '₇', '₈', '₉'][c as usize - '0' as usize],
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        'i' => 'ᵢ',
        'n' => 'ₙ',
        'x' => 'ₓ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'm' => 'ₘ',
        _ => return None,
    })
}

/// 从 `cs[pos..]` 读一个「组」：`{…}`（含嵌套）或单个字符。返回 (内容, 新位置)。
fn read_group(cs: &[char], pos: usize) -> (String, usize) {
    if pos < cs.len() && cs[pos] == '{' {
        let mut depth = 0usize;
        let mut s = String::new();
        let mut k = pos;
        while k < cs.len() {
            match cs[k] {
                '{' => {
                    depth += 1;
                    if depth > 1 {
                        s.push('{');
                    }
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return (s, k + 1);
                    }
                    s.push('}');
                }
                c => s.push(c),
            }
            k += 1;
        }
        return (s, k);
    }
    if pos < cs.len() {
        (cs[pos].to_string(), pos + 1)
    } else {
        (String::new(), pos)
    }
}

/// 只在以 `{` 开头时读组；否则 `None`（位置不动）。
fn read_group_opt(cs: &[char], pos: usize) -> (Option<String>, usize) {
    if pos < cs.len() && cs[pos] == '{' {
        let (s, ni) = read_group(cs, pos);
        (Some(s), ni)
    } else {
        (None, pos)
    }
}

/// 子/上标在**卡片**里的视觉分档：`Sup` = 上标（小一号 + 上移）、`Sub` = 下标（小一号 + 下移）。
/// 字符串版 `math_to_unicode` 用 Unicode 上/下标码位（`ᵢ₌₁ⁿ` 这类），但 `∞` 等没有码位
/// 只能正文大小平排 → `∫₋∞∞` 的极限看起来不对。卡片改用 `math_to_segments` + 字号/偏移。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MathPos {
    Normal,
    Sub,
    Sup,
}

#[derive(Clone, Debug)]
struct MathSeg {
    text: String,
    pos: MathPos,
}

/// 同 `math_to_unicode` 的扫描，但把顶层 `^{…}`/`_{…}` 拆成独立段（Sup/Sub）。
/// 普通文本兜底进 Normal 段，`\frac/\sqrt/\text` 等命令整体仍按 math_to_unicode 转进
/// Normal 段（组内嵌套的 `^`/`_` 在递归里转成 Unicode 码位即可，不再细分）。
fn math_to_segments(src: &str) -> Vec<MathSeg> {
    let cs: Vec<char> = src.chars().collect();
    let mut segs: Vec<MathSeg> = Vec::new();
    let mut buf = String::with_capacity(src.len());
    let mut i = 0;
    let flush = |segs: &mut Vec<MathSeg>, buf: &mut String| {
        if !buf.is_empty() {
            segs.push(MathSeg { text: std::mem::take(buf), pos: MathPos::Normal });
        }
    };
    while i < cs.len() {
        match cs[i] {
            '{' | '}' => i += 1,
            '^' => {
                flush(&mut segs, &mut buf);
                let (g, ni) = read_group(&cs, i + 1);
                let g = math_to_unicode(&g);
                let mut t = String::new();
                for c in g.chars() {
                    t.push(sup_map(c).unwrap_or(c));
                }
                segs.push(MathSeg { text: t, pos: MathPos::Sup });
                i = ni;
            }
            '_' => {
                flush(&mut segs, &mut buf);
                let (g, ni) = read_group(&cs, i + 1);
                let g = math_to_unicode(&g);
                let mut t = String::new();
                for c in g.chars() {
                    t.push(sub_map(c).unwrap_or(c));
                }
                segs.push(MathSeg { text: t, pos: MathPos::Sub });
                i = ni;
            }
            '\\' => {
                if cs.get(i + 1) == Some(&'\\') {
                    buf.push('\n');
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                while j < cs.len() && cs[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let name: String = cs[i + 1..j].iter().collect();
                match name.as_str() {
                    "begin" | "end" => {
                        if let (Some(_), ni) = read_group_opt(&cs, j) {
                            i = ni;
                        } else {
                            i = j;
                        }
                    }
                    "cr" | "hfill" => {
                        buf.push('\n');
                        i = j;
                    }
                    "frac" | "tfrac" | "dfrac" => {
                        let (a, ni) = read_group_opt(&cs, j);
                        let (b, nj) = match &a {
                            Some(_) => read_group_opt(&cs, ni),
                            None => (None, j),
                        };
                        if let Some(a) = &a {
                            buf.push_str(&math_to_unicode(a));
                        }
                        buf.push('/');
                        if let Some(b) = &b {
                            buf.push_str(&math_to_unicode(b));
                        }
                        i = nj;
                    }
                    "sqrt" => {
                        buf.push('√');
                        let (g, ni) = read_group_opt(&cs, j);
                        match g {
                            Some(g) => {
                                if g.chars().count() > 1 {
                                    buf.push('(');
                                    buf.push_str(&math_to_unicode(&g));
                                    buf.push(')');
                                } else {
                                    buf.push_str(&math_to_unicode(&g));
                                }
                            }
                            None => {}
                        }
                        i = ni;
                    }
                    "text" | "mathrm" | "mathit" | "operatorname" | "mbox" => {
                        let (g, ni) = read_group_opt(&cs, j);
                        if let Some(g) = g {
                            buf.push_str(&math_to_unicode(&g));
                        }
                        i = ni;
                    }
                    "left" | "right" | "big" | "Big" | "Bigl" | "Bigr" | "bigl" | "bigr"
                    | "small" | "limits" | "nolimits" => i = j,
                    _ => {
                        if let Some(sym) = LATEX_SYMBOLS.iter().find(|(n, _)| *n == name) {
                            buf.push(sym.1);
                        }
                        i = j;
                    }
                }
            }
            c => {
                if c == '&' {
                    buf.push(' ');
                } else {
                    buf.push(c);
                }
                i += 1;
            }
        }
    }
    flush(&mut segs, &mut buf);
    segs
}

/// 裸露 LaTeX → Unicode 排版（无光栅渲染器时的「公式感」显示）。
/// 处理上下标（`^`/`_`，含 `{…}` 组）、`\frac`/`\sqrt`/`\text` 等、希腊字母与常用符号。
/// 行内数学继续用这个扁平字符串版；**卡片**走 `math_to_segments`（细分段、可缩小+偏移）。
fn math_to_unicode(src: &str) -> String {
    math_to_segments(src)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// mermaid 节点形状。
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MmShape {
    /// 普通矩形。
    Rect,
    /// `{ … }` 判断/菱形。
    Diamond,
    /// `[ … ]` 圆角矩形（stadium）。
    Round,
    /// `(( … ))` 圆形。
    Circle,
}

/// 解析 `A[...]` / `A{...}` / `A((...))` / `A(...)` / 裸 `A` → (id, 可选标签, 形状)。
fn mm_node(s: &str) -> (String, Option<String>, MmShape) {
    let s = s.trim();
    if let Some(oi) = s.find("((") {
        let id = s[..oi].trim().to_string();
        let rest = &s[oi + 2..];
        if let Some(ci) = rest.find("))") {
            return (id, Some(rest[..ci].trim().to_string()), MmShape::Circle);
        }
        return (id, None, MmShape::Circle);
    }
    for (open, close, shp) in [
        ('[', ']', MmShape::Round),
        ('{', '}', MmShape::Diamond),
        ('(', ')', MmShape::Round),
    ] {
        if let Some(oi) = s.find(open) {
            let id = s[..oi].trim().to_string();
            let rest = &s[oi + 1..];
            if let Some(ci) = rest.find(close) {
                return (id, Some(rest[..ci].trim().to_string()), shp);
            }
            return (id, None, shp);
        }
    }
    (s.to_string(), None, MmShape::Rect)
}

/// 解析一条边 `A -->|标| B` / `A --> B` / `A --- B` → (起点文本, 终点文本, 边标)。
fn mm_edge(s: &str) -> Option<(String, String, Option<String>)> {
    let s = s.trim();
    let marker = if s.contains("-->") {
        "-->"
    } else if s.contains("---") {
        "---"
    } else {
        return None;
    };
    let idx = s.find(marker)?;
    let lhs = s[..idx].trim().to_string();
    let mut rhs = s[idx + marker.len()..].trim().to_string();
    let mut elab = None;
    if let Some(rest) = rhs.strip_prefix('|') {
        if let Some(end) = rest.find('|') {
            elab = Some(rest[..end].trim().to_string());
            rhs = rest[end + 1..].trim().to_string();
        } else {
            rhs = rest.to_string();
        }
    }
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    Some((lhs, rhs, elab))
}

/// 解析后的 mermaid 图（节点 + 边），供层式布局与矢量绘制用。
#[derive(Clone, Debug)]
struct MmGraph {
    /// (id, 显示标签, 形状)。
    nodes: Vec<(String, String, MmShape)>,
    /// (起点 id, 终点 id, 边标)。
    edges: Vec<(String, String, Option<String>)>,
}

/// 解析 `graph TD/LR` 源码 → 节点与边（缺省标签回退到 id）。
fn mermaid_graph(source: &str) -> MmGraph {
    let mut nodes: Vec<(String, String, MmShape)> = Vec::new();
    let mut edges: Vec<(String, String, Option<String>)> = Vec::new();
    let mut add = |id: &str, lab: Option<String>, shp: MmShape| {
        if let Some(n) = nodes.iter_mut().find(|(nid, _, _)| nid == id) {
            // 已有定义：只有本次给出**显式标签**才更新，裸 id 引用（`B -->` 里的 `B`）
            // 不得用回退的「id 本身」覆盖之前的 `B{判断}` → 判断。
            if let Some(l) = lab {
                if !l.is_empty() {
                    n.1 = l;
                }
            }
            return;
        }
        let lab = lab.filter(|l| !l.is_empty()).unwrap_or_else(|| id.to_string());
        nodes.push((id.to_string(), lab, shp));
    };
    for raw in source.lines() {
        let mut t = raw.trim();
        if let Some(s) = t.strip_suffix(';') {
            t = s.trim();
        }
        if t.is_empty()
            || t.starts_with("graph ")
            || t.starts_with("flowchart ")
            || t.starts_with("sequenceDiagram")
        {
            continue;
        }
        if let Some((a, b, elab)) = mm_edge(t) {
            let (aid, alab, ashp) = mm_node(&a);
            let (bid, blab, bshp) = mm_node(&b);
            add(&aid, alab, ashp);
            add(&bid, blab, bshp);
            edges.push((aid, bid, elab));
            continue;
        }
        let (id, lab, shp) = mm_node(t);
        add(&id, lab, shp);
    }
    MmGraph { nodes, edges }
}

/// 最长路径分层：`layer[b] ≥ layer[a] + 1`，收敛后返回各节点层号。
fn mermaid_layer_map(g: &MmGraph) -> std::collections::HashMap<String, usize> {
    use std::collections::HashMap;
    let mut layer: HashMap<String, usize> = HashMap::new();
    for (id, _, _) in &g.nodes {
        layer.insert(id.clone(), 0);
    }
    // 边界情况：节点多则多轮，收敛即止。
    for _ in 0..g.nodes.len().max(1) {
        let mut changed = false;
        for (a, b, _) in &g.edges {
            let la = layer.get(a).copied().unwrap_or(0);
            if layer.get(b).copied().unwrap_or(0) < la + 1 {
                layer.insert(b.clone(), la + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    layer
}

/// 卡片布局常量（paint 与卡片高度共用，不得分开改）。
const MM_TOP_PAD: f32 = 30.;
const MM_LANE: f32 = 52.;
const MM_NODE_H: f32 = 30.;
const MM_NODE_XPAD: f32 = 14.;
const MM_SEP: f32 = 20.;
const MM_EDGE_X: f32 = 14.;

/// Mermaid 图卡片高度：由层数决定（无窗口也能算）。
fn mermaid_card_height(source: &str) -> Pixels {
    let g = mermaid_graph(source);
    if g.nodes.is_empty() {
        return px(140.);
    }
    let layers = mermaid_layer_map(&g);
    let n = layers.values().copied().max().unwrap_or(0) + 1;
    px(MM_TOP_PAD + n as f32 * MM_LANE + 8.)
}

/// 已摆放的节点（layout 算好几何，paint 与测试共用）。
pub(crate) struct MmPlaced {
    pub id: String,
    pub label: String,
    pub shape: MmShape,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// mermaid `graph` 的层式布局（与绘制解耦，便于单测/光栅化验证）：
/// 返回摆放好的节点与「节点索引对」边。`area` 是卡片内容区（左上 + 可用宽）。
pub(crate) fn mm_layout(
    window: &Window,
    theme: &WyTheme,
    source: &str,
    area_x: f32,
    area_y: f32,
    area_w: f32,
) -> (Vec<MmPlaced>, Vec<(usize, usize, Option<String>)>) {
    let g = mermaid_graph(source);
    if g.nodes.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let layer = mermaid_layer_map(&g);
    let n_layers = layer.values().copied().max().unwrap_or(0) + 1;
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); n_layers];
    for (idx, (id, _, _)) in g.nodes.iter().enumerate() {
        let l = layer.get(id).copied().unwrap_or(0).min(n_layers - 1);
        rows[l].push(idx);
    }
    let fs = theme.font_size;
    let font = theme.font.clone();
    let avail = (area_w - 2. * MM_EDGE_X).max(1.);
    let xc = area_x + MM_EDGE_X;
    let mut placed: Vec<MmPlaced> = Vec::new();
    for (li, row) in rows.iter().enumerate() {
        let mut widths: Vec<f32> = Vec::with_capacity(row.len());
        for &i in row {
            let (_, label, _) = &g.nodes[i];
            let run = [TextRun { len: label.len(), font: font.clone(), color: theme.text, ..Default::default() }];
            let w = f32::from(window.text_system().shape_line(label.clone().into(), fs, &run, None).width());
            widths.push(w + 2. * MM_NODE_XPAD);
        }
        let row_total = widths.iter().sum::<f32>() + MM_SEP * (row.len() as f32 - 1.);
        let mut x = xc + ((avail - row_total) / 2.).max(0.);
        for (j, &i) in row.iter().enumerate() {
            let (id, label, shape) = &g.nodes[i];
            placed.push(MmPlaced {
                id: id.clone(),
                label: label.clone(),
                shape: *shape,
                x,
                y: area_y + MM_TOP_PAD + li as f32 * MM_LANE,
                w: widths[j],
                h: MM_NODE_H,
            });
            x += widths[j] + MM_SEP;
        }
    }
    let pos = |id: &str| placed.iter().position(|p| p.id == *id);
    let mut edges: Vec<(usize, usize, Option<String>)> = Vec::new();
    for (a, b, elab) in &g.edges {
        if let (Some(ia), Some(ib)) = (pos(a), pos(b)) {
            edges.push((ia, ib, elab.clone()));
        }
    }
    (placed, edges)
}

/// 让「所有可点击链接」（`[[wiki]]` / `#tag` / 裸 URL，以及 parser 已产出的
/// `[text](url)`）都套上链接色 + 下划线（与预览一致）。在数学 splice 之前合并进
/// `r.runs`（显示字节空间，pre-splice）。
fn style_links_in_runs(r: &mut DocRow, raw: &str) {
    if r.byte_map.is_empty() || recognition::links(raw).is_empty() {
        return;
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (src, _) in recognition::links(raw) {
        let d0 = r.byte_map.partition_point(|&b| b < src.start);
        let d1 = r.byte_map.partition_point(|&b| b < src.end);
        if d1 > d0 {
            ranges.push(d0..d1);
        }
    }
    if ranges.is_empty() {
        return;
    }
    let old = std::mem::take(&mut r.runs);
    let len = r.text.len();
    let mut breaks: Vec<usize> = vec![0, len];
    for (rg, _) in &old {
        breaks.push(rg.start);
        breaks.push(rg.end);
    }
    for rg in &ranges {
        breaks.push(rg.start);
        breaks.push(rg.end);
    }
    breaks.sort_unstable();
    breaks.dedup();
    let mut out: Vec<(Range<usize>, RunKind)> = Vec::new();
    for w in breaks.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let kind = if ranges.iter().any(|rg| rg.start <= a && b <= rg.end) {
            Some(RunKind::Link)
        } else if let Some((_, k)) = old.iter().find(|(rg, _)| rg.start <= a && b <= rg.end) {
            Some(*k)
        } else {
            None
        };
        if let Some(k) = kind {
            out.push((a..b, k));
        }
    }
    r.runs = out;
}

/// 行内链接：源码区间（行坐标）→ 显示字节区间（经 byte_map 左界映射）。
fn link_hits(raw: &str, byte_map: &[usize]) -> Vec<RowLink> {
    let mut out = Vec::new();
    for (src, target) in recognition::links(raw) {
        let d0 = source_to_display(byte_map, src.start);
        let d1 = source_to_display(byte_map, src.end);
        if d1 > d0 {
            out.push(RowLink { disp: d0..d1, target });
        }
    }
    out
}

/// 行内图片：`RunKind::Image`（alt 显示区）反向解析源 src（经 byte_map 找 `!` 位，
/// 再在原文尾随 `![…](…)` 取 src）。`r.byte_map` 是「剥标记后、数学 splice 前」的
/// display→源（源值不变）；`srow.byte_map` 是 splice 后的——显示区间统一以**源偏移**
/// 为准重算（两种 map 都成立）。
///
/// 坑：`RunKind::Image` 的 run 覆盖的是 **alt 文本**（`![alt](…)` 里 disc 剥掉记号后
/// 只留 alt，byte_map[alt] = alt 源起点）。所以：
/// - src 起点要从 `byte_map[rg.start]` **往前退一格**（`!` 紧贴 alt 前）再喂
///   [`inline_style::inline_image_spans`]——否则 tail 从 alt 开始、找不到 `![`，
///   行内图片永远不显示。
/// - 显示区间只能用 **alt 的长度**（= run 长），不能用整段 `![…](…)` 的源跨度——
///   那会越过 shaped 行的末尾，`position_for_index` 返回 None → paint 阶段被跳过。
fn inline_image_hits(r: &DocRow, srow: &ShapedRow, raw: &str) -> Vec<InlineImageHit> {
    let mut out = Vec::new();
    let pre = r.byte_map.as_slice();
    let post = srow.byte_map.as_slice();
    for (rg, k) in &r.runs {
        if *k != RunKind::Image {
            continue;
        }
        let Some(&alt_src) = pre.get(rg.start.min(pre.len().saturating_sub(1))) else {
            continue;
        };
        // 源里 `![` 的 `!` 在 **alt 前两个字节**（`!` + `[` 各一字节）——`byte_map` 把
        // alt 显示字节映射到 alt 源起点，`alt_src-1` 是 `[`，`alt_src-2` 才是 `!`。
        if alt_src < 2 || alt_src > raw.len() {
            continue;
        }
        let start = alt_src - 2;
        if raw.as_bytes().get(start) != Some(&b'!')
            || raw.as_bytes().get(start + 1) != Some(&b'[')
        {
            continue;
        }
        let tail = &raw[start..];
        let Some((full, srcr)) = inline_style::inline_image_spans(tail).into_iter().next() else {
            continue;
        };
        let _ = full;
        let src = tail[srcr].to_string();
        let alt_len = rg.end.saturating_sub(rg.start);
        let d0 = source_to_display(post, alt_src);
        let d1 = source_to_display(post, (alt_src + alt_len).min(raw.len()));
        if d1 > d0 {
            out.push(InlineImageHit { display_off: d0, len: d1 - d0, src: src.into() });
        }
    }
    out
}

/// 代码围栏块：等宽 + 语法高亮（GitHub 明/暗色板，同预览）+ 首/末空白带，
/// 行高/字号用代码专属尺寸。不换行（超宽由宿主横向裁剪，与预览 nowrap 一致）。
/// 围栏内**内容行可编辑**：`byte_map`（块级）+ `multi_offsets`（每源码行起始渲染
/// 字节）供命中/光标/选区换算；围栏标记本身只读（不渲染）。
/// 文本额外缩进 `x_off = 12`（面板内 `pl_3`）：背景带与正文对齐（content 边距 12），
/// 文字再内衬 12 —— 命中/光标/选区经通用 `r.x_off` 对齐，与预览逐像素一致。
fn shape_code_row(
    r: DocRow,
    window: &mut Window,
    theme: &WyTheme,
    width: Pixels,
) -> ShapedRow {
    let kind = r.kind;
    let (code_lines, lang) = match &r.payload {
        DocPayload::Code { code_lines, language } => (code_lines.clone(), language.clone()),
        _ => (Vec::new(), None),
    };
    // A2: ```mermaid 围栏折叠成「图卡片」——画一张节点/连线的矢量图。
    if lang.as_deref() == Some("mermaid") {
        let source = code_lines.join("\n");
        let height = mermaid_card_height(&source);
        // 布局（`MmPlaced` 卡片相对坐标，area=(0,0)）在 **shape 期**算好缓存——paint
        // 不再每帧重排/重测全部标签宽（否则滚动/键入时可见的 mermaid 卡片每帧整套
        // 解析+排版；行整形缓存使其只在内容/宽度/主题变化时重算一次）。
        let area_w = (f32::from(width) - 2. * f32::from(px(12.))).max(1.);
        let mm = mm_layout(window, theme, &source, 0., 0., area_w);
        return ShapedRow {
            src_line: r.src_line,
            src_lines: r.lines.clone(),
            wrapped: std::rc::Rc::new(Vec::new()),
            has_non_ascii: false,
            sub_heights: vec![height],
            byte_map: std::rc::Rc::new(Vec::new()),
            editable: false,
            multi_offsets: Vec::new(),
            code_ranges: Vec::new(),
            lh: px(0.),
            x_off: px(0.),
            height,
            top: px(0.),
            kind,
            table: None,
            table_edit: None,
            image: None,
            mm: Some(std::rc::Rc::new(mm)),
            extra: RowExtra {
                 card: Some(BlockCard::Mermaid),
                ..Default::default()
            },
        };
    }
    let fs = theme.code_size();
    let lh = theme.code_line();
    let mono = theme.mono.clone();
    let base = theme.text;
    // 语法高亮：按围栏语言整块解析一次（与预览同源）。
    let hl_runs = match lang.as_deref() {
        Some(l) if !l.is_empty() && !code_lines.is_empty() => {
            let mut code = String::new();
            for (i, l) in code_lines.iter().enumerate() {
                if i > 0 {
                    code.push('\n');
                }
                code.push_str(l.as_ref());
            }
            parse_code_lines(l, &code)
        }
        _ => None,
    };
    let resolver = hl_runs.as_ref().map(|_| github_theme_cached(theme.is_dark));
    let mut wrapped = Vec::new();
    let mut sub_heights = Vec::with_capacity(code_lines.len().max(1));
    // 首/末空白带（预览 code_blank × 2）。
    let mut height = lh * 2.;
    for (i, line) in code_lines.iter().enumerate() {
        let runs: Vec<gpui::TextRun> = if let (Some(runs), Some(res)) = (hl_runs.as_ref(), resolver)
        {
            let line_runs = runs.get(i).map(|r| r.as_slice()).unwrap_or(&[]);
            resolve_line_styles_with(line_runs, line.len(), res)
                .into_iter()
                .map(|(range, style)| {
                    let mut f = mono.clone();
                    if let Some(w) = style.font_weight {
                        f.weight = w;
                    }
                    if let Some(s) = style.font_style {
                        f.style = s;
                    }
                    gpui::TextRun {
                        len: range.len(),
                        font: f,
                        color: style.color.unwrap_or(base),
                        ..Default::default()
                    }
                })
                .collect()
        } else {
            vec![gpui::TextRun { len: line.len(), font: mono.clone(), color: base, ..Default::default() }]
        };
        let v = window
            .text_system()
            .shape_text(line.clone(), fs, &runs, None, None)
            .map(|ls| ls.into_vec())
            .unwrap_or_default();
        let rows = v.iter().map(|wl| wl.wrap_boundaries().len() + 1).sum::<usize>().max(1);
        let sh = lh * rows as f32;
        height += sh;
        sub_heights.push(sh);
        wrapped.extend(v);
    }
    // 每源码行在其整块渲染文本（代码行以 '\n' 连接）中的起始渲染字节偏移——
    // 与 `wrapped`/`sub_heights` 逐行对齐（每行一个 WrappedLine，nowrap 不折行）。
    let editable = !code_lines.is_empty();
    let mut multi_offsets = Vec::with_capacity(code_lines.len());
    {
        let mut acc = 0usize;
        for (k, ln) in code_lines.iter().enumerate() {
            if k > 0 {
                acc += 1;
            }
            multi_offsets.push(acc);
            acc += ln.len();
        }
    }
    let has_non_ascii = wrapped.iter().any(|wl| !wl.text.is_ascii());
    let wrapped = std::rc::Rc::new(wrapped);
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped,
        has_non_ascii,
        sub_heights,
        byte_map: std::rc::Rc::new(r.byte_map),
        editable,
        multi_offsets,
        code_ranges: Vec::new(),
        lh,
        x_off: px(12.),
        height,
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: None,
        mm: None,
        extra: RowExtra::default(),
    }
}

/// 表格块：列宽按内容自适应（与预览 `table_column_widths` 同式：最大文字宽+20，
/// 下限 48），不缩放/不省略（超宽由宿主裁剪）；paint 期画成网格。
/// 单元格**可编辑**：shape 期对每可见行（表头 + 数据行）逐单元格排一行单行文本
/// （与 paint 同字体/加粗/对齐位移），连同「渲染字节→源字节」映射存进
/// `table_edit`（命中/光标据此把屏幕位置算出源码字节，编辑保留表格结构）。
fn shape_table_row(
    r: DocRow,
    window: &mut Window,
    font_size: Pixels,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    let kind = r.kind;
    let (header, rows, aligns) = match &r.payload {
        DocPayload::Table(t) => (t.header.clone(), t.rows.clone(), t.aligns.clone()),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };
    let ncols = header.len().max(1);
    let mut widths: Vec<f32> = (0..ncols).map(|_| 48.).collect();
    let mut measure = |window: &mut Window, c: &str, ci: usize| {
        widths[ci] = widths[ci].max(
            shaped_text_width(window, c, font_size, base_font, theme.text) + 20.,
        );
    };
    for (ci, c) in header.iter().enumerate() {
        measure(window, c.as_ref(), ci);
    }
    for row in &rows {
        for (ci, c) in row.iter().enumerate().take(ncols) {
            measure(window, c.as_ref(), ci);
        }
    }
    let total_w = widths.iter().sum();
    let row_h = table_row_h(theme);
    // 表格块总高 = 网格高 + 底部滚动条预留带（`TABLE_SCROLLBAR_RESERVE_PX`，与 preview
    // `markdown/view` 的 `.pb()` 同）——滚动条画在网格**之下**、不遮内容。带内/带外同式
    // （`estimate_height` 同加该带）→ 总高随可视带恒定、滚动不跳。
    let height = table_block_height(theme, rows.len())
        + px(crate::config::markdown::TABLE_SCROLLBAR_RESERVE_PX);
    let mut aligns2 = aligns.clone();
    aligns2.resize(ncols, TableAlign::Left);
    let rows2: Vec<Vec<gpui::SharedString>> = rows
        .into_iter()
        .map(|mut rr| {
            rr.resize(ncols, gpui::SharedString::default());
            rr
        })
        .collect();
    let mappings: Option<Vec<crate::markdown::doc::TableRowMap>> =
        r.table_rows.as_ref().map(|m| m.as_ref().clone());

    // 逐可见行 shape 每个单元格（表头加粗、数据行常规，与 draw_table_row 同 runs/对齐）。
    let mut trow_edit: Vec<TableRowCellsLayout> = Vec::new();
    let mk_row = |window: &mut Window,
                  src_line: usize,
                  texts: Vec<gpui::SharedString>,
                  maps: &[crate::markdown::doc::TableCellMap],
                  is_header: bool| {
        let mut cells = Vec::with_capacity(texts.len());
        for (ci, c) in texts.iter().enumerate() {
            let col_w = widths.get(ci).copied().unwrap_or(0.);
            let mut font = base_font.clone();
            let mut color = theme.text;
            if is_header {
                font.weight = gpui::FontWeight::BOLD;
                color.a = 0.9;
            }
            let run = [gpui::TextRun {
                len: c.len(),
                font,
                color,
                ..Default::default()
            }];
            let line = window.text_system().shape_line(c.clone(), font_size, &run, None);
            let w = f32::from(line.width());
            let text_x = match aligns2.get(ci).copied().unwrap_or(TableAlign::Left) {
                TableAlign::Center => ((col_w - w) / 2.).max(0.),
                TableAlign::Right => (col_w - w - 10.).max(0.),
                _ => 10.,
            };
            // 稀疏行（少于列数）把末尾空列映射到最后一个真实单元格的尾部。
            let m = maps.len().saturating_sub(1).min(ci);
            let src = maps.get(m).map(|cm| cm.src.clone()).unwrap_or(0..0);
            let map = maps
                .get(m)
                .map(|cm| std::rc::Rc::new(cm.map.clone()))
                .unwrap_or_default();
            cells.push(TableCellLayout { src, text_x, map, line });
        }
        TableRowCellsLayout { src_line, cells }
    };
    // 表头（源行 = r.src_line）。
    let head_maps = mappings.as_ref().and_then(|r| r.first()).map(|m| m.cells.as_slice());
    trow_edit.push(mk_row(
        window,
        r.src_line,
        header.clone(),
        head_maps.unwrap_or(&[]),
        true,
    ));
    // 数据行：k=0 对应源行 = src_line + 2（分隔行为 src_line+1，不渲染、不可编辑）。
    for (k, texts) in rows2.iter().enumerate() {
        let src_line = r.src_line + 1 + (k + 1);
        let maps = mappings
            .as_ref()
            .and_then(|r| r.get(k + 1))
            .map(|m| m.cells.as_slice())
            .unwrap_or(&[]);
        trow_edit.push(mk_row(window, src_line, texts.clone(), maps, false));
    }
    let editable = !trow_edit.is_empty();
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped: std::rc::Rc::new(Vec::new()),
            has_non_ascii: false,
        sub_heights: vec![height],
        byte_map: std::rc::Rc::new(Vec::new()),
        editable,
        multi_offsets: Vec::new(),
        code_ranges: Vec::new(),
        lh: row_h,
        x_off: px(0.),
        height,
        top: px(0.),
        kind,
        table_edit: if trow_edit.is_empty() {
            None
        } else {
            Some(TableCellsLayout { rows: trow_edit })
        },
        table: Some(ShapedTable {
            widths,
            total_w,
            row_h,
            header_texts: header,
            rows_texts: rows2,
        }),
        image: None,
        mm: None,
        extra: RowExtra::default(),
    }
}

/// 独立图片行的显示高（px）：已加载按真实宽高比换算（宽封顶 `IMAGE_MAX_W`，
/// 由 `avail` 决定实际宽），未加载/失败用占位高。`build_frame` 的带外估算与
/// `shape_image_row` 必须用同一函数 → 加载后带内/带外高度一致（滚动不跳）。
fn image_row_height(editor: &WyEditor, src: &str, avail: f32) -> Pixels {
    let max_w = avail.min(crate::config::markdown::IMAGE_MAX_W);
    match editor.image_size(src) {
        Some(Some((iw, ih))) if iw > 0. && ih > 0. => px((max_w * ih / iw.max(1.)).max(1.)),
        _ => px(crate::config::markdown::IMAGE_ROW_DEFAULT_H),
    }
}

/// 独立图片行：shape 期定显示框（已加载按真实宽高比、未加载用占位高）。
/// 非文本行（不可编辑）；图片内容在 paint 期异步加载后画出。
fn shape_image_row(r: DocRow, editor: &WyEditor, avail: f32) -> ShapedRow {
    let kind = r.kind;
    let alt = r.text.clone();
    let src = match &r.payload {
        DocPayload::Image { src } => src.clone(),
        _ => gpui::SharedString::default(),
    };
    let max_w = avail.min(crate::config::markdown::IMAGE_MAX_W);
    let h = image_row_height(editor, src.as_ref(), avail);
    let w = max_w;
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped: std::rc::Rc::new(Vec::new()),
            has_non_ascii: false,
        sub_heights: vec![h],
        byte_map: std::rc::Rc::new(Vec::new()),
        editable: false,
        multi_offsets: Vec::new(),
        code_ranges: Vec::new(),
        lh: h,
        x_off: px(0.),
        height: h,
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: Some(ShapedImage { src, alt, w, h: f32::from(h) }),
        mm: None,
        extra: RowExtra::default(),
    }
}

/// 把一个物化的 `DocRow` 真正 shape 成可视行（仅带内块走这里）——按块类型分派。
fn shape_docrow(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    avail: f32,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    match r.kind {
        IrKind::Code => shape_code_row(r, window, theme, width),
        IrKind::Table => shape_table_row(r, window, theme.font_size, theme, base_font),
        IrKind::Image => shape_image_row(r, editor, avail),
        _ => shape_text_row_decorated(r, editor, window, width, theme, base_font),
    }
}

/// 数学块卡片的显示行：源码按换行 + LaTeX `\\`（行断）切成多行，逐段做 Unicode
/// 排版。`math_card_height` 与 `paint_card` 必须用同一函数 → 高与绘制逐行对应。
pub(crate) fn math_display_lines(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for ln in src.split('\n') {
        for seg in ln.split("\\\\") {
            let s = math_to_unicode(seg.trim());
            if !s.is_empty() {
                out.push(s);
            }
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 数学块卡片高度：已排版就绪 → 顶部标签留白 + 图片逻辑高；否则回退
/// Unicode 逐行（`20 + n×行高`）。两分支都与 `paint_card` 对应。
pub(crate) fn math_card_height(editor: &WyEditor, reg: &MathRegion) -> Pixels {
    if let Some(super::MathSlot::Ready(_, _, h)) =
        editor.math_slots().borrow().get(reg.source.as_str())
    {
        return px(20. + h.max(16.));
    }
    // 回退：按**显示行数**估高。`math_display_lines` 对每源做 LaTeX→Unicode 转换，
    // 结构编辑时 fold_plan 会对全篇数学块重复求高（3000 块 ~38ms）——走缓存计数。
    let n = editor.math_line_count(reg.source.as_str());
    px(20. + n as f32 * 22.)
}

/// 装配一张数学块卡片行（A1）：整块（含围栏）折叠成一张面板，占据 `height`。
fn build_math_card_row(reg: &MathRegion, src_line: usize, height: Pixels, top: Pixels) -> ShapedRow {
    ShapedRow {
        src_line,
        src_lines: reg.range.start..reg.range.end,
        wrapped: std::rc::Rc::new(Vec::new()),
            has_non_ascii: false,
        sub_heights: vec![height],
        byte_map: std::rc::Rc::new(Vec::new()),
        editable: false,
        multi_offsets: Vec::new(),
        code_ranges: Vec::new(),
        lh: px(0.),
        x_off: px(0.),
        height,
        top,
        kind: IrKind::Paragraph,
        table: None,
        table_edit: None,
        image: None,
        mm: None,
        extra: RowExtra {
            card: Some(BlockCard::Math { align: reg.align, latex: reg.source.clone() }),
            ..Default::default()
        },
    }
}

/// 构建一帧：**窗口化**——仅 `band`（元素局部 y）内的块被物化 + shape，
/// 带外块用 [`estimate_height`] 占位（空 `wrapped`、空 `byte_map`），块高循环
/// 仍覆盖全篇以得到正确 total（滚动条量程）与行序。
pub(crate) fn build_frame(
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    band: (f32, f32),
) -> Frame {
    let _t_bf = crate::prof::begin();
    // 字形/字号一律取**主题**（创建时定格），不读 `window.text_style()`——measure 与
    // prepaint 两阶段该值不同（栈内带/不带 `live-body.text_size`），每帧现取会在两种
    // 字号间交替，滚动时文字忽大忽小。
    let theme = &editor.theme;
    let line_h = theme.para_line();
    let base_font = theme.font.clone();
    let doc = editor.doc();
    let n = doc.block_count();
    let padding = px(12.);
    let rgen = editor.content_gen();

    if n == 0 {
        return Frame {
            padding,
            line_h,
            rows: vec![ShapedRow {
                src_line: 0,
                src_lines: 0..1,
                wrapped: std::rc::Rc::new(Vec::new()),
            has_non_ascii: false,
                sub_heights: vec![line_h],
                byte_map: std::rc::Rc::new(Vec::new()),
                editable: false,
                multi_offsets: Vec::new(),
                code_ranges: Vec::new(),
                lh: line_h,
                x_off: px(0.),
                height: line_h,
                top: px(0.),
                kind: IrKind::Blank,
                table: None,
                table_edit: None,
                image: None,
                mm: None,
                extra: RowExtra::default(),
            }],
            base: std::rc::Rc::new(std::cell::RefCell::new(BlockLayout::new(
                vec![f32::from(line_h)],
                vec![0],
                doc.line_starts().len(),
            ))),
            deltas: std::rc::Rc::from(vec![]),
            total: line_h,
            band,
            width,
            rgen,
            igen: editor.images_gen(),
            mgen: editor.math_gen(),
        };
    }

    let (band_lo, band_hi) = band;
    let avail = (f32::from(width) - f32::from(padding) * 2.).max(1.);
    let width_bits = f32::from(width).to_bits();
    let igen = editor.images_gen();
    let mgen = editor.math_gen();

    // 基础布局：代 = `(struct_ver, 图片/数学/宽度)`。滚动/带移动与**非结构性编辑**
    // 都复用（编辑只对受影响块做 Fenwick 点更新），不再每档/每键对整个 O(n) 高度
    // 数组重算前缀和（旧实现在内容代变化时对全篇重扫 ~400ms，是「一编辑就卡」之一）。
    let _t_setup = crate::prof::begin();
    let epoch = super::height_epoch(editor.struct_ver(), igen, width_bits);
    let (rebuilt, base, old_layout) = editor.ensure_block_layout(n, epoch);
    // 非结构性编辑：基础布局复用，只修补受影响块高（估计 → Fenwick 点更新，O(log n)）。
    if !rebuilt
        && let Some(ixs) = editor.take_pending_patch_blocks()
    {
        let mut b = base.borrow_mut();
        for i in ixs.into_iter().filter(|&i| i < n) {
            if let Some(meta) = doc.block_meta(i) {
                let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
                let h = f32::from(estimate_height(&meta, chars, ascii, theme, avail, line_h));
                b.set(i, h);
            }
        }
    }

    // 结构扫描（derived 缓存 → O(1) Rc 复用）；`math_regions` 在 Pass3 卡片组装用。
    let derived = editor.derived();
    let math_regions = &derived.borrow().math_regions;
    crate::prof::end("wy.bf.setup", _t_setup);

    if rebuilt {
        let _t_p1 = crate::prof::begin();
        // Pass 1（仅每内容/宽度代一次）：全块**基础高**（与折叠/卡片态无关）+ 起始源码行。
        //
        // **三段式增量**（B3 前缀 + 本步尾部复用）：结构编辑后块数组 =
        // [未变前缀][变化中段][未变后缀]。编辑线（`struct_dirty_line`）之前的块内容/
        // 索引未变 → **前缀高度沿用**；编辑影响的旧区间（`old_edit_end_line`）之前的
        // **中段**（编辑线所在的块 → 新尾部起点）重新估算；之后**尾部块内容未变 →
        // 高度直接沿用旧布局**（只平移 src_line）。结构编辑从「dirty_line → 文尾」全
        // 后缀重估（大文档 Enter/代码/表格 183-375ms）降到「仅中段估算 + Vec 复制」。
        let dirty_line = editor.struct_dirty_line();
        let old_end_line = editor.old_edit_end_line();
        let line_delta = editor.edit_line_delta();
        let doc_lines = doc.line_starts().len();
        // 前缀复用长度：块 k 完全落在 dirty_line 之上（块末行 = 下块首行-1 < dirty）
        // → 内容不变、索引稳定。src_line 随块下标单调 → **二分**求「src_line ≤
        // dirty_line 的块数 p」，前缀 keep = p.saturating_sub(1)（编辑线所在块之前）。
        // 旧实现从 0 逐块线性扫（大文档每次 Pass1 O(块数) ~2-5ms）。
        let p = {
            let mut lo = 0usize;
            let mut hi = n;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let sl = doc.block_meta(mid).map(|m| m.src_line as usize).unwrap_or(doc_lines);
                if sl <= dirty_line {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        // 二分界内的块 0..=keep 下标稳定（内容/索引未变）→ 前缀高度沿用。
        let keep = p.saturating_sub(1);
        let old_holder = old_layout.as_ref().cloned(); // 保活旧布局 Rc 以便借用
        let old = old_holder.as_ref().map(|rc| rc.borrow());
        let (old_heights, old_src): (Option<&[f32]>, Option<&[u32]>) = match &old {
            Some(ob) => (Some(ob.heights()), Some(ob.src_lines())),
            None => (None, None),
        };
        let (mut heights, mut src_lines) = match &old {
            Some(ob) => {
                let k = keep.min(ob.n());
                (ob.heights()[..k].to_vec(), ob.src_lines()[..k].to_vec())
            }
            None => (Vec::with_capacity(n), Vec::with_capacity(n)),
        };
        // 旧尾部起点 j：首个旧块 src_line > 编辑影响区间终点。
        let old_exist = old_src.is_some();
        let j = match old_src {
            Some(os) => os.partition_point(|&s| (s as usize) <= old_end_line),
            None => 0,
        };
        // 新尾部起点 k：首个新块 src_line ≥ 旧尾部 src_line + 行数净增。旧布局缺失
        // （首建）或编辑近文末（无旧尾部）时中段扫到底。块 src_line 随下标单调 →
        // 直接**二分**首个 `src_line ≥ tail_src`，替代旧实现对 `[keep, n)` 全部块逐个
        // `doc.block_meta` + `collect` 成 Vec（中段编辑 57k 块 × O(log) 是 pass1 的
        // 4-6ms 主成本）。
        let tail_src = match (old_src, old_src.and_then(|os| os.get(j).copied())) {
            (_, Some(s)) => s as i64 + line_delta,
            _ => doc_lines as i64,
        };
        let k = if !old_exist {
            n
        } else if keep < n {
            let mut lo = keep;
            let mut hi = n;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let sl = doc.block_meta(mid).map(|m| m.src_line as i64).unwrap_or(i64::MAX);
                if sl >= tail_src {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            lo
        } else {
            keep
        };
        // 中段（编辑所在 → 新尾部起点）重新估算。
        let est_block = |i: usize| -> (f32, u32) {
            let meta = if let Some(m) = doc.block_meta(i) {
                m
            } else {
                return (f32::from(line_h), 0);
            };
            let srcline = meta.src_line as usize;
            let h: f32 = if meta.kind == RowKind::Code
                && doc.line_str(srcline).trim_start().starts_with("```mermaid")
            {
                // A2: ```mermaid 围栏 → 图卡片高（与 shape_code_row 同式）。
                let mut src = String::new();
                for k in 0..meta.aux {
                    if k > 0 {
                        src.push('\n');
                    }
                    src.push_str(&doc.line_str(srcline + 1 + k as usize));
                }
                f32::from(mermaid_card_height(&src))
            } else if meta.kind == RowKind::Image {
                // 图片行：用当前加载尺寸按 `shape_image_row` 同式算高（占位→真实由
                // `igen` 触发整表重建，带内外必须同一公式防滚动跳）。
                let src = crate::markdown::parser::parse_image_line(doc.line_str(srcline).trim())
                    .map(|(_, s)| s)
                    .unwrap_or_default();
                f32::from(image_row_height(editor, &src, avail))
            } else {
                let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
                f32::from(estimate_height(&meta, chars, ascii, theme, avail, line_h))
            };
            (h, meta.src_line)
        };
        for i in heights.len()..k {
            let (h, sl) = est_block(i);
            heights.push(h);
            src_lines.push(sl);
        }
        // 尾部复用：旧 [j..] 高度原样搬入新 [k..]（长度恰好吻合时才成立）。
        let mut tail_reused = false;
        if let (Some(oh), Some(os)) = (old_heights, old_src) {
            if k <= n && j <= os.len() && os.len() - j == n - k {
                src_lines.extend(os[j..].iter().map(|&s| (s as i64 + line_delta).max(0) as u32));
                heights.extend_from_slice(&oh[j..]);
                tail_reused = true;
            }
        }
        // 兜底：尾部未复用（首建 / 对齐异常）→ 剩余块照旧逐块重估（正确 > 快）。
        if !tail_reused {
            for i in heights.len()..n {
                let (h, sl) = est_block(i);
                heights.push(h);
                src_lines.push(sl);
            }
        }
        debug_assert_eq!(heights.len(), n, "Pass1 三段式产出的块数与新索引一致");
        base.borrow_mut().rebuild(heights, src_lines, doc_lines);
        crate::prof::end("wy.pass1", _t_p1);
    }
    // 折叠/重排计划（与可视带无关、不含 caret，跨带/跨光标复用——滚动与输入不再重算
    // `math_start`/`fold_lines`/`deltas`）。caret 所在区域的 reveal 由 `fold_reveal`
    // 按带现算叠加（最多一个数学块 + 一个折叠体）。⚠️ 必须在 Pass1 之后调用：
    // `deltas` 读基础布局当前高度（重建过的 epoch 使缓存键变化 → 自动重建）。
    let caret_line = editor.row_col(editor.caret()).0;
    let _t_plan = crate::prof::begin();
    let _t_fold = crate::prof::begin();
    let plan = editor.fold_plan(base.clone(), epoch, mgen);
    crate::prof::end("wy.bf.plan.fold", _t_fold);
    let _t_rv = crate::prof::begin();
    let reveal = editor.fold_reveal(caret_line, &plan, &base);
    crate::prof::end("wy.bf.plan.reveal", _t_rv);
    let math_start = &plan.math_start;
    let math_body = &plan.math_body;
    let fold_lines = &plan.fold_lines;
    // 有效增量：无 reveal（常见：光标在普通行）→ 直接复用计划 deltas（零拷贝）；
    // 有 reveal → 过滤掉被归零的块（最多几个），每带一次 O(k) 拷贝。
    let (deltas, delta_sum): (std::rc::Rc<[(usize, f32)]>, f32) = if reveal.is_empty() {
        (plan.deltas.clone(), plan.delta_sum)
    } else {
        let eff: Vec<(usize, f32)> = plan
            .deltas
            .iter()
            .copied()
            .filter(|(ix, _)| !reveal.zeroed.contains(ix))
            .collect();
        let s: f32 = eff.iter().map(|&(_, v)| v).sum();
        (eff.into_boxed_slice().into(), s)
    };
    let revealed = |srcline: usize| reveal.contains(srcline);
    crate::prof::end("wy.bf.plan", _t_plan);

    // 带内块区间 [i0, i1)：用「有效前缀」（基础 + 动态增量，单调）二分。
    let _t_bis = crate::prof::begin();
    // `deltas` 前缀和表：`delta_pref[p]` = 前 p 项增量之和（p ∈ [0, len]），一次
    // O(len) 建成 → 任意 `eff_top(i)` 只需 O(log len) 定位（partition_point 找
    // 首个 `ix >= i`），旧实现每个二分步都对前缀现算 O(len) 求和：带内 3000 条增量
    // 时 bisect ~2.8ms/帧、Pass3 逐行 O(rows×len) ~27ms/帧。
    let delta_pref: Vec<f32> = {
        let mut v = Vec::with_capacity(deltas.len() + 1);
        let mut s = 0f32;
        v.push(0.);
        for &(_, d) in deltas.iter() {
            s += d;
            v.push(s);
        }
        v
    };
    let eff_top = |i: usize| -> f32 {
        let b = base.borrow();
        let k = deltas.partition_point(|&(ix, _)| ix < i);
        b.top(i) + delta_pref[k]
    };
    // 二分终止右闭（k ∈ [0, n]）：返回有效前缀满足谓词的 k 计数。
    let count_true = |pred: &dyn Fn(usize) -> bool| -> usize {
        let mut l = 0usize;
        let mut r = n + 1;
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
    let i0 = count_true(&|k: usize| eff_top(k) <= band_lo).saturating_sub(1);
    let i1 = count_true(&|k: usize| eff_top(k) < band_hi).min(n);
    crate::prof::end("wy.bf.bisect", _t_bis);

    // Pass 2：带内非折叠/非数学块 shape，真实高 `set` 回基表（带外沿用收敛真值）。
    // reveal 覆盖：caret 所在区域的行不再 blocked（还原源码 shape）。
    // 行整形缓存（滚动/带重建复用）：键 = epoch ^ mgen；caret 严格落在含行内公式的
    // 文本行时（reveal，shape 依赖光标）该行不缓存；编辑作废由 `replace_range` 负责。
    let shape_key = epoch ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    let _t_p2 = crate::prof::begin();
    let mut shaped: Vec<(usize, ShapedRow)> = Vec::new();
    {
        let mut row_cache = editor.shape_rows.borrow_mut();
        let mut i = i0;
        while i < i1 {
            let srcline = base.borrow().src_lines()[i] as usize;
            let blocked = !revealed(srcline)
                && (fold_lines.contains(&srcline)
                    || math_body.contains(&srcline)
                    || math_start.contains_key(&srcline));
            if blocked {
                i += 1;
                continue;
            }
            let lo = i;
            while i < i1 {
                let s = base.borrow().src_lines()[i] as usize;
                if !revealed(s)
                    && (fold_lines.contains(&s)
                        || math_body.contains(&s)
                        || math_start.contains_key(&s))
                {
                    break;
                }
                i += 1;
            }
            let hi = i;
            // A1「命中先行」：整带内先判哪些块**不需要**物化 DocRow——命中 = 缓存键
            // 匹配且非 caret 行（caret 行 shape 依赖光标，即便已在缓存也必须强制物化
            // 重 shape、不缓存；逐帧至多一行）。只对「未命中 ∪ caret 行」批量物化
            // （连续段合并复用批量 parse），命中块免去 `rows_in_block_window` 的整带
            // parse+assemble（byte_map / 表格 / code payload 装配）——每键/每带重建
            // 从「整带物化」降到「仅 miss 块」。
            let mut need_row: Vec<usize> = Vec::new();
            let mut hit_map: std::collections::HashMap<usize, std::rc::Rc<ShapedRow>> =
                std::collections::HashMap::new();
            for k in lo..hi {
                let srcline = base.borrow().src_lines()[k] as usize;
                if caret_line == srcline {
                    need_row.push(k);
                } else if let Some(rc) = row_cache.get(k, shape_key) {
                    if shaped_row_has_notdef(&rc) && !editor.notdef_settled(k) {
                        // 命中但含 .notdef（此前冷启动坏字形被缓存/预热写入）：视作
                        // miss，强制以新鲜平台重排——否则豆腐块被旧缓存钉住整次运行。
                        // 已 `settled`（判定确无字形）的块例外：tofu 即正确显示，正常命中。
                        need_row.push(k);
                    } else {
                        hit_map.insert(k, rc);
                    }
                } else {
                    // 缓存 miss。若存在**旧键**条目（内容/图片/数学/宽度换代把旧行逐出），
                    // 旧 `settled/verified/attempts` 判决对应的是上一代结果——可能在图片
                    // 解码卡顿窗口里留下（.notdef 预算烧光→settled / 非零占位字形→verified），
                    // 换代重排前作废，让本块按新代重新校验/重试。图片加载完成的 `igen` 换代
                    // 重建会走到这里 → 自愈，不再整次运行永久豆腐块。保留冷却帧不闪跳。
                    if row_cache.key_mismatch(k, shape_key) {
                        editor.clear_stale_verdicts(k);
                    }
                    need_row.push(k);
                }
            }
            // 只物化 miss / caret 块（连续段合并）。
            let mut rows_map: std::collections::HashMap<usize, DocRow> =
                std::collections::HashMap::new();
            let _t_parse = crate::prof::begin();
            let mut ni = 0usize;
            while ni < need_row.len() {
                let lo2 = need_row[ni];
                let mut hi2 = lo2;
                ni += 1;
                while ni < need_row.len() && need_row[ni] == hi2 + 1 {
                    hi2 += 1;
                    ni += 1;
                }
                for (off, r) in doc.rows_in_block_window(lo2, hi2).into_iter().enumerate() {
                    rows_map.insert(lo2 + off, r);
                }
            }
            crate::prof::end("wy.bf.blockparse", _t_parse);
            // 整带顺序装配（Pass3 依赖 `shaped` 与块序一致）：命中 → 浅克隆
            // （heavy 数据都是 Arc/Rc，top 在 Pass3 现设）；否则从物化行 shape。
            let _t_shape = crate::prof::begin();
            for k in lo..hi {
                let srow = if let Some(rc) = hit_map.remove(&k) {
                    (*rc).clone()
                } else if let Some(last) = editor.cooling_row(k) {
                    // 冷却帧：不调用 gpui shape（被钉的 `LineLayoutCache` 键本帧不被
                    // 引用 → 帧末自然淘汰），复用上一份 notdef 行几何继续显示。次帧
                    // 重排才是新键、真打到平台（DWrite 回退自愈的必要条件）。
                    // 强制下一帧：静态视图不会再自渲染，若不请求重绘则自愈停摆。
                    window.refresh();
                    #[cfg(test)]
                    editor.bump_coolings();
                    (*last).clone()
                } else {
                    let r = rows_map.remove(&k).expect("未命中 / caret 行应已被物化");
                    let is_caret = !matches!(r.kind, IrKind::Code | IrKind::Table | IrKind::Image)
                        && caret_line == r.src_line;
                    let rl = r.src_line;
                    #[cfg(test)]
                    editor.bump_reshapes();
                    let srow = shape_docrow(r, editor, window, width, avail, theme, &base_font);
                    if shaped_row_has_notdef(&srow) {
                        // —— 冷启动回退未命中（glyph id=0）→「冷却一帧 → 新鲜平台重排」——
                        // 一律重试（**不**因“回退已温”就静默缓存成豆腐块：探针只覆盖少数
                        // (font,size)，真实行字号/字体可能未探到而首排仍坏——实测首排坏、
                        // 次排好；冷却一帧让被钉的 LineLayoutCache 键淘汰，次排才是新键、
                        // 真打到 DWrite 平台）。
                        if editor.notdef_settled(k) {
                            // 已判定缺字形（仅 caret 行会到此处，非 caret 行在 settled 时
                            // 已入缓存、走命中路径）：直接显示，不再冷却空转。
                            srow
                        } else if editor.cool_start(k) {
                            // 连续 N 次真实平台重排仍 .notdef → 判定字符确无字形
                            // （tofu 即正确显示），从此不再重排空转。
                            notdef_debug(format_args!(
                                "pass2 give-up k={} src={} caret={is_caret}",
                                k, rl
                            ));
                            if is_caret {
                                srow
                            } else {
                                let rc = row_cache.put(k, shape_key, srow);
                                (*rc).clone()
                            }
                        } else {
                            // 入冷却：本帧几何留作下帧显示、且下帧不 shape（被钉的
                            // LineLayoutCache 键不被引用 → 帧末淘汰 → 再下帧重排才是
                            // 新键、真重新打到 DWrite）。caret 行同样受益（否则首行 ❗
                            // 每帧命中旧坏键，豆腐块永不消失）。强制下一帧推进自愈。
                            window.refresh();
                            editor.cool_keep_last(k, std::rc::Rc::new(srow.clone()));
                            notdef_debug(format_args!(
                                "pass2 notdef k={} src={} caret={is_caret} -> cooling",
                                k, rl
                            ));
                            srow
                        }
                    } else if row_uses_fallback(window.text_system(), &base_font, &srow)
                        && !editor.fallback_verified(k)
                    {
                        // —— 冷启动回退占位字形（glyph id≠0，检测不到 .notdef）——
                        // 首次遇到用到回退字体（字体族 ≠ 基字体）的行，DWrite 首次 shape
                        // 可能给「错误回退字体里的非零占位字形」→ 渲染成方框，且因 glyph
                        // id≠0 逃过上面的 notdef 检测、会被直接缓存钉住（caret 行正常 /
                        // 非 caret 行方框的翻转就是它）。不缓存首次结果：标记已校验 + 冷却
                        // 一帧强制第二次真实重排（实测第二次即正常），次排走正常缓存路径。
                        editor.mark_fallback_verified(k);
                        editor.verify_cool(k);
                        window.refresh();
                        editor.cool_keep_last(k, std::rc::Rc::new(srow.clone()));
                        notdef_debug(format_args!(
                            "pass2 fallback-verify k={} src={} caret={is_caret}",
                            k, rl
                        ));
                        srow
                    } else {
                        // 干净排（无回退，或已校验过回退字形）：清掉本块此前任何重试/
                        // settled 状态。caret 行若**无行内数学**（reveal 不依赖光标）也入
                        // 缓存——用新鲜的正确答案覆盖早期冷启动缓存的坏行：用户点击该行
                        // 一次后永久修复（否则每次点击离开又回到旧坏行 = “点击翻转”）。
                        editor.clear_notdef(k);
                        if is_caret && !srow.extra.inline_maths.is_empty() {
                            srow // reveal 依赖光标 → 保持不缓存
                        } else {
                            let rc = row_cache.put(k, shape_key, srow);
                            (*rc).clone()
                        }
                    }
                };
                let srcl = base.borrow().src_lines()[k] as usize;
                debug_special_row(editor, window, srcl, &srow, &base_font);
                base.borrow_mut().set(k, f32::from(srow.height));
                shaped.push((k, srow));
            }
            crate::prof::end("wy.bf.shape", _t_shape);
        }
    }
    crate::prof::end("wy.pass2", _t_p2);

    // Pass 3：组装带内行（有效 top = 基础前缀 + 动态增量前缀，`set` 后已是最终高度）。
    let _t_pass3 = crate::prof::begin();
    let mut rows: Vec<ShapedRow> = Vec::new();
    let mut shaped_iter = shaped.into_iter().peekable();
    let total = {
        let b = base.borrow();
        b.total()
    } + delta_sum;
    // 各路行按 top 从 base+deltas 现算；total 已含全部。
    // `deltas` 按下标升序 → 用滚动指针 + 累加器：每行 O(1) 摊还拿到「ix<i 的增量和」
    // 与「ix==i 的命中增量」，替代旧实现的逐行 `partition_point`+O(k) 求和+线性
    // `find`（带 3000 条增量时 Pass3 ~27ms/帧 → ~2ms）。
    let mut di = 0usize;
    let mut acc = 0f32;
    for i in i0..i1 {
        while di < deltas.len() && deltas[di].0 < i {
            acc += deltas[di].1;
            di += 1;
        }
        let srcline = base.borrow().src_lines()[i] as usize;
        let b = base.borrow();
        let d_here = if di < deltas.len() && deltas[di].0 == i {
            Some(deltas[di].1)
        } else {
            None
        };
        let h = b.height(i) + d_here.unwrap_or(0.);
        let top = b.top(i) + acc;
        drop(b);
        if h <= 0. {
            continue; // 折叠 body / 数学 body：0 高不物化
        }
        let shown_source = revealed(srcline); // caret 在此区域 → 显示源码而非卡片/折叠
        let folded_here = !shown_source
            && (fold_lines.contains(&srcline) || math_body.contains(&srcline));
        if !folded_here
            && !shown_source
            && let Some(&ridx) = math_start.get(&srcline)
        {
            // 数学块卡片（A1）。
            let reg = &math_regions[ridx];
            rows.push(build_math_card_row(reg, srcline, px(h), px(top)));
            continue;
        }
        if shaped_iter.peek().map(|(k, _)| *k) == Some(i) {
            let (_, mut srow) = shaped_iter.next().unwrap();
            srow.top = px(top);
            rows.push(srow);
        }
    }
    crate::prof::end("wy.pass3", _t_pass3);
    crate::prof::end("wy.build_frame", _t_bf);
    Frame {
        padding,
        line_h,
        rows,
        base,
        deltas,
        total: px(total),
        band,
        width,
        rgen,
        igen,
        mgen,
    }
}

/// 源列 → 渲染列（左界：首个 `byte_map[v] >= target`；与 `display_to_source` 互逆，
/// 光标才落回点击处——用 `>` 会整格右移一字符）。
fn source_to_display(byte_map: &[usize], target: usize) -> usize {
    byte_map.partition_point(|&b| b < target)
}

/// 渲染列 → 源列（byte_map 查值；越界钳到末端哨兵 = 源段尾）。
fn display_to_source(byte_map: &[usize], dcol: usize) -> usize {
    let idx = dcol.min(byte_map.len().saturating_sub(1));
    *byte_map.get(idx).unwrap_or(byte_map.last().unwrap_or(&0))
}

/// 单个可编辑块的渲染列 → 块内光标位置（`position_for_index` 对折行返回
/// `(子行内 x, 子行偏移·行高)`，谓以块首为原点）。
fn render_point_of(wrapped: &[gpui::WrappedLine], line_h: Pixels, dcol: usize) -> Point<Pixels> {
    if let Some(w0) = wrapped.first() {
        w0.position_for_index(dcol, line_h)
            .unwrap_or_else(|| point(px(0.), px(0.)))
    } else {
        point(px(0.), px(0.))
    }
}

/// 文本行正文因行内公式撑高而整体下移的量（px；未撑高 = 0）——paint 文本/公式、
/// 光标几何、点击命中、选区、链接 hitbox 必须共用，否则撑高行上光标/点击/选区
/// 会错位（用户实测：点击该行闪烁光标位置不对）。
fn text_row_shift(row: &ShapedRow) -> f32 {
    let Some(w0) = row.wrapped.first() else {
        return 0.0;
    };
    let band = row.sub_heights.get(0).copied().unwrap_or(row.lh);
    let orig = row.lh * (w0.wrap_boundaries().len() + 1) as f32;
    ((f32::from(band) - f32::from(orig)) / 2.).max(0.)
}

/// 代码块内 dy（相对块首 y）→ 源码行组序号（`sub_heights` 累积；块首 1 行空白带
/// 归第 0 组——代码块 `row.lh` = code_line，与 paint 的首空白带同高）。
fn code_group_at(row: &ShapedRow, dy: f32) -> usize {
    if row.sub_heights.is_empty() {
        return 0;
    }
    let mut acc = f32::from(row.lh);
    for (k, sh) in row.sub_heights.iter().enumerate() {
        acc += f32::from(*sh);
        if dy < acc {
            return k;
        }
    }
    row.sub_heights.len() - 1
}

/// 块级渲染字节 → 所在源码行组序号（`multi_offsets` 起始偏移左界折半）。
fn group_from_rend(row: &ShapedRow, rend: usize) -> usize {
    if row.multi_offsets.is_empty() {
        return 0;
    }
    row.multi_offsets
        .partition_point(|&o| o <= rend)
        .saturating_sub(1)
        .min(row.multi_offsets.len() - 1)
}

/// 第 `g` 个源码行组的**渲染字节长**（组内列上限）。
fn group_len(row: &ShapedRow, g: usize) -> usize {
    let total = row.byte_map.len().saturating_sub(1);
    let start = row.multi_offsets.get(g).copied().unwrap_or(0);
    let next = row
        .multi_offsets
        .get(g + 1)
        .copied()
        .unwrap_or(total)
        .min(total);
    next.saturating_sub(start).max(1)
}

/// 代码块第 `g` 个源码行组的垂直偏移（相对块首，含 1 行首空白带）。
fn code_group_top(row: &ShapedRow, g: usize) -> f32 {
    let mut acc = f32::from(row.lh);
    for sh in row.sub_heights.iter().take(g) {
        acc += f32::from(*sh);
    }
    acc
}

/// 表格块内 dy（相对块首）→ 可见行号（0=表头，i=数据行 i）。布局与
/// [`paint_table_row`] 同式（上边框 1px、行间分隔 1px）。带外/空白间隙返回 None。
fn table_row_at(t: &ShapedTable, dy: f32) -> Option<usize> {
    let rh = f32::from(t.row_h);
    if dy < 1. {
        return None;
    }
    if dy < 1. + rh {
        return Some(0);
    }
    let mut y = 1. + rh;
    for i in 0..t.rows_texts.len() {
        y += 1.; // 行间分隔线
        if dy < y + rh {
            return Some(1 + i);
        }
        y += rh;
    }
    None
}

/// 可见行 `row_ix` 的块内顶部 y（与 [`table_row_at`] 互逆）。
fn table_row_top(t: &ShapedTable, row_ix: usize) -> f32 {
    if row_ix == 0 {
        return 1.;
    }
    1. + f32::from(t.row_h) + (row_ix - 1) as f32 * (1. + f32::from(t.row_h)) + 1.
}

/// 内容坐标 x → 列号（循环列宽；超出总宽返回 None）。
fn table_cell_index(widths: &[f32], x: f32) -> Option<usize> {
    let mut acc = 0f32;
    for (ci, w) in widths.iter().enumerate() {
        if x < acc + *w {
            return Some(ci);
        }
        acc += *w;
    }
    None
}

/// 列号 → 该列左缘坐标（内容坐标）。
fn table_cell_x0(widths: &[f32], cell_ix: usize) -> f32 {
    widths.iter().take(cell_ix).sum()
}

/// src caret（相对行首）→ 渲染字节（`map` 左界，<>`source_to_display`）。
fn table_cell_display(map: &[usize], target: usize) -> usize {
    map.partition_point(|&s| s < target)
}

/// src caret（相对行首）→ 表格单元格几何：(单元格列号, 列内渲染列)。找不到返回 None。
fn table_cell_at_src(
    rows: &[TableRowCellsLayout],
    row_line: usize,
    target: usize,
) -> Option<(usize, usize)> {
    let row = rows.iter().find(|r| r.src_line == row_line)?;
    let nonempty: Vec<usize> = row
        .cells
        .iter()
        .enumerate()
        .filter(|(_, c)| c.src.start < c.src.end)
        .map(|(i, _)| i)
        .collect();
    let cell_ix = row
        .cells
        .iter()
        .position(|c| {
            c.src.start < c.src.end && target >= c.src.start && target <= c.src.end
        })
        .unwrap_or_else(|| {
            // 落在单元格间隙（管道/空白区）：取最近的非空格尾；都无则取第一个非空格。
            nonempty
                .iter()
                .rev()
                .copied()
                .find(|&i| row.cells[i].src.start <= target)
                .or_else(|| nonempty.first().copied())
                .unwrap_or(0)
        });
    let dcol = table_cell_display(&row.cells[cell_ix].map, target);
    let dcol = dcol.min(row.cells[cell_ix].line.len());
    Some((cell_ix, dcol))
}

/// **D1 滚动预热**：把「当前带 + 滚动方向的下一量子带」内尚未整形的块提前 shape 进
/// [`RowShapeCache`]（键 = `epoch ^ mgen`，与 band/帧键**无关**，与 `build_frame` Pass2
/// 命中判据同式）→ 带穿越时 Pass2 直接命中缓存，每 512px 一次的 ~6.4ms/band shape
/// 突发被摊平（预算按滚动速度自适应：静止 3 块/帧、快滚最高 24 块/帧，见
/// [`super::Prewarm::effective_budget`]）。
///
/// 硬约束：
/// - **只 `put` 缓存、绝不 `base.set()`**——不提前改 total，滚动条量程/布局零抖动；
///   真高收敛仍由进带时 Pass2 的 `set` 完成。
/// - 跳过折叠/数学块（不自走整形缓存）与 caret 行（shape 依赖光标，进带时 Pass2 强制
///   重 shape）；fold_plan 无碍正确性——Pass2 的 `blocked` 判定始终以现算 plan 为准，
///   预热条目若因 reveal 变活跃也只是浅克隆复用。
/// - 方向只由带序号跨界（`band.top/512` 取整变化）判定；预算按速度自适应
///   （`Prewarm::effective_budget`：基础 `blocks_per_frame` + 速度×4，硬上限 24），
///   `bias` 滑动窗每帧推进、回绕兜底——区间扫满即全部命中/跳过，空转开销≈HashMap 查找。
pub(crate) fn prewarm_band(e: &WyEditor, frame: &Frame, window: &mut Window) {
    let q = 512.;
    let mut st = e.prewarm.borrow_mut();
    let (blo, bhi) = frame.band;
    let bi = (blo / q) as i32;
    // 滚动速度（带/帧）：本帧相对上一帧登记的带序号跳跃量。快滚（跳多档）时按速度放大
    // 每帧预热预算，否则 3 块/帧追不上视口 → 落点带仍冷、每带穿越补 Pass2 突发。
    let vel = st
        .last_band_idx
        .map(|lbi| bi.abs_diff(lbi) as usize)
        .unwrap_or(1);
    if let Some(lbi) = st.last_band_idx {
        if bi != lbi {
            st.dir = if bi > lbi { 1 } else { -1 };
            st.bias = 0;
        }
    } else {
        // 首帧：无方向可判，只登记带序号。
        st.last_band_idx = Some(bi);
        return;
    }
    st.last_band_idx = Some(bi);
    if st.dir == 0 {
        return;
    }
    // 目标量子带 = 当前带沿方向平移一个量子。
    let (tlo, thi) = if st.dir > 0 { (bhi, bhi + q) } else { (blo - q, blo) };
    if tlo < 0. || tlo >= thi {
        return;
    }
    let (i0, i1) = frame.band_block_range(tlo, thi);
    if i0 >= i1 {
        return;
    }
    // shape_key 与 `build_frame` 完全同式（epoch = struct_ver ^ igen ^ width_bits）。
    let width_bits = f32::from(frame.width).to_bits();
    let epoch = super::height_epoch(e.struct_ver(), e.images_gen(), width_bits);
    let sk = epoch ^ e.math_gen().wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    let caret_line = e.row_col(e.caret()).0;
    let plan = e.fold_plan_rc();
    let doc = e.doc();
    let avail = (f32::from(frame.width) - f32::from(frame.padding) * 2.).max(1.);
    let bpf = st.effective_budget(vel);
    let mut rc = e.shape_rows.borrow_mut();
    let n = i1 - i0;
    if st.bias >= n {
        st.bias = 0;
    }
    let _t = crate::prof::begin();
    // 每帧预热**时间硬上限**：块数预算（最高 24 块/帧）× 昂贵块（debug 单块可就几十
    // ms）能把单帧干到 1.3s（实测快滚跨新带）。按已过时间截断，再不阻塞主线程——
    // 预热只是「提前整形、进带可浅克隆复用」，少预几张由进带 Pass2 补，不影响正确性。
    let t0 = std::time::Instant::now();
    let time_budget = std::time::Duration::from_millis(8);
    let mut done = 0usize;
    let mut k = i0 + st.bias;
    loop {
        if done >= bpf || t0.elapsed() > time_budget {
            break;
        }
        let srcline = frame.base.borrow().src_lines()[k] as usize;
        // 大块跳过预热：单块 DocRow 覆盖整块源区间，`rows_in_block_window(k,k)` 会
        // 一次性物化并整形整个块（debug 胖表格/长代码段可达 1.6s/块，实测快滚跨界时
        // 单帧窒息）。预热只挑小/中块；大块进带时由 Pass2 权威整形（那时本来就要渲染）。
        // 判定用「字节跨度 + 行跨度」双阈值——行数不反映行的宽度（胖表格多列），字节更准。
        let big = {
            let base_ref = frame.base.borrow();
            let sl = base_ref.src_lines();
            let s0 = sl.get(k).copied().unwrap_or(0) as usize;
            let s1 = sl.get(k + 1).copied().map(|v| v as usize).unwrap_or(s0 + 1);
            let span_lines = s1.saturating_sub(s0);
            let span_bytes = doc
                .block_meta(k)
                .and_then(|m| doc.block_meta(k + 1).map(|n| n.byte as i64 - m.byte as i64))
                .unwrap_or(0);
            span_lines > 64 || span_bytes > 4096
        };
        // 与 Pass2 `blocked` 同判据：折叠/math 块不物化整形行。
        let blocked = match &plan {
            Some(p) => {
                p.fold_lines.contains(&srcline)
                    || p.math_body.contains(&srcline)
                    || p.math_start.contains_key(&srcline)
            }
            None => false,
        };
        if !blocked && !big && caret_line != srcline && rc.get(k, sk).is_none() {
            // 与 Pass2 miss 分支同式：整形成功即入缓存（随后 Pass2 命中时浅克隆复用）。
            // 含 .notdef 的坏行**不入缓存**——进带后由 Pass2 以「冷却一帧 → 新鲜重排」
            // 自愈（一律重试，不为“回退已温”静默缓存）。
            if let Some(r) = doc.rows_in_block_window(k, k).into_iter().next() {
                #[cfg(test)]
                e.bump_prewarm_shapes();
                let row =
                    shape_docrow(r, e, window, frame.width, avail, &e.theme, &e.theme.font);
                if !shaped_row_has_notdef(&row) {
                    rc.put(k, sk, row);
                }
            }
            done += 1;
        }
        k += 1;
        if k >= i1 {
            k = i0;
        }
        if k == i0 + st.bias {
            break; // 一圈完成（全命中/跳过）→ 本帧收工，防止空转
        }
    }
    crate::prof::end("wy.prewarm", _t);
    st.bias = (st.bias + bpf) % n;
}

/// Windows 冷启动 color-emoji 光栅预热（一次/进程）。实测：DWrite 在冷进程里**首次**
/// 光栅化彩色 COLR 字形时 `rasterize_color` 失败，gpui 回退为单色（彩色 emoji 没有
/// 单色轮廓 → 空白/方框），且 `sprite_atlas` 会按 (font,glyph,size) 把这份坏结果钉住
/// 整次运行。这里在 WYSIWYG 首帧 paint 阶段（任何真实行打印之前）打热彩字管线；
/// 具体实现见 [`super::warm_emoji_rasterizer`]（共享给预览/源码编辑区，先画的窗格先
/// 预热，杜绝被先画窗格毒化）。
fn warm_emoji_rasterizer(window: &mut Window) {
    super::warm_emoji_rasterizer(window);
}

/// 元素：auto-grow、宿主滚动。
pub(crate) struct WyEditorElement {
    pub editor: Entity<WyEditor>,
}

/// 元素一帧的 prepaint 产物：可点击链接的 hand 光标 hitbox（paint 期设光标样式）。
#[derive(Default)]
pub(crate) struct WyPrepaint {
    link_grips: Vec<Hitbox>,
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
            // 第一轮布局：available 为 Indefinite 且 known 宽也为空（探测性测量）。
            // 此时构建的帧（默认 800px 宽）下一轮必被真实宽度替换——纯浪费
            // （实测每键 ~100ms debug）。改为复用现成帧总高 / 行数估算，不建帧；
            // 下一轮 definite 宽度到达时再真正构建一次。
            let probe = !width_definite && known.width.is_none();
            // 帧缓存：内容/宽度/可视带/图片代数都没变 → 直接复用总高，不再整篇重建
            // （大文档的每帧“卡死”就是这里：每次 notify 都重排全 16 万行）。
            let rgen = e.content_gen();
            let igen = e.images_gen();
            let mgen = e.math_gen();
            let band = e.shape_band().unwrap_or((0., 2048.));
            let cached_total = e
                .frame
                .borrow()
                .as_ref()
                .filter(|f| {
                    f.rgen == rgen
                        && f.igen == igen
                        && f.mgen == mgen
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

        // 带移动 / 内容 / 宽度 / 图片/数学加载变化 → 按新状态重建窗口化帧。
        let rgen = self.editor.read(cx).content_gen();
        let igen = self.editor.read(cx).images_gen();
        let mgen = self.editor.read(cx).math_gen();
        let width = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map(|f| f.width).unwrap_or(px(800.))
        };
        let stale = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map_or(true, |f| {
                f.rgen != rgen
                    || f.igen != igen
                    || f.mgen != mgen
                    || f.width != width
                    || f.band != band
            })
        };
        if stale {
            // 诊断：本次重建的诱因（排查 measure/prepaint 双重建；仅 WY_TRACE）。
            if crate::prof::trace_active() {
                let f = self.editor.read(cx).frame.borrow();
                let reason = f.as_ref().map_or("no-frame".into(), |f| {
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
                    if f.width != width {
                        v.push("width");
                    }
                    if f.band != band {
                        v.push("band");
                    }
                    v.join(",")
                });
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
            let frame = build_frame(&*self.editor.read(cx), window, width, band);
            crate::prof::end("wy.prepaint.bf", _t_bb);
            self.editor.update(cx, |e, _| {
                *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
            });
        }

        // 光标可视位置（元素局部）→ IME bounds。
        let frame = {
            let e = self.editor.read(cx);
            e.frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        if let Some(f) = frame.as_ref() {
            let editor = self.editor.read(cx);
            let (on, x, ctop) = caret_geometry(&*editor, f);
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
        }
        // D1：滚动预热——带穿越前把下一量子带块提前 shape 进缓存（预算内摊平突发）。
        if let Some(f) = frame.as_ref() {
            prewarm_band(&*self.editor.read(cx), f, window);
        }
        crate::prof::end("wy.prepaint", _t_pp);
        WyPrepaint { link_grips }
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
        // 数学排版同步（可见带懒光栅）：只对**当前带内**公式发起排版（对齐
        // view/editor 只渲染可见区；不再开篇全量光栅 1.3 万公式占满后台 CPU）。
        let _t_sm = crate::prof::begin();
        self.editor
            .update(cx, |e, ecx| e.sync_math(ecx, f));
        crate::prof::end("wy.paint.synmath", _t_sm);
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
        let _t_row = crate::prof::begin();
        for row in &f.rows {
            if row.kind == IrKind::Quote && !row.wrapped.is_empty() {
                paint_quote_band(origin, &theme, f, row, alert_kinds, window);
            }
        }
        for row in &f.rows {
            // 块级卡片（数学块 / Mermaid 图）优先自绘，跳过普通文本/代码路径。
            if let Some(card) = &row.extra.card {
                paint_card(&self.editor, origin, &theme, f, row, card, window, cx);
                continue;
            }
            // 非文本块按类型自绘：代码面板 / 表格网格 / 图片。
            match row.kind {
                IrKind::Code => {
                    // 带外（未 shape）代码块不画（占位无内容）。
                    if row.wrapped.is_empty() {
                        continue;
                    }
                    paint_code_row(&self.editor, origin, &theme, f, row, window, cx);
                    continue;
                }
                IrKind::Table => {
                    if let Some(t) = &row.table {
                        paint_table_row(&self.editor, origin, &theme, f, row, t, window, cx);
                    }
                    continue;
                }
                IrKind::Image => {
                    if let Some(img) = &row.image {
                        paint_image_row(
                            &self.editor,
                            origin,
                            &theme,
                            f,
                            row,
                            img,
                            window,
                            cx,
                        );
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
            paint_row_gutter(origin, &theme, f, row, window, cx);
            // 引述块背景已在上面预画；这里只画文字 + 警报 chrome（标签/箭头叠在文字上）。
            if row.kind == IrKind::Quote {
                paint_text_row(origin, &theme, f, row, window, cx);
                paint_alert_chrome(origin, &theme, f, row, window, cx);
            } else {
                paint_text_row(origin, &theme, f, row, window, cx);
                // 标题折叠箭头（标题保留行右缘 muted 小三角；点击翻转视图本地折叠态）。
                if matches!(row.kind, IrKind::Heading(_)) {
                    paint_heading_chevron(&self.editor, origin, &theme, f, row, window, cx);
                }
            }
            // 行内数学 / 行内图片叠加（B1/B2）。
            paint_inline_maths(&self.editor, origin, &theme, f, row, window, cx);
            paint_inline_images(&self.editor, origin, f, row, window, cx);
        }
        crate::prof::end("wy.paint.rowloop", _t_row);
        // C5: 悬浮在可点击链接上 → hand 光标。
        for hb in &prepaint.link_grips {
            window.set_cursor_style(CursorStyle::PointingHand, hb);
        }
        // 选区高亮（可编辑行与源选区相交的部分，按字形几何逐个 WrappedLine 画）。
        if let Some(range) = editor_selection(&self.editor, cx) {
            paint_selection(&self.editor, origin, &theme, f, &range, window, cx);
        }
        // 光标几何每帧按 caret 现算（帧缓存不含 caret，改 caret 无需整帧重建）。
        let (caret_on, caret_x, caret_top) = {
            let e = self.editor.read(cx);
            caret_geometry(&*e, f)
        };
        if caret_on && focus.is_focused(window) {
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
        crate::prof::end("wy.paint", _t_paint);
        // 操作级实时计时结算：本帧若含未结算操作（type/backspace/scroll/open…），
        // 在此以「开始→绘制完成」的墙钟 + 帧内阶段 breakdown 落地一行 OP 记录。
        crate::prof::op_finish_after_paint();
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;


