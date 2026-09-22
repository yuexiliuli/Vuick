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

use super::{CaretVis, WyEditor, WyTheme};
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

/// 一帧后的表格渲染数据（shape 期算好列宽/全量单元格文本，paint 直接画网格）。
/// 与预览一致：列宽按内容自适应（不缩放、不省略），整表自然宽，超出由宿主裁剪。
pub(crate) struct ShapedTable {
    pub widths: Vec<f32>,
    pub total_w: f32,
    pub row_h: Pixels,
    pub header_texts: Vec<gpui::SharedString>,
    pub rows_texts: Vec<Vec<gpui::SharedString>>,
    pub aligns: Vec<TableAlign>,
}

/// 一帧后的图片渲染数据（显示框 + 源 + alt 兜底）。
pub(crate) struct ShapedImage {
    pub src: gpui::SharedString,
    pub alt: gpui::SharedString,
    pub w: f32,
    pub h: f32,
}

/// 表格单元格的编辑排版（shape 期算了对齐位移与排好版的单行，命中/光标列↔x 换算用）。
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
pub(crate) struct TableRowCellsLayout {
    pub src_line: usize,
    pub cells: Vec<TableCellLayout>,
}

/// 表格块整块的单元格排版（`ShapedRow.table_edit`）。
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
    Mermaid { source: String },
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
}

impl BlockLayout {
    /// 由整块高度 + 起始源码行建（O(n) 建 Fenwick）。
    pub(crate) fn new(heights: Vec<f32>, block_src_lines: Vec<u32>) -> Self {
        let mut fen = vec![0f64; heights.len() + 1];
        for (i, &h) in heights.iter().enumerate() {
            let mut j = i + 1;
            let d = h as f64;
            while j < fen.len() {
                fen[j] += d;
                j += j & (!j + 1);
            }
        }
        Self { heights, fen, block_src_lines }
    }
    /// 整表重建（同代内容变更/首次打开用；O(n)）。
    pub(crate) fn rebuild(&mut self, heights: Vec<f32>, block_src_lines: Vec<u32>) {
        *self = BlockLayout::new(heights, block_src_lines);
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
    pub(crate) fn n(&self) -> usize {
        self.heights.len()
    }
    pub(crate) fn src_lines(&self) -> &[u32] {
        &self.block_src_lines
    }
}


pub(crate) struct ShapedRow {
    pub src_line: usize,
    pub src_lines: Range<usize>,
    /// 该块的可视行（`shape_text` 结果；一个块 = 一个或多个 `WrappedLine`）。
    /// 带外块为空（未 shape，paint 跳过）。
    pub wrapped: Vec<gpui::WrappedLine>,
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
    /// 每行的增强装饰/内嵌数据（列表 gutter、链接、行内数学/图片、块级卡片）。
    pub extra: RowExtra,
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
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped,
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
        return ShapedRow {
            src_line: r.src_line,
            src_lines: r.lines.clone(),
            wrapped: Vec::new(),
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
            extra: RowExtra {
                card: Some(BlockCard::Mermaid { source }),
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
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped,
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
        wrapped: Vec::new(),
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
            aligns: aligns2,
        }),
        image: None,
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
        wrapped: Vec::new(),
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
        IrKind::Code => shape_code_row(r, window, theme),
        IrKind::Table => shape_table_row(r, window, theme.font_size, theme, base_font),
        IrKind::Image => shape_image_row(r, editor, avail),
        _ => shape_text_row_decorated(r, editor, window, width, theme, base_font),
    }
}

/// 数学块卡片的显示行：源码按换行 + LaTeX `\\`（行断）切成多行，逐段做 Unicode
/// 排版。`math_card_height` 与 `paint_card` 必须用同一函数 → 高与绘制逐行对应。
fn math_display_lines(src: &str) -> Vec<String> {
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
fn math_card_height(editor: &WyEditor, reg: &MathRegion) -> Pixels {
    if let Some(super::MathSlot::Ready(_, _, h)) =
        editor.math_slots().borrow().get(reg.source.as_str())
    {
        return px(20. + h.max(16.));
    }
    let n = math_display_lines(&reg.source).len();
    px(20. + n as f32 * 22.)
}

/// 装配一张数学块卡片行（A1）：整块（含围栏）折叠成一张面板，占据 `height`。
fn build_math_card_row(reg: &MathRegion, src_line: usize, height: Pixels, top: Pixels) -> ShapedRow {
    ShapedRow {
        src_line,
        src_lines: reg.range.start..reg.range.end,
        wrapped: Vec::new(),
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
                wrapped: Vec::new(),
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
                extra: RowExtra::default(),
            }],
            base: std::rc::Rc::new(std::cell::RefCell::new(BlockLayout::new(
                vec![f32::from(line_h)],
                vec![0],
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
    // 数组重算前缀和（旧实现在内容代变化时重扫全篇 ~400ms，是「一编辑就卡」之一）。
    let epoch = super::height_epoch(editor.struct_ver(), igen, mgen, width_bits);
    let (rebuilt, base) = editor.ensure_block_layout(n, epoch);
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

    // 结构扫描（derived 缓存 → O(1) Rc 复用）。
    let content = editor.content();
    let caret_line = editor.row_col(editor.caret()).0;
    // 数学块（`$$…$$`）：首源码行 → 张卡片；其余行折叠成 0 高（caret 在块内则整块保留源码）。
    let math_regions = &editor.derived().math_regions;
    let mut math_start: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut math_body: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, reg) in math_regions.iter().enumerate() {
        if caret_line >= reg.range.start && caret_line < reg.range.end {
            continue; // reveal-on-caret：光标在块内 → 不折叠，显示源码
        }
        math_start.insert(reg.range.start, idx);
        math_body.extend(reg.range.start + 1..reg.range.end);
    }
    // 可折叠 callout（`> [!NOTE]-`）：body 行折叠成 0 高。
    let alert_fold = &editor.derived().alert_fold_regions;
    let mut fold_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (rnge, folded) in alert_fold {
        if !*folded || (caret_line >= rnge.start + 1 && caret_line < rnge.end) {
            continue;
        }
        fold_lines.extend(rnge.start + 1..rnge.end);
    }
    // 标题折叠（视图本地状态）。
    {
        let folded = editor.folded_headings.borrow();
        if !folded.is_empty() {
            for rnge in inline_style::heading_fold_regions(content, &folded) {
                if caret_line >= rnge.start + 1 && caret_line < rnge.end {
                    continue;
                }
                fold_lines.extend(rnge.start + 1..rnge.end);
            }
        }
    }

    if rebuilt {
        // Pass 1（仅每内容/宽度代一次）：全块**基础高**（与折叠/卡片态无关——折叠 body
        // 与数学源行都用普通段高估算，动态覆盖走 deltas）+ 起始源码行，一次物化布局。
        let mut heights = Vec::with_capacity(n);
        let mut src_lines = Vec::with_capacity(n);
        for i in 0..n {
            let meta = doc.block_meta(i).unwrap_or_else(|| BlockMeta {
                src_line: 0,
                byte: 0,
                kind: RowKind::Blank,
                raw_len: 0,
                aux: 1,
            });
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
            heights.push(h);
            src_lines.push(meta.src_line);
        }
        base.borrow_mut().rebuild(heights, src_lines);
    }
    // 动态高度覆盖（折叠 body→0 高、数学卡片→卡高），每档按 caret/折叠态重算（通常很小）。
    let mut dyn_map: std::collections::HashMap<usize, f32> = std::collections::HashMap::new();
    {
        let base_br = base.borrow();
        let src_lines = base_br.src_lines();
        let block_ix_of = |srcline: usize| -> usize {
            src_lines
                .partition_point(|&s| (s as usize) <= srcline)
                .saturating_sub(1)
                .min(src_lines.len() - 1)
        };
        for &srcline in fold_lines.iter() {
            let ix = block_ix_of(srcline);
            dyn_map.insert(ix, -base_br.height(ix));
        }
        for &srcline in math_body.iter() {
            let ix = block_ix_of(srcline);
            dyn_map.insert(ix, -base_br.height(ix));
        }
        for (&srcline, &ridx) in math_start.iter() {
            let ix = block_ix_of(srcline);
            let card_h = f32::from(math_card_height(editor, &math_regions[ridx]));
            // 折叠优先：同一行若被折叠（0 高）则不装卡片。
            dyn_map.entry(ix).or_insert(card_h - base_br.height(ix));
        }
    }
    let mut deltas_vec: Vec<(usize, f32)> = dyn_map.into_iter().collect();
    deltas_vec.sort_unstable_by_key(|&(ix, _)| ix);
    let deltas: std::rc::Rc<[(usize, f32)]> = deltas_vec.into_boxed_slice().into();

    // 带内块区间 [i0, i1)：用「有效前缀」（基础 + 动态增量，单调）二分。
    let eff_top = |i: usize| -> f32 {
        let b = base.borrow();
        let k = deltas.partition_point(|&(ix, _)| ix < i);
        b.top(i) + deltas[..k].iter().map(|&(_, v)| v).sum::<f32>()
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

    // Pass 2：带内非折叠/非数学块 shape，真实高 `set` 回基表（带外沿用收敛真值）。
    let mut shaped: Vec<(usize, ShapedRow)> = Vec::new();
    {
        let mut i = i0;
        while i < i1 {
            let srcline = base.borrow().src_lines()[i] as usize;
            let blocked = fold_lines.contains(&srcline)
                || math_body.contains(&srcline)
                || math_start.contains_key(&srcline);
            if blocked {
                i += 1;
                continue;
            }
            let lo = i;
            while i < i1 {
                let s = base.borrow().src_lines()[i] as usize;
                if fold_lines.contains(&s) || math_body.contains(&s) || math_start.contains_key(&s) {
                    break;
                }
                i += 1;
            }
            let hi = i;
            let docrows = doc.rows_in_block_window(lo, hi.saturating_sub(1));
            for (j, r) in docrows.into_iter().enumerate() {
                let k = lo + j;
                let srow = shape_docrow(r, editor, window, width, avail, theme, &base_font);
                base.borrow_mut().set(k, f32::from(srow.height));
                shaped.push((k, srow));
            }
        }
    }

    // Pass 3：组装带内行（有效 top = 基础前缀 + 动态增量前缀，`set` 后已是最终高度）。
    let mut rows: Vec<ShapedRow> = Vec::new();
    let mut shaped_iter = shaped.into_iter().peekable();
    let total = {
        let b = base.borrow();
        b.total()
    } + deltas.iter().map(|&(_, v)| v).sum::<f32>();
    // 各路行按 top 从 base+deltas 现算；total 已含全部。
    for i in i0..i1 {
        let srcline = base.borrow().src_lines()[i] as usize;
        let b = base.borrow();
        let h = b.height(i) + deltas.iter().find(|(ix, _)| *ix == i).map(|&(_, v)| v).unwrap_or(0.);
        let top = {
            let k = deltas.partition_point(|&(ix, _)| ix < i);
            b.top(i) + deltas[..k].iter().map(|&(_, v)| v).sum::<f32>()
        };
        drop(b);
        if h <= 0. {
            continue; // 折叠 body / 数学 body：0 高不物化
        }
        let folded_here = fold_lines.contains(&srcline) || math_body.contains(&srcline);
        if !folded_here
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
            let width = match available.width {
                gpui::AvailableSpace::Definite(w) => w,
                _ => known.width.unwrap_or(px(800.)),
            };
            let width = if f32::from(width).is_finite() && f32::from(width) > 1. {
                width
            } else {
                px(800.)
            };
            // 帧缓存：内容/宽度/可视带/图片代数都没变 → 直接复用总高，不再整篇重建
            // （大文档的每帧“卡死”就是这里：每次 notify 都重排全 16 万行）。
            let rgen = e.content_gen();
            let igen = e.images_gen();
            let mgen = e.math_gen();
            let band = e.shape_band().unwrap_or((0., 6500.));
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
                None => {
                    let frame = build_frame(e, window, width, band);
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
            let frame = build_frame(&*self.editor.read(cx), window, width, band);
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
        // 数学排版同步（幂等）：扫描新公式发起离屏排版；主题色变化清缓存。
        self.editor
            .update(cx, |e, ecx| e.sync_math(ecx));
        let origin = bounds.origin;
        // 背景层：先给所有可见引述行画灰底带 + 左指示条（`py_1` 上下各越 4px，相邻带连片）。
        // ⚠️ 必须**先画全部背景再画全部文字**——若逐行「带 + 文字」连画，后一行的带会盖住
        // 前一行文字底部（引述第一行被第二行背景盖掉的 bug）。
        // callout 区域（marker + body 行）的左条统一用警报色，普通引述用灰条。
        // `alert_region_kinds` 是内容纯函数 → 走 derived 缓存（滚动/每帧不重扫全篇）。
        let derived = {
            let editor = self.editor.read(cx);
            editor.derived()
        };
        let alert_kinds = &derived.alert_region_kinds;
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
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;


