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
    App, Bounds, Element, ElementId, ElementInputHandler, Entity, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, Window, point, px, relative, size,
};

use super::{CaretVis, WyEditor, WyTheme};
use crate::markdown::doc::{DocPayload, DocRow};
use crate::markdown::ir::IrKind;
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
}

/// `DocRow.runs` → gpui `TextRun`（`RunKind` → 视觉样式）。
fn runs_to_text_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    color: gpui::Hsla,
    mono: &gpui::Font,
    accent: gpui::Hsla,
    code_bg: gpui::Hsla,
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
                tr.background_color = Some(code_bg);
                tr.color = accent;
            }
            RunKind::Strikethrough => {
                tr.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                });
            }
            RunKind::Link | RunKind::Image => {
                tr.color = accent;
                tr.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(accent),
                    wavy: false,
                });
            }
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

/// 文本块的高度缓存键：`(内容代数, 图片加载代数, 源行, 类型, shaping 宽)`。
fn height_cache_key(rgen: u64, igen: u64, src_line: u32, kind: RowKind, width_bits: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgen.hash(&mut h);
    igen.hash(&mut h);
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

/// 文本块（段落/标题/引述/列表/分隔线/空行）：按类型行样式形状成折行可视行。
fn shape_text_row(
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
    let runs = runs_to_text_runs(
        r.text.as_ref(),
        &r.runs,
        &base,
        st.color,
        &theme.mono,
        theme.accent,
        theme.code_bg,
    );
    let wrapped = window
        .text_system()
        .shape_text(r.text.clone(), st.fs, &runs, Some(width), None)
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
    let editable = !r.byte_map.is_empty();
    // 引述文本缩进 = 内容边距(12) + 左侧指示条(3) + 带内左内衬(12) − 内容边距(12)。
    let x_off = if kind == IrKind::Quote { px(15.) } else { px(0.) };
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped,
        sub_heights,
        byte_map: std::rc::Rc::new(r.byte_map),
        editable,
        multi_offsets: Vec::new(),
        lh: st.lh,
        x_off,
        height,
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: None,
    }
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
        lh,
        x_off: px(12.),
        height,
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: None,
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
        lh: px(h),
        x_off: px(0.),
        height: px(h),
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: Some(ShapedImage { src, alt, w, h }),
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
        _ => shape_text_row(r, window, width, theme, base_font),
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
                lh: line_h,
                x_off: px(0.),
                height: line_h,
                top: px(0.),
                kind: IrKind::Blank,
                table: None,
                table_edit: None,
                image: None,
            }],
            total: line_h,
            band,
            width,
            rgen,
            igen: editor.images_gen(),
        };
    }

    let (band_lo, band_hi) = band;
    let avail = (f32::from(width) - f32::from(padding) * 2.).max(1.);
    let width_bits = f32::from(width).to_bits();
    let igen = editor.images_gen();

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
        let cache_key = match meta.kind {
            // 折行文本块（段/标题/引述/列表）缓存**真实可视行数**（带外沿用真值防
            // 滚动跳变）。代码/表格/图片**不入缓存**：它们不折行、`estimate_height`
            // 与 shape 高同式（代码 = (行数+2)×code_line）——入缓存反而会用
            // `line_h × 行数`（正文行高×n）重建出错误高度（比真实矮 2 行代码高）。
            RowKind::Paragraph | RowKind::Heading(_) | RowKind::Quote | RowKind::Bullet => {
                Some(height_cache_key(rgen, igen, meta.src_line, meta.kind, width_bits))
            }
            _ => None,
        };
        let h = match cache_key.and_then(|k| editor.cached_height_lines(k)) {
            Some(lines) => line_h * lines as f32,
            None => {
                let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
                estimate_height(&meta, chars, ascii, theme, avail, line_h)
            }
        };
        let bottom = y + f32::from(h);
        plan.push(Plan {
            shaped: bottom > band_lo && y < band_hi,
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
                    lh: line_h,
                    x_off: px(0.),
                    height: h,
                    top: total,
                    kind: crate::markdown::doc::row_kind(meta.kind),
                    table: None,
                    table_edit: None,
                    image: None,
                }
            }
        };
        total += sr.height;
        rows.push(sr);
    }
    Frame { padding, line_h, rows, total, band, width, rgen, igen }
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
    rows: &[ShapedRow],
) -> (bool, Pixels, Pixels) {
    let (cl, col) = editor.row_col(editor.caret());
    let line_start = editor.line_start(cl);
    let line_end = editor.line_end(cl);
    for r in rows {
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
                return (true, px(12.) + r.x_off + p.x, y);
            }
            return (false, px(12.), r.top);
        }
        // 单源码行文本块：源列 → 渲染列（`source_to_display`，左界）——与
        // `source_offset_at` 的反查互为逆映射，光标才落在点击处。
        let target = col.min(line_end.saturating_sub(line_start));
        let dcol = source_to_display(&r.byte_map, target);
        // 折行时 p.y 是子行偏移，叠加到块首才算准缓存所在的可视行。
        let p = render_point_of(&r.wrapped, r.lh, dcol);
        // `px(12.)` = 内容边距；引述/代码等额外缩进走 `r.x_off`（与 paint 同 x）。
        return (true, px(12.) + r.x_off + p.x, r.top + p.y);
    }
    let fallback_lh = rows.first().map_or(px(20.), |r| r.lh);
    let y = rows.last().map_or(fallback_lh, |r| r.top + r.height);
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
                let g = code_group_at(r, rel_y - top);
                let Some(wl) = r.wrapped.get(g) else {
                    return editor.caret();
                };
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off))
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
                // （`rel_y - top`）：`closest_index_for_position` 据此选折行的第几
                // 可视行——写成 0 会把第二行的点击映射回第一行。
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off))
                    .max(0.);
                let rel = point(px(rel_x), px(rel_y - top));
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

/// 代码块：圆角面板（对齐预览：外层 `px_3` → 面板贴**内容区左缘**（=content 边距，
/// 与正文对齐），而非再缩进）；行文本再 `pl_3`（面板内 12px 内衬，走 `row.x_off`，
/// 命中/光标/选区同 x）。首/末各留一个 code_line 空白带。
fn paint_code_row(
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
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(
            point(panel_x, origin.y + row.top),
            size(px(panel_w), row.height),
        ),
        corner_radii: gpui::Corners::all(px(6.)),
        background: theme.code_bg.into(),
        border_widths: gpui::Edges::default(),
        border_color: gpui::Hsla::default(),
        border_style: gpui::BorderStyle::Solid,
    });
    // 顶空白带后逐行画文本（x = 面板左 + 内衬 x_off，行高 = code_line）。
    let text_x = panel_x + row.x_off;
    let mut sub_top = row.top + theme.code_line();
    for wl in &row.wrapped {
        let o = point(text_x, origin.y + sub_top);
        let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
        sub_top += row.lh * (wl.wrap_boundaries().len() + 1) as f32;
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

/// 文本块（段落/标题/引述等可编辑行）逐可视行绘制（x 含行级缩进 `row.x_off`）。
fn paint_text_row(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    let _ = theme;
    let mut sub_top = row.top;
    for wl in &row.wrapped {
        let o = point(origin.x + f.padding + row.x_off, origin.y + sub_top);
        let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
        sub_top += row.lh * (wl.wrap_boundaries().len() + 1) as f32;
    }
}

/// 引述块：灰底带 + 左侧指示条（对齐预览：`px_3` 内缩、`py_1` 内衬、3px 边条）。
/// 每行带从行上/下各向外 4px（`py_1`），相邻引述带重叠 → 整段引述连片灰底。
fn paint_quote_band(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
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
    // 左侧深色指示条（预览固定色 0xa8b1bd）。
    let bar: gpui::Hsla = gpui::rgb(crate::config::markdown::QUOTE_BAR_COLOR).into();
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
    paint_text_row(origin, theme, f, row, window, cx);
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
    let mut hl = theme.accent;
    hl.a = 0.28;
    let e = editor.read(cx);
    for r in &f.rows {
        if !r.editable || r.wrapped.is_empty() {
            continue;
        }
        if !r.multi_offsets.is_empty() {
            // 代码块：逐源码行求与选区的交集，按字形几何画高亮（每行一条）。
            let block_start = e.line_start(r.src_line);
            let g_count = r.multi_offsets.len();
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
                                point(origin.x + f.padding + r.x_off + p0.x, origin.y + y),
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
            let y = r.top + p1.y.min(p0.y);
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

impl IntoElement for WyEditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WyEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
            let band = e.shape_band().unwrap_or((0., 6500.));
            let cached_total = e
                .frame
                .borrow()
                .as_ref()
                .filter(|f| {
                    f.rgen == rgen && f.igen == igen && f.width == width && f.band == band
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
    ) {
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

        // 带移动 / 内容 / 宽度 / 图片加载变化 → 按新状态重建窗口化帧。
        let rgen = self.editor.read(cx).content_gen();
        let igen = self.editor.read(cx).images_gen();
        let width = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map(|f| f.width).unwrap_or(px(800.))
        };
        let stale = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map_or(true, |f| {
                f.rgen != rgen || f.igen != igen || f.width != width || f.band != band
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
            let (on, x, ctop) = caret_geometry(&*editor, &f.rows);
            super::set_caret_vis(CaretVis {
                x,
                top: if on { ctop } else { px(0.) },
                line_height: f.line_h,
            });
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
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
        let origin = bounds.origin;
        for row in &f.rows {
            // 非文本块按类型自绘：代码面板 / 表格网格 / 图片。
            match row.kind {
                IrKind::Code => {
                    // 带外（未 shape）代码块不画（占位无内容）。
                    if row.wrapped.is_empty() {
                        continue;
                    }
                    paint_code_row(origin, &theme, f, row, window, cx);
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
            // 引述块：灰底带 + 左侧指示条（与预览一致：px_3 内缩、py_1 内衬、3px 边条）。
            // 相邻引述行（源码行连续）合并成一片背景，否则各自带 py_1 空隙。
            if row.kind == IrKind::Quote {
                paint_quote_band(origin, &theme, f, row, window, cx);
                continue;
            }
            paint_text_row(origin, &theme, f, row, window, cx);
        }
        // 选区高亮（可编辑行与源选区相交的部分，按字形几何逐个 WrappedLine 画）。
        if let Some(range) = editor_selection(&self.editor, cx) {
            paint_selection(&self.editor, origin, &theme, f, &range, window, cx);
        }
        // 光标几何每帧按 caret 现算（帧缓存不含 caret，改 caret 无需整帧重建）。
        let (caret_on, caret_x, caret_top) = {
            let e = self.editor.read(cx);
            caret_geometry(&*e, &f.rows)
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
            let (on, _, caret_top) = caret_geometry(ed.read(cx), &frame.rows);
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
                let (on, _, ctop) = caret_geometry(ed.read(cx), &frame.rows);
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
            let (on, c0, _) = caret_geometry(ed.read(cx), &frame.rows);
            assert!(on);
            assert!(
                (f32::from(c0) - text_start).abs() < 0.6,
                "光标 x {} 应 = 文本起始 x {text_start}",
                f32::from(c0)
            );
            // 2) 点击面板内、文本左的内衬空白 → 光标收拢到行首（= 文本起始 x），不右偏。
            let off1 = source_offset_at(ed.read(cx), &frame, point(px(pad + 6.), px(lh * 1.5)));
            ed.update(cx, |e, _| e.set_caret(off1));
            let (on, c1, _) = caret_geometry(ed.read(cx), &frame.rows);
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
            let (on, c2, _) = caret_geometry(ed.read(cx), &frame.rows);
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
            let (on, caretx, carety) = caret_geometry(ed.read(cx), &frame.rows);
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
}

