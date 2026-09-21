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
use crate::markdown::md_syntax::{self, AlertKind, LinkHit};
use crate::markdown::markdown_syntax::{self, MathAlign, MathRegion};
use crate::markdown::parser::{BlockMeta, RowKind, RunKind, TableAlign};
use crate::syntax::{github_theme_cached, parse_code_lines, resolve_line_styles_with};

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
pub(crate) struct Frame {
    pub padding: Pixels,
    pub line_h: Pixels,
    pub rows: Vec<ShapedRow>,
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
    accent: gpui::Hsla,
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

/// `RowKind` → 稳定小整数（高度缓存键组成用，Heading 带级数）。
fn kind_tag(k: RowKind) -> u8 {
    match k {
        RowKind::Paragraph => 0,
        RowKind::Heading(n) => 1 + n.min(6),
        RowKind::Code => 8,
        RowKind::Quote => 9,
        RowKind::Bullet => 10,
        RowKind::Rule => 11,
        RowKind::Table => 12,
        RowKind::Image => 13,
        RowKind::Blank => 14,
    }
}

/// 文本块的高度缓存键：`(内容代数, 图片/数学加载代数, 源行, 类型, shaping 宽)`。
fn height_cache_key(
    rgen: u64,
    igen: u64,
    mgen: u64,
    src_line: u32,
    kind: RowKind,
    width_bits: u32,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgen.hash(&mut h);
    igen.hash(&mut h);
    mgen.hash(&mut h);
    src_line.hash(&mut h);
    kind_tag(kind).hash(&mut h);
    width_bits.hash(&mut h);
    h.finish()
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
        RowKind::Table => table_block_height(theme, meta.aux.max(1) as usize),
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
    let spans = markdown_syntax::inline_math_spans(rtext);
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
        let latex = markdown_syntax::inline_math_latex(rtext, &span);
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
        let (w, n, paint_ready) = match ready {
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
        markdown_syntax::splice_inline_math(rtext, &runs, &map, &formulas, &gap);
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
        && let Some(p) = markdown_syntax::blockquote_prefix(&raw)
    {
        // 对「剥掉 `>` 后的 body」再去首空白匹配（`r.text` 也已是 trim 后的主体），
        // 这样 `>  [!NOTE]` 这类多空格变体也能精确剥掉标记。
        let body = &raw[p..];
        let trimmed = body.trim_start();
        if let Some((akind, consumed, fold)) = markdown_syntax::alert_prefix(trimmed) {
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
    let mut flush = |segs: &mut Vec<MathSeg>, buf: &mut String| {
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

/// 把 mermaid `graph` **画成一张真图**：圆角节点框（判断/圆形用对应形状）+ 正交连线
/// （带箭头与边标），不再只是文本行。
fn paint_mermaid_diagram(
    window: &mut Window,
    theme: &WyTheme,
    source: &str,
    area_x: f32,
    area_y: f32,
    area_w: f32,
    cx: &mut App,
) {
    let (placed, edges) = mm_layout(window, theme, source, area_x, area_y, area_w);
    if placed.is_empty() {
        // 无可解析节点 → 弱化提示。
        let hint: SharedString = "空图 / 渲染器未接入".into();
        let mut reduced = theme.muted;
        reduced.a = 0.55;
        let hrun = [TextRun { len: hint.len(), font: theme.mono.clone(), color: reduced, ..Default::default() }];
        let hline = window.text_system().shape_line(hint, px(12.), &hrun, None);
        let hw = f32::from(hline.width());
        let _ = hline.paint(
            point(px(area_x + (area_w - hw) / 2.), px(area_y + 40.)),
            px(16.),
            TextAlign::Left,
            None,
            window,
            cx,
        );
        return;
    }
    let label_h = px(18.);
    let edge_color = { let mut c = theme.muted; c.a = 0.85; c };
    // 先画边（线在框底下）。
    for (ia, ib, elab) in &edges {
        let (pa, pb) = (&placed[*ia], &placed[*ib]);
        let ax = pa.x + pa.w / 2.;
        let ay = pa.y + pa.h;
        let bx = pb.x + pb.w / 2.;
        let by = pb.y;
        let mid_y = ay + (by - ay) / 2.;
        let mut pth = gpui::PathBuilder::stroke(px(1.6));
        pth.move_to(point(px(ax), px(ay)));
        if (bx - ax).abs() < 1. {
            pth.line_to(point(px(bx), px(by - 4.)));
        } else {
            pth.line_to(point(px(ax), px(mid_y)));
            pth.line_to(point(px(bx), px(mid_y)));
            pth.line_to(point(px(bx), px(by - 4.)));
        }
        if let Ok(path) = pth.build() {
            window.paint_path(path, edge_color);
        }
        // 箭头。
        let mut ah = gpui::PathBuilder::stroke(px(1.6));
        ah.move_to(point(px(bx - 4.), px(by - 5.)));
        ah.line_to(point(px(bx), px(by - 1.)));
        ah.line_to(point(px(bx + 4.), px(by - 5.)));
        if let Ok(path) = ah.build() {
            window.paint_path(path, edge_color);
        }
        // 边标（画在折线水平段中点右侧）。
        if let Some(l) = elab {
            let lx = (ax + bx) / 2. + 7.;
            let ly = mid_y - f32::from(label_h) - 3.;
            let run = [TextRun { len: l.len(), font: theme.mono.clone(), color: theme.muted, ..Default::default() }];
            let ln = window.text_system().shape_line(l.clone().into(), px(12.), &run, None);
            let _ = ln.paint(point(px(lx), px(ly)), px(14.), TextAlign::Left, None, window, cx);
        }
    }
    // 再画节点框 + 标签（“图”本体）。
    let box_bg = theme.code_bg;
    let box_border = { let mut c = theme.muted; c.a = 0.6; c };
    for p in &placed {
        let r = match p.shape {
            MmShape::Circle => p.h / 2.,
            MmShape::Round => 10.,
            MmShape::Diamond => 4.,
            MmShape::Rect => 6.,
        };
        let b = Bounds::new(point(px(p.x), px(p.y)), size(px(p.w), px(p.h)));
        window.paint_quad(gpui::PaintQuad {
            bounds: b,
            corner_radii: Corners::all(px(r)),
            background: box_bg.into(),
            border_widths: Edges::all(px(1.2)),
            border_color: box_border,
            border_style: gpui::BorderStyle::Solid,
        });
        let run = [TextRun { len: p.label.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
        let ln = window.text_system().shape_line(p.label.clone().into(), theme.font_size, &run, None);
        let tw = f32::from(ln.width());
        let _ = ln.paint(
            point(px(p.x + (p.w - tw) / 2.), px(p.y + (p.h - f32::from(label_h)) / 2.)),
            label_h,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// 让「所有可点击链接」（`[[wiki]]` / `#tag` / 裸 URL，以及 parser 已产出的
/// `[text](url)`）都套上链接色 + 下划线（与预览一致）。在数学 splice 之前合并进
/// `r.runs`（显示字节空间，pre-splice）。
fn style_links_in_runs(r: &mut DocRow, raw: &str) {
    if r.byte_map.is_empty() || md_syntax::links(raw).is_empty() {
        return;
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (src, _) in md_syntax::links(raw) {
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
    for (src, target) in md_syntax::links(raw) {
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
///   [`markdown_syntax::inline_image_spans`]——否则 tail 从 alt 开始、找不到 `![`，
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
        let Some((full, srcr)) = markdown_syntax::inline_image_spans(tail).into_iter().next() else {
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
    let height = table_block_height(theme, rows.len());
    let mut aligns2 = aligns.clone();
    aligns2.resize(ncols, TableAlign::Left);
    let mut rows2: Vec<Vec<gpui::SharedString>> = rows
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
    let (w, h) = match editor.image_size(src.as_ref()) {
        Some(Some((iw, ih))) if iw > 0. && ih > 0. => {
            let w = max_w;
            (w, (w * ih / iw.max(1.)).max(1.))
        }
        _ => (max_w, crate::config::markdown::IMAGE_ROW_DEFAULT_H),
    };
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped: Vec::new(),
        sub_heights: vec![px(h)],
        byte_map: std::rc::Rc::new(Vec::new()),
        editable: false,
        multi_offsets: Vec::new(),
        code_ranges: Vec::new(),
        lh: px(h),
        x_off: px(0.),
        height: px(h),
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: Some(ShapedImage { src, alt, w, h }),
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

    // 结构扫描（O(n) 行扫描，随内容代变化；build_frame 每代顶多跑一次）。
    let content = editor.content();
    let caret_line = editor.row_col(editor.caret()).0;
    // 数学块（`$$…$$`）：首源码行 → 张卡片；其余行折叠成 0 高（caret 在块内则整块保留源码）。
    let math_regions = markdown_syntax::math_regions(content);
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
    // reveal-on-caret 仅当光标落在 **body 行**（start+1..end）时生效——光标在 marker
    // 行上正是点击折叠后的常态，若也算「区域内有光标」则刚点的折叠会被强制展开
    // （「点折叠却折不起来 + 多 callout 互相串」的根因）。
    let mut fold_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (rnge, folded) in markdown_syntax::alert_fold_regions(content) {
        if !folded || (caret_line >= rnge.start + 1 && caret_line < rnge.end) {
            continue;
        }
        fold_lines.extend(rnge.start + 1..rnge.end);
    }
    // 标题折叠（视图本地状态）：folded 集合里的标题，其 section body 行折叠成 0 高
    // （标题行本身保留，折叠箭头画在那里）。reveal-on-caret 仅当光标**严格落在 body
    // 行**（start+1..end）生效——光标在标题行上正是点击折叠后的常态，不算 reveal
    // （与 callout 同一教训：否则点了折不起来）。
    {
        let folded = editor.folded_headings.borrow();
        if !folded.is_empty() {
            for rnge in markdown_syntax::heading_fold_regions(content, &folded) {
                if caret_line >= rnge.start + 1 && caret_line < rnge.end {
                    continue;
                }
                fold_lines.extend(rnge.start + 1..rnge.end);
            }
        }
    }
    let collapsed = |srcline: usize| fold_lines.contains(&srcline) || math_body.contains(&srcline);

    // Pass 1：决定每块高度（文本块：缓存真值优先，无则用真实字数估算）+ 是否入带。
    #[derive(Clone, Copy)]
    struct Plan {
        shaped: bool,
        h: Pixels,
        cache_key: Option<u64>,
    }
    let mut plan: Vec<Plan> = Vec::with_capacity(n);
    let mut y: f32 = 0.;
    for i in 0..n {
        let meta = doc.block_meta(i).unwrap_or_else(|| BlockMeta {
            src_line: 0,
            byte: 0,
            kind: RowKind::Blank,
            raw_len: 0,
            aux: 1,
        });
        let srcline = meta.src_line as usize;
        // A2: ```mermaid 围栏（Code 块）整块折叠成「图卡片」。
        let is_mermaid = meta.kind == RowKind::Code
            && doc.line_str(srcline).trim_start().starts_with("```mermaid");
        let math_card = math_start.get(&srcline).copied();
        let special = collapsed(srcline) || math_card.is_some() || is_mermaid;
        let cache_key = if special {
            None
        } else {
            match meta.kind {
                // 折行文本块（段/标题/引述/列表）缓存**真实可视行数**（带外沿用真值防
                // 滚动跳变）。代码/表格/图片**不入缓存**：它们不折行、`estimate_height`
                // 与 shape 高同式（代码 = (行数+2)×code_line）——入缓存反而会用
                // `line_h × 行数`（正文行高×n）重建出错误高度（比真实矮 2 行代码高）。
                RowKind::Paragraph | RowKind::Heading(_) | RowKind::Quote | RowKind::Bullet => {
                    Some(height_cache_key(rgen, igen, mgen, meta.src_line, meta.kind, width_bits))
                }
                _ => None,
            }
        };
        let h = if collapsed(srcline) {
            px(0.)
        } else if let Some(idx) = math_card {
            math_card_height(editor, &math_regions[idx])
        } else if is_mermaid {
            // 由围栏内容行重建源码算卡片高（与 shape_code_row 一致）。
            let mut src = String::new();
            for k in 0..meta.aux {
                if k > 0 {
                    src.push('\n');
                }
                src.push_str(&doc.line_str(srcline + 1 + k as usize));
            }
            mermaid_card_height(&src)
        } else {
            match cache_key.and_then(|k| editor.cached_height_lines(k)) {
                Some(lines) => line_h * lines as f32,
                None => {
                    let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
                    estimate_height(&meta, chars, ascii, theme, avail, line_h)
                }
            }
        };
        let bottom = y + f32::from(h);
        let mut shaped = bottom > band_lo && y < band_hi;
        // 数学卡片块在 pass 3 直接装配（不入带内 shape）；body/callout 折叠块不 shape。
        if math_card.is_some() || collapsed(srcline) {
            shaped = false;
        }
        plan.push(Plan {
            shaped,
            h,
            cache_key,
        });
        y = bottom;
    }

    // Pass 2：相邻带内块合并成窗口调用物化 + shape，并把真实可视行数写回缓存。
    let mut shaped: Vec<Option<ShapedRow>> =
        std::iter::repeat_with(|| None).take(n).collect();
    let mut i = 0;
    while i < n {
        if !plan[i].shaped {
            i += 1;
            continue;
        }
        let lo = i;
        while i < n && plan[i].shaped {
            i += 1;
        }
        let hi = i;
        let docrows = doc.rows_in_block_window(lo, hi.saturating_sub(1));
        for (j, r) in docrows.into_iter().enumerate() {
            let srow = shape_docrow(r, editor, window, width, avail, theme, &base_font);
            // 缓存该块真实可视行数（带外再遇此块时沿用真值 → 不跳）。折行代码/长段落
            // 会与"估算不折行"偏差，缓存消除滚动跳变；表格（无折行、估算同式）不入缓存。
            let visual = srow
                .wrapped
                .iter()
                .map(|wl| wl.wrap_boundaries().len() + 1)
                .sum::<usize>()
                .max(1) as u32;
            if let Some(k) = plan[lo + j].cache_key {
                editor.cache_height_lines(k, visual);
            }
            shaped[lo + j] = Some(srow);
        }
    }

    // Pass 3：按块序组装（真实 top 累加），带外块填占位。
    let empty_bm: std::rc::Rc<Vec<usize>> = std::rc::Rc::new(Vec::new());
    let mut rows: Vec<ShapedRow> = Vec::with_capacity(n);
    let mut total = px(0.);
    for i in 0..n {
        let meta = doc.block_meta(i).unwrap_or_else(|| BlockMeta {
            src_line: 0,
            byte: 0,
            kind: RowKind::Blank,
            raw_len: 0,
            aux: 1,
        });
        let srcline = meta.src_line as usize;
        // 数学块卡片（A1）：在首源码行装配整张卡片，比整个块更高。
        // 被折叠的行（标题/引述折叠，collapsed）不装配卡片——走下方普通 0 高占位，
        // 否则 0 高的卡片行仍会经 `extra.card` 触发 paint_card 画出 "公式" 标签与
        // 公式图（多个公式重叠堆在标题上方）。
        if !collapsed(srcline) && let Some(&idx) = math_start.get(&srcline) {
            let reg = &math_regions[idx];
            let h = plan[i].h;
            rows.push(build_math_card_row(reg, srcline, h, total));
            total += h;
            continue;
        }
        let sr = match shaped[i].take() {
            Some(mut sr) => {
                sr.top = total;
                total += sr.height;
                rows.push(sr);
                continue;
            }
            None => {
                let span = match meta.kind {
                    RowKind::Code | RowKind::Table => meta.aux.max(1) as usize,
                    _ => 1,
                };
                let src_line = meta.src_line as usize;
                let h = plan[i].h;
                ShapedRow {
                    src_line,
                    src_lines: src_line..src_line + span,
                    wrapped: Vec::new(),
                    sub_heights: vec![h],
                    byte_map: empty_bm.clone(),
                    editable: false,
                    multi_offsets: Vec::new(),
                    code_ranges: Vec::new(),
                    lh: line_h,
                    x_off: px(0.),
                    height: h,
                    top: total,
                    kind: crate::markdown::doc::row_kind(meta.kind),
                    table: None,
                    table_edit: None,
                    image: None,
                    extra: RowExtra::default(),
                }
            }
        };
        total += sr.height;
        rows.push(sr);
    }
    Frame { padding, line_h, rows, total, band, width, rgen, igen, mgen }
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

/// 源 caret → 光标条几何。非文本行停在块首（只读）；代码块内停在对应内容行。
/// 行高用 `r.lh`（标题/代码等类型各自行高），否则光标会错位。
fn caret_geometry(
    editor: &WyEditor,
    f: &Frame,
) -> (bool, Pixels, Pixels) {
    let (cl, col) = editor.row_col(editor.caret());
    let line_start = editor.line_start(cl);
    let line_end = editor.line_end(cl);
    for r in &f.rows {
        if !r.src_lines.contains(&cl) {
            continue;
        }
        if !r.editable {
            return (false, px(12.), r.top);
        }
        if let (Some(te), Some(t)) = (&r.table_edit, &r.table) {
            // 表格：caret 落在某个可见行（表头/数据行）→ 定位到单元格内容处。
            let Some(row_ix) = te.rows.iter().position(|rr| rr.src_line == cl) else {
                return (false, px(12.), r.top);
            };
            let row_l = &te.rows[row_ix];
            let target = col.min(line_end.saturating_sub(line_start));
            let Some((cell_ix, dcol)) = table_cell_at_src(&te.rows, cl, target) else {
                return (false, px(12.), r.top);
            };
            let cell = &row_l.cells[cell_ix];
            let cell_x0 = table_cell_x0(&t.widths, cell_ix);
            let col_x = f32::from(cell.line.x_for_index(dcol.min(cell.line.len())));
            // 垂直居中于该行（与 paint 文字纵向居中一致；光标高用正文行高近似）。
            let caret_h = f32::from(editor.theme.para_line());
            let row_top = r.top + px(table_row_top(t, row_ix));
            let ctop = row_top + px(((f32::from(t.row_h) - caret_h) / 2.).max(0.));
            return (true, px(12.) + px(cell_x0 + cell.text_x + col_x), ctop);
        }
        if !r.multi_offsets.is_empty() {
            // 代码块：caret 落在围栏内容行（`src_line+1 ..= src_line+n`）才画光标，
            // 经块级 byte_map 映射到该代码行的渲染列与垂直偏移；围栏行上不画。
            let n = r.multi_offsets.len();
            if cl >= r.src_line + 1 && cl <= r.src_line + n {
                let block_start = editor.line_start(r.src_line);
                let target = (line_start + col).saturating_sub(block_start);
                let rend = source_to_display(&r.byte_map, target);
                let g = group_from_rend(r, rend);
                let dcol = rend
                    .saturating_sub(r.multi_offsets[g])
                    .min(group_len(r, g));
                let p = r
                    .wrapped
                    .get(g)
                    .and_then(|wl| wl.position_for_index(dcol, r.lh))
                    .unwrap_or_else(|| point(px(0.), px(0.)));
                let y = r.top + px(code_group_top(r, g) + f32::from(p.y));
                let scroll = code_hscroll_clamped(editor, f, r);
                return (true, px(12.) + r.x_off + p.x - px(scroll), y);
            }
            return (false, px(12.), r.top);
        }
        // 单源码行文本块：源列 → 渲染列（`source_to_display`，左界）——与
        // `source_offset_at` 的反查互为逆映射，光标才落在点击处。
        let target = col.min(line_end.saturating_sub(line_start));
        let dcol = source_to_display(&r.byte_map, target);
        // 折行时 p.y 是子行偏移，叠加到块首 + 撑高下移才算准缓存所在的可视行。
        let p = render_point_of(&r.wrapped, r.lh, dcol);
        // `px(12.)` = 内容边距；引述/代码等额外缩进走 `r.x_off`（与 paint 同 x）。
        return (true, px(12.) + r.x_off + p.x, r.top + px(text_row_shift(r)) + p.y);
    }
    let fallback_lh = f.rows.first().map_or(px(20.), |r| r.lh);
    let y = f.rows.last().map_or(fallback_lh, |r| r.top + r.height);
    (true, px(12.), y)
}

/// 点命中（**元素局部坐标**）→ 源字节偏移（帧内 byte_map 反查）。
/// 命中用的行/列全是元素局部（paint 以 `origin = bounds.origin` 为起点绘制）；
/// 传入前把窗口坐标减掉元素原点即可（见 [`source_offset_at`]）。
pub(crate) fn source_offset_at_local(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    let rel_y = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if rel_y >= top && rel_y < bottom {
            if !r.editable {
                return editor.line_end(r.src_line).max(editor.line_start(r.src_line));
            }
            if let (Some(te), Some(t)) = (&r.table_edit, &r.table) {
                // 表格：y 定可见行（表头/数据行），x 定列，再按对齐位移映射列内渲染列
                // → 单元格 byte_map 反查源字节（相对该行行首）。
                let Some(row_ix) = table_row_at(t, rel_y - top) else {
                    return editor.caret();
                };
                let Some(row_l) = te.rows.get(row_ix) else {
                    return editor.caret();
                };
                let rel_x = (f32::from(p.x) - f32::from(frame.padding)).max(0.);
                let cell_ix = table_cell_index(&t.widths, rel_x)
                    .unwrap_or(t.widths.len().saturating_sub(1));
                let Some(cell) = row_l.cells.get(cell_ix) else {
                    return editor.caret();
                };
                let cell_x0 = table_cell_x0(&t.widths, cell_ix);
                let in_cell = (rel_x - cell_x0 - cell.text_x).max(0.);
                let col = cell
                    .line
                    .closest_index_for_x(px(in_cell))
                    .min(cell.map.len().saturating_sub(1))
                    .min(cell.line.len());
                let local = *cell.map.get(col).unwrap_or(&0);
                let ls = editor.line_start(row_l.src_line);
                let le = editor.line_end(row_l.src_line).max(ls);
                return (ls + local).min(le);
            }
            if !r.multi_offsets.is_empty() {
                // 代码块：y 定源码行组（`code_group_at`，首空白带归第 0 组），x 定列；
                // 经块级 byte_map 反查 块首源偏移 → 绝对源字节（钳到末内容行行尾）。
                // 横向滚动时内容坐标 = 视图坐标 + scroll（paint 平移的逆映射）。
                let g = code_group_at(r, rel_y - top);
                let Some(wl) = r.wrapped.get(g) else {
                    return editor.caret();
                };
                let scroll = code_hscroll_clamped(editor, frame, r);
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off)
                    + scroll)
                    .max(0.);
                let rel = point(px(rel_x), px(0.));
                // `closest_index_for_position` 的 `Err` 是越界钳制后的有效索引
                // （点过行尾/行首）；把它当 0 会让点行尾时光标跳回行首。
                let dcol = match wl.closest_index_for_position(rel, r.lh) {
                    Ok(i) | Err(i) => i,
                };
                let rend = r.multi_offsets[g] + dcol.min(group_len(r, g));
                let local = display_to_source(&r.byte_map, rend);
                let ls = editor.line_start(r.src_line);
                let le = editor.line_end(r.src_line + r.multi_offsets.len()).max(ls);
                return (ls + local).min(le);
            }
            if let Some(w0) = r.wrapped.first() {
                // x 先扣内容边距与行级缩进（引述等），y 用点击在块内的**子行偏移**
                // （`rel_y - top - 撑高下移`）：`closest_index_for_position` 据此选折行的
                // 第几可视行——写成 0 或漏掉下移量会把撑高行上的点击映射错行/错列。
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off))
                    .max(0.);
                let sub_y = (rel_y - top - text_row_shift(r)).max(0.);
                let rel = point(px(rel_x), px(sub_y));
                // `closest_index_for_position` 的 `Err` 是越界钳制后的有效索引
                // （点过行尾/行首）；把它当 0 会让点行尾时光标跳回行首。
                let dcol = match w0.closest_index_for_position(rel, r.lh) {
                    Ok(i) | Err(i) => i,
                };
                let local = display_to_source(&r.byte_map, dcol);
                let ls = editor.line_start(r.src_line);
                let le = editor.line_end(r.src_line);
                return (ls + local).min(le);
            }
        }
    }
    editor.caret()
}

/// 点命中（**窗口坐标**，`MouseDownEvent.position`）→ 源字节偏移。
pub(crate) fn source_offset_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    // window 坐标先换回元素局部（否则元素被 `live-body` 内边距/滚动/窗口位置偏移后，
    // 点击会映射到错误的行/列），再走统一的局部命中。
    let origin = editor.last_bounds().map(|b| b.origin).unwrap_or_else(|| point(px(0.), px(0.)));
    let local = point(p.x - origin.x, p.y - origin.y);
    source_offset_at_local(editor, frame, local)
}

/// 元素局部坐标（把窗口坐标减掉元素原点）。
fn to_local(editor: &WyEditor, p: Point<Pixels>) -> Point<Pixels> {
    let origin = editor.last_bounds().map(|b| b.origin).unwrap_or_else(|| point(px(0.), px(0.)));
    point(p.x - origin.x, p.y - origin.y)
}

/// 命中任务框（C1）：返回 (源行, 是否已勾选)。击中「任务行 gutter（☐/☑ 字形区）」
/// 才返回，否则 None（与 `paint_row_gutter` 的 GUTTER_MARK_X 几何一致）。
pub(crate) fn checkbox_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        let Some(RowGutter::Check { checked }) = &r.extra.gutter else {
            continue;
        };
        // 字形区 ≈ [padding, padding + 24)（与任务行正文缩进 x_off 内）。
        let hi = (f32::from(frame.padding) + 24.).max(f32::from(r.x_off));
        if px >= f32::from(frame.padding) && px <= hi {
            return Some((r.src_line, *checked));
        }
    }
    None
}

/// 命中警报折叠箭头（C3）：击中警报行 gutter（[padding, padding+ALERT_GUTTER_W] × 行高）
/// 返回 (源行, 当前是否折叠)。
pub(crate) fn alert_chevron_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        let Some(RowGutter::Alert { kind: _, fold }) = &r.extra.gutter else {
            continue;
        };
        let Some(folded) = fold else {
            continue;
        };
        if px >= f32::from(frame.padding) && px <= f32::from(frame.padding) + ALERT_GUTTER_W {
            return Some((r.src_line, *folded));
        }
    }
    None
}

/// 命中标题折叠箭头：击中标题行的 chevron 区（文本右缘 + 固定宽的命中盒）。
/// 返回 (源行, 当前是否折叠)。几何与 `paint_heading_chevron` 共用。
pub(crate) fn heading_chevron_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(editor, p);
    let py = f32::from(p.y);
    let px = f32::from(p.x);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || !matches!(r.kind, IrKind::Heading(_)) {
            continue;
        }
        let Some((cx0, cw)) = heading_chevron_box(frame, r) else {
            // 带外（未 shape）标题行没有 wrapped：非可视区，跳过。
            continue;
        };
        if px >= cx0 && px <= cx0 + cw {
            let key = editor.content()[editor.line_start(r.src_line)..editor.line_end(r.src_line)]
                .trim()
                .to_string();
            let folded = editor.folded_headings.borrow().contains(&key);
            return Some((r.src_line, folded));
        }
    }
    None
}

/// 标题折叠箭头几何：画在**左缘 gutter**（[padding, padding+HEADING_GUTTER_W]，正文
/// 已右移 `x_off = HEADING_GUTTER_W`）；返回元素局部坐标 (x0, w)。paint 与 hit-test 共用。
fn heading_chevron_box(f: &Frame, _row: &ShapedRow) -> Option<(f32, f32)> {
    Some((f32::from(f.padding), HEADING_GUTTER_W))
}

/// 命中行内 `$…$` 公式的 spacer：击中其显示区 → 返回该公式的**绝对源字节区间**
/// （含定界符 `$…$`，`InlineMathText.source`）。双击命中时把光标塞进区间起点 + 1
/// reveal 出可编辑的原始 LaTeX（B2 双击编辑）。
pub(crate) fn inline_math_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<Range<usize>> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.extra.inline_maths.is_empty() {
            continue;
        }
        let Some(w0) = r.wrapped.first() else {
            continue;
        };
        let ox = f32::from(frame.padding) + f32::from(r.x_off);
        for im in &r.extra.inline_maths {
            let (Some(p0), Some(p1)) = (
                w0.position_for_index(im.display_off, r.lh),
                w0.position_for_index(im.display_off + im.len, r.lh),
            ) else {
                continue;
            };
            // spacer 恒在首可视行（`visual_block_line_for_index` = 0），只按首行 y 命中。
            let y0 = top + text_row_shift(r) + f32::from(p0.y);
            let x0 = ox + f32::from(p0.x);
            let x1 = ox + f32::from(p1.x);
            if py >= y0
                && py < y0 + f32::from(r.lh)
                && px >= x0
                && px <= x1.max(x0 + 1.)
            {
                if !im.source.is_empty() {
                    return Some(im.source.clone());
                }
            }
        }
    }
    None
}

/// 命中数学块卡片（A1，`$$…$$` 折叠成的 `BlockCard::Math` 行）：击中卡片范围 →
/// 返回该块的**首源码行行首绝对字节**（把光标放进去即 reveal 源码进入编辑）。
/// 单击命中不应移动光标（否则 reveal 回源码字符串——用户看到的「单击变回字符串」）；
/// 双击编辑由调用方在 `click_count >= 2` 时把光标放到这里。
pub(crate) fn math_card_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        if !matches!(&r.extra.card, Some(BlockCard::Math { .. })) {
            continue;
        }
        // 卡片铺满内容宽：左 = padding，右 = padding + 内容宽。
        let x0 = f32::from(frame.padding);
        let x1 = f32::from(frame.width) - f32::from(frame.padding);
        if px >= x0 && px <= x1 {
            return Some(editor.line_start(r.src_line));
        }
    }
    None
}

/// 命中的可点击链接目标（C5）：窗口坐标 → 行内链接显示区间集合，命中的返回 `LinkHit`。
pub(crate) fn link_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<LinkHit> {
    let p = to_local(editor, p);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.extra.links.is_empty() || r.wrapped.is_empty() {
            continue;
        }
        let Some(w0) = r.wrapped.first() else {
            continue;
        };
        let ox = f32::from(frame.padding) + f32::from(r.x_off);
        for lk in &r.extra.links {
            let (Some(p0), Some(p1)) = (
                w0.position_for_index(lk.disp.start, r.lh),
                w0.position_for_index(lk.disp.end, r.lh),
            ) else {
                continue;
            };
            let p0y = top + text_row_shift(r) + f32::from(p0.y);
            let p1y = top + text_row_shift(r) + f32::from(p1.y);
            let contains = if f32::from(p0.y) == f32::from(p1.y) {
                let x0 = ox + f32::from(p0.x);
                let x1 = ox + f32::from(p1.x);
                py >= p0y && py < p0y + f32::from(r.lh) && f32::from(p.x) >= x0 && f32::from(p.x) <= x1
            } else {
                // 折行：头段或尾段
                let head_ok = py >= p0y && py < p0y + f32::from(r.lh)
                    && f32::from(p.x) >= ox + f32::from(p0.x)
                    && f32::from(p.x) <= ox + f32::from(w0.width());
                let tail_ok = py >= p1y && py < p1y + f32::from(r.lh)
                    && f32::from(p.x) >= ox
                    && f32::from(p.x) <= ox + f32::from(p1.x);
                head_ok || tail_ok
            };
            if contains {
                return Some(lk.target.clone());
            }
        }
    }
    None
}

/// 表格的一行单元格文本（表头加粗/浅色，按列对齐）。
fn draw_table_row(
    window: &mut Window,
    cx: &mut App,
    cells: &[gpui::SharedString],
    widths: &[f32],
    aligns: &[TableAlign],
    x0: f32,
    row_top_y: f32,
    row_h: f32,
    is_header: bool,
    theme: &WyTheme,
) {
    let font_size = theme.font_size;
    let mut acc = 0f32;
    for (ci, c) in cells.iter().enumerate() {
        let col_w = widths.get(ci).copied().unwrap_or(0.);
        let mut font = theme.font.clone();
        let text_col = if is_header {
            let mut c = theme.text;
            c.a = 0.9;
            c
        } else {
            theme.text
        };
        if is_header {
            font.weight = gpui::FontWeight::BOLD;
        }
        let run = gpui::TextRun { len: c.len(), font, color: text_col, ..Default::default() };
        let line = window.text_system().shape_line(c.clone(), font_size, &[run], None);
        {
            let w = f32::from(line.width());
            let x = match aligns.get(ci) {
                Some(TableAlign::Center) => acc + ((col_w - w) / 2.).max(0.),
                Some(TableAlign::Right) => acc + (col_w - w - 10.).max(0.),
                _ => acc + 10.,
            };
            // 单元格纵向居中（预览 `items_center`）：文字块中点对齐行中点。
            let text_lh = f32::from(theme.para_line());
            let ty = row_top_y + ((row_h - text_lh) / 2.).max(0.);
            let _ = line.paint(
                point(px(x0 + x), px(ty)),
                px(text_lh),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        acc += col_w;
    }
}

/// 画表格块网格：外框 + 表头背景 + 行/列分隔线 + 单元格文本（配色对齐预览）。
fn paint_table_row(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    t: &ShapedTable,
    window: &mut Window,
    cx: &mut App,
) {
    let x0 = f32::from(origin.x) + f32::from(f.padding);
    let top0 = f32::from(origin.y) + f32::from(row.top);
    let total_w = t.total_w.max(1.);
    let row_h_f = f32::from(t.row_h);
    let ncols = t.header_texts.len().max(1);
    let n_rows = t.rows_texts.len();
    let block_h = f32::from(table_block_height(theme, n_rows));
    let border: gpui::Hsla = theme.divider;
    // 外框（先画：内容随后的矩形都缩进 1px，不盖住外框线）。
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(point(px(x0), px(top0)), size(px(total_w), px(block_h))),
        corner_radii: gpui::Corners::all(px(6.)),
        background: gpui::Hsla { h: 0., s: 0., l: 0., a: 0. }.into(),
        border_widths: gpui::Edges::all(px(1.)),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
    let inner_lo = top0 + 1.;
    let inner_hi = top0 + block_h - 1.;
    // 表头背景（圆角顶）。
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(
            point(px(x0 + 1.), px(inner_lo)),
            size(px((total_w - 2.).max(0.)), px(row_h_f)),
        ),
        corner_radii: gpui::Corners {
            top_left: px(5.),
            top_right: px(5.),
            bottom_left: px(0.),
            bottom_right: px(0.),
        },
        background: theme.table_head_bg.into(),
        border_widths: gpui::Edges::default(),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
    let mut cy = inner_lo;
    draw_table_row(
        window,
        cx,
        &t.header_texts,
        &t.widths,
        &t.aligns,
        x0,
        cy,
        row_h_f,
        true,
        theme,
    );
    cy += row_h_f;
    for cells in &t.rows_texts {
        // 行间分隔线（表头下 + 数据行之间）。
        window.paint_quad(gpui::PaintQuad {
            bounds: Bounds::new(
                point(px(x0 + 1.), px(cy)),
                size(px((total_w - 2.).max(0.)), px(1.)),
            ),
            corner_radii: gpui::Corners::default(),
            background: border.into(),
            border_widths: gpui::Edges::default(),
            border_color: border,
            border_style: gpui::BorderStyle::Solid,
        });
        cy += 1.;
        draw_table_row(
            window,
            cx,
            cells,
            &t.widths,
            &t.aligns,
            x0,
            cy,
            row_h_f,
            false,
            theme,
        );
        cy += row_h_f;
    }
    // 列分隔竖线（内容区内）。
    let mut acc = 0f32;
    for ci in 0..ncols {
        acc += t.widths.get(ci).copied().unwrap_or(0.);
        if ci + 1 < ncols {
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(
                    point(px(x0 + acc), px(inner_lo)),
                    size(px(1.), px((inner_hi - inner_lo).max(0.))),
                ),
                corner_radii: gpui::Corners::default(),
                background: border.into(),
                border_widths: gpui::Edges::default(),
                border_color: border,
                border_style: gpui::BorderStyle::Solid,
            });
        }
    }
}

/// 代码块横向滚动条几何（内容超宽才返回）。
/// 返回**元素局部** x（paint 加 `origin.x`，命中直接用）：
/// `(bar_x, bar_w, thumb_x, thumb_w, max_scroll)`。`viewport_text` = 面板内文本可视宽
/// （面板宽 − 左右内衬 `x_off`×2）；`max_scroll` = 最宽行 − viewport。
pub(crate) fn code_hscroll_geo(
    editor: &WyEditor,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    let panel_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let inset = f32::from(row.x_off).max(1.);
    let viewport = (panel_w - inset * 2.).max(1.);
    let max_w = row
        .wrapped
        .iter()
        .map(|wl| f32::from(wl.width()))
        .fold(0.0f32, f32::max);
    if max_w <= viewport + 1. {
        return None;
    }
    let max_scroll = max_w - viewport;
    let scroll = editor.code_h_scroll_of(row.src_line).min(max_scroll).max(0.);
    let frac = if max_scroll > 0. { scroll / max_scroll } else { 0. };
    let bar_x = f32::from(f.padding) + inset;
    let bar_w = viewport.max(1.);
    // 与 view 的 gpui-component 同式：thumb = container/scroll_area × container，最小 48。
    let thumb_w =
        (bar_w * viewport / max_w)
            .max(crate::config::layout::CODE_HSCROLL_THUMB_MIN)
            .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some((bar_x, bar_w, thumb_x, thumb_w, max_scroll))
}

/// 代码块当前生效的横向滚动偏移（px）：内容不超宽 → 0（忽略陈旧滚动值，宽窗口下
/// 长行不再偏移）；超宽 → 按 `max_scroll` clamp。paint / 命中 / 光标 / 选区共用。
pub(crate) fn code_hscroll_clamped(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> f32 {
    match code_hscroll_geo(editor, f, row) {
        Some((_, _, _, _, max_scroll)) => editor
            .code_h_scroll_of(row.src_line)
            .min(max_scroll)
            .max(0.),
        None => 0.,
    }
}

/// 代码块横向滚动条命中（元素局部→窗口坐标）：命中**底部滚动条带**
/// （轨道 ~`CODE_HSCROLL_BAND` 高）→ 返回
/// `(块首源行, 是否点在滑块上, 指针相对滑块中心的水平偏移)`。
/// 命中轨道（不在滑块上）→ 调用方让滑块跳到点击处；点在滑块上 → 进入拖动。
/// 与 `paint_code_row` 的滚动条几何同源（view 的 gpui-component 语义：thumb 上拖、
/// 轨道上跳转）。
pub(crate) fn code_hscrollbar_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.multi_offsets.is_empty() || r.wrapped.is_empty() {
            continue;
        }
        let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = code_hscroll_geo(editor, frame, r) else {
            continue;
        };
        // 底部滚动条带：轨道顶（面板底 − `CODE_HSCROLL_BAND`）之间。
        let band_top = f32::from(r.top) + f32::from(r.height) - crate::config::layout::CODE_HSCROLL_BAND;
        if py < band_top {
            continue;
        }
        if px >= bar_x && px <= bar_x + bar_w {
            let on_thumb = px >= thumb_x && px <= thumb_x + thumb_w;
            let grab = px - (thumb_x + thumb_w / 2.0);
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}

/// 代码块：圆角面板（对齐预览：外层 `px_3` → 面板贴**内容区左缘**（=content 边距，
/// 与正文对齐），而非再缩进）；行文本再 `pl_3`（面板内 12px 内衬，走 `row.x_off`，
/// 命中/光标/选区同 x）。首/末各留一个 code_line 空白带。
/// 长代码行（nowrap 超宽）**裁剪在面板内** + 底部横向滚动条（视图本地，可拖动）。
fn paint_code_row(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    let pad = f32::from(f.padding);
    let content_w = f32::from(f.width) - pad * 2.;
    let panel_x = origin.x + f.padding;
    let panel_w = content_w.max(1.);
    let panel_rect = Bounds::new(
        point(panel_x, origin.y + row.top),
        size(px(panel_w), row.height),
    );
    window.paint_quad(gpui::PaintQuad {
        bounds: panel_rect,
        corner_radii: gpui::Corners::all(px(8.)), // 与预览代码块 `rounded_t_lg`/`rounded_b_lg`（lg=8）对齐
        background: theme.code_bg.into(),
        border_widths: gpui::Edges::default(),
        border_color: gpui::Hsla::default(),
        border_style: gpui::BorderStyle::Solid,
    });
    let text_x = panel_x + row.x_off;
    // 内容超宽？超宽时的真实 `max_scroll` 与滚动条几何（同一函数单一源）；
    // 不超宽 → 无滚动（并忽略陈旧滚动值，宽窗口下长行不再偏移）。
    let hgeo = code_hscroll_geo(editor.read(cx), f, row);
    let scroll = code_hscroll_clamped(editor.read(cx), f, row);
    // 文本裁剪在面板内（横向滚动时把超出的代码藏进面板，不再溢到页面背景上）。
    // ⚠️ 用 `with_content_mask`（给 glyph/quad 打 content mask），不是 `paint_layer`
    // —— 后者只改 z 序不裁剪（GPUI 场景层的 `StartLayer` 不设 content mask）。
    // 顶空白带之后逐行画文本（x = 面板左 + 内衬 x_off − 滚动偏移）。
    window.with_content_mask(Some(gpui::ContentMask { bounds: panel_rect }), |window| {
        let mut sub_top = row.top + theme.code_line();
        for wl in &row.wrapped {
            let o = point(text_x - px(scroll), origin.y + sub_top);
            let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
            sub_top += row.lh * (wl.wrap_boundaries().len() + 1) as f32;
        }
    });
    // 内容超宽 → 底部横向滚动条（view 的 gpui-component 同款：轨道 16 高 +
    // 内衬 4 + 圆角滑块 8 高/最小 48 宽；点击轨道跳转、滑块可拖）。覆在末空白带上。
    if let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = hgeo {
        let track_h = crate::config::layout::CODE_HSCROLL_BAND;
        let track_top =
            f32::from(row.top) + f32::from(row.height) - track_h;
        let inset = 4.;
        let th = crate::config::layout::CODE_HSCROLL_THUMB_H;
        let ty = track_top + inset;
        // 轨道：极淡分隔色（hover/拖动前也可见，像 view 的 Always 模式）。
        let mut track = theme.divider;
        track.a = 0.35;
        window.paint_quad(gpui::PaintQuad {
            bounds: Bounds::new(
                point(origin.x + px(bar_x), origin.y + px(track_top)),
                size(px(bar_w), px(track_h)),
            ),
            corner_radii: gpui::Corners::default(),
            background: track.into(),
            border_widths: gpui::Edges::default(),
            border_color: gpui::Hsla::default(),
            border_style: gpui::BorderStyle::Solid,
        });
        // 滑块：圆角半透明 muted（比旧 2px 细条更易抓取）。
        let mut thumb = theme.muted;
        thumb.a = 0.6;
        window.paint_quad(gpui::PaintQuad {
            bounds: Bounds::new(
                point(origin.x + px(thumb_x), origin.y + px(ty)),
                size(px(thumb_w), px(th)),
            ),
            corner_radii: gpui::Corners::all(px(th / 2.)),
            background: thumb.into(),
            border_widths: gpui::Edges::default(),
            border_color: gpui::Hsla::default(),
            border_style: gpui::BorderStyle::Solid,
        });
    }
}

/// 图片行占位/失败兜底：浅底细边框盒 + 居中 alt 文字（加载完成前的占位与加载失败体验）。
fn paint_image_fallback(
    theme: &WyTheme,
    f: &Frame,
    bounds: Bounds<Pixels>,
    img: &ShapedImage,
    window: &mut Window,
    cx: &mut App,
) {
    let border: gpui::Hsla = { let mut c = theme.divider; c.a = 0.6; c };
    let mut text = theme.muted;
    text.a = 0.8;
    window.paint_quad(gpui::PaintQuad {
        bounds,
        corner_radii: gpui::Corners::all(px(4.)),
        background: theme.code_bg.into(),
        border_widths: gpui::Edges::all(px(1.)),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
    if img.alt.is_empty() {
        return;
    }
    let run = gpui::TextRun {
        len: img.alt.len(),
        font: theme.font.clone(),
        color: text,
        ..Default::default()
    };
    let line = window
        .text_system()
        .shape_line(img.alt.clone(), theme.font_size, &[run], None);
    {
        let w = f32::from(line.width());
        let h = f32::from(bounds.size.height);
        let lh = f32::from(f.line_h);
        let x = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - w) / 2.;
        let y = f32::from(bounds.origin.y) + ((h - lh) / 2.).max(0.);
        let _ = line.paint(point(px(x), px(y)), f.line_h, gpui::TextAlign::Left, None, window, cx);
    }
}

/// 画图片行：经 `AnyImageCache` 异步加载已就绪的 `RenderImage` 后画出；
/// 加载中/失败画 alt 占位卡片。加载成功（或已预读固有尺寸）即把真实宽高比尺寸
/// 写回 `WyEditor`（`images_gen` 自增 → 下帧重建帧以真实行高布局）。
fn paint_image_row(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    img: &ShapedImage,
    window: &mut Window,
    cx: &mut App,
) {
    let content_w = f32::from(f.width) - f32::from(f.padding) * 2.;
    let x0 = f32::from(origin.x) + f32::from(f.padding) + ((content_w - img.w) / 2.).max(0.);
    let top0 = f32::from(origin.y) + f32::from(row.top);
    let bounds = Bounds::new(point(px(x0), px(top0)), size(px(img.w), px(img.h)));
    // 已知加载失败：直接画 alt 兜底，不再反复发起加载。
    let state = {
        let e = editor.read(cx);
        e.image_size(img.src.as_ref())
    };
    if matches!(state, Some(None)) {
        paint_image_fallback(theme, f, bounds, img, window, cx);
        return;
    }
    let resource = {
        let e = editor.read(cx);
        e.image_load_resource(img.src.as_ref())
    };
    let cache = {
        let e = editor.read(cx);
        e.image_cache()
    };
    let anycache: gpui::AnyImageCache = cache.clone().into();
    match anycache.load(&resource, window, cx) {
        Some(Ok(data)) => {
            let img_size = data.size(0);
            let iw = img_size.width.0 as f32;
            let ih = img_size.height.0 as f32;
            if iw > 0. && ih > 0. {
                let w = img.w.max(1.);
                let h = (w * ih / iw).max(1.);
                if state.is_none() {
                    editor.update(cx, |e, _| e.set_image_size(img.src.clone(), Some((w, h))));
                }
                let b = Bounds::new(point(px(x0), px(top0)), size(px(w), px(h)));
                let _ = window.paint_image(b, b, gpui::Corners::all(px(4.)), data, 0, false);
            } else {
                paint_image_fallback(theme, f, bounds, img, window, cx);
            }
        }
        Some(Err(_)) => {
            editor.update(cx, |e, _| e.set_image_size(img.src.clone(), None));
            paint_image_fallback(theme, f, bounds, img, window, cx);
        }
        None => paint_image_fallback(theme, f, bounds, img, window, cx),
    }
}

/// 行内代码（反引号）圆角 pill 底：把 `row.code_ranges`（已扩展空白）区间按折行切成
/// 多段矩形并 paint，与正文 `wrapped.first()` 的同一 layout 对齐（背景/字形/折行一致）。
/// 与预览 [`TextTrack::paint_code_ranges`] 同式：先画底、再委托 WrappedLine 画文字；
/// 颜色单一源 `WyTheme.code_inline_bg`（← StyleParams）。
fn paint_inline_code_pills(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
) {
    if row.code_ranges.is_empty() {
        return;
    }
    let Some(wl) = row.wrapped.first() else {
        return;
    };
    let line_h = row.lh.as_f32();
    let shift = text_row_shift(row);
    let text_origin_x = origin.x.as_f32() + f.padding.as_f32() + f32::from(row.x_off);
    let text_origin_y = origin.y.as_f32() + row.top.as_f32() + shift;
    for range in &row.code_ranges {
        let Some(p0) = wl.position_for_index(range.start, row.lh) else {
            continue;
        };
        let Some(p1) = wl.position_for_index(range.end, row.lh) else {
            continue;
        };
        let y0 = (p0.y.as_f32() / line_h).floor() as usize;
        let y1 = (p1.y.as_f32() / line_h).floor() as usize;
        if y0 == y1 {
            // 单行内：一段矩形
            let b = Bounds::new(
                point(
                    px(text_origin_x + p0.x.as_f32()),
                    px(text_origin_y + p0.y.as_f32()),
                ),
                size(px((p1.x.as_f32() - p0.x.as_f32()).max(0.0)), px(line_h)),
            );
            window.paint_quad(
                gpui::fill(b, theme.code_inline_bg)
                    .corner_radii(gpui::Corners::all(px(5.))),
            );
        } else {
            // 跨行：首段画到该行行尾、中间整行、末段从行首到终点
            for real_ri in y0..=y1 {
                let (rx0, rx1) = if real_ri == y0 {
                    (p0.x.as_f32(), wl.width().as_f32())
                } else if real_ri == y1 {
                    (0.0, p1.x.as_f32())
                } else {
                    (0.0, wl.width().as_f32())
                };
                let b = Bounds::new(
                    point(px(text_origin_x + rx0), px(text_origin_y + real_ri as f32 * line_h)),
                    size(px((rx1 - rx0).max(0.0)), px(line_h)),
                );
                window.paint_quad(
                    gpui::fill(b, theme.code_inline_bg)
                        .corner_radii(gpui::Corners::all(px(5.))),
                );
            }
        }
    }
}

/// 文本块（段落/标题/引述等可编辑行）逐可视行绘制（x 含行级缩进 `row.x_off`）。
/// 行高按 `sub_heights`（行内公式会把所在行撑高，见 `shape_text_row`）；撑高时该
/// BlockLine 的文本组在带内垂直居中（公式在其行的上下溢出留白差异由此吸收）。
fn paint_text_row(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    // 行内代码圆角 pill 底先画（同一 layout 的像素几何），再画文字。
    paint_inline_code_pills(origin, theme, f, row, window);
    let mut band_top = row.top;
    for (k, wl) in row.wrapped.iter().enumerate() {
        let band_h = row.sub_heights.get(k).copied().unwrap_or(row.lh);
        let orig = row.lh * (wl.wrap_boundaries().len() + 1) as f32;
        // 撑高后的下移：文本组在 [band_top, band_top+band_h] 里垂直居中。
        let shift = ((f32::from(band_h) - f32::from(orig)) / 2.).max(0.);
        let o = point(
            origin.x + f.padding + row.x_off,
            origin.y + band_top + px(shift),
        );
        let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
        band_top += band_h;
    }
}

/// 引述块的灰底带 + 左侧指示条（背景层；**不得**在这里画文字）。对齐预览：
/// `px_3` 内缩、`py_1` 内衬、3px 边条。每行带从行上/下各向外 4px（`py_1`），
/// 相邻引述带重叠 → 整段引述连片灰底。
///
/// `alert_kinds`（src 行号 → 警报种类）让 **callout 的 body 行**也用警报色画左条
/// （否则只有 marker 行有彩条、body 行是灰条——「展开的 callout 标题彩、内容灰」）。
///
/// ⚠️ 画带与画文字必须**分两趟**：先给所有引述行画背景（本函数），再画全部文字
/// （`paint_text_row`）。若逐行「带 + 文字」连画，后一行（带向上越 4px）会盖住
/// 前一行文字底部——引述第一行文字被第二行背景盖掉的 bug 根源。
fn paint_quote_band(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    alert_kinds: &std::collections::HashMap<usize, AlertKind>,
    window: &mut Window,
) {
    let x0 = origin.x + f.padding;
    let y0 = origin.y + row.top - px(4.);
    // 外层等价 `px_3`（内容边距已含），带再内缩 12 → 带宽 = 内容宽 − 24。
    let band_w = (f32::from(f.width) - f32::from(f.padding) * 2. - 24.).max(1.);
    let band_h = row.height + px(8.);
    let border: gpui::Hsla = { let mut c = theme.divider; c.a = 0.2; c };
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(point(x0, y0), size(px(band_w), band_h)),
        corner_radii: gpui::Corners::default(),
        background: theme.quote_bg.into(),
        border_widths: gpui::Edges::default(),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
    // 左侧指示条：callout region 内的行（marker + body）用警报色；普通引述用灰条。
    let bar: gpui::Hsla = match alert_kinds.get(&row.src_line) {
        Some(kind) => alert_color(theme, *kind),
        None => gpui::rgb(crate::config::markdown::QUOTE_BAR_COLOR).into(),
    };
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(
            point(x0, y0),
            size(px(crate::config::markdown::QUOTE_BAR_WIDTH_PX), band_h),
        ),
        corner_radii: gpui::Corners::default(),
        background: bar.into(),
        border_widths: gpui::Edges::default(),
        border_color: bar,
        border_style: gpui::BorderStyle::Solid,
    });
}

/// 每个警报（callout）对应的条色（明/暗主题微调，区分五类警报）。
fn alert_color(theme: &WyTheme, kind: AlertKind) -> Hsla {
    let mut c: Hsla = match kind {
        AlertKind::Note => gpui::rgb(0x3b82f6).into(),
        AlertKind::Tip => gpui::rgb(0x10b981).into(),
        AlertKind::Important => gpui::rgb(0x8b5cf6).into(),
        AlertKind::Warning => gpui::rgb(0xf59e0b).into(),
        AlertKind::Caution => gpui::rgb(0xef4444).into(),
    };
    if theme.is_dark {
        c.l = (c.l + 0.12).clamp(0., 1.);
        c.s = (c.s + 0.05).min(1.);
    } else {
        c.l = (c.l - 0.06).max(0.05);
    }
    c
}

/// 行首装饰：列表项目符号 / 有序号 / 任务框（画在内容区左缘 gutter，文本已缩进到
/// `row.x_off` 之后）。与 hit-test（`checkbox_hit`）共用几何常量。
fn paint_row_gutter(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(g) = &row.extra.gutter else {
        return;
    };
    let x0 = f32::from(origin.x + f.padding);
    let center_y = f32::from(origin.y) + f32::from(row.top) + f32::from(row.height) / 2.;
    // 与预览（view/row.rs）一致：项目符号 / 有序号 / 任务框都是正文色字形（☐/☑/•/N.）。
    // 警报（Alert）不画任何字形——它的 chrome（彩条 + 粗体标签 + 折叠箭头）由
    // `paint_alert_chrome` 单独画；这里若给 `•` 会在彩条与文字之间漏个黑点。
    let glyph: SharedString = match g {
        RowGutter::Check { checked } => {
            if *checked {
                "☑".into()
            } else {
                "☐".into()
            }
        }
        RowGutter::Ordered(n) => n.clone().into(),
        RowGutter::Bullet => "•".into(),
        RowGutter::Alert { .. } => return,
    };
    let rrun = [TextRun {
        len: glyph.len(),
        font: theme.font.clone(),
        color: theme.text,
        ..Default::default()
    }];
    let rline = window.text_system().shape_line(glyph, theme.font_size, &rrun, None);
    let _ = rline.paint(
        point(px(x0 + GUTTER_MARK_X), px(center_y - f32::from(row.lh) / 2.)),
        row.lh,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// 警报（callout）行 chrome：左侧彩条（覆盖引述灰条）+ 粗体标签 + 可选折叠箭头。
/// 标签/箭头画在 gutter（正文已缩进 84px），geometry 与 `alert_chevron_hit` 对齐。
fn paint_alert_chrome(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(RowGutter::Alert { kind, fold }) = &row.extra.gutter else {
        return;
    };
    let color = alert_color(theme, *kind);
    let x0 = origin.x + f.padding;
    let top0 = origin.y + row.top;
    // 左彩条（覆盖引述灰条；高对齐引述带）。
    window.paint_quad({
        let b = Bounds::new(point(x0, top0 - px(4.)), size(px(3.), row.height + px(8.)));
        fill(b, color)
    });
    // 粗体标签。
    let label: SharedString = kind.label().into();
    let mut lfont = theme.font.clone();
    lfont.weight = gpui::FontWeight::BOLD;
    let lrun = [TextRun { len: label.len(), font: lfont, color, ..Default::default() }];
    let lline = window.text_system().shape_line(label.clone(), theme.font_size, &lrun, None);
    let _ = lline.paint(point(x0 + px(6.), top0), row.lh, TextAlign::Left, None, window, cx);
    // 折叠箭头（`-` 折叠 / `+` 展开 / 无 fold char 不画）：自绘实心小三角。
    // 不用 `▸`/`▾` 字形——Segoe UI（Windows 系统 UI 字体）缺 U+25B8/25BE，GPUI 单族
    // 无回退时会画成方框 tofu（「图标不对」）。PathBuilder::fill 画三角则字体无关，
    // Windows / Ubuntu（换系统字体也不丢字形）都稳定。命中区仍是整条 gutter，不受影响。
    if let Some(folded) = fold {
        let gx = x0 + px(6.) + lline.width() + px(4.);
        let cy = f32::from(top0) + f32::from(row.lh) * 0.5;
        let w = 9.0_f32;
        let h = 7.0_f32;
        let ax = f32::from(gx) + 1.0;
        let ay = cy - h * 0.5;
        let mut p = gpui::PathBuilder::fill();
        if *folded {
            // ▸ 右向（已折叠，点击展开）
            p.move_to(point(px(ax), px(ay)));
            p.line_to(point(px(ax), px(ay + h)));
            p.line_to(point(px(ax + w), px(cy)));
        } else {
            // ▾ 下向（展开，点击折叠）
            p.move_to(point(px(ax + w * 0.5), px(ay + h)));
            p.line_to(point(px(ax), px(ay)));
            p.line_to(point(px(ax + w), px(ay)));
        }
        p.close();
        if let Ok(path) = p.build() {
            window.paint_path(path, color);
        }
    }
}

/// 标题折叠箭头（`▾` 展开 / `▸` 已折叠）：画在**左缘 gutter**（正文已右移
/// `HEADING_GUTTER_W`，与勾选/项目符号同侧的左缘字形区，muted 半透明）。自绘实心
/// 小三角（同 callout 折叠箭头：PathBuilder 字体无关，避免 Segoe UI 缺 `▸`/`▾`
/// 字形画成 tofu 的坑 S）。geometry 与 `heading_chevron_hit` 共用。
fn paint_heading_chevron(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    if row.wrapped.is_empty() {
        return;
    }
    let key = editor
        .read(cx)
        .content()[editor.read(cx).line_start(row.src_line)..editor.read(cx).line_end(row.src_line)]
        .trim()
        .to_string();
    let folded = editor.read(cx).folded_headings.borrow().contains(&key);
    let gx = origin.x + f.padding + px(GUTTER_MARK_X);
    let cy = f32::from(origin.y) + f32::from(row.top) + f32::from(row.lh) * 0.5;
    let mut col = theme.text;
    col.a *= 0.45;
    let w = 9.0_f32;
    let h = 7.0_f32;
    let ax = f32::from(gx) + 1.0;
    let ay = cy - h * 0.5;
    let mut p = gpui::PathBuilder::fill();
    if folded {
        // ▸ 右向（已折叠，点击展开）。
        p.move_to(point(px(ax), px(ay)));
        p.line_to(point(px(ax), px(ay + h)));
        p.line_to(point(px(ax + w), px(cy)));
    } else {
        // ▾ 下向（展开，点击折叠）。
        p.move_to(point(px(ax + w * 0.5), px(ay + h)));
        p.line_to(point(px(ax), px(ay)));
        p.line_to(point(px(ax + w), px(ay)));
    }
    p.close();
    if let Ok(path) = p.build() {
        window.paint_path(path, col);
    }
}

/// 行内 `$…$` 数学：在 splice 出的 spacer 位置画排版位图（已就绪，参考 zorite
/// `shape_inline_math`）/ 斜体 LaTeX（排版中或失败，B2 原路径）。
fn paint_inline_maths(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    if row.extra.inline_maths.is_empty() || row.wrapped.is_empty() {
        return;
    }
    let w0 = &row.wrapped[0];
    let tx = origin.x + f.padding + row.x_off;
    let ty = origin.y + row.top;
    let fs = w0.unwrapped_layout.font_size;
    let lh = row.lh;
    // 主题色变化清缓存会改公式色，paint 期优先取当前槽（最新着色）；若刚好在
    // 重排版窗口（Loading）则用 shape 期截获的位图兜底；都没有 → Unicode 文本。
    let mut mfont = theme.font.clone();
    mfont.style = gpui::FontStyle::Italic;
    let mcolor = super::math_color(theme);
    for im in &row.extra.inline_maths {
        let Some(p) = w0.position_for_index(im.display_off, row.lh) else { continue };
        let active = match editor
            .read(cx)
            .math_slots()
            .borrow()
            .get(super::inline_math_key(im.latex.trim()).as_ref())
            .cloned()
        {
            Some(super::MathSlot::Ready(img, w, h)) => Some((img, w, h)),
            _ => im.ready.clone(),
        };
        // 撑高行的 band 顶（前面 BlockLine 累计）+ 本带文本下移——与 `paint_text_row`
        // 同源；`p.y` 是 shaped 行内 y（k×lh）。
        let li = im.line.min(row.sub_heights.len().saturating_sub(1));
        let band_top = row.sub_heights[..li].iter().map(|b| f32::from(*b)).sum::<f32>();
        let band_h = row.sub_heights.get(li).copied().unwrap_or(row.lh);
        let orig = lh * (w0.wrap_boundaries().len() + 1) as f32;
        let shift = ((f32::from(band_h) - f32::from(orig)) / 2.).max(0.);
        let line_y = ty + px(band_top + shift) + p.y;
        if let Some((img, w, h)) = active {
            // 位图就绪：按 `inline_math_fit_h`（字号 + 行高上限）缩放画在 spacer 上，
            // 以该行文本中线为锚垂直居中（参考 zorite inline_renderer）。
            let fit = inline_math_fit_h(fs, lh, h);
            let scale = if h > 0.0 {
                fit / h
            } else {
                f32::from(fs) / crate::config::math::MATH_FONT_SIZE
            };
            let pw = (w * scale).max(1.);
            let ph = (h * scale).max(1.);
            let by = line_y + px((f32::from(row.lh) - ph) / 2.);
            let b = Bounds::new(point(tx + p.x, by), size(px(pw), px(ph)));
            let _ = window.paint_image(b, b, gpui::Corners::all(px(2.)), img, 0, false);
            continue;
        }
        // 排版中/失败：斜体 Unicode 文本（定位同上）。
        let run = [TextRun { len: im.latex.len(), font: mfont.clone(), color: mcolor, ..Default::default() }];
        let line = window.text_system().shape_line(im.latex.clone(), fs, &run, None);
        let _ = line.paint(point(tx + p.x, line_y), row.lh, TextAlign::Left, None, window, cx);
    }
}

/// 行内 `![alt](src)`：在 alt 文本（显示区）上方画等比缩小的缩略图（B1）。
/// 加载中/失败保留 alt 文字（已被正文画过）。
fn paint_inline_images(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    if row.extra.inline_images.is_empty() || row.wrapped.is_empty() {
        return;
    }
    let w0 = &row.wrapped[0];
    let tx = origin.x + f.padding + row.x_off;
    let ty = origin.y + row.top;
    for ii in &row.extra.inline_images {
        let (Some(p0), Some(p1)) = (
            w0.position_for_index(ii.display_off, row.lh),
            w0.position_for_index(ii.display_off + ii.len.max(1), row.lh),
        ) else {
            continue;
        };
        let x = tx + p0.x;
        let y = ty + p0.y;
        let show_w = (p1.x - p0.x).max(px(6.));
        let resource = {
            let e = editor.read(cx);
            e.image_load_resource(ii.src.as_ref())
        };
        let cache = {
            let e = editor.read(cx);
            e.image_cache()
        };
        let anycache: gpui::AnyImageCache = cache.into();
        if let Some(Ok(data)) = anycache.load(&resource, window, cx) {
            let sz = data.size(0);
            let (iw, ih) = (sz.width.0 as f32, sz.height.0 as f32);
            if iw > 0. && ih > 0. {
                let bbox = (f32::from(show_w), f32::from(row.lh));
                let sf = (bbox.0 / iw).min(bbox.1 / ih).max(0.01);
                let (w, h) = (iw * sf, ih * sf);
                let x2 = f32::from(x) + (bbox.0 - w) / 2.;
                let y2 = f32::from(y) + (bbox.1 - h) / 2.;
                let b = Bounds::new(point(px(x2), px(y2)), size(px(w), px(h)));
                let _ = window.paint_image(b, b, Corners::all(px(3.)), data, 0, false);
            }
        }
    }
}

/// 块级卡片（数学块 / Mermaid 图）的绘制：圆角面板 + 居中排版 / 占位提示。
fn paint_card(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    card: &BlockCard,
    window: &mut Window,
    cx: &mut App,
) {
    let pad = f32::from(f.padding);
    let content_w = (f32::from(f.width) - pad * 2.).max(1.);
    let x0 = f32::from(origin.x) + pad;
    let top0 = f32::from(origin.y) + f32::from(row.top);
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(point(px(x0), px(top0)), size(px(content_w), row.height)),
        corner_radii: Corners::all(px(8.)),
        background: theme.code_bg.into(),
        border_widths: Edges::default(),
        border_color: Hsla::default(),
        border_style: gpui::BorderStyle::Solid,
    });
    match card {
        BlockCard::Math { align, latex } => {
            // 左上角小标签（“公式”，弱化）。
            let tag: SharedString = "公式".into();
            let mut muted = theme.muted;
            muted.a = 0.7;
            let trun = [TextRun { len: tag.len(), font: theme.font.clone(), color: muted, ..Default::default() }];
            let tline = window.text_system().shape_line(tag, px(11.), &trun, None);
            let _ = tline.paint(point(px(x0 + 12.), px(top0 + 6.)), px(14.), TextAlign::Left, None, window, cx);
            // 排版已就绪 → 画 `RenderImage`（参考 zorite：RaTeX 真排版——分式横线/根号/
            // 极限上下标/求和限都按 LaTeX 排，取代 Unicode 近似）。未就绪/失败 → Unicode。
            let slot = editor
                .read(cx)
                .math_slots()
                .borrow()
                .get(latex.as_str())
                .cloned();
            if let Some(super::MathSlot::Ready(img, w, h)) = slot {
                let iw = w.max(1.);
                let ih = h.max(1.);
                let x = match align {
                    MathAlign::Left => x0 + 16.,
                    MathAlign::Center => x0 + ((content_w - iw) / 2.).max(8.),
                    MathAlign::Right => x0 + (content_w - iw - 16.).max(8.),
                };
                let avail_h = (f32::from(row.height) - 20.).max(1.);
                let y = top0 + 20. + ((avail_h - ih) / 2.).max(0.);
                let b = Bounds::new(point(px(x), px(y)), size(px(iw), px(ih)));
                let _ = window.paint_image(b, b, gpui::Corners::all(px(2.)), img, 0, false);
                return;
            }
            // LaTeX 逐行排版（斜体、正文色）。行 = 源码换行 + `\\` 行断。
            // 上下标拆成独立段（Sup/Sub）：小一号字体 + 上/下偏移——`∞` 等没有 Unicode
            // 上/下标码位的字符也能正确显示为极限（`∫₋∞∞` → `∫` + 小 ∞ 下/上标）。
            let mut mfont = theme.font.clone();
            mfont.style = gpui::FontStyle::Italic;
            let lh = px(22.);
            let fs = (f32::from(theme.font_size) * 1.15).max(14.);
            let ss_fs = (fs * 0.78).max(10.);
            let sup_dy = (fs * 0.34).max(2.);
            let sub_dy = (fs * 0.18).max(2.);
            let mcolor = super::math_color(theme);
            let lines = math_display_lines(latex);
            let n = lines.len().max(1);
            let block_h = f32::from(row.height);
            let mut y = top0 + 10. + ((block_h - 20. - n as f32 * 22.) / 2.).max(0.);
            for shown in &lines {
                let segs = math_to_segments(shown);
                // 整行宽度 = 各段 shaping 宽之和（Normal 用 fs，Sub/Sup 用 ss_fs）。
                let mut plan: Vec<(SharedString, f32, f32, MathPos)> = Vec::with_capacity(segs.len());
                let mut total = 0.0_f32;
                for s in &segs {
                    let sfs = if s.pos == MathPos::Normal { fs } else { ss_fs };
                    let run = [TextRun { len: s.text.len(), font: mfont.clone(), color: mcolor, ..Default::default() }];
                    let sh = window.text_system().shape_line(s.text.clone().into(), px(sfs), &run, None);
                    let w = f32::from(sh.width());
                    total += w;
                    plan.push((s.text.clone().into(), sfs, w, s.pos));
                }
                let start_x = match align {
                    MathAlign::Left => x0 + 16.,
                    MathAlign::Center => x0 + ((content_w - total) / 2.).max(8.),
                    MathAlign::Right => x0 + (content_w - total - 16.).max(8.),
                };
                let mut cursor = start_x;
                for (text, sfs, w, pos) in plan {
                    let run = [TextRun { len: text.len(), font: mfont.clone(), color: mcolor, ..Default::default() }];
                    let sh = window.text_system().shape_line(text, px(sfs), &run, None);
                    let paint_y = match pos {
                        MathPos::Normal => y,
                        MathPos::Sup => y - sup_dy,
                        MathPos::Sub => y + sub_dy,
                    };
                    let _ = sh.paint(point(px(cursor), px(paint_y)), lh, TextAlign::Left, None, window, cx);
                    cursor += w;
                }
                y += 22.;
            }
        }
        BlockCard::Mermaid { source } => {
            // 左上角小标签（弱化）。
            let tag: SharedString = "mermaid".into();
            let mut muted = theme.muted;
            muted.a = 0.7;
            let trun = [TextRun { len: tag.len(), font: theme.font.clone(), color: muted, ..Default::default() }];
            let tline = window.text_system().shape_line(tag, px(11.), &trun, None);
            let _ = tline.paint(point(px(x0 + 12.), px(top0 + 6.)), px(14.), TextAlign::Left, None, window, cx);
            // 矢量图渲染：圆角节点框 + 正交连线/箭头/边标。
            paint_mermaid_diagram(window, theme, source, x0, top0, content_w, cx);
        }
    }
}

/// 当前选区（源字节归一化区间）；无选区返回 `None`。
fn editor_selection(editor: &gpui::Entity<WyEditor>, cx: &mut App) -> Option<Range<usize>> {
    editor.read(cx).selection()
}

/// 选区高亮：对可编辑且与源选区相交的行，按字形几何逐可视行画半透明 accent 矩形。
/// 与光标共用 `x_off`/`lh`，因此引述缩进里的选中文字也高亮对齐。
fn paint_selection(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    range: &Range<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    // 选区背景统一用预览同款（`#4d6bfe` 蓝，暗 38% / 亮 26%），与右预览一致。
    let hl = crate::markdown::view::selection_color(cx);
    let e = editor.read(cx);
    for r in &f.rows {
        if !r.editable || r.wrapped.is_empty() {
            continue;
        }
        if !r.multi_offsets.is_empty() {
            // 代码块：逐源码行求与选区的交集，按字形几何画高亮（每行一条）。
            // 横向滚动时高亮随内容平移（−scroll），并裁剪在面板内（不溢到页面背景）。
            let block_start = e.line_start(r.src_line);
            let g_count = r.multi_offsets.len();
            let scroll = code_hscroll_clamped(editor.read(cx), f, r);
            let panel_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
            let panel_rect = Bounds::new(
                point(origin.x + f.padding, origin.y + r.top),
                size(px(panel_w), r.height),
            );
            window.with_content_mask(Some(gpui::ContentMask { bounds: panel_rect }), |window| {
                for g in 0..g_count {
                    let ls = e.line_start(r.src_line + 1 + g);
                    let le = e.line_end(r.src_line + 1 + g).max(ls);
                    let from = range.start.max(ls).min(le);
                    let to = range.end.max(ls).min(le);
                    if to > from {
                        let rd0 = source_to_display(&r.byte_map, from - block_start);
                        let rd1 = source_to_display(&r.byte_map, to - block_start);
                        let span = group_len(r, g);
                        let mut c0 = rd0.saturating_sub(r.multi_offsets[g]).min(span);
                        let mut c1 = rd1.saturating_sub(r.multi_offsets[g]).min(span);
                        if c0 > c1 {
                            std::mem::swap(&mut c0, &mut c1);
                        }
                        if let Some(wl) = r.wrapped.get(g)
                            && let Some(p0) = wl.position_for_index(c0, r.lh)
                            && let Some(p1) = wl.position_for_index(c1, r.lh)
                        {
                            let w = f32::from(p1.x - p0.x).max(2.);
                            let y = r.top + px(code_group_top(r, g));
                            let h = r.sub_heights.get(g).copied().unwrap_or(r.lh);
                            window.paint_quad(gpui::PaintQuad {
                                bounds: Bounds::new(
                                    point(
                                        origin.x + f.padding + r.x_off + p0.x - px(scroll),
                                        origin.y + y,
                                    ),
                                    size(px(w), h),
                                ),
                                corner_radii: gpui::Corners::default(),
                                background: hl.into(),
                                border_widths: gpui::Edges::default(),
                                border_color: gpui::Hsla::default(),
                                border_style: gpui::BorderStyle::Solid,
                            });
                        }
                    }
                }
            });
            continue;
        }
        let ls = e.line_start(r.src_line);
        let le = e.line_end(r.src_line).max(ls);
        let from = range.start.max(ls).min(le);
        let to = range.end.max(ls).min(le);
        if to <= from {
            continue;
        }
        let d0 = source_to_display(&r.byte_map, from - ls);
        let d1 = source_to_display(&r.byte_map, to - ls);
        let Some(w0) = r.wrapped.first() else {
            continue;
        };
        let len = w0.len().max(1);
        let d0 = d0.min(len);
        let d1 = d0.max(d1.min(len));
        if d1 <= d0 {
            continue;
        }
        // 逐换行边界组成的可视行字节区间：换行点字节 = 边界前最后 glyph 的起点。
        let runs = &w0.unwrapped_layout.runs;
        let mut rows: Vec<(usize, usize)> = Vec::new();
        let mut prev = 0usize;
        for b in w0.wrap_boundaries() {
            let mut byte = 0usize;
            for (ri, run) in runs.iter().enumerate() {
                let last = run.glyphs.len().saturating_sub(1);
                if ri < b.run_ix {
                    byte = run.glyphs.get(last).map(|g| g.index + 1).unwrap_or(byte);
                } else if ri == b.run_ix {
                    byte = run
                        .glyphs
                        .get(b.glyph_ix.min(last))
                        .map(|g| g.index)
                        .unwrap_or(byte);
                    break;
                }
            }
            if byte > prev {
                rows.push((prev, byte));
                prev = byte;
            }
        }
        rows.push((prev, len));
        for (a, b2) in rows {
            let c0 = d0.max(a);
            let c1 = d1.min(b2);
            if c1 <= c0 {
                continue;
            }
            let p0 = match w0.position_for_index(c0, r.lh) {
                Some(p) => p,
                None => continue,
            };
            let p1 = w0.position_for_index(c1, r.lh).unwrap_or(p0);
            let y = r.top + px(text_row_shift(r)) + p1.y.min(p0.y);
            let w = f32::from(p1.x - p0.x).max(2.);
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(
                    point(origin.x + f.padding + r.x_off + p0.x, origin.y + y),
                    size(px(w), r.lh),
                ),
                corner_radii: gpui::Corners::default(),
                background: hl.into(),
                border_widths: gpui::Edges::default(),
                border_color: gpui::Hsla::default(),
                border_style: gpui::BorderStyle::Solid,
            });
        }
    }
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
                    drop(e);
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
        let alert_kinds = {
            let editor = self.editor.read(cx);
            markdown_syntax::alert_region_kinds(editor.content())
        };
        for row in &f.rows {
            if row.kind == IrKind::Quote && !row.wrapped.is_empty() {
                paint_quote_band(origin, &theme, f, row, &alert_kinds, window);
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
                        paint_table_row(origin, &theme, f, row, t, window, cx);
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
mod tests {
    use super::{
        build_frame, caret_geometry, display_to_source, source_offset_at, source_to_display,
        table_block_height, table_cell_x0, table_row_top, WyEditor,
    };
    use gpui::{
        AppContext, Context, Entity, Render, TestAppContext, VisualTestContext, Window, point, px,
    };

    struct WyHost {
        ed: Entity<WyEditor>,
    }
    impl Render for WyHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.ed.clone()
        }
    }

    #[test]
    fn byte_map_round_trip_is_identity_for_plain_text() {
        // 普通段落：渲染 == 源（map[k] = k，末位哨兵 = len）。
        let map: Vec<usize> = (0..=5).collect(); // "abcde" + 哨兵
        for j in 0..=5 {
            let src = display_to_source(&map, j);
            assert_eq!(src, j);
            assert_eq!(
                source_to_display(&map, src),
                j,
                "点渲染列 {j} → 源 {src} → 应回渲染列 {j}"
            );
        }
    }

    #[test]
    fn hidden_markers_map_round_trip() {
        // `**bold**`：渲染文本 "bold"，map = [2,3,4,5,6]（哨兵 6 = 源段尾）。
        let map: Vec<usize> = vec![2, 3, 4, 5, 6];
        for j in 0..4 {
            let src = display_to_source(&map, j);
            assert_eq!(src, 2 + j);
            assert_eq!(
                source_to_display(&map, src),
                j,
                "点渲染列 {j} 与光标回渲染列应一致"
            );
        }
        // 光标在段尾（源 6）→ 渲染列 4（行尾）；点行尾（渲染列 4）→ 源段尾 6。
        assert_eq!(source_to_display(&map, 6), 4);
        assert_eq!(display_to_source(&map, 4), 6);
    }

    #[gpui::test]
    fn inline_code_pill_ranges_expand_over_whitespace(cx: &mut TestAppContext) {
        // 行内代码（反引号）在 shape 期记录渲染字节区间（已扩展空白）供 paint 自绘圆角
        // pill——与预览 `TextTrack::paint_code_ranges` 同一行为源。用贴题原句验证 `we`。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "我们`we`可以将它理解成一个**标识接口**\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Paragraph)
                .expect("应有段落行");
            assert!(
                !para.code_ranges.is_empty(),
                "含行内代码的文本行应记录 pill 区间"
            );
            // 渲染文本 = `我们we可以将它理解成一个标识接口`，`we` 在字节 [6..8)
            // （我们=2×3 字节）；两侧不是空白 → 不扩展。
            assert_eq!(
                para.code_ranges[0],
                6..8,
                "`we` pill 应恰好覆盖渲染字节 6..8"
            );
            // 圆角 pill 底色单一源：与预览 `StyleParams.code_inline_bg` 同值。
            let p = crate::markdown::view::style::StyleParams::from_theme_with_font(
                cx,
                Some(f32::from(theme.font_size)),
            );
            assert_eq!(theme.code_inline_bg, p.code_inline_bg, "pill 底色抄自 StyleParams");
        });
    }

    #[gpui::test]
    fn wrapped_row_click_on_second_visual_line_lands_in_second_half(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 长文本，在 300px 宽度窗口内必然折成 ≥2 个可视行。
        let words = "1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(words));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (_, o2) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            assert_eq!(frame.rows.len(), 1, "单段文档应只有一行（块）");
            let w0 = frame.rows[0]
                .wrapped
                .first()
                .expect("行应有 shaped 结果");
            assert!(
                w0.wrap_boundaries().len() >= 1,
                "长文本应折行：wrap 边界 {} 个",
                w0.wrap_boundaries().len()
            );
            let line_h = f32::from(frame.line_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(frame.rows[0].top);
            // 第一可视行与第二可视行各自的「行内靠左」点击（x 相同、y 分属两个可视行）。
            let p1 = point(px(pad + 5.), px(top + line_h * 0.5));
            let p2 = point(px(pad + 5.), px(top + line_h * 1.5));
            let o1 = source_offset_at(ed.read(cx), &frame, p1);
            let o2 = source_offset_at(ed.read(cx), &frame, p2);
            // 第二可视行的点击必须落在第一可视行文本之后（而非被映射回第一行）。
            assert!(
                o2 > o1,
                "第二可视行点击的源偏移 {o2} 应大于第一可视行行尾 {o1}"
            );
            (o1, o2)
        });
        // 用 `source_offset_at` 反查出的第二可视行源偏移直接落光标，验证
        // `caret_geometry` 把光标竖条画在第二可视行（top = 块首 + 一行高）。
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| e.set_caret(o2));
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let (on, _, caret_top) = caret_geometry(ed.read(cx), &frame);
            let line_h = f32::from(frame.line_h);
            let top = f32::from(frame.rows[0].top);
            let top2 = f32::from(caret_top);
            assert!(on, "折行行应可放置光标");
            assert!(
                (top2 - (top + line_h)).abs() < 0.5,
                "光标竖条应画在第二可视行：块首 top={top}，得到 {top2}"
            );
        });
    }

    #[gpui::test]
    fn large_doc_windowed_shaping_shapes_only_band(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        cx.update(gpui_component::init);
        // 首个超长折行段 + 大量单行段（≈2 万块）：**窄带只 shape 带内块**，
        // 其余用 BlockMeta 估算占位——大文档再也不会整篇每帧 shape。
        let head = "一二三四五六七八九十 ".repeat(300); // 长中文行，必然折行
        let body = "paragraph line of text 0123456789".to_string();
        let mut text = head;
        for _ in 0..20_000 {
            text.push('\n');
            text.push_str(&body);
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 只覆盖顶部 ~80px 的窄带 → 真正 shape 的块应极少。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 80.));
            let n = frame.rows.len();
            assert!(n > 10_000, "应有大量块：{n}");
            let shaped = frame.rows.iter().filter(|r| !r.wrapped.is_empty()).count();
            assert!(
                shaped > 0 && shaped < 20,
                "窄带只应 shape 极少量块，实际 {shaped}/{n}"
            );
            assert!(
                f32::from(frame.total) > 100_000.,
                "总高应由估算铺满（>.1M px），实际 {:.0}",
                f32::from(frame.total)
            );
            assert!(
                frame.rows.iter().any(|r| r.wrapped.is_empty()),
                "带外块应保持未 shape（占位）"
            );
        });
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 全篇带 → 全部 shape（与旧行为一致，小文档回归护栏）。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            assert_eq!(
                frame.rows.iter().filter(|r| r.wrapped.is_empty()).count(),
                0,
                "全带应全部 shape"
            );
        });
    }

    #[gpui::test]
    fn off_band_reuses_shaped_height_no_jump(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        cx.update(gpui_component::init);
        // 短行 + 必折行的长段：先全带 shape（记录真实高），再窄带（长段被挤到带外）
        // ——带外高度必须复用缓存真值，而不是跳回估算（滚动“字体大小抖动”的根因）。
        let text = format!(
            "short line\n{}\n{}",
            "the quick brown fox jumps over the lazy dog ".repeat(40),
            "tail\n"
        );
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (h_real, top_real) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let rows: Vec<_> = frame
                .rows
                .iter()
                .map(|r| (r.height, r.top))
                .collect();
            assert!(rows.len() >= 3 && rows[1].0 > px(24.) * 3., "长段应多行：{:?}", rows[1]);
            (rows[1].0, rows[1].1)
        });
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 窄带 (0,1)：第一条短行（24px）之后即带外。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 1.));
            assert!(frame.rows[1].wrapped.is_empty(), "长段应已带外（未 shape）");
            assert_eq!(
                frame.rows[1].height, h_real,
                "带外长段高度应沿用缓存真值（不跳回估算）"
            );
            assert_eq!(frame.rows[1].top, top_real, "带外长段 top 也应一致");
            // 全带再回来 → 高度仍一致（缓存跨帧稳定）。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            assert_eq!(frame.rows[1].height, h_real);
        });
    }

    #[gpui::test]
    fn code_table_image_blocks_shape_with_payload(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "\
```rust
let a = 1;
let b = 2;
let c = 3;
```

| A | B |
| --- | --- |
| 1 | 2 |
| 3 | 4 |

![图](x.png)
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));

            // 代码块：3 行逐行 shape、内容行可编辑、块高 = (行数+2)×code_line。
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable, "代码内容行应可编辑");
            assert_eq!(code.wrapped.len(), 3, "3 行代码应各一个 WrappedLine");
            // 每源码行起始渲染字节偏移（"let a/ b/ c = 3;" 各 10 字节 + 2 个 '\n' 分隔）。
            assert_eq!(code.multi_offsets, vec![0, 11, 22], "每行渲染起始字节");
            assert_eq!(
                code.byte_map.len(),
                "let a = 1;\nlet b = 2;\nlet c = 3;".len() + 1,
                "块级 byte_map = 整块渲染文本长 + 哨兵"
            );
            // 渲染「块级映射」往返：第 2 行中间列 → 块内源偏移 → 应指到 `let c = 3;`。
            let line2 = "let c = 3;";
            let ok = (0..line2.len()).all(|i| {
                let src = display_to_source(&code.byte_map, 22 + i);
                let col = src.saturating_sub(display_to_source(&code.byte_map, 22));
                line2.as_bytes().get(col) == line2.as_bytes().get(i)
            });
            assert!(ok, "第 2 行渲染字节应回落到该行对应源字节");
            let expect_h = f32::from(theme.code_line()) * 5.;
            assert!(
                (f32::from(code.height) - expect_h).abs() < 1.,
                "代码块高=(行数+2)×code_line，实际 {}",
                f32::from(code.height)
            );

            // 表格块：表头 2 列 × 数据 2 行，渲染数据齐备、单元格可编辑。
            let table = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table)
                .expect("应有表格块");
            assert!(table.editable, "表格单元格应可编辑");
            let t = table.table.as_ref().expect("表格应有渲染数据");
            assert_eq!(t.header_texts.len(), 2);
            assert_eq!(t.rows_texts.len(), 2);
            assert_eq!(t.header_texts[0].as_ref(), "A");
            assert_eq!(t.rows_texts[1][1].as_ref(), "4");
            assert!(t.total_w > 0.);
            // 编辑排版：表头(1) + 数据行(2) = 3 个可见行，每行 2 格，源行号与块一致。
            let te = table.table_edit.as_ref().expect("表格应有编辑排版");
            assert_eq!(te.rows.len(), 3, "表头 + 2 数据行");
            for (i, row) in te.rows.iter().enumerate() {
                assert_eq!(row.cells.len(), 2, "每行 2 格");
            }
            assert_eq!(te.rows[0].src_line, 6, "表头源行");
            assert_eq!(te.rows[1].src_line, 8, "首数据行源行（隔开分隔行）");
            assert_eq!(te.rows[2].src_line, 9, "次数据行源行");
            assert!(
                (f32::from(table.height) - f32::from(table_block_height(&theme, 2))).abs() < 1.,
                "表格高与估算同式"
            );

            // 图片块：占位框（未加载），带 src/alt。
            let img_row = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Image)
                .expect("应有图片块");
            assert!(!img_row.editable);
            let im = img_row.image.as_ref().expect("图片应有渲染数据");
            assert_eq!(im.src.as_ref(), "x.png");
            assert_eq!(im.alt.as_ref(), "图");
            assert!(f32::from(img_row.height) > 0.);
        });
    }

    #[gpui::test]
    fn theme_mirrors_style_params_single_source(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# t\n"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            // WYSIWYG 主题必须逐项抄自预览单一排版源 StyleParams（改样式只动一处）。
            let p =
                crate::markdown::view::style::StyleParams::from_theme_with_font(
                    cx,
                    Some(f32::from(theme.font_size)),
                );
            assert_eq!(theme.text, p.fg, "正文色应抄自 StyleParams");
            assert_eq!(theme.code_bg, p.code_bg, "代码块背景应抄自 StyleParams");
            assert_eq!(
                theme.code_inline_fg, p.code_inline_fg,
                "行内代码文字色应抄自 StyleParams"
            );
            assert_eq!(
                theme.code_inline_bg, p.code_inline_bg,
                "行内代码 pill 底色应抄自 StyleParams"
            );
            assert_eq!(theme.divider, p.divider, "分隔线应抄自 StyleParams");
            assert_eq!(theme.table_head_bg, p.table_head_bg, "表头底色应抄自 StyleParams");
            assert_eq!(theme.para_line(), p.para_line, "正文行高应抄自 StyleParams");
            assert_eq!(theme.code_size(), p.code_size, "代码字号应抄自 StyleParams");
            assert_eq!(theme.code_line(), p.code_line, "代码行高应抄自 StyleParams");
            for lv in 1..=6 {
                let (fs, lh) = p.heading_metrics(lv);
                assert_eq!(theme.heading_fs(lv), fs, "h{lv} 字号应抄自 StyleParams");
                assert_eq!(theme.heading_line(lv), lh, "h{lv} 行高应抄自 StyleParams");
            }
        });
    }

    #[gpui::test]
    fn image_load_updates_size_and_rebuilds_frame(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "![图](x.png)\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let img_row = frame
                .rows
                .iter()
                .find(|r| r.image.is_some())
                .expect("应有图片块");
            let img = img_row.image.as_ref().unwrap();
            // 未加载：占位高 = IMAGE_ROW_DEFAULT_H。
            assert_eq!(
                f32::from(img_row.height),
                crate::config::markdown::IMAGE_ROW_DEFAULT_H
            );
            // 模拟加载完成：设定源像素尺寸（200×100 = 宽高比 2:1）→ 重建帧 →
            // 显示宽取内容宽、高按宽高比。
            ed.update(cx, |e, _| {
                e.set_image_size(img.src.clone(), Some((200., 100.)));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let img_row = frame
                .rows
                .iter()
                .find(|r| r.image.is_some())
                .expect("应有图片块");
            let img = img_row.image.as_ref().unwrap();
            assert!(
                (img.h - img.w / 2.).abs() < 1.,
                "图高应保持宽高比 2:1，w={} h={}",
                img.w,
                img.h
            );
            assert!(
                (f32::from(img_row.height) - img.h).abs() < 1.,
                "块高应等于显示图高 {}",
                img.h
            );
        });
    }

    #[gpui::test]
    fn ime_composition_replaces_previous_marked_text(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 回归「输入“你好”→ “nnini'hni'hao你好”」：Windows 组合更新不携带区间，
        // 必须替换上一次 marked，而不是一前一后堆叠。
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.set_caret("你好".len());
                e.replace_and_mark_utf16(None, "n", None, cx);
                assert_eq!(e.content.as_ref(), "你好n", "组合首键插在光标后");
                e.replace_and_mark_utf16(None, "ni", None, cx);
                assert_eq!(e.content.as_ref(), "你好ni", "组合更新应替换旧 marked，不得残留 n");
                e.replace_and_mark_utf16(None, "nih", None, cx);
                assert_eq!(e.content.as_ref(), "你好nih", "再次更新仍替换");
                // 提交：`replace_text_utf16(None)` 落在 marked 上并清掉 marked。
                e.replace_text_utf16(None, "你好", cx);
                assert_eq!(e.content.as_ref(), "你好你好", "提交文本替换组合占位，不重复");
                // 新组合不得再把“已提交中文”当旧 marked 替换。
                e.replace_and_mark_utf16(None, "n", None, cx);
                e.replace_and_mark_utf16(None, "ni", None, cx);
                assert_eq!(e.content.as_ref(), "你好你好ni", "新组合不重复旧提交");
            });
        });
    }

    #[gpui::test]
    fn ime_new_selected_range_is_relative_to_marked(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.set_caret(0);
                // AppKit setMarkedText 的 selectedRange 是**相对组合文本**的偏移，
                // 不是文档绝对偏移——光标应落在偏移 1（“好”前），而非文档开头。
                e.replace_and_mark_utf16(None, "你好", Some(1..2), cx);
                assert_eq!(e.content.as_ref(), "你好abc", "无选区时组合插入光标处");
                assert_eq!(
                    e.caret(),
                    3,
                    "光标 = 组合起点 0 + 相对偏移 1 的字节（“好”前），而非文档绝对 1"
                );
            });
        });
    }

    #[gpui::test]
    fn selection_drag_delete_and_select_all(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("一二三五六"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.begin_selection_at(0);
                e.extend_selection_to("一二三".len());
                assert_eq!(e.selection(), Some(0.."一二三".len()), "拖选得源字节区间");
                // 输入/删除应作用于选区。
                e.backspace(cx);
                assert_eq!(e.content.as_ref(), "五六", "Backspace 应删除整段选区");
                assert_eq!(e.selection(), None);
                // 全选 + 范围替换。
                e.select_all();
                assert_eq!(e.selection(), Some(0.."五六".len()));
                e.replace_range(e.selection().unwrap(), "你", cx);
                assert_eq!(e.content.as_ref(), "你", "全选范围替换");
            });
        });
    }

    #[gpui::test]
    fn quote_blocks_are_editable_with_indent(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# 标题\n> 引用内容\n正文"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let quote = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Quote)
                .expect("应有引述行");
            assert!(quote.editable, "引述应可编辑（byte_map 非空）");
            assert!(f32::from(quote.x_off) > 0., "引述应有文本缩进");
            // 在引述渲染文本内落点 → 源字节在 “引用内容” 上，可正常输入。
            let off = ed.read(cx).content.as_ref().find("引").unwrap();
            ed.update(cx, |e, cx| {
                e.set_caret(off + 3);
                e.replace_range(off + 3..off + 6, "X", cx);
            });
            assert!(
                ed.read(cx).content.as_ref().contains("引X"),
                "引述行应可编辑：{}",
                ed.read(cx).content.as_ref()
            );
        });
    }

    #[gpui::test]
    fn code_block_click_lands_on_correct_source_line(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "\
```rust
let a = 1;
let b = 22;
let c = 333;
```
尾段
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable, "代码内容行应可编辑");
            let lh = f32::from(code.lh);
            let top = f32::from(code.top);
            let pad = f32::from(frame.padding);
            // 点击三个代码行的中部（y = 块首 + 首空白带 + k×行高）→ 逐行命中内容。
            for (k, expects) in
                [(0usize, "let a = 1;"), (1, "let b = 22;"), (2, "let c = 333;")]
            {
                let p = point(px(pad + 30.), px(top + lh * 1.5 + lh * k as f32));
                let off = source_offset_at(ed.read(cx), &frame, p);
                let (line, _) = ed.read(cx).row_col(off);
                assert_eq!(line, 1 + k, "点击第 {k} 个代码行应命中源行 {}", 1 + k);
                let ls = ed.read(cx).line_start(line);
                let le = ed.read(cx).line_end(line);
                assert_eq!(
                    &ed.read(cx).content.as_ref()[ls..le],
                    expects,
                    "命中文本应为该行内容"
                );
            }
        });
    }

    #[gpui::test]
    fn code_block_overflow_shows_hscroll_and_shifts_hit(cx: &mut TestAppContext) {
        // 长代码行（nowrap 超出窄面板）→ 底部横向滚动条；滚动偏移使**同一可视 x**
        // 命中更多源列（paint 平移的逆映射）；拖动滚动条更新偏移（视图本地，不写源码）。
        use crate::markdown::ir::IrKind;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, size};
        cx.update(gpui_component::init);
        let text = format!("```rust\n{}\nb\n```\n", "a".repeat(400));
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                .expect("应有代码块");
            let blk = code.src_line;
            let (bar_x, bar_w, _, _, max_scroll) =
                super::code_hscroll_geo(ed.read(cx), &frame, code)
                    .expect("长行应超宽显示滚动条");
            assert!(max_scroll > 50., "内容应显著超宽");
            assert!(bar_w > 10., "滚动条轨道应有宽度");
            // 首内容行中部 y（组 0 = 首空白带 + 首行）。
            let y = f32::from(code.top) + f32::from(code.lh) * 1.5;
            let view_x = f32::from(frame.padding) + f32::from(code.x_off) + 60.;
            let p = point(px(view_x), px(y));
            let off0 = super::source_offset_at(ed.read(cx), &frame, p);
            // 滚到最右（大偏移）→ 同一可视 x 命中更后的源列。
            ed.update(cx, |e, _| e.set_code_h_scroll(blk, max_scroll));
            let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let off1 = super::source_offset_at(ed.read(cx), &f2, p);
            let (_, col0) = ed.read(cx).row_col(off0);
            let (_, col1) = ed.read(cx).row_col(off1);
            assert!(
                col1 > col0,
                "横向滚动后同一 x 应命中更后源列：col0={col0} col1={col1}"
            );
            // 拖动滚动条：点击轨道 90% → 偏移大；拖到 10% → 偏移小；松开清除拖拽态。
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(f2)));
            let band_top = f32::from(code.top) + f32::from(code.height)
                - crate::config::layout::CODE_HSCROLL_BAND;
            let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
            let down_x = bar_x + bar_w * 0.9;
            let up_x = bar_x + bar_w * 0.1;
            let ev_down = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(down_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev_down, window, cx));
            let scrolled = ed.read(cx).code_h_scroll_of(blk);
            assert!(scrolled > 0., "按下轨道应产生滚动偏移：{scrolled}");
            let ev_move = gpui::MouseMoveEvent {
                position: point(px(up_x), px(band_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&ev_move, cx));
            let scrolled2 = ed.read(cx).code_h_scroll_of(blk);
            assert!(
                scrolled2 < scrolled,
                "拖到 10% 应减小偏移：{scrolled2} < {scrolled}"
            );
            let ev_up = MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(up_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            ed.update(cx, |e, cx| e.on_mouse_up(&ev_up, cx));
            assert!(
                ed.read(cx).code_h_drag.borrow().is_none(),
                "松开应清除拖拽态"
            );
            // 视图本地状态：源码未变。
            assert_eq!(ed.read(cx).text().as_ref(), text);
        });
    }

    #[gpui::test]
    fn code_block_short_lines_have_no_hscroll(cx: &mut TestAppContext) {
        // 短代码行不超宽 → 无横向滚动条（`code_hscroll_geo` None），且陈旧滚动值被
        // `code_hscroll_clamped` 忽略（宽窗口下长行不再偏移）。防「内容超出代码块」回归。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "```rust\nlet a = 1;\nlet b = 22;\n```\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                .expect("应有代码块");
            assert!(
                super::code_hscroll_geo(ed.read(cx), &frame, code).is_none(),
                "短行不应出现横向滚动条"
            );
            // 即便手动塞一个陈旧滚动值：paint/命中共用的 clamped 也应归 0。
            ed.update(cx, |e, _| e.set_code_h_scroll(code.src_line, 999.));
            assert_eq!(
                super::code_hscroll_clamped(ed.read(cx), &frame, code),
                0.,
                "内容不超宽时应忽略陈旧滚动值"
            );
            assert!(super::code_hscroll_geo(ed.read(cx), &frame, code).is_none());
        });
    }

    #[gpui::test]
    fn code_block_hscroll_track_click_jumps(cx: &mut TestAppContext) {
        // view 语义：点击**轨道**（不在滑块上）→ 滑块跳到点击处；点击**滑块** → 拖动。
        // 旧实现点轨道空白处无反应（手感差），此处回归点击轨道即跳转。
        use crate::markdown::ir::IrKind;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, size};
        cx.update(gpui_component::init);
        let text = format!("```rust\n{}\nb\n```\n", "a".repeat(400));
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let (blk, ctop, ctop_h, bar_x, bar_w, _thumb_x, thumb_w, max_scroll) = {
                let code = frame
                    .rows
                    .iter()
                    .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                    .expect("应有代码块");
                let (bar_x, bar_w, thumb_x, thumb_w, max_scroll) =
                    super::code_hscroll_geo(ed.read(cx), &frame, code).unwrap();
                // 初始 scroll=0 → 滑块贴着轨道左端。
                assert!(thumb_x <= bar_x + 1., "初态滑块应在左端");
                (
                    code.src_line,
                    f32::from(code.top),
                    f32::from(code.height),
                    bar_x,
                    bar_w,
                    thumb_x,
                    thumb_w,
                    max_scroll,
                )
            };
            // 把刚建的帧放入编辑器，供 `on_mouse_down`/命中读取。
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let band_top = ctop + ctop_h - crate::config::layout::CODE_HSCROLL_BAND;
            let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
            // 点击轨道**末端**（远离初始左端滑块，必不在滑块上）→ 直接跳转（不进拖动态）。
            let jump_x = bar_x + bar_w - 4.0;
            let jump_y = band_y;
            // 断言**点击前**命中的是轨道（非滑块）。
            let (blk_pre, on_thumb_pre, _) = super::code_hscrollbar_hit(
                ed.read(cx),
                &ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
                point(px(jump_x), px(jump_y)),
            )
            .expect("轨道末应在滚动条带内");
            assert_eq!(blk_pre, blk);
            assert!(!on_thumb_pre, "点击末端的点不应落在初始滑块上");
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(jump_x), px(jump_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            let jumped = ed.read(cx).code_h_scroll_of(blk);
            assert!(jumped > 0., "点击轨道应立刻跳转（不拖也动）：{jumped}");
            assert!(
                ed.read(cx).code_h_drag.borrow().is_none(),
                "轨道跳转不应进入拖动态"
            );
            assert!(
                jumped <= max_scroll,
                "跳转偏移应不超过 max_scroll（{jumped} ≤ {max_scroll}）"
            );
            // 再点滑块中心 → 进入拖动态，拖动到左端归零。
            let geo2 = super::code_hscroll_geo(
                ed.read(cx),
                &ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
                &ed.read(cx).frame.borrow().as_ref().unwrap().rows.iter().find(|r| r.src_line == blk).unwrap(),
            ).unwrap();
            let (_, _, thumb_x2, thumb_w2, _) = geo2;
            let thumb_cx = thumb_x2 + thumb_w2 / 2.;
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(thumb_cx), px(jump_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(
                ed.read(cx).code_h_drag.borrow().is_some(),
                "点滑块应进入拖动态"
            );
            let ev_move = MouseMoveEvent {
                position: point(px(bar_x + 6.), px(jump_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&ev_move, cx));
            let dragged = ed.read(cx).code_h_scroll_of(blk);
            assert!(dragged < jumped, "拖到左端应减小偏移：{dragged} < {jumped}");
            let ev_up = MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(bar_x + 6.), px(jump_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            ed.update(cx, |e, cx| e.on_mouse_up(&ev_up, cx));
            assert!(ed.read(cx).code_h_drag.borrow().is_none(), "松开应清拖拽态");
        });
    }

    #[gpui::test]
    fn code_block_caret_tracks_clicked_content_line(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "\
```py
one
two
three
```
tail
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (offsets, code_top, lh) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable);
            let e = ed.read(cx);
            let offs: Vec<usize> = (1usize..=3).map(|ln| e.line_start(ln) + 1).collect();
            (offs, f32::from(code.top), f32::from(code.lh))
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            for (k, off) in offsets.iter().enumerate() {
                ed.update(cx, |e, _| e.set_caret(*off));
                let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
                let (on, _, ctop) = caret_geometry(ed.read(cx), &frame);
                assert!(on, "代码内容行应可放光标");
                // caret 顶 = 块首 + 1 行首空白带 + k×行高。
                let expect = code_top + lh * (1 + k) as f32;
                assert!(
                    (f32::from(ctop) - expect).abs() < 0.6,
                    "k={k} 光标顶应 {expect}，得 {}",
                    f32::from(ctop)
                );
            }
        });
    }

    #[gpui::test]
    fn code_block_edit_typing_backspace_enter(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "```rust\nlet a = 1;\nlet b = 2;\n```\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                // 光标落在首个代码行内输入 → 源文本在围栏内更新。
                let ls = e.line_start(1);
                e.set_caret(ls + 4);
                e.replace_range(e.caret()..e.caret(), "X", cx);
                assert!(
                    e.content.as_ref().starts_with("```rust\nlet Xa = 1;"),
                    "围栏内输入生效：{}",
                    e.content.as_ref()
                );
                // 退格删去 X（插入后光标已在 X 之后）。
                e.backspace(cx);
                assert_eq!(
                    &e.content.as_ref()[ls..ls + 9],
                    "let a = 1",
                    "退格删回原位，围栏未损坏"
                );
                // 末内容行行尾的删除是 no-op（保护闭合围栏）；再回车在闭合围栏前新起代码行。
                let e2 = e.line_end(2);
                e.set_caret(e2);
                e.delete(cx);
                assert!(
                    e.content.as_ref().contains("let b = 2;\n```"),
                    "末内容行行尾删除应被守卫（闭合围栏不被吃掉）：{}",
                    e.content.as_ref()
                );
                let e2 = e.line_end(2);
                e.set_caret(e2);
                e.newline(cx);
                assert!(
                    e.content.as_ref().contains("let b = 2;\n\n```"),
                    "回车应在闭合围栏前新起一代码行：{}",
                    e.content.as_ref()
                );
                // 首内容行行首退格是 no-op（保护开围栏）。
                e.set_caret(e.line_start(1));
                e.backspace(cx);
                assert!(
                    e.content.as_ref().starts_with("```rust\n"),
                    "首内容行行首退格应被守卫（开围栏不被吃掉）：{}",
                    e.content.as_ref()
                );
            });
        });
    }

    #[gpui::test]
    fn code_block_arrow_nav_skips_fences(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "前段\n```rust\nlet a = 1;\nlet b = 2;\n```\n后段\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _cx| {
                // 首内容行行首向上 → 「前段」段尾，光标不落进开围栏。
                e.set_caret(e.line_start(2));
                e.move_up();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 0, "首内容行向上应到行 0（前段），得 {l}");
                // 「前段」行尾向右 → 跳过开围栏进代码首内容行（行 2）。
                e.set_caret(e.line_end(0));
                e.move_right();
                let (l, col) = e.row_col(e.caret());
                assert!(l == 2 && col == 0, "前段行尾向右应到首内容行行首，得 ({l},{col})");
                // 内容行内上下移动 → 光标行始终在内容行 2/3。
                e.set_caret(e.line_start(2) + 3);
                e.move_down();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 3, "内容行内下移应在行 3，得 {l}");
                e.move_up();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 2, "内容行内上移回行 2，得 {l}");
                // 末内容行行尾向右 → 跳过闭合围栏到「后段」（行 5）段首。
                e.set_caret(e.line_end(3));
                e.move_right();
                let (l, col) = e.row_col(e.caret());
                assert!(l == 5 && col == 0, "末内容行行尾向右应到后段行首，得 ({l},{col})");
                // 后段向上 → 进代码块末内容行（行 3）。
                e.set_caret(e.line_start(5) + 2);
                e.move_up();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 3, "后段向上应进代码块末内容行 3，得 {l}");
            });
        });
    }

    #[gpui::test]
    fn code_block_off_band_height_matches_shaped(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        // 一长段把代码块推到 300px 带之外 → 带外估算高必须与整形式高度一致（防滚动跳变）。
        let head = "the quick brown fox jumps over the lazy dog ".repeat(300);
        let text = format!("{head}\n```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n尾行\n");
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let shaped_h = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable);
            f32::from(code.height)
        });
        let off_h = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 窄带：长段 (~1000px) 全占，代码块与尾行在带外 → 占位（不 shape）。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 300.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块占位");
            assert!(!code.editable, "带外代码块不 shape");
            f32::from(code.height)
        });
        assert!(
            (off_h - shaped_h).abs() < 1.,
            "代码块带外估算高 {off_h} 应与整形高 {shaped_h} 一致（防滚动跳变）"
        );
    }

    #[gpui::test]
    fn code_block_caret_aligns_with_click_x(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        // 回归「点击 (x,y) 实际输入在 (x+n,y)/光标盖住字符」：代码行文本内衬 = 12px
        // （`x_off`，面板 pl_3），命中/光标必须都从 `padding + x_off`(24) 起算。
        let text = "```rust\nlet a = 1;\n```\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable);
            let pad = f32::from(frame.padding);
            let off_x = f32::from(code.x_off);
            assert!((off_x - 12.).abs() < 0.1, "代码行文本内衬 = 面板 pl_3（12px），得 {off_x}");
            let lh = f32::from(code.lh);
            let text_start = pad + off_x; // 24：代码文本起始 x（面板背景在 pad=12，与正文对齐）
            // 1) 点击文本起始处 → 光标 x == 文本起始 x（不右偏、不盖字）。
            let off0 = source_offset_at(ed.read(cx), &frame, point(px(text_start), px(lh * 1.5)));
            let (line0, _) = ed.read(cx).row_col(off0);
            assert_eq!(line0, 1, "点击文本始应命中代码内容行，得行 {line0}");
            ed.update(cx, |e, _| e.set_caret(off0));
            let (on, c0, _) = caret_geometry(ed.read(cx), &frame);
            assert!(on);
            assert!(
                (f32::from(c0) - text_start).abs() < 0.6,
                "光标 x {} 应 = 文本起始 x {text_start}",
                f32::from(c0)
            );
            // 2) 点击面板内、文本左的内衬空白 → 光标收拢到行首（= 文本起始 x），不右偏。
            let off1 = source_offset_at(ed.read(cx), &frame, point(px(pad + 6.), px(lh * 1.5)));
            ed.update(cx, |e, _| e.set_caret(off1));
            let (on, c1, _) = caret_geometry(ed.read(cx), &frame);
            assert!(on);
            assert!(
                (f32::from(c1) - text_start).abs() < 0.6,
                "内衬空白点击应收拢到行首 {text_start}，得 {}",
                f32::from(c1)
            );
            // 3) 文本中部点击 → 光标落点在 <1 字宽误差内贴合点击处。
            let cx_ = text_start + 28.;
            let off2 = source_offset_at(ed.read(cx), &frame, point(px(cx_), px(lh * 1.5)));
            ed.update(cx, |e, _| e.set_caret(off2));
            let (on, c2, _) = caret_geometry(ed.read(cx), &frame);
            assert!(on);
            assert!(
                (f32::from(c2) - cx_).abs() < 14.,
                "光标 x {} 应贴近点击 x {}（<1 字宽）",
                f32::from(c2),
                cx_
            );
        });
    }

    #[gpui::test]
    fn table_click_lands_in_correct_cell(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let row = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table)
                .expect("应有表格");
            assert!(row.editable, "表格单元格应可编辑");
            let t = row.table.as_ref().expect("表格渲染数据");
            let te = row.table_edit.as_ref().expect("表格编辑排版");
            let rh = f32::from(t.row_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(row.top);
            // (可见行, 列, 期望字符)：表头第一格 A；数据行2(源行3) 两格 3/4。
            for (row_ix, cell_ix, want) in [(0usize, 0usize, 'A'), (2, 0, '3'), (2, 1, '4')] {
                let tx = te.rows[row_ix].cells[cell_ix].text_x;
                let y = top + table_row_top(t, row_ix) + rh / 2.;
                let x = pad + table_cell_x0(&t.widths, cell_ix) + tx + 1.;
                let off = source_offset_at(ed.read(cx), &frame, point(px(x), px(y)));
                let c = ed.read(cx).content.as_ref()[off..]
                    .chars()
                    .next()
                    .unwrap_or('_');
                assert_eq!(c, want, "单元格 (行{row_ix},列{cell_ix}) 应命中字符 {want}");
            }
        });
    }

    #[gpui::test]
    fn table_caret_aligns_with_clicked_cell(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let row = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table)
                .expect("应有表格");
            let t = row.table.as_ref().unwrap();
            let te = row.table_edit.as_ref().unwrap();
            let rh = f32::from(t.row_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(row.top);
            // 数据第 2 行第 2 格（"4"@源行3）：点击 → set_caret → caret x 应贴合点击 x。
            let cell_ix = 1usize;
            let row_ix = 2usize;
            let tx = te.rows[row_ix].cells[cell_ix].text_x;
            let x = pad + table_cell_x0(&t.widths, cell_ix) + tx + 20.;
            let y = top + table_row_top(t, row_ix) + rh / 2.;
            let off = source_offset_at(ed.read(cx), &frame, point(px(x), px(y)));
            ed.update(cx, |e, _| e.set_caret(off));
            let (on, caretx, carety) = caret_geometry(ed.read(cx), &frame);
            assert!(on, "表格单元格应可放光标");
            assert!(
                (f32::from(caretx) - x).abs() < 12.,
                "光标 x {} 应贴合点击 x {}（<1 字宽）",
                f32::from(caretx),
                x
            );
            let row_top_y = top + table_row_top(t, row_ix);
            assert!(
                f32::from(carety) >= row_top_y - 0.5 && f32::from(carety) <= row_top_y + rh + 0.5,
                "光标 y {} 应在点击行带 [{row_top_y}, {}] 内",
                f32::from(carety),
                row_top_y + rh
            );
        });
    }

    #[gpui::test]
    fn table_cell_edit_typing_and_boundary_guards(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "| 1 | 2 |\n| --- | --- |\n| a | b |\n尾\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                // 在数据行第一格 "a"（源行 2，位 2）内输入 X。
                let ls = e.line_start(2);
                e.set_caret(ls + 2);
                e.replace_range(e.caret()..e.caret(), "X", cx);
                assert!(
                    e.content.as_ref().contains("| Xa | b |"),
                    "格内输入生效：{}",
                    e.content.as_ref()
                );
                // 退格在格内容起点（'X'）→ no-op（守卫 `| ` 不被吃掉）。
                e.set_caret(ls + 2);
                e.backspace(cx);
                assert!(
                    e.content.as_ref().contains("| Xa | b |"),
                    "格起点退格应被守卫：{}",
                    e.content.as_ref()
                );
                // 删除在格内容尾 → no-op（守卫 `|` 不被吃掉）。
                let ls2 = e.line_start(2);
                e.set_caret(ls2 + 4);
                e.delete(cx);
                assert!(
                    e.content.as_ref().contains("| Xa | b |"),
                    "格内容尾删除应被守卫：{}",
                    e.content.as_ref()
                );
                // 表格内回车 → no-op（不拆表）。
                let before = e.content.as_ref().to_string();
                e.set_caret(ls2 + 3);
                e.newline(cx);
                assert_eq!(
                    e.content.as_ref(),
                    &before,
                    "表格内回车应 no-op：{}",
                    e.content.as_ref()
                );
            });
        });
    }

    #[gpui::test]
    fn table_arrow_nav_skips_delimiter(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n尾\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _cx| {
                // 表头（行 0）down → 首数据行（行 2，跳过分隔行 1）。
                e.set_caret(e.line_start(0) + 2);
                e.move_down();
                assert_eq!(e.row_col(e.caret()).0, 2, "表头 down 应到首数据行");
                // 数据行 up → 上一数据行。
                e.set_caret(e.line_start(3) + 4);
                e.move_up();
                assert_eq!(e.row_col(e.caret()).0, 2, "次数据行 up 应到首数据行");
                // 首数据行 up → 表头（跳过分隔行）。
                e.move_up();
                assert_eq!(e.row_col(e.caret()).0, 0, "首数据行 up 应到表头");
                // 末数据行 down → 出表到尾段（行 4）。
                e.set_caret(e.line_start(3) + 4);
                e.move_down();
                assert_eq!(e.row_col(e.caret()).0, 4, "末数据行 down 应出表到尾段");
                // 单元格内 Left：行首 Left 被钳在格内容（不落管道空白）。
                e.set_caret(e.line_start(2) + 2); // "1"
                e.move_left();
                {
                    let (l, c) = e.row_col(e.caret());
                    assert_eq!(l, 2, "内容行 Left 不应出表");
                    assert_eq!(c, 2, "格内容起点 Left 被钳在格内（col {c}）");
                }
            });
        });
    }

    #[gpui::test]
    fn new_features_shape_gutters_cards_links(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        use super::{BlockCard, LinkHit, RowGutter};
        cx.update(gpui_component::init);
        let text = "\
# 标题

- [x] 完成
- [ ] 待办

> [!NOTE]- 提示正文
> 折叠内容

行内链接 [示例](https://example.com) 与数学 $x^2$ 同段

$$
\\int x dx
$$

```mermaid
graph TD
A-->B
```
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            // C1: 任务行 gutter = Check；未勾选行在 src 行 3。
            let check = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Check { checked: false })))
                .expect("应有未勾选任务行");
            assert_eq!(check.src_line, 3, "`- [ ] 待办` 的源行");
            // C3: 警报 marker 行 gutter = Alert；其下 body 行折叠成 0 高。
            let alert = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Alert { .. })))
                .expect("应有警报行");
            assert_eq!(alert.src_line, 5, "`> [!NOTE]-` marker 源行");
            let folded = frame
                .rows
                .iter()
                .find(|r| r.src_lines.contains(&6))
                .expect("callout 主体行应存在");
            assert_eq!(f32::from(folded.height), 0., "折叠 callout 主体应 0 高");
            // C5: 有可点击链接的行。
            let linkrow = frame
                .rows
                .iter()
                .find(|r| !r.extra.links.is_empty())
                .expect("应有链接行");
            assert!(
                linkrow.extra.links.iter().any(|l| matches!(l.target, LinkHit::Url(_))),
                "应命中 URL 链接"
            );
            // B2: 行内数学 splice 成 spacer 并翻译成 Unicode 排版（`x^2` → `x²`）。
            let math_row = frame
                .rows
                .iter()
                .find(|r| !r.extra.inline_maths.is_empty())
                .expect("应有行内数学");
            let im = &math_row.extra.inline_maths[0];
            assert!(
                im.latex.as_ref().contains('²') || im.latex.as_ref().contains('π'),
                "行内数学应翻译成 Unicode：「{}」",
                im.latex
            );
            assert!(
                !math_row.extra.links.is_empty(),
                "含 `[text](url)` 的行应为可点击链接"
            );
            // A1: 数学块折叠成 Math 卡片（整块一行）。
            let mathcard = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .expect("应有数学卡片");
            assert!(f32::from(mathcard.height) >= 40., "数学卡片高");
            assert_eq!(mathcard.src_lines, 10..13, "数学卡片覆盖 `$$…$$` 块");
            // A2: mermaid 围栏 → Mermaid 卡片。
            let mm = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid { .. })))
                .expect("应有 mermaid 卡片");
            assert!(
                f32::from(mm.height) >= 100.,
                "mermaid 卡片高按层数计算：{}",
                f32::from(mm.height)
            );
            let _ = IrKind::Paragraph;
        });
    }

    #[gpui::test]
    fn mermaid_renders_diagram_to_image(cx: &mut TestAppContext) {
        // mermaid 卡片：层式布局（4 节点 3 边）+ 高度按层数算 + 把**同一套几何**
        // 光栅化成 PNG（盒子+连线+箭头），供外部识图核验“真图”。
        use super::{BlockCard, MmPlaced, mm_layout};
        let _ = std::fs::create_dir_all(r"C:\Users\C22500~1\AppData\Local\Temp\kilo");
        const SRC: &str = "A[开始] --> B{判断}\nB -->|是| C[完成]\nB -->|否| D[返回]";
        cx.update(gpui_component::init);
        let text = "\
# 标题

```mermaid
graph TD
    A[开始] --> B{判断}
    B -->|是| C[完成]
    B -->|否| D[返回 A]
```

尾段
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (card_h, n_nodes, n_edges, png_path, ink, size) =
            vcx.update(|window, cx| {
                let ed = host.read(cx).ed.clone();
                let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
                let mm = frame
                    .rows
                    .iter()
                    .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid { .. })))
                    .expect("应有 mermaid 卡片");
                let card_h = f32::from(mm.height);
                let theme = ed.read(cx).theme.clone();
                let (placed, edges) = mm_layout(window, &theme, SRC, 0., 0., 560.);
                let (img, ink) = rasterize_mm_png(&placed, &edges);
                let path = r"C:\Users\C22500~1\AppData\Local\Temp\kilo\mermaid_check.png".to_string();
                let saved = img.save(&path).is_ok();
                assert!(saved, "应能保存 PNG");
                let size = (img.width(), img.height());
                (card_h, placed.len(), edges.len(), path, ink, size)
            });
        assert!(card_h >= 100., "卡片高按层数计算：{card_h}");
        assert_eq!(n_nodes, 4, "4 个节点");
        assert_eq!(n_edges, 3, "3 条边");
        assert!(ink > 300, "PNG 应画出盒子/连线（非白像素 {ink}）");
        eprintln!("mermaid PNG 已导出：{png_path}（{size:?}，涂墨 {ink}px）");
    }

    /// 把布局几何光栅化成 PNG（盒子边框 + 连线 + 箭头；中文标签不进位图，
    /// 由「框 + 箭头」结构本身表明这是一张流程图）。
    fn rasterize_mm_png(
        placed: &[super::MmPlaced],
        edges: &[(usize, usize, Option<String>)],
    ) -> (image::RgbaImage, usize) {
        const PAD: i32 = 40;
        if placed.is_empty() {
            return (image::RgbaImage::new(1, 1), 0);
        }
        let mut x0f = f32::MAX;
        let mut y0f = f32::MAX;
        let mut x1f = f32::MIN;
        let mut y1f = f32::MIN;
        for p in placed {
            x0f = x0f.min(p.x);
            y0f = y0f.min(p.y);
            x1f = x1f.max(p.x + p.w);
            y1f = y1f.max(p.y + p.h);
        }
        let w = ((x1f - x0f) as i32 + 2 * PAD).max(8);
        let h = ((y1f - y0f) as i32 + 2 * PAD).max(8);
        let mut img = image::RgbaImage::from_pixel(
            w as u32,
            h as u32,
            image::Rgba([255, 255, 255, 255]),
        );
        let ox = -x0f as i32 + PAD;
        let oy = -y0f as i32 + PAD;
        let mut ink = 0usize;
        let dark = [40u8, 45, 60, 255];
        let fill = [235u8, 238, 245, 255];
        // 边：三层折线 + 箭头。
        for (ia, ib, _) in edges {
            let (pa, pb) = (&placed[*ia], &placed[*ib]);
            let ax = (pa.x + pa.w / 2.) as i32 + ox;
            let ay = (pa.y + pa.h) as i32 + oy;
            let bx = (pb.x + pb.w / 2.) as i32 + ox;
            let by = pb.y as i32 + oy;
            let midy = (ay + by) / 2;
            if (bx - ax).abs() < 2 {
                mm_blit(&mut img, &mut ink, ax, ay, bx, by - 4, dark);
            } else {
                mm_blit(&mut img, &mut ink, ax, ay, ax, midy, dark);
                mm_blit(&mut img, &mut ink, ax, midy, bx, midy, dark);
                mm_blit(&mut img, &mut ink, bx, midy, bx, by - 4, dark);
            }
            mm_blit(&mut img, &mut ink, bx - 4, by - 6, bx, by - 1, dark);
            mm_blit(&mut img, &mut ink, bx + 4, by - 6, bx, by - 1, dark);
        }
        // 节点：填充 + 边框。
        for p in placed {
            let (x0, y0) = (p.x as i32 + ox, p.y as i32 + oy);
            let (x1, y1) = ((p.x + p.w) as i32 + ox, (p.y + p.h) as i32 + oy);
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    if xx >= 0 && yy >= 0 && (xx as u32) < img.width() && (yy as u32) < img.height() {
                        img.put_pixel(xx as u32, yy as u32, fill.into());
                        ink += 1;
                    }
                }
            }
            mm_blit(&mut img, &mut ink, x0, y0, x1, y0, dark);
            mm_blit(&mut img, &mut ink, x0, y1, x1, y1, dark);
            mm_blit(&mut img, &mut ink, x0, y0, x0, y1, dark);
            mm_blit(&mut img, &mut ink, x1, y0, x1, y1, dark);
        }
        (img, ink)
    }

    fn mm_blit(img: &mut image::RgbaImage, ink: &mut usize, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]) {
        let (w, h) = (img.width() as i32, img.height() as i32);
        let (mut x0, mut y0) = (x0, y0);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            if x0 >= 0 && y0 >= 0 && x0 < w && y0 < h {
                img.put_pixel(x0 as u32, y0 as u32, c.into());
                *ink += 1;
            }
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    #[gpui::test]
    fn callout_fold_click_path_round_trips(cx: &mut TestAppContext) {
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        use super::{Frame, RowGutter};
        cx.update(gpui_component::init);
        let text = "> [!NOTE]- 甲\n> 乙\n> 丙\n尾";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(point(px(0.), px(0.)), size(px(600.), px(600.)))));
            });
            let chevron_point = |f: &Frame| {
                let marker = f
                    .rows
                    .iter()
                    .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Alert { .. })))
                    .expect("应有警报 marker 行");
                let y = f32::from(marker.top) + f32::from(marker.height) / 2.;
                point(px(12. + 40.), px(y))
            };
            // 折叠态 → 点击展开。
            let frame1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame1)));
            let f1 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev1 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f1),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]+"), "点击应展开");
            // 展开态 → 再点折叠回。
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame2)));
            let f2 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f2),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"), "再点应折叠回");
            assert_eq!(ed.read(cx).text().as_ref(), text, "往返后内容还原");
            // 关键断言：折叠后 body 行必须真正折叠（0 高）——光标在 marker 行也不得被 reveal 弹开。
            let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body = frame3.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(body.height), 0., "点击折叠后 body 应真正折叠（回归）");
        });
    }

    #[gpui::test]
    fn two_callouts_fold_independently_with_caret_on_marker(cx: &mut TestAppContext) {
        // 回归：两个 callout 相邻（中间空行）。各自折叠/展开应互不影响；
        // 尤其「光标落在 marker 行（点击折叠后的常态）」时，该 callout 仍要真正折叠。
        cx.update(gpui_component::init);
        let text = "> [!NOTE]- 点击箭头折叠\n> 主体一\n> 主体二\n\n> [!WARNING]- 警告\n> 警告内容。\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 光标在 NOTE marker 行：两个 callout 的 body 都应折叠。
            ed.update(cx, |e, _| e.set_caret(0));
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let note = frame.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(note.height), 0., "光标在 marker 行时 NOTE body 也应折叠");
            let warn = frame.rows.iter().find(|r| r.src_lines.contains(&5)).unwrap();
            assert_eq!(f32::from(warn.height), 0., "WARNING body 也应折叠");
            // 展开 NOTE → 再折叠（光标仍在 NOTE marker 行）。
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(0, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]+"), "先展开");
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(0, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"), "再折叠回");
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let note2 = frame2.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(note2.height), 0., "折叠后 NOTE body 应真正折叠（不是 reveal）");
            // 互不影响：折叠 WARNING 不应改动 NOTE。
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(4, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"), "NOTE 折叠态保持");
            assert!(
                ed.read(cx).text().as_ref().find("> [!WARNING]+").is_some()
                    || ed.read(cx).text().as_ref().find("> [!WARNING]-").is_some(),
                "WARNING 折叠符存在"
            );
        });
    }

    #[gpui::test]
    fn heading_chevron_click_round_trips(cx: &mut TestAppContext) {
        // 标题折叠箭头 → 视图本地折叠态翻转（不写源码、不进撤销栈）：`## 甲` 的
        // section body 行折叠成 0 高；再点展开还原。折叠后光标停在标题行，不得被
        // reveal 弹开（与 callout 的坑 P 同一教训）。
        use super::Frame;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "## 甲\nbody 一\nbody 二\n## 乙\n尾";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(point(px(0.), px(0.)), size(px(600.), px(600.)))));
            });
            let chevron_point = |f: &Frame| {
                let heading = f
                    .rows
                    .iter()
                    .find(|r| matches!(r.kind, crate::markdown::ir::IrKind::Heading(_)))
                    .expect("应有标题行");
                let (cx0, _) = super::heading_chevron_box(f, heading).unwrap();
                let y = f32::from(heading.top) + f32::from(heading.lh) / 2.;
                point(px(cx0 + 8.), px(y))
            };
            // 折叠 `## 甲`（单击标题行左缘箭头）：body 行真正折叠（0 高）。
            let frame1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame1)));
            let f1 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev1 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f1),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
            assert!(
                ed.read(cx).folded_headings.borrow().contains("## 甲"),
                "点击标题箭头应折叠"
            );
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body = frame2.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(body.height), 0., "折叠后 body 应真正折叠");
            let h2 = frame2.rows.iter().find(|r| r.src_lines.contains(&0)).unwrap();
            assert!(f32::from(h2.height) > 0., "标题行本身保留");
            // 展开回：body 高度恢复，文本不变（视图本地，不写源码）。
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame2)));
            let f2 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f2),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(
                !ed.read(cx).folded_headings.borrow().contains("## 甲"),
                "再点应展开"
            );
            assert_eq!(ed.read(cx).text().as_ref(), text, "折叠/展开不写源码");
            let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body3 = frame3.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert!(f32::from(body3.height) > 0., "展开后 body 恢复高度");
        });
    }

    #[gpui::test]
    fn heading_fold_stays_folded_with_caret_on_heading_line(cx: &mut TestAppContext) {
        // 回归（坑 P 同款）：点击折叠后光标停在标题行，该 section 不得被 reveal 弹开；
        // 光标严格落在 body 行内才 reveal。
        cx.update(gpui_component::init);
        let text = "# A\nbody\n## B\nmore\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.toggle_heading_fold_at_line(0, cx);
            });
            // 光标默认在行首（标题行）→ 折叠保持。
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body = frame.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(body.height), 0., "光标在标题行时折叠保持");
            // 光标移到 body（源字节 6 = "body" 起点所在行）→ reveal：body 恢复高度。
            ed.update(cx, |e, _| e.set_caret(text.find("body").unwrap()));
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body2 = frame2.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert!(f32::from(body2.height) > 0., "光标在 body 行内应 reveal");
        });
    }

    #[gpui::test]
    fn toggle_heading_fold_moves_caret_out_when_folding_from_body(cx: &mut TestAppContext) {
        // 折叠时若光标在 section body 内，先把光标移到标题行末（否则 body 被 reveal，
        // 折叠无效）——与 live 的 `move_to(end)` 同款。
        cx.update(gpui_component::init);
        let text = "## Goals\n- 任务一\n- 任务二\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 光标放在列表行（body 内）。
            ed.update(cx, |e, _| e.set_caret(text.find("- 任务一").unwrap()));
            let before = ed.read(cx).caret();
            assert!(before > 6, "光标在 body 内");
            ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
            assert!(
                ed.read(cx).folded_headings.borrow().contains("## Goals"),
                "折叠成功"
            );
            // 折叠后光标在标题行内、不在 body（第 0 行），保证折叠不被 reveal 弹开。
            let (line, _) = ed.read(cx).row_col(ed.read(cx).caret());
            assert_eq!(line, 0, "折叠时光标应移到标题行");
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body = frame.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(body.height), 0., "body 真正折叠");
        });
    }

    #[gpui::test]
    fn heading_fold_hides_math_cards_completely(cx: &mut TestAppContext) {
        // 回归：折叠标题，其 section 内的 `$$…$$` 数学块卡片（“公式”标签 + 公式图）
        // 必须一并折叠成 0 高，不得因 `extra.card` 继续绘制而重叠堆在标题上方。
        use super::BlockCard;
        cx.update(gpui_component::init);
        let text = "# 公式区\n$$\nx^2\n$$\n$$\n\\int dx\n$$\n尾\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 展开时：两个数学卡片都在（src_lines 覆盖 `$$…$$`）。
            let frame_open = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let open_cards: Vec<_> = frame_open
                .rows
                .iter()
                .filter(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .collect();
            assert_eq!(open_cards.len(), 2, "展开时应有 2 个数学卡片");
            // 折叠标题：卡片行必须消失（fold 后 math_start 不装配卡片，走普通 0 高占位）。
            ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
            assert!(
                ed.read(cx).folded_headings.borrow().contains("# 公式区"),
                "已折叠"
            );
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let cards: Vec<_> = frame
                .rows
                .iter()
                .filter(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .collect();
            assert!(
                cards.is_empty(),
                "折叠标题后数学卡片不得残留绘制（{}）",
                cards.len()
            );
            // 原卡片行应折叠成 0 高（普通占位），不再是卡片。
            let m1 = frame.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert_eq!(f32::from(m1.height), 0., "`\\$\\$` 起始行折叠成 0 高");
        });
    }

    #[gpui::test]
    fn callout_body_rows_share_alert_bar_color(cx: &mut TestAppContext) {
        // 回归：展开的 callout，其 body 行左条应与 marker 同色（警报色），不是灰条。
        // 检查 frame 里 callout body 行的 bar 配色走 alert 路径（间接断言：
        // `alert_region_kinds` 把 marker + body 行都标上 kind，paint 据此取警报色）。
        use crate::markdown::markdown_syntax;
        use crate::markdown::md_syntax::AlertKind;
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "> [!NOTE]+ 提示\n> body一\n> body二\n\n> 普通引述\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let bodies: Vec<_> = frame
                .rows
                .iter()
                .filter(|r| r.kind == IrKind::Quote && r.src_line >= 1 && r.src_line <= 2)
                .collect();
            assert_eq!(bodies.len(), 2, "callout body 行应存在");
            let kinds = markdown_syntax::alert_region_kinds(ed.read(cx).content());
            for b in &bodies {
                assert_eq!(
                    kinds.get(&b.src_line),
                    Some(&AlertKind::Note),
                    "body 行 {} 左条应取警报色",
                    b.src_line
                );
            }
            // 普通引述行不进入警报色表。
            let plain = frame.rows.iter().find(|r| r.src_line == 4).unwrap();
            assert!(!kinds.contains_key(&4), "普通引述不应走警报色");
        });
    }

    #[gpui::test]
    fn paint_smoke_all_new_features(cx: &mut TestAppContext) {
        // 把含任务框/警报折叠/链接/行内数学/数学块/mermaid 的内容画一遍，确保不 panic。
        use super::BlockCard;
        cx.update(gpui_component::init);
        let text = "\
- [ ] 待办

# 标题折叠

> [!WARNING]+ 重要
> 内容

> 连续引述第一行
> 连续引述第二行

链接 [示例](https://a.b) 与 $x^2$

$$
\\sqrt{\\pi}
$$

```mermaid
graph TD
A-->B
```
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                frame.rows.iter().any(|r| matches!(r.extra.card, Some(BlockCard::Mermaid { .. }))),
                "应渲染出 mermaid 卡片"
            );
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            // 触发一次真实绘制（measure→prepaint→paint），catch 任何渲染 panic。
            let _ = window.draw(cx);
        });
    }

    #[test]
    fn math_to_unicode_translates_superscripts_and_symbols() {
        use super::math_to_unicode;
        assert!(math_to_unicode("E=mc^2").contains('²'), "上标 2 → ²");
        assert!(math_to_unicode("\\int_0^1 x^2 dx").contains('∫'), "积分符号");
        assert!(math_to_unicode("\\sqrt{\\pi}").contains('√'), "根号");
        assert!(math_to_unicode("\\sum_{i=1}^{n}").contains('∑'), "求和");
        assert_eq!(math_to_unicode("\\tfrac{1}{3}"), "1/3", "分数 → a/b");
    }

    #[test]
    fn mermaid_graph_parses_nodes_and_edges() {
        use super::{MmShape, mermaid_graph};
        let src = "graph TD\n    A[开始] --> B{判断}\n    B -->|是| C[完成]\n    B -->|否| D[返回 A]\n";
        let g = mermaid_graph(src);
        assert_eq!(g.nodes.len(), 4, "4 节点");
        assert_eq!(g.edges.len(), 3, "3 边");
        assert!(g.nodes.iter().any(|(id, lab, _)| id == "A" && lab == "开始"), "A=开始");
        assert!(g.nodes.iter().any(|(id, lab, _)| id == "C" && lab == "完成"), "C=完成");
        // 回归：`B{判断}` 的定义不能被后续裸引用 `B -->` 覆盖成 “B”。
        assert!(
            g.nodes.iter().any(|(id, lab, _)| id == "B" && lab == "判断"),
            "B 的标签应保持为「判断」，却被后续裸引用覆盖：{:?}",
            g.nodes.iter().filter(|(id, _, _)| id == "B").collect::<Vec<_>>()
        );
        let (id, _, shp) = g.nodes.iter().find(|(id, _, _)| id == "B").unwrap();
        assert!(id == "B" && *shp == MmShape::Diamond, "B 为判断（菱形）");
        let (a, b, lab) = &g.edges[1];
        assert!(a == "B" && b == "C" && lab.as_deref() == Some("是"), "边 B→C(是)");
    }

    #[gpui::test]
    fn undo_redo_round_trip(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.set_caret(1);
                e.replace_range(1..1, "X", cx);
            });
            assert_eq!(ed.read(cx).text().as_ref(), "aXbc");
            ed.update(cx, |e, cx| e.undo(cx));
            assert_eq!(ed.read(cx).text().as_ref(), "abc");
            ed.update(cx, |e, cx| e.redo(cx));
            assert_eq!(ed.read(cx).text().as_ref(), "aXbc");
        });
    }

    #[test]
    fn link_runs_render_in_blue_with_underline() {
        // C5 修复：WYSIWYG 链接不该用 `colors.accent`（浅色主题 = neutral-100 灰），
        // 而是单一源 `StyleParams.link`（明 #0969da / 暗 #79c0ff，与源码编辑器同蓝）。
        use super::runs_to_text_runs;
        use gpui::FontWeight;
        let base = gpui::font("Arial");
        let mono = gpui::font("Consolas");
        let text = "点 示例 你";
        // 渲染字节 [2..4) 是链接（“示例”，两个 3 字节汉字 → 6 字节）。
        let runs = vec![((2..8), crate::markdown::parser::RunKind::Link)];
        let accent = gpui::Hsla::default();
        let link: gpui::Hsla = gpui::rgb(0x0969da).into();
        let code_inline_fg = gpui::Hsla::default();
        let out = runs_to_text_runs(
            text,
            &runs,
            &base,
            gpui::Hsla::default(),
            &mono,
            accent,
            link,
            code_inline_fg,
        );
        let link_run = out
            .iter()
            .find(|r| {
                r.color == link && r.len == 6
            })
            .expect("链接段应为蓝色 TextRun（len=6 汉字）");
        assert_eq!(link_run.color, link, "链接文字色应等于 StyleParams.link 蓝");
        assert!(
            link_run.underline.is_some(),
            "链接应有下划线"
        );
        let ul = link_run.underline.as_ref().unwrap();
        assert_eq!(ul.color, Some(link), "下划线颜色与文字同蓝");
        // 非链接段保持预设底色（基色），不误伤。
        let _ = FontWeight::default();
        assert!(
            out.iter().any(|r| r.len == 2),
            "应有链接前导“点 ”（2 字节）段"
        );
    }

    #[gpui::test]
    fn link_theme_single_source_equals_editor_blue(cx: &mut TestAppContext) {
        // 回检测试（坑 D 单一源）：WyTheme.link ← StyleParams.link ← 明/暗主题蓝，
        // 且与源码编辑器 `editor/markdown.rs` 的 LINK_LIGHT/LINK_DARK 同值。
        use crate::markdown::view::style::StyleParams;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("none"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            let p = StyleParams::from_theme_with_font(cx, Some(f32::from(theme.font_size)));
            assert_eq!(theme.link, p.link, "WyTheme.link 抄自 StyleParams.link");
            // 浅色主题（main.rs 强制 Light）：应与编辑器 LINK_LIGHT #0969da 逐字节一致。
            let expect = if p.is_dark {
                gpui::rgb(crate::config::colors::LINK_DARK).into()
            } else {
                gpui::rgb(crate::config::colors::LINK_LIGHT).into()
            };
            assert_eq!(theme.link, expect, "链接蓝单一源 config::colors::LINK_*");
        });
    }

    #[gpui::test]
    fn inline_image_hit_is_found_and_within_display_range(cx: &mut TestAppContext) {
        // B1 修复：`![alt](src)` 的 `RunKind::Image` 只覆盖 alt——byte_map[alt] 是
        // **alt 起点**（不是 `!`）。旧实现在 alt 起点处查 `![` 永远找不到（tail 没有
        // `!`），且显示区间按整段源跨度算、越出 shaped 行 → `position_for_index` 落空。
        // 现在：src 起点从 `!` 反查，显示区间收在 alt 内。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "前文 ![内联蓝块](shot_blue.png) 后文\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Paragraph)
                .expect("应有段落行");
            assert!(
                !para.extra.inline_images.is_empty(),
                "行内图片应被命中（旧实现此处为空 → 永远不显示）"
            );
            let hit = &para.extra.inline_images[0];
            assert_eq!(hit.src.as_ref(), "shot_blue.png", "src 取自 `!(…)`");
            // 显示区间必须在 shaped 行长度内（否则 paint 的 position_for_index 返回 None）。
            let dl = para.wrapped.first().map(|w| w.len()).unwrap_or(0);
            assert!(
                hit.display_off + hit.len <= dl,
                "图片显示区间 {}..{} 须在 shaped 行 {} 内",
                hit.display_off,
                hit.display_off + hit.len,
                dl
            );
            assert_eq!(hit.len, "内联蓝块".len(), "显示区只覆盖 alt（不含标记）");
        });
    }

    #[test]
    fn math_block_env_and_line_breaks_render_cleanly() {
        // A1 修复：结构化 LaTeX 不再泄漏 `\begin{...}`/字面 `\\`。
        use super::{math_display_lines, math_to_unicode};
        // `\\` 行断 → 拆成两行（矩阵/对齐环境也如此）。
        let lines = math_display_lines("a = b \\\\ c = d");
        assert_eq!(lines.len(), 2, "`\\\\`（LaTeX 行断）应拆成两行");
        assert_eq!(lines[0], "a = b");
        assert_eq!(lines[1], "c = d");
        // `\begin{matrix}` 环境名剥掉、`&` 列分隔 → 空格。
        let m = math_to_unicode("\\begin{matrix}1 & 2 \\\\ 3 & 4\\end{matrix}");
        assert!(!m.contains("begin"), "不应泄漏 \\begin：{m}");
        assert!(!m.contains('\\'), "不应残留反斜杠：{m}");
        assert!(m.contains('1') && m.contains('2') && m.contains('3') && m.contains('4'));
        // 既有的普通公式仍工作。
        assert!(math_to_unicode("\\int_0^1 x^2 dx").contains('∫'));
    }

    #[test]
    fn math_to_segments_split_sup_sub_for_limit_layout() {
        // A1 回归：`∫_{-\infty}^{\infty}` 里 `∞` 没有 Unicode 上/下标码位，字符串版只能
        // 正文大小平排（`∫₋∞∞`）。卡片现在把顶层 `^{…}`/`_{…}` 拆成 Sup/Sub 段 →
        // 绘制时小字号 + 上下偏移，`∞` 也能正确显示成极限。
        use super::{math_to_segments, math_to_unicode, MathPos};
        let src = r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}";
        let segs = math_to_segments(src);
        assert!(
            segs.iter().any(|s| s.pos == MathPos::Sub && s.text.contains('∞')),
            "下标组应拆出 Sub 段（含 ∞）：{segs:?}"
        );
        assert!(
            segs.iter().any(|s| s.pos == MathPos::Sup && s.text.contains('∞')),
            "上标组应拆出 Sup 段（含 ∞）：{segs:?}"
        );
        // 平铺后 = 旧的扁平字符串（行内数学/兼容路径输出不变）。
        let flat: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(flat, math_to_unicode(src), "segments 平铺应与 math_to_unicode 一致");
        // `e^{-x^2}` 的指数也应在 Sup 段里。
        assert!(
            segs.iter().any(|s| s.pos == MathPos::Sup && s.text.contains('²')),
            "e^(-x^2) 的指数应在 Sup 段：{segs:?}"
        );
        // `\sum_{i=1}^{n}`：下标/上标各一段，且平铺含 ∑。
        let sum_segs = math_to_segments(r"\sum_{i=1}^{n} i");
        assert!(sum_segs.iter().any(|s| s.pos == MathPos::Sub && s.text.contains('ᵢ')));
        assert!(sum_segs.iter().any(|s| s.pos == MathPos::Sup && s.text.contains('ⁿ')));
    }

    #[test]
    fn inline_math_fit_h_keeps_readable_size_within_band() {
        // 行内公式「不能太小也不能溢出」的平衡（撑高行方案）：
        // - 矮公式按原生字号缩放（fs/FONT_SIZE），不做额外缩小；
        // - 高公式（22px 排 `\int_0^1` 约 70px）显示高 = 1× 字号缩放值，但封顶
        //   `INLINE_MATH_BAND × 行高`（撑高行容纳，而不是压扁到 0.95×行高）。
        use super::inline_math_fit_h;
        let fs = px(16.);
        let lh = px(19.);
        // 矮公式：按原生字号缩放（fs/FONT_SIZE×h），不被行高压扁。
        let short = inline_math_fit_h(fs, lh, 25.0);
        assert!((short - 16.0 / 22.0 * 25.0).abs() < 1e-3, "矮公式按字号缩放：{short}");
        assert!(short <= 19.0, "25px 高公式 × 字号缩放后仍在行高内：{short}");
        // 中等公式：字号缩放略超行高 → 所在行会小幅撑高（不再压扁）。
        let mid = inline_math_fit_h(fs, lh, 30.0);
        assert!((mid - 16.0 / 22.0 * 30.0).abs() < 1e-3, "仍按字号缩放：{mid}");
        // 高公式（`\int_0^1 x^2 dx` 22px 下 h≈70.5）：比旧 `0.95×行高`（≈18）大得多，
        // 且绝不超 `INLINE_MATH_BAND × 行高`（只压超高算子公式，其余保持字号缩放）。
        let tall = inline_math_fit_h(fs, lh, 70.5);
        assert!(tall > 19.0 * 0.95, "超高算子仍大于 0.95×行高：{tall}");
        assert!(
            tall <= 19.0 * crate::config::math::INLINE_MATH_BAND + 1e-3,
            "不超 INLINE_MATH_BAND×行高上限：{tall}"
        );
        // 0 高兜底 ≥1px（不除零）。
        assert!(inline_math_fit_h(fs, lh, 0.0) >= 1.0);
    }

    #[gpui::test]
    fn inline_math_double_click_enters_edit(cx: &mut TestAppContext) {
        // B2 双击编辑：双击 `$…$` spacer → 光标进公式（`$` 后 1 字节）→ shape 期
        // reveal 显示可编辑的原始 LaTeX（不再 splice 成 spacer）；光标移走恢复。
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "前置文字 $x^2+\\alpha$ 后置文字\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para = frame
                .rows
                .iter()
                .find(|r| !r.extra.inline_maths.is_empty())
                .expect("应有行内公式行");
            let im = para.extra.inline_maths[0].clone();
            assert!(!im.source.is_empty(), "spacer 应带绝对源区间");
            let w0 = para.wrapped.first().unwrap();
            let p0 = w0.position_for_index(im.display_off, para.lh).unwrap();
            let (src_line, x_off, top, lh) = (
                para.src_line,
                f32::from(para.x_off),
                f32::from(para.top),
                para.lh,
            );
            let x = f32::from(frame.padding) + x_off + f32::from(p0.x) + 2.0;
            let y = top + f32::from(lh) / 2.;
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 2,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            let caret = ed.read(cx).caret();
            assert!(
                caret > im.source.start && caret < im.source.end,
                "双击应把光标放进公式内：caret={caret} ∈ {:?}",
                im.source
            );
            // reveal：重建帧后该行公式不再 splice。
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para2 = frame2
                .rows
                .iter()
                .find(|r| r.src_lines.contains(&src_line))
                .unwrap();
            assert!(
                para2.extra.inline_maths.is_empty(),
                "光标在公式内应 reveal（不 splice spacer）"
            );
            // 光标移走 → 恢复 splice（公式感回来）。
            ed.update(cx, |e, _| e.set_caret(0));
            let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para3 = frame3
                .rows
                .iter()
                .find(|r| r.src_lines.contains(&src_line))
                .unwrap();
            assert!(
                !para3.extra.inline_maths.is_empty(),
                "光标离开公式应恢复 splice"
            );
        });
    }

    #[gpui::test]
    fn math_block_single_click_keeps_card_double_click_edits(cx: &mut TestAppContext) {
        // A1 交互语义修复：单击数学块卡片**不改光标**（不会因 caret 进块被 reveal 回
        // 源码字符串）；**双击**把光标放进块首 → 整块 reveal 源码进入编辑。
        use super::BlockCard;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "$$\n\\int_0^1 x^2 dx\n$$\n尾行\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            ed.update(cx, |e, _| {
                e.set_caret(text.find("尾").unwrap())
            }); // 光标在块外（末行）
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let x = f32::from(frame.padding) + 40.;
            let y = {
                let card = frame
                    .rows
                    .iter()
                    .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                    .expect("应有数学卡片");
                f32::from(card.top) + f32::from(card.height) / 2.
            };
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            // 单击：光标不动，卡片保持（不 reveal）。
            let ev1 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
            let caret_before = ed.read(cx).caret();
            assert_eq!(caret_before, text.find("尾").unwrap(), "单击卡片不得移动光标");
            let f1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                f1.rows
                    .iter()
                    .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "单击后卡片仍在（未 reveal）"
            );
            // 双击：光标进块首（源码行 0）→ reveal 源码（不再有卡片）。
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 2,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            let caret = ed.read(cx).caret();
            assert!(ed.read(cx).row_col(caret).0 == 0, "双击应把光标放进块首");
            let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                !f2.rows
                    .iter()
                    .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "双击后 reveal 源码（无卡片）"
            );
        });
    }

    #[gpui::test]
    fn math_block_single_line_double_click_reveals_immediately(cx: &mut TestAppContext) {
        // 单行 `$$…$$` 数学块（`$$\sum…$$` 独占一行）：双击必须**直接**还原成源码
        // 字符串，不得先变成行内小公式再双击一次。根因：光标放在行首 0 列时
        // `splice_math_text` 的 `c > span.start` 不成立 → 仍被 splice 成小号行内公式；
        // 现在放 `$$` 行内容内 1 字节 → 块 reveal 与行内 splice reveal 同时满足。
        use super::BlockCard;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "$$\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}$$\n尾行\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            ed.update(cx, |e, _| e.set_caret(text.find("尾").unwrap())); // 光标在块外
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let card = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .expect("单行 `$$…$$` 也应是数学卡片");
            let (x, y) = (
                f32::from(frame.padding) + 40.,
                f32::from(card.top) + f32::from(card.height) / 2.,
            );
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 2,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            // 双击一次：光标在行内（>0 列），卡片消失、不再 splice（直接是字符串）。
            let caret = ed.read(cx).caret();
            let (_, col) = ed.read(cx).row_col(caret);
            assert!(col > 0, "双击后光标应进入块行内部，col={col}");
            let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                !f2.rows
                    .iter()
                    .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "双击后应无卡片（reveal 源码）"
            );
            let row0 = f2.rows.iter().find(|r| r.src_lines.contains(&0)).unwrap();
            assert!(
                row0.extra.inline_maths.is_empty(),
                "单行块双击应直接还原字符串（不得再 splice 成行内公式）"
            );
            // 光标移出块（到尾行）后恢复卡片渲染。
            ed.update(cx, |e, _| e.set_caret(text.find("尾").unwrap()));
            let f3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                f3.rows.iter().any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "光标移出后应恢复数学卡片（公式感）"
            );
        });
    }

    #[gpui::test]
    fn sync_math_claims_block_and_inline_slots(cx: &mut TestAppContext) {
        // 数学排版集成（参考 zorite ensure_content_math）：内容含 `$$…$$` 块 + `$…$`
        // 行内公式 → `sync_math` 按「去定界符的 LaTeX」为键把两块都占为 Loading（真正
        // 位图由后台 executor 光栅；CPU 级光栅正确性已由 math_typeset 单测覆盖）。
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "$$\n\\int_0^1 x^2 dx\n$$\n内联 $e^{i\\pi}+1=0$ 结束";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, ecx| e.sync_math(ecx));
        });
        vcx.update(|_window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let slots = ed.read(cx).math_slots();
            let b = slots.borrow();
            assert!(
                b.contains_key(r"\int_0^1 x^2 dx"),
                "块公式应占位（Loading）：keys={:?}",
                b.keys()
            );
            assert!(
                b.contains_key(super::super::inline_math_key(r"e^{i\pi}+1=0").as_ref()),
                "行内公式应占位（Loading，带 inline: 前缀键）：keys={:?}",
                b.keys()
            );
        });
    }

    #[gpui::test]
    fn math_card_height_uses_typeset_size_when_slot_ready(cx: &mut TestAppContext) {
        // 数学排版切 RaTeX 后：公式就绪 → 卡片高按排版位图逻辑高（`20 + h`），
        // 不再按 Unicode 行数（`20 + n×22`）——上限的分式/根号终于有足够高度。
        use crate::markdown::markdown_syntax;
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "$$\na \\\\ b\n$$"; // 两行 → Unicode 回退高 = 20+44=64
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut App| {
            // 一个 40×24 的假位图逻辑尺寸 20×12（RenderImage 是纯数据构造，无需 GPU）。
            let buf = image::RgbaImage::from_raw(40, 24, vec![0u8; 40 * 24 * 4]).unwrap();
            let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
            let latex = "a \\\\ b"; // math_regions 的 source（fenced 去 `$$` 后）
            let ed = host.read(cx).ed.clone();
            ed.read(cx).math_slots().borrow_mut().insert(
                latex.to_string().into(),
                super::super::MathSlot::Ready(img, 20., 12.),
            );
            let e = ed.read(cx);
            let reg = markdown_syntax::math_regions(&e.content())
                .into_iter()
                .next()
                .expect("应扫到一块公式");
            assert_eq!(reg.source, "a \\\\ b");
            // 就绪后高 = 20 + max(h,16) = 20+16 = 36（而非 Unicode 两行的 64）。
            let h = super::math_card_height(&e, &reg);
            assert_eq!(f32::from(h), 36.0, "就绪应占位排版高度，而非 Unicode 行高");
            // 未就绪（新公式）→ 回退 Unicode 行高 = 20 + 2×22 = 64。
            e.math_slots().borrow_mut().clear();
            let h2 = super::math_card_height(&e, &reg);
            assert_eq!(f32::from(h2), 64.0, "未就绪应回退 Unicode 逐行高");
        });
    }

    #[gpui::test]
    fn inline_math_grown_row_caret_and_click_stay_aligned(cx: &mut TestAppContext) {
        // 用户实测：行内公式把行撑高后，点击该行「闪烁光标位置不对」——光标几何与
        // 点击映射必须都按撑高下移量（text_row_shift）定位，否则光标落在块顶空白处。
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "a $\\int_0^1 x^2 dx$ b";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 注入就绪位图（假数据，纯 CPU，无需 GPU）：40×70 逻辑 px（≈22px 排版的
            // `\int_0^1` 高）→ 行内 fit = min(fs/22×70, 2.2×行高) ≈ 41.8px > 行高 19。
            let buf = image::RgbaImage::from_raw(80, 140, vec![0u8; 80 * 140 * 4]).unwrap();
            let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
            ed.read(cx).math_slots().borrow_mut().insert(
                super::super::inline_math_key("\\int_0^1 x^2 dx"),
                super::super::MathSlot::Ready(img, 40., 70.),
            );
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let row = &frame.rows[0];
            let lh = f32::from(row.lh);
            let band = f32::from(row.height);
            assert!(band > lh + 1.0, "行内公式应撑高行：band={band} > lh={lh}");
            // 文本带 = [shift, shift+lh]（撑高的下移量，与 paint/公式同源）。
            let w0 = row.wrapped.first().unwrap();
            let orig = lh * (w0.wrap_boundaries().len() + 1) as f32;
            let shift = ((band - orig) / 2.).max(0.);
            let pad = f32::from(frame.padding);
            // 点击文本带中部（正文 "a … b" 区域）→ 源偏移应落在行内。
            let y = shift + lh / 2.;
            let x = pad + 30.;
            let off = super::source_offset_at(ed.read(cx), &frame, point(px(x), px(y)));
            let len = ed.read(cx).content().len() as f32;
            assert!(off as f32 <= len, "点击应解析到行内源偏移：off={off}");
            ed.update(cx, |e, _| e.set_caret(off));
            let (on, _caretx, carety) = caret_geometry(ed.read(cx), &frame);
            assert!(on, "该行可放光标");
            assert!(
                f32::from(carety) >= shift - 1.0 && f32::from(carety) <= shift + lh + 1.0,
                "光标 y {} 应在撑高后的文本带 [{shift}, {}] 内，不能回到块顶空白",
                f32::from(carety),
                shift + lh
            );
        });
    }

    #[gpui::test]
    fn keys_ctrl_a_c_v_select_copy_paste_round_trip(cx: &mut TestAppContext) {
        // D1/Ctrl 修复：Windows 上 Ctrl 报 `modifiers.control`，而 `cmd-*` 绑定是
        // `modifiers.platform`（= Win 键）→ 旧实现 Ctrl+C/V/A 全部失效。现在与
        // `live.rs` 一样显式补 `ctrl-a/c/v` 绑定，经真实键位派发验证整条链路。
        use gpui::{App, Keystroke};
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("hello world"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        // 安装 WYSIWYG 键位（与 main.rs 的 bind_keys 装配一致）。
        vcx.update(|_w, cx: &mut App| crate::markdown::wysiwyg::bind_keys(cx));
        // 聚焦编辑器 → 键位上下文生效。
        vcx.update(|window, cx| {
            let fh = host.read(cx).ed.read(cx).focus_handle();
            window.focus(&fh, cx);
        });
        let wh = vcx.windows().first().copied().expect("窗口句柄");

        // Ctrl+A 全选。
        vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-a").unwrap());
        vcx.update(|_w, cx| {
            assert_eq!(
                host.read(cx).ed.read(cx).selection(),
                Some(0.."hello world".len()),
                "Ctrl+A 应全选"
            );
        });
        // Ctrl+C 复制 → 剪贴板。
        vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-c").unwrap());
        let clip: Option<String> = vcx.update(|_w, cx: &mut App| {
            cx.read_from_clipboard().map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(clip.as_deref(), Some("hello world"), "Ctrl+C 应写入剪贴板");
        // 光标置开头后 Ctrl+V 粘贴（选区替换为插入）。
        vcx.update(|_w, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_caret(0);
            });
        });
        vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
        vcx.update(|_w, cx| {
            assert_eq!(
                host.read(cx).ed.read(cx).text().as_ref(),
                "hello worldhello world",
                "Ctrl+V 应在光标处粘贴剪贴板内容"
            );
        });
    }
}

