//! 共享行整形核心（B5 蓝图 Phase 1 里程碑 1，纯新增）。
//!
//! 把 [`DocRow`]（唯一行数据）+ 主题 + 宽度 → 一组**横向分片**（视觉行粒度的几何：
//! 宽/高/左缩进/行高系数，以及渲染身份：显示文本/runs/code_ranges/byte_map 段），
//! 作为 Phase 2「两引擎共用单一几何真值」的第一个家。
//!
//! **本里程碑不改任何现有调用方**：WYSIWYG `wysiwyg::shape::shape_docrow` 与预览
//! `view::row::render_*` 原样不动；本模块是新增面。落地方式 = 独立实现 + 逐值对拍：
//! - 与 WYSIWYG：`Σ shard.h == shape_docrow(...).height`（`tests::parity_*` 钉死，逐块）；
//! - 与预览：`shared_shard_height_matches_preview_row`（真实渲染 harness 实测对照，
//!   放在 `markdown::view::tests`——见 `view/tests/layout_shared.rs`）。
//!
//! ## 语义口径（写死，供 Phase 2 对齐）
//! - **高度** = WYSIWYG `shape/*` 的确定性公式（行高系数 × 折行段数 / 块公式），
//!   **不含** 预览元素树的盒属性（文本行 `py_1` 垂直内衬、`para_gap`、gpui 自然行距、
//!   gutter 宽度、图片题注模型、表格滚动条预留、空行/分隔行高度）——这些 A/B 已知
//!   系统性差在对照测试里逐类断言并留档（见测试注释）。
//! - **折行宽** = `avail − gutter(kind)`（`avail` 由调用方喂入；纯段落/标题 gutter=0）。
//!   与 WYSIWYG `shape_text_row_decorated` 同式（`帧宽 − 2×边距 − gutter`）；与预览对照时，
//!   测试把预览**实际内容宽**（帧宽 − 2×宿主侧边 gap − 2×行内边距）喂给本模块（见
//!   `view/tests/layout_shared.rs`），保证同一折行语义。
//! - **行内数学 `$…$`**：splice 是 B 独有（预览原样渲染源码），Phase 1 未复制——本模块
//!   对「剥标记后的 `DocRow.text`」原样折行；对照样例只用不改变折行的**中性公式**
//!   并留档边界。
//! - **代码**：nowrap 每源码行 1 视觉行（语法高亮只影响色，不影响几何，未复制）。
//!
//! 不引入新依赖；依赖方向 `layout → (doc/ir/parse/wysiwyg::WyTheme/view::expand…)`。

use std::ops::Range;

use gpui::{FontWeight, Hsla, Pixels, SharedString, TextRun, WindowTextSystem};

use config::markdown::{
    CONTENT_EDGE_PAD_X_PX, IMAGE_CAPTION_GAP_PX, IMAGE_MAX_W, IMAGE_ROW_DEFAULT_H,
    TABLE_SCROLLBAR_RESERVE_PX,
};
use crate::markdown::doc::{row_kind, DocPayload, DocRow};
use crate::markdown::ir::IrKind;
use crate::markdown::parse::{Row, RowKind, RunKind, TableAlign};
use crate::markdown::wysiwyg::WyTheme;

/// 把背景区间向左右扩展吞掉紧邻空白：留给圆角背景的左右视觉间距。
/// （高亮区间即"代码文本 + 两侧各一个空格"，等宽 overrides 只覆盖代码本体，
/// 空格保持正文字宽 → 圆角灰底略宽于字形，左右有呼吸空间。）
/// 预览与 WYSIWYG 的行内代码 pill 共用同一扩展规则（单一行为源；M5-B5 P4-2 由
/// `markdown::view::row` 迁入本层——`view` 预览引擎已删）。
pub(crate) fn expand_bg_over_whitespace(text: &str, range: &Range<usize>) -> Range<usize> {
    let mut start = range.start;
    let mut end = range.end;
    if start > 0 {
        let prev = text[..start].chars().next_back();
        if let Some(c) = prev
            && c.is_whitespace()
        {
            start -= c.len_utf8();
        }
    }
    if end < text.len() {
        let next = text[end..].chars().next();
        if let Some(c) = next
            && c.is_whitespace()
        {
            end += c.len_utf8();
        }
    }
    start..end
}

/// 标题行左缘 gutter（折叠箭头预留）——与 `wysiwyg::element::HEADING_GUTTER_W` 同源常量。
const HEADING_GUTTER_W: f32 = 20.;
/// 引述正文左缘缩进（非警报）——与 `wysiwyg::shape::QUOTE_TEXT_INDENT_PX` 同源常量。
const QUOTE_TEXT_INDENT_PX: f32 = 15.;
/// 代码行文本再内缩（背景带内衬）——与 `shape_code_row` 的 `x_off` 同源常量。
const CODE_TEXT_PAD_X_PX: f32 = CONTENT_EDGE_PAD_X_PX;
/// 标题块底部额外留白（`shape_text_row` 用 `text_style_for` 的 `pad`）。
const HEADING_BLOCK_PAD_PX: f32 = 8.;
/// 表格单元格左右内距合计（px，左右各 10）——列宽分配 = 字形宽 + 此值，下限 48。
/// 与 `wysiwyg::shape::shape_table_row` / 预览 `table.rs` 同源。
const CELL_PAD_X2_PX: f32 = 20.;

/// 一个布局分片（横向条带）：一个视觉行的几何 + 渲染身份。
///
/// 粒度 = gpui `shape_text` 的一个 `WrappedLine`（文本行）；代码行 1 源码行 1 片
/// （外加首/末空白带各 1 片）；表格/图片等块级行各 1 片（图片题注仍按折行拆片）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Shard {
    /// 视觉行在整块内的 0 基序号。
    pub line_ix: usize,
    /// 该条带高（px）：B 确定性语义（行高系数 × 折行段数 / 块公式）。
    pub h: f32,
    /// 该条带自然宽（px）（可 < 可用宽；空条带 = 0）。
    pub w: f32,
    /// 该条带在内容盒内的额外左缩进（gutter/x_off，与 `shape/*` 同源常量）。
    pub x_off: f32,
    /// 该条带的基准行高系数（正文 `para_line` / 标题 `heading_line` / 代码 `code_line`）。
    pub lh: f32,
    /// 该条带显示文本（`shape_text` 的这一个 WrappedLine）。
    pub text: SharedString,
    /// 行内样式 runs（字节区间相对**本条带文本**）。
    pub runs: Vec<(Range<usize>, RunKind)>,
    /// 行内代码（反引号）区间（相对本条带文本；已扩展空白）。
    pub code_ranges: Vec<Range<usize>>,
    /// 该条带在**整块** `byte_map` 中的显示字节区间（`byte_map[b]` 为源字节）。
    /// 代码块为块级映射（`multi_offsets` 语义）；空条带为 `0..0`。
    pub byte_map: Range<usize>,
    /// 表格网格几何（仅 [`IrKind::Table`] 块的首片；其余 None）。P3-1c-2：表格行
    /// 内容切 shard——列宽/单元格高/表头数据都在这里，渲染与量程共用同一几何。
    pub table: Option<ShardTable>,
    /// 图片盒几何（仅 [`IrKind::Image`] 块的首片；其余 None）。P3-1c-2：图片行
    /// 内容切 shard——盒宽/盒高（按固尺寸比例）+ 源，渲染据此画盒。
    pub image: Option<ShardImage>,
}

impl Shard {
    #[allow(clippy::too_many_arguments)] // 几何数据载体的纯构造
    fn new(
        line_ix: usize,
        h: f32,
        w: f32,
        x_off: f32,
        lh: f32,
        text: SharedString,
        runs: Vec<(Range<usize>, RunKind)>,
        code_ranges: Vec<Range<usize>>,
        byte_map: Range<usize>,
    ) -> Self {
        Shard {
            line_ix,
            h,
            w,
            x_off,
            lh,
            text,
            runs,
            code_ranges,
            byte_map,
            table: None,
            image: None,
        }
    }

    fn with_table(mut self, t: ShardTable) -> Self {
        self.table = Some(t);
        self
    }

    fn with_image(mut self, i: ShardImage) -> Self {
        self.image = Some(i);
        self
    }
}

/// 表格网格的**单源几何**（`shape_table_shard` 产出；渲染/量程共用，R2）：列宽
/// 分配 / 单元格高 / 表头数据文本 / 对齐。列宽与 `wysiwyg::shape::shape_table_row`
/// **同式**：逐格字形实测宽 + 左右各 10px 内距（+20）、下限 48px——预览表格与
/// WYSIWYG 表格列宽同一来源；单元格高 = `para_line + 12`（同 `table_row_h`）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ShardTable {
    /// 每列宽（px）。
    pub widths: Vec<f32>,
    /// 网格总宽（px，= `Σ widths`）。
    pub total_w: f32,
    /// 单元格高（px；表头/数据行同高）。
    pub row_h: f32,
    /// 表头单元格文本（列数 = `widths.len()`）。
    pub header: Vec<SharedString>,
    /// 数据行单元格文本（每行已补齐到列数）。
    pub rows: Vec<Vec<SharedString>>,
    /// 每列对齐（已补齐到列数）。
    pub aligns: Vec<TableAlign>,
}

/// 独立图片行的**单源盒几何**（`shape_image_shards` 产出；渲染/量程共用）：图片盒
/// 宽/高 + 源。盒高按固尺寸比例换算（`box_w * ih / iw`）→ 高度随可用宽缩放与现状
/// （`aspect_ratio`）一致；未加载/无尺寸用占位高 [`IMAGE_ROW_DEFAULT_H`]。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ShardImage {
    /// 图片盒宽（px，= `min(avail, IMAGE_MAX_W)`）。
    pub box_w: f32,
    /// 图片盒高（px，= `box_w * ih/iw` 或占位高）。
    pub box_h: f32,
    /// 图片源（渲染 `ImageSource` / 水平布局用）。
    pub src: SharedString,
}

/// 把 [`DocRow`] 整形为横向分片序列：高度语义 = `wysiwyg::shape::shape_docrow`
/// 的确定性几何（`Σ shard.h` 与其 `ShapedRow.height` 逐块对拍由测试钉死）。
///
/// - `ts`：字体整形上下文（`WindowTextSystem`——`Window::text_system()` 即它；
///   也可 `WindowTextSystem::new(App::text_system())` 构造，测试无需真实窗口）。
/// - `theme`：`WyTheme`（由预览 `StyleParams` 单源派生，两引擎同口径）。
/// - `avail`：内容可用宽（px）——调用方与预览/编辑区喂**同一源**（= 帧宽 − 2×边距）。
/// - `max_w`：块级最大渲染宽（px）——代码/表格的水平范围参照（Phase 1 只影响 w 记录）。
///
/// 签名相对蓝图 `shape_docrow_shard(dr, theme, avail, max_w)` 多一个 `ts`：整形必须
/// 访问字体整形上下文（px 宽/折行），显式传入可中性喂两引擎（不背 editor/window）。
///
/// 高度占位（不给图固有尺寸）时图片行用 [`IMAGE_ROW_DEFAULT_H`] 占位高；需要「量程与
/// 渲染同源」的调用方（预览量程/渲染）走 [`shape_docrow_shard_with_image`] 喂真实固
/// 尺寸——图片盒高随之等比缩放（P3-1c-2）。
pub(crate) fn shape_docrow_shard(
    ts: &WindowTextSystem,
    dr: &DocRow,
    theme: &WyTheme,
    avail: f32,
    max_w: f32,
) -> Vec<Shard> {
    shape_docrow_shard_with_image(ts, dr, theme, avail, max_w, None)
}

/// [`shape_docrow_shard`] 的图固有尺寸版本：`img_size = Some((w, h))` 只对
/// [`IrKind::Image`] 行生效（图片盒高按比例换算）；其余行型与无 size 完全一致。
pub(crate) fn shape_docrow_shard_with_image(
    ts: &WindowTextSystem,
    dr: &DocRow,
    theme: &WyTheme,
    avail: f32,
    max_w: f32,
    img_size: Option<(u32, u32)>,
) -> Vec<Shard> {
    match dr.kind {
        IrKind::Code => shape_code_shards(ts, dr, theme, max_w),
        IrKind::Table => shape_table_shard(ts, dr, theme, avail),
        IrKind::Image => shape_image_shards(ts, dr, theme, avail, img_size),
        _ => shape_text_shards(ts, dr, theme, avail),
    }
}

/// 文本行（段落/标题/引述/列表/空行/分隔线）的行样式：字号/行高/加粗/底部留白。
/// 与 `wysiwyg::shape::text_style_for` 同口径（只取影响几何/样式的字段，色值不影响宽度）。
fn text_style_for(kind: IrKind, theme: &WyTheme) -> (Pixels, Pixels, bool, f32) {
    match kind {
        IrKind::Heading(n) => (
            theme.heading_fs(n),
            theme.heading_line(n),
            true,
            HEADING_BLOCK_PAD_PX,
        ),
        _ => (theme.font_size, theme.para_line(), false, 0.),
    }
}

/// 该块类型的左缘 gutter（px）：同时决定「折行宽扣除」与「shard.x_off」。
/// 与 `wysiwyg::shape::shape_text_row_decorated` 的 gutter 计算同口径（不含
/// 需要 source 全文识别的警报/任务/有序号装饰——Phase 2 迁移时再细化）。
fn text_gutter(kind: IrKind) -> f32 {
    match kind {
        IrKind::Heading(_) => HEADING_GUTTER_W,
        IrKind::Quote => QUOTE_TEXT_INDENT_PX,
        IrKind::Bullet => 18.,
        _ => 0.,
    }
}

/// 单一 run 构造核心（[`runs_to_text_runs`]/[`font_runs`] 的公共基）：
/// - 逐 run 守卫（字符边界吸收 / 空区间丢弃 / 未覆盖区间补默认 / 尾段补齐）为**同一条
///   `wysiwyg::shape::runs_to_text_runs`** 逐行复刻；
/// - `base`：run 之外的默认字体（当前 run 字体由 `deco` 派生）；未覆盖区间（两个 run
///   之间的空隙 / 尾段）一律 `base`、无装饰——与两份旧实现逐字段等值。
/// - `deco`：`RunKind → TextRunDeco`（字体 + 可选色/划线/下划线）。`wysiwyg::shape`
///   返回完整装饰；`layout::font_runs` 只取字形相关维度（加粗/斜体/等宽），色/划线恒
///   `None`（宽度/折行由字形决定，色不影响几何）。
pub(crate) fn text_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    deco: impl Fn(RunKind) -> TextRunDeco,
) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len().saturating_add(1).max(1));
    let mut i = 0usize;
    for (range, kind) in runs {
        let s = crate::util::char_align_byte(text, range.start).max(i);
        let e = crate::util::char_align_byte(text, range.end);
        if e <= s {
            continue;
        }
        if s > i {
            out.push(TextRun {
                len: s - i,
                font: base.clone(),
                ..Default::default()
            });
        }
        let d = deco(*kind);
        let mut tr = TextRun {
            len: e - s,
            font: d.font,
            ..Default::default()
        };
        if let Some(c) = d.color {
            tr.color = c;
        }
        if let Some(st) = d.strikethrough {
            tr.strikethrough = Some(st);
        }
        if let Some(ul) = d.underline {
            tr.underline = Some(ul);
        }
        out.push(tr);
        i = e;
    }
    if i < text.len() {
        out.push(TextRun {
            len: text.len() - i,
            font: base.clone(),
            ..Default::default()
        });
    }
    out
}

/// [`text_runs`] 的逐 run 装饰：字体 + 颜色/划线/下划线（全 `Option`，可只取字形）。
#[derive(Clone)]
pub(crate) struct TextRunDeco {
    pub font: gpui::Font,
    pub color: Option<Hsla>,
    pub strikethrough: Option<gpui::StrikethroughStyle>,
    pub underline: Option<gpui::UnderlineStyle>,
}

/// 纯字体决策（加粗/斜体/行内代码等宽；不复制色）——`layout::shape_text_shards` 用。
/// 与 `wysiwyg::shape::runs_to_text_runs` 的字体分支同规则（两处字体断言同一）。
/// `Font` 按值传入（代价 = 浅克隆；闭包不捕获引用 → 无寿命歧义）。
fn kind_font_deco(base: gpui::Font, mono: gpui::Font) -> impl Fn(RunKind) -> TextRunDeco {
    move |kind| {
        let mut f = base.clone();
        match kind {
            RunKind::Bold => f.weight = FontWeight::BOLD,
            RunKind::Italic => f.style = gpui::FontStyle::Italic,
            RunKind::InlineCode => f = mono.clone(),
            // Strikethrough/Link/Image 不影响字形宽度 → 沿用基字。
            _ => {}
        }
        TextRunDeco {
            font: f,
            color: None,
            strikethrough: None,
            underline: None,
        }
    }
}

/// `DocRow.runs` → gpui `TextRun`：只保留**影响宽度/折行**的字体维度
/// （加粗/斜体/行内代码等宽），镜像 `wysiwyg::shape::runs_to_text_runs` 的
/// 对齐/间隙/尾段逻辑（字符边界吸收、未覆盖区间补默认、尾段补齐）。
fn font_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    mono: &gpui::Font,
) -> Vec<TextRun> {
    text_runs(text, runs, base, kind_font_deco(base.clone(), mono.clone()))
}

/// 按显示字节区间裁剪 runs / code_ranges（相对本条带文本的起点重定基）。
fn slice_runs(
    runs: &[(Range<usize>, RunKind)],
    range: &Range<usize>,
) -> Vec<(Range<usize>, RunKind)> {
    runs.iter()
        .filter_map(|(rg, k)| {
            let s = rg.start.max(range.start);
            let e = rg.end.min(range.end);
            (s < e).then(|| (s - range.start..e - range.start, *k))
        })
        .collect()
}

fn slice_ranges(ranges: &[Range<usize>], range: &Range<usize>) -> Vec<Range<usize>> {
    ranges
        .iter()
        .filter_map(|rg| {
            let s = rg.start.max(range.start);
            let e = rg.end.min(range.end);
            (s < e).then(|| s - range.start..e - range.start)
        })
        .collect()
}

/// 文本块：按 `avail − gutter` 折行，每个 `WrappedLine` 一片。
/// 高度与 `shape_text_row` 同式（`lh × (折行段数+1)`，标题块底留白折进首片），
/// 整块高度 `Σ shard.h` 与其 `ShapedRow.height` 一致（对拍测试钉死）。
fn shape_text_shards(ts: &WindowTextSystem, dr: &DocRow, theme: &WyTheme, avail: f32) -> Vec<Shard> {
    let kind = dr.kind;
    let (fs, lh, bold, pad) = text_style_for(kind, theme);
    let x_off = text_gutter(kind);
    let wrap_w = (avail - x_off).max(1.);
    let mut base = theme.font.clone();
    if bold {
        base.weight = gpui::FontWeight::BOLD;
    }
    let font_runs = font_runs(dr.text.as_ref(), &dr.runs, &base, &theme.mono);
    // 行内代码区间（已扩展空白）——与 `shape_text_row` 同源规则。
    let block_code: Vec<Range<usize>> = dr
        .runs
        .iter()
        .filter(|(_, k)| *k == RunKind::InlineCode)
        .map(|(rg, _)| expand_bg_over_whitespace(dr.text.as_ref(), rg))
        .collect();

    let wrapped = ts
        .shape_text(dr.text.clone(), fs, &font_runs, Some(gpui::px(wrap_w)), None)
        .map(|ls| ls.into_vec())
        .unwrap_or_default();

    if wrapped.is_empty() {
        // 空行/分隔线：B 走文本路径的兜底——一个 `lh` 片。
        return vec![Shard::new(
            0,
            f32::from(lh) + pad,
            0.,
            x_off,
            f32::from(lh),
            SharedString::default(),
            Vec::new(),
            Vec::new(),
            0..0,
        )];
    }

    let mut out = Vec::with_capacity(wrapped.len());
    let mut start = 0usize;
    for (i, wl) in wrapped.iter().enumerate() {
        let byte_end = start + wl.text.len();
        let seg_h = f32::from(lh) * (wl.wrap_boundaries().len() + 1) as f32;
        let mut sh = Shard::new(
            i,
            seg_h,
            wl.width().as_f32(),
            x_off,
            f32::from(lh),
            wl.text.clone(),
            slice_runs(&dr.runs, &(start..byte_end)),
            slice_ranges(&block_code, &(start..byte_end)),
            start..byte_end.min(dr.byte_map.len().saturating_sub(1)),
        );
        // 标题块底留白折进首片（`Σ shard.h` 恒等于 `shape` 的 `pad + 行高和`）。
        if i == 0 && pad > 0. {
            sh.h += pad;
        }
        out.push(sh);
        start = byte_end;
    }
    out
}

/// 代码块：首/末空白带 + 每源码行 1 片（nowrap，恒 1 视觉行）。语法高亮只影响色，
/// 不影响几何 → 未复制（Phase 2 迁移时再接）。整块高 `(行数+2)×code_line` == `shape_code_row`。
fn shape_code_shards(
    ts: &WindowTextSystem,
    dr: &DocRow,
    theme: &WyTheme,
    _max_w: f32,
) -> Vec<Shard> {
    let code_lines: Vec<SharedString> = match &dr.payload {
        DocPayload::Code { code_lines, .. } => code_lines.clone(),
        _ => Vec::new(),
    };
    let fs = theme.code_size();
    let lh = f32::from(theme.code_line());
    let mono = theme.mono.clone();
    let x_off = CODE_TEXT_PAD_X_PX;
    let mut out = Vec::with_capacity(code_lines.len() + 2);
    let mut line_ix = 0usize;
    // 首空白带（与预览 code_blank 同宽语义）。
    out.push(Shard::new(line_ix, lh, 0., x_off, lh, SharedString::default(), Vec::new(), Vec::new(), 0..0));
    // 每源码行 1 片；byte_map = 块级渲染字节区间（代码行以 `\n` 连接）。
    let mut base = 0usize;
    for cl in &code_lines {
        line_ix += 1;
        // nowrap：单 WrappedLine 的自然宽即该行宽。
        let w = ts
            .shape_text(cl.clone(), fs, &[TextRun { len: cl.len(), font: mono.clone(), ..Default::default() }], None, None)
            .map(|ls| ls.first().map(|wl| wl.width().as_f32()).unwrap_or(0.))
            .unwrap_or(0.);
        out.push(Shard::new(
            line_ix,
            lh,
            w,
            x_off,
            lh,
            cl.clone(),
            Vec::new(),
            Vec::new(),
            base..base + cl.len().min(dr.byte_map.len().saturating_sub(1)),
        ));
        base += cl.len() + 1;
    }
    // 末空白带。
    out.push(Shard::new(line_ix + 1, lh, 0., x_off, lh, SharedString::default(), Vec::new(), Vec::new(), 0..0));
    out
}

/// 表格块：单片，高 = 网格高 + 底部滚动条预留带——与 `shape_table_row` 同式。
/// P3-1c-2 起承载**真实单元格绘制几何**（[`ShardTable`]）：列宽按逐格字形实测宽 +
/// 20 / 下限 48（与 `wysiwyg::shape::shape_table_row` 同式）、单元格高 = `para_line + 12`、
/// 表头/数据文本与对齐——渲染直接按它画网格（删旧 `render_table_row` 的内容路径），
/// 量程 `shard.h` 与其渲染（网格高 + 预留带）同源。
fn shape_table_shard(ts: &WindowTextSystem, dr: &DocRow, theme: &WyTheme, _avail: f32) -> Vec<Shard> {
    let (header, rows, aligns) = match &dr.payload {
        DocPayload::Table(t) => (t.header.clone(), t.rows.clone(), t.aligns.clone()),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };
    let ncols = header.len().max(1);
    // 列宽 = 逐格字形实测宽 + 左右内距（+20）、下限 48 —— 与 `shape_table_row` 同式
    // （同一 `shape_line` 整形上下文：`theme.font` / `theme.font_size`）。
    let mut widths: Vec<f32> = (0..ncols).map(|_| 48.).collect();
    let shape_len = |ts: &WindowTextSystem, c: &SharedString| -> f32 {
        let run = [TextRun {
            len: c.len(),
            font: theme.font.clone(),
            ..Default::default()
        }];
        ts.shape_line(c.clone(), theme.font_size, &run, None)
            .width()
            .as_f32()
    };
    for (ci, c) in header.iter().enumerate() {
        widths[ci] = widths[ci].max(shape_len(ts, c) + CELL_PAD_X2_PX);
    }
    // 数据行同样测宽（含稀疏行补列后的空串——宽 0，`max` 不变）。
    let rows2: Vec<Vec<SharedString>> = rows
        .iter()
        .map(|r| {
            let mut v = r.clone();
            v.resize(ncols, SharedString::default());
            v
        })
        .collect();
    for row in &rows2 {
        for (ci, c) in row.iter().enumerate() {
            widths[ci] = widths[ci].max(shape_len(ts, c) + CELL_PAD_X2_PX);
        }
    }
    let total_w: f32 = widths.iter().sum();
    let mut aligns2 = aligns.clone();
    aligns2.resize(ncols, TableAlign::Left);
    let n_rows = rows2.len();
    let row_h = f32::from(theme.para_line()) + 12.;
    let h = (1. + n_rows as f32) * row_h + n_rows as f32 + 2. + TABLE_SCROLLBAR_RESERVE_PX;
    vec![
        Shard::new(0, h, total_w, 0., row_h, SharedString::default(), Vec::new(), Vec::new(), 0..0)
            .with_table(ShardTable {
                widths,
                total_w,
                row_h,
                header,
                rows: rows2,
                aligns: aligns2,
            }),
    ]
}

/// 独立图片行：图片盒片（按固尺寸等比换算的真实盒高 + 题注间隙）+ 题注折行片。
/// 与 `shape_image_row` 同式（占位口径：未给固尺寸 / 未加载图用 `IMAGE_ROW_DEFAULT_H`）。
/// P3-1c-2 起盒几何（[`ShardImage`]）随 shard 承载——渲染据此画盒（删 `render_image_row`），
/// 给尺寸后盒高随可用宽等比缩放（量程与渲染同源，R2）。
fn shape_image_shards(
    ts: &WindowTextSystem,
    dr: &DocRow,
    theme: &WyTheme,
    avail: f32,
    img_size: Option<(u32, u32)>,
) -> Vec<Shard> {
    let alt = dr.text.clone();
    let src = match &dr.payload {
        DocPayload::Image { src } => src.clone(),
        _ => SharedString::default(),
    };
    let box_w = avail.min(IMAGE_MAX_W);
    let img_h = match img_size {
        Some((iw, ih)) if iw > 0 && ih > 0 => (box_w * ih as f32 / iw as f32).max(1.),
        _ => IMAGE_ROW_DEFAULT_H,
    };
    let gap = IMAGE_CAPTION_GAP_PX;
    // 题注（muted、正文行高）按内容宽折行——与 `shape_image_row` 同式。
    let cap_lh = f32::from(theme.para_line());
    let cap_run = [TextRun {
        len: alt.len(),
        font: theme.font.clone(),
        ..Default::default()
    }];
    let cap_wrapped = ts
        .shape_text(alt, theme.font_size, &cap_run, Some(gpui::px(avail)), None)
        .map(|ls| ls.into_vec())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(cap_wrapped.len().max(1) + 1);
    // 图片盒片（真实/占位高 + 题注间隙——`shape_image_row` 折进同一高度行）。
    out.push(
        Shard::new(0, img_h + gap, box_w, 0., cap_lh, SharedString::default(), Vec::new(), Vec::new(), 0..0)
            .with_image(ShardImage { box_w, box_h: img_h, src }),
    );
    if cap_wrapped.is_empty() {
        out.push(Shard::new(1, cap_lh, 0., 0., cap_lh, SharedString::default(), Vec::new(), Vec::new(), 0..0));
    } else {
        let mut start = 0usize;
        for (i, wl) in cap_wrapped.iter().enumerate() {
            let byte_end = start + wl.text.len();
            out.push(Shard::new(
                i + 1,
                cap_lh * (wl.wrap_boundaries().len() + 1) as f32,
                wl.width().as_f32(),
                0.,
                cap_lh,
                wl.text.clone(),
                slice_runs(&dr.runs, &(start..byte_end)),
                Vec::new(),
                start..byte_end.min(dr.byte_map.len().saturating_sub(1)),
            ));
            start = byte_end;
        }
    }
    out
}

/// 预览 `parse::Row` → 高度可用的 [`DocRow`]（M5-B5 P3 前置「量程精确化」）。
///
/// `shape_docrow_shard` 对 `DocRow` 只读 `kind/text/runs/payload/byte_map.len()`——
/// `byte_map` 只被 shard 当作「条带显示字节区间」的钳位上界，**不影响高度**，故置空。
/// 其余字段与 [`crate::markdown::doc::Doc::assemble`] 同源：`text/runs` 直接来自
/// 预览同款 `parse_block_window(_rope)` 产出、`kind` 用 `row_kind` 同式、payload 逐类
/// 同式装配 → 高度与真实 `DocRow`（[`crate::markdown::doc::Doc::rows_in_block_window`]）
/// **逐位一致**，由 `markdown::view::tests::docrow_for_height_matches_doc_docrow_shard`
/// 对拍钉死。这让预览高度模型不必持有完整 `Doc`（省 `line_starts`/索引复制），
/// 又能把真实 `DocRow` 喂给 `shape_docrow_shard` 得精确量程。
pub(crate) fn docrow_for_height(r: &Row) -> DocRow {
    let kind = row_kind(r.kind);
    let payload = match r.kind {
        RowKind::Code => DocPayload::Code {
            code_lines: r.code_lines().to_vec(),
            language: r.code_language().map(Into::into),
        },
        RowKind::Image => DocPayload::Image {
            src: r.image().map(|i| i.src.clone()).unwrap_or_default(),
        },
        RowKind::Table => r.table().cloned().map(DocPayload::Table).unwrap_or_default(),
        _ => DocPayload::Text,
    };
    DocRow {
        kind,
        src_line: r.src_line,
        lines: r.src_line..r.src_line + 1,
        text: r.text.clone(),
        runs: r.runs.clone(),
        byte_map: Vec::new(),
        payload,
        table_rows: None,
        align: r.align,
        notes: r.notes.clone(),
        note_defs: r.note_defs.clone(),
    }
}

#[cfg(test)]
mod tests;
