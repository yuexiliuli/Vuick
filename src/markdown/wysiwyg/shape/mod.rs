//! WYSIWYG **行整形层（shape）**：把 [`super::element::DocRow`] 按块类型整形为
//! [`super::element::ShapedRow`]（文本/代码/表格/图片），含行内装饰（链接/行内图/脚注）
//! 与高度估计、数学卡片行构造。纯整形、无绘制/命中——下游由 [`super::frame`]（build_frame）
//! 消费，绘制在 [`super::paint`]；被分隔走的是 `math.rs`（数学/mermaid 显示化）与本文件的
//! 各类块整形。
//!
//! 依赖方向：`types ← math ← shape ← frame ← element(胶水)`，全部经 `mod.rs` 扁平重导出
//! （`use super::*`）共享 [`super::element`] 布局类型与 [`super::math`]/[`super::geom`] 助手。

use super::*;

use gpui::{Pixels, Window};

use crate::markdown::doc::{DocPayload, DocRow};
use crate::markdown::ir::IrKind;
use crate::markdown::parse::inline_style::{self, MathRegion};
use crate::markdown::parse::recognition;
use crate::markdown::parse::{BlockMeta, RowKind, RunKind, TableAlign};
use crate::syntax::{
    CodeLineRuns, github_theme_cached, parse_code_lines, resolve_line_styles_with,
};

/// 引述（非警报）正文的**左缘缩进**（px，`x_off`）：= 内容边距(12) + 左侧指示条(3)
/// + 带内左内衬(12) − 内容边距(12) = 15。正文起点在内容盒内再缩进，文字折行宽必须
/// 一并扣除（`gutter_x` 与 `shape_text_row` 的行 `x_off` 同源），否则右缘越出内容盒。
const QUOTE_TEXT_INDENT_PX: f32 = 15.0;

/// `DocRow.runs` → gpui `TextRun`（`RunKind` → 视觉样式）。
/// 行内代码：等宽字体 + `code_inline_fg` 文字色（**无 run 背景**——圆角 pill 由
/// [`paint_text_row`] 在 paint 阶段按像素几何自绘，与预览 [`TextTrack::paint_code_ranges`]
/// 同式；`code_inline_*` 单一源 [`super::WyTheme`] ← [`StyleParams`]）。
#[allow(clippy::too_many_arguments)] // 样式参数互为正交的纯映射助手
pub(crate) fn runs_to_text_runs(
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
    // 防御：run 边界必须落在 `text` 的 UTF-8 **字符边界**上（CJK/emoji 多字节字符
    // 下，编辑→重析→`style_links_in_runs`/`strip_prefix_bytes` 等按字节搬运的区间
    // 可能把边界切进字符内部）——GPUI `Text`/`StyledText` 在非字符边界处切 run 会
    // panic（`text.rs` 的 `is_char_boundary(run.start)` assert / shaping 崩）。与预览
    // `markdown::view::row::char_align_range`、源码编辑器 `render::char_align_highlights`
    // 三处合一的共享基元（`crate::util::char_align_byte`，M1.1-P5）：可将 start/end
    // 分别向前进位吸收到最近字符边界、钳入 `text`，空区间丢弃。这使本渲染路径即便
    // 上游混入坏区间也永不 panic。
    let mut out = Vec::new();
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
                color,
                ..Default::default()
            });
        }
        let mut tr = TextRun {
            len: e - s,
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
pub(crate) fn table_block_height(theme: &WyTheme, n_rows: usize) -> Pixels {
    let row_h = f32::from(theme.para_line()) + 12.;
    px((1. + n_rows as f32) * row_h + n_rows as f32 + 2.)
}

/// 表格单行高（表头/数据行同高）＝正文行高 + 12（预览 `render_table` 的 `row_h`）。
pub(crate) fn table_row_h(theme: &WyTheme) -> Pixels {
    px(f32::from(theme.para_line()) + 12.)
}

/// 带外（且从未整形过）块的高度估算：用**真实字数**（`Doc::line_chars_ascii`）
/// 与近似字符宽（ASCII ≈ 0.5 字高、含 CJK ≈ 0.95 字高）估可视行数，尽量贴合真实
/// shape，使带外/带内高度差 ≈ 0（滚动带边界不再跳「字体大小」）；代码用
/// `(行数+2) × code_line`（与预览同式）、表格用 [`table_block_height`]、
/// 标题用各级行高 + 底部留白；图片用占位高度；空行/分隔线固定一行。
/// 只作兜底——整形过的块走缓存真值。
pub(crate) fn estimate_height(
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
pub(crate) struct RowTextStyle {
    pub(crate) fs: Pixels,
    pub(crate) lh: Pixels,
    pub(crate) color: gpui::Hsla,
    pub(crate) bold: bool,
    /// 块底部额外留白（标题 py_1）。
    pub(crate) pad: Pixels,
}

pub(crate) fn text_style_for(kind: IrKind, theme: &WyTheme) -> RowTextStyle {
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
/// `wrap_w` 是该行**实际折行宽**（px）：调用方（`shape_text_row_decorated`）已按
/// `帧宽 − 帧内衬 − 本行 gutter(x_off)` 算好——保证 `padding + x_off + wrap_w`
/// 恰为内容盒右缘（`帧宽 − padding`），任一带 gutter 的行（项目符号/警报/标题）右缘
/// 都不越出内容盒被裁、右侧留白与左侧对称。
pub(crate) fn shape_text_row(
    editor: &WyEditor,
    r: DocRow,
    window: &mut Window,
    _width: Pixels,
    theme: &WyTheme,
    base_font: &gpui::Font,
    wrap_w: f32,
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
        if cl == r.src_line { Some(col) } else { None }
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
        wrap_w.max(1.),
        &base,
        &math_font,
        super::math_color(theme),
        &code_ranges,
    );
    // 折行宽由调用方按「帧宽 − 帧内衬 − 本行 gutter」给定（见 `shape_text_row_decorated`）。
    // 旧的 `Some(width)` 只裁左内衬，导致右侧文字一直排到帧右缘、右 gap 比左 gap 小
    // 一个内衬（左右不对称、贴滚动条）；本版把 gutter 与右内衬一并计入，任意行型都留在
    // 内容盒内，与预览 `markdown::view` 的 `avail` 语义一致（命中/光标/划线同源常量）。
    let wrapped = window
        .text_system()
        .shape_text(
            rtext.clone().into(),
            st.fs,
            &runs2,
            Some(px(wrap_w.max(1.))),
            None,
        )
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
    // 高度用 splice 就已按内容盒宽钳好的 `disp_h`（与 paint 画图同源，单值一致）。
    for im in &mut inline_maths {
        let Some((_, _, _)) = im.ready else { continue };
        let li = visual_block_line_for_index(&wrapped, im.display_off);
        im.line = li;
        let need = im.disp_h.max(f32::from(sub_heights[li]));
        let old = f32::from(sub_heights[li]);
        if need > old {
            height += px(need - old);
            sub_heights[li] = px(need);
        }
    }
    let editable = !map2.is_empty();
    // 引述文本缩进 = 内容边距(12) + 左侧指示条(3) + 带内左内衬(12) − 内容边距(12)。
    let x_off = if kind == IrKind::Quote {
        px(QUOTE_TEXT_INDENT_PX)
    } else {
        px(0.)
    };
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
        extra: RowExtra {
            inline_maths,
            ..Default::default()
        },
    }
}

/// 文本行的装饰化入口（在 [`shape_text_row`] 之上叠加：警报标记剥除、列表/任务
/// gutter、可点击链接、行内图片）。需要源文本以识别构造，故把 `editor` 穿进来。
pub(crate) fn shape_text_row_decorated(
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
    // —— 先定 gutter / x_off（折行宽必须按其扣除，否则带 gutter 的行右缘越出内容盒）——
    // C3: 警报（callout）——剥掉 `[!KIND]` 标记，paint 画彩条 + 粗体标签 + 折叠箭头。
    let mut alert: Option<RowGutter> = None;
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
            alert = Some(RowGutter::Alert { kind: akind, fold });
        }
    }
    // C1: 列表/任务行左缘 gutter（项目符号 / 有序号码，正文右移 18/24px）。
    let mut bullet_g: Option<RowGutter> = None;
    if r.kind == IrKind::Bullet
        && let Some(g) = bullet_gutter(&raw)
    {
        bullet_g = Some(g);
    }
    let pad = f32::from(px(crate::config::markdown::CONTENT_EDGE_PAD_X_PX));
    let gutter_x: f32 = if alert.is_some() {
        84.
    } else if let Some(g) = &bullet_g {
        if matches!(g, RowGutter::Check { .. }) {
            24.
        } else {
            18.
        }
    } else if matches!(r.kind, IrKind::Heading(_)) {
        HEADING_GUTTER_W
    } else if matches!(r.kind, IrKind::Quote) {
        // 普通引述正文起点在内容盒内再缩进（`shape_text_row` 的 `x_off` 同源常量）。
        QUOTE_TEXT_INDENT_PX
    } else {
        // 普通段落无 gutter（x_off=0，正文起点即内容盒左缘）。
        0.
    };
    // 折行宽 = 帧宽 − 帧内衬×2 − gutter：文字从 `padding + x_off` 起画，折行宽补到
    // `帧宽 − padding`（内容盒右缘）为止——左右内衬对称生效，任意带 gutter 行（项目
    // 符号/警报/标题）右侧留白都与左侧一致、不越界被裁（与预览 `markdown::view` 同式）。
    let wrap_w = (f32::from(width) - 2. * pad - gutter_x).max(1.);
    let mut srow = shape_text_row(editor, r.clone(), window, width, theme, base_font, wrap_w);
    // C3: 警报花括 gutter + 正文右移 84px。
    if let Some(g @ RowGutter::Alert { .. }) = &alert {
        srow.extra.gutter = Some(g.clone());
        srow.x_off = px(84.);
    }
    // C1: 列表/任务行首装饰 + 正文缩进（与预览一致：正文色符号 + 缩进）。
    if let Some(g) = bullet_g {
        let is_check = matches!(g, RowGutter::Check { .. });
        srow.extra.gutter = Some(g);
        srow.x_off = if is_check { px(24.) } else { px(18.) };
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
pub(crate) fn strip_prefix_bytes(r: &mut DocRow, n: usize) {
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
pub(crate) fn bullet_gutter(raw: &str) -> Option<RowGutter> {
    let t = raw.trim_start();
    for b in ["- ", "* ", "+ "] {
        for (mark, checked) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
            if let Some(rest) = t.strip_prefix(&format!("{b}{mark}"))
                && !rest.is_empty()
            {
                return Some(RowGutter::Check { checked });
            }
        }
    }
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return Some(RowGutter::Bullet);
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..4).contains(&digits) && (t[digits..].starts_with(". ") || t[digits..].starts_with(") "))
    {
        return Some(RowGutter::Ordered(format!("{}.", &t[..digits])));
    }
    None
}

/// 让「所有可点击链接」（`[[wiki]]` / `#tag` / 裸 URL，以及 parser 已产出的
/// `[text](url)`）都套上链接色 + 下划线（与预览一致）。在数学 splice 之前合并进
/// `r.runs`（显示字节空间，pre-splice）。
pub(crate) fn style_links_in_runs(r: &mut DocRow, raw: &str) {
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
pub(crate) fn link_hits(raw: &str, byte_map: &[usize]) -> Vec<RowLink> {
    let mut out = Vec::new();
    for (src, target) in recognition::links(raw) {
        let d0 = source_to_display(byte_map, src.start);
        let d1 = source_to_display(byte_map, src.end);
        if d1 > d0 {
            out.push(RowLink {
                disp: d0..d1,
                target,
            });
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
pub(crate) fn inline_image_hits(r: &DocRow, srow: &ShapedRow, raw: &str) -> Vec<InlineImageHit> {
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
        if raw.as_bytes().get(start) != Some(&b'!') || raw.as_bytes().get(start + 1) != Some(&b'[')
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
            out.push(InlineImageHit {
                display_off: d0,
                len: d1 - d0,
                src: src.into(),
            });
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
pub(crate) fn shape_code_row(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    theme: &WyTheme,
    width: Pixels,
) -> ShapedRow {
    let kind = r.kind;
    let (code_lines, lang) = match &r.payload {
        DocPayload::Code {
            code_lines,
            language,
        } => (code_lines.clone(), language.clone()),
        _ => (Vec::new(), None),
    };
    // A2: ```mermaid 围栏折叠成「图卡片」——画一张节点/连线的矢量图。
    if lang.as_deref() == Some("mermaid") {
        let source = code_lines.join("\n");
        // 滚轮逐卡缩放：卡高与布局同步乘 `zoom`（几何在 `mm_layout` 后等比缩放，paint
        // 按 `layout.zoom` 等比缩放字号——见 `scale_mm_layout`/`paint_mermaid_placed`）。
        let zoom = editor.mermaid_zoom(r.src_line);
        let height = px(f32::from(mermaid_card_height(&source)) * zoom);
        // 布局（`MmPlaced` 卡片相对坐标，area=(0,0)）在 **shape 期**算好缓存——paint
        // 不再每帧重排/重测全部标签宽（否则滚动/键入时可见的 mermaid 卡片每帧整套
        // 解析+排版；行整形缓存使其只在内容/宽度/主题/缩放变化时重算一次）。
        // 卡面可用宽 = 帧宽 − 帧内衬 ×2（单一来源 `CONTENT_EDGE_PAD_X_PX`，与 `avail`
        // 同式）；缩放据此等比，paint 才不裁边。
        let area_w = (f32::from(width)
            - 2. * f32::from(px(crate::config::markdown::CONTENT_EDGE_PAD_X_PX)))
        .max(1.);
        let mut mm = mm_layout(window, theme, &source, 0., 0., area_w);
        scale_mm_layout(&mut mm, zoom, area_w);
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
    // 语法高亮：小/廉价块同步整块解析（首帧即上色）；大中型块（>8 内容行，非 TOML）
    // 走后台解析缓存——debug 下 syntect 整块解析可达 ~300ms/块，绝不能在主线程串行做；
    // 缓存未就绪先按纯文本渲染（`request_code_parse` 触发后台解析，就绪后下一帧自动
    // 上色），首次滚动进视野不再冻结一帧。
    let t_parse = crate::prof::begin();
    let code_join = code_lines
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    let hl_runs: Option<CodeLineRuns> = match lang.as_deref() {
        Some(l) if !l.is_empty() && !code_lines.is_empty() => {
            if code_lines.len() > 8 && !l.eq_ignore_ascii_case("toml") {
                match editor.code_parse_lookup(l, &code_join) {
                    CodeParseLookup::Ready(runs) => runs,
                    CodeParseLookup::NotReady => {
                        editor.request_code_parse(l, &code_join, r.src_line);
                        None
                    }
                }
            } else {
                parse_code_lines(l, &code_join)
            }
        }
        _ => None,
    };
    crate::prof::end("wy.code.parse", t_parse);
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
            vec![gpui::TextRun {
                len: line.len(),
                font: mono.clone(),
                color: base,
                ..Default::default()
            }]
        };
        let t_st = crate::prof::begin();
        let v = window
            .text_system()
            .shape_text(line.clone(), fs, &runs, None, None)
            .map(|ls| ls.into_vec())
            .unwrap_or_default();
        crate::prof::end("wy.code.shapetext", t_st);
        let rows = v
            .iter()
            .map(|wl| wl.wrap_boundaries().len() + 1)
            .sum::<usize>()
            .max(1);
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
        // 代码块行再内缩 `CONTENT_EDGE_PAD_X_PX`（= 预览 px_3 内衬；= paint/命中/光标的
        // `r.x_off` 同源常量）。
        x_off: px(crate::config::markdown::CONTENT_EDGE_PAD_X_PX),
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
pub(crate) fn shape_table_row(
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
    // 表格缓存失效重排是本路径最重的开销：既有实现对同一文本 shape **两次**（先
    // `shaped_text_width` 测宽、排版再 shape 一次）。这里改为每单元格只 shape 一次
    // （测宽字体 = `base_font` 常规、`theme.text` 色，与既有 `shaped_text_width` 完全同
    // 参数），列宽直接复用 `line.width()`；数据行（常规字体）的 shape 产物留存给下方
    // 排版阶段按序搬用。表头排版用加粗是唯一例外——列宽分配仍按常规字宽（保持现状
    // 像素级一致），仅表头一行重排，开销可忽略。
    let shape_regular = |window: &mut Window, c: &gpui::SharedString| -> gpui::ShapedLine {
        let run = [gpui::TextRun {
            len: c.len(),
            font: base_font.clone(),
            color: theme.text,
            ..Default::default()
        }];
        window
            .text_system()
            .shape_line(c.clone(), font_size, &run, None)
    };
    for (ci, c) in header.iter().enumerate() {
        let line = shape_regular(window, c);
        widths[ci] = widths[ci].max(f32::from(line.width()) + 20.);
    }
    let rows2: Vec<Vec<gpui::SharedString>> = rows
        .into_iter()
        .map(|mut rr| {
            rr.resize(ncols, gpui::SharedString::default());
            rr
        })
        .collect();
    // 数据行同样各 shape 一次（含稀疏行补列后的空串——宽 0，`max` 不变）。`rows_shapes`
    // 与 `rows2` 同构（每行 ncols），排版阶段逐行按序搬用。
    let mut rows_shapes: Vec<Vec<gpui::ShapedLine>> = Vec::with_capacity(rows2.len());
    for row in &rows2 {
        let mut shapes = Vec::with_capacity(ncols);
        for c in row {
            shapes.push(shape_regular(window, c));
        }
        rows_shapes.push(shapes);
    }
    for row_lines in &rows_shapes {
        for (ci, line) in row_lines.iter().enumerate() {
            widths[ci] = widths[ci].max(f32::from(line.width()) + 20.);
        }
    }
    let total_w = widths.iter().sum();
    let row_h = table_row_h(theme);
    // 表格块总高 = 网格高 + 底部滚动条预留带（`TABLE_SCROLLBAR_RESERVE_PX`，与 preview
    // `markdown/view` 的 `.pb()` 同）——滚动条画在网格**之下**、不遮内容。带内/带外同式
    // （`estimate_height` 同加该带）→ 总高随可视带恒定、滚动不跳。
    let height = table_block_height(theme, rows2.len())
        + px(crate::config::markdown::TABLE_SCROLLBAR_RESERVE_PX);
    let mut aligns2 = aligns.clone();
    aligns2.resize(ncols, TableAlign::Left);
    let mappings: Option<Vec<crate::markdown::doc::TableRowMap>> =
        r.table_rows.as_ref().map(|m| m.as_ref().clone());

    // 逐可见行排版每单元格（表头加粗重排、数据行搬用上面唯一一次 shape 产物；runs/对齐
    // 与 `draw_table_row` 一致）。
    let mut trow_edit: Vec<TableRowCellsLayout> = Vec::new();
    let mk_row = |window: &mut Window,
                  src_line: usize,
                  texts: Vec<gpui::SharedString>,
                  maps: &[crate::markdown::doc::TableCellMap],
                  lines: Option<Vec<gpui::ShapedLine>>| {
        // 表头（`lines == None`）排版用加粗——列宽已按常规测宽（像素保真），此重排仅此一行。
        let is_header = lines.is_none();
        let mut lines = lines.unwrap_or_default().into_iter();
        let mut cells = Vec::with_capacity(texts.len());
        for (ci, c) in texts.iter().enumerate() {
            let col_w = widths.get(ci).copied().unwrap_or(0.);
            let line = if is_header {
                let mut font = base_font.clone();
                let mut color = theme.text;
                font.weight = gpui::FontWeight::BOLD;
                color.a = 0.9;
                let run = [gpui::TextRun {
                    len: c.len(),
                    font,
                    color,
                    ..Default::default()
                }];
                window
                    .text_system()
                    .shape_line(c.clone(), font_size, &run, None)
            } else {
                lines
                    .next()
                    .expect("数据行 shape 产物须与列数等长（`rows_shapes` 与 `rows2` 同构）")
            };
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
            cells.push(TableCellLayout {
                src,
                text_x,
                map,
                line,
            });
        }
        TableRowCellsLayout { src_line, cells }
    };
    // 表头（源行 = r.src_line）。
    let head_maps = mappings
        .as_ref()
        .and_then(|r| r.first())
        .map(|m| m.cells.as_slice());
    trow_edit.push(mk_row(
        window,
        r.src_line,
        header.clone(),
        head_maps.unwrap_or(&[]),
        None,
    ));
    // 数据行：k=0 对应源行 = src_line + 2（分隔行为 src_line+1，不渲染、不可编辑）。
    // 逐行搬用唯一一次 shape 产物（`rows_shapes` 与 `rows2` 每行同 ncols）。
    let mut rows_shapes_iter = rows_shapes.into_iter();
    for (k, texts) in rows2.iter().enumerate() {
        let src_line = r.src_line + 1 + (k + 1);
        let maps = mappings
            .as_ref()
            .and_then(|r| r.get(k + 1))
            .map(|m| m.cells.as_slice())
            .unwrap_or(&[]);
        trow_edit.push(mk_row(
            window,
            src_line,
            texts.clone(),
            maps,
            Some(rows_shapes_iter.next().unwrap_or_default()),
        ));
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

/// 独立图片行的显示宽（px）：有用户拖拽调整（`image_width` 覆写）→ 覆写值钳在
/// `[IMAGE_MIN_W, IMAGE_MAX_W]` 内且不超可用宽；否则自然宽 = `min(avail, IMAGE_MAX_W)`。
/// `image_row_height` / `shape_image_row` 必须用同一函数 → 带内/带外、拖拽后高度一致。
pub(crate) fn effective_image_w(editor: &WyEditor, src: &str, avail: f32) -> f32 {
    let natural = avail.min(crate::config::markdown::IMAGE_MAX_W);
    match editor.image_width(src) {
        Some(w) => w
            .clamp(
                crate::config::markdown::IMAGE_MIN_W,
                crate::config::markdown::IMAGE_MAX_W,
            )
            .min(avail)
            .max(1.),
        None => natural,
    }
}

/// 独立图片行的显示高（px）：已加载按真实宽高比换算（宽 = [`effective_image_w`]），
/// 未加载/失败用占位高。`build_frame` 的带外估算与 `shape_image_row` 必须用同一函数
/// → 加载后/拖拽后带内带外高度一致（滚动不跳）。
pub(crate) fn image_row_height(editor: &WyEditor, src: &str, avail: f32) -> Pixels {
    let w = effective_image_w(editor, src, avail);
    match editor.image_size(src) {
        Some(Some((iw, ih))) if iw > 0. && ih > 0. => px((w * ih / iw.max(1.)).max(1.)),
        _ => px(crate::config::markdown::IMAGE_ROW_DEFAULT_H),
    }
}

/// 独立图片块的**基础布局高**（px）：图片盒高（[`image_row_height`]）+ 题注间隙 +
/// 题注行高（`line_h` 按单行估算，整形后 Pass2 以真实折行高回写）。
/// `pass1_base_layout` 与 `patch_pending_image_heights` 必须用**同一公式**——整篇重建
/// 与稀疏定向覆写产出的带外块高一致，占位→真实 / 拖拽改宽后滚动不跳（带内/带外同一
/// 公式防滚动跳，同 `image_row_height` 注释）。
pub(crate) fn image_block_base_height(
    editor: &WyEditor,
    src: &str,
    avail: f32,
    line_h: Pixels,
) -> f32 {
    f32::from(image_row_height(editor, src, avail))
        + crate::config::markdown::IMAGE_CAPTION_GAP_PX
        + f32::from(line_h)
}

/// 独立图片行：**可编辑**。布局 = 图片盒（顶部，按真实宽高比/占位高）+
/// 题注（alt 文本行，置于图片之下、水平居中）。题注走与普通文本行同构的
/// `byte_map`/`wrapped`/`x_off`（命中/光标/选区据此把屏幕位置换算成源字节，
/// 编辑正确改写 `![alt](…)` 中的 alt）。图片内容在 paint 期异步加载后画出。
pub(crate) fn shape_image_row(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    theme: &WyTheme,
    base_font: &gpui::Font,
    avail: f32,
) -> ShapedRow {
    let kind = r.kind;
    let alt = r.text.clone();
    let src = match &r.payload {
        DocPayload::Image { src } => src.clone(),
        _ => gpui::SharedString::default(),
    };
    // A-4 冷开惰性化：仅**带内**图片行首次 shape 才读一次真实尺寸（不在 load 路径
    // 同步读全篇；pass1 估算走 `image_row_height` 的占位高，进带由本行 shape 修正）。
    editor.lazy_ensure_image_size(src.as_ref());
    let w = effective_image_w(editor, src.as_ref(), avail);
    let img_h = image_row_height(editor, src.as_ref(), avail);
    // 题注（alt 可编辑文本）行：muted 色、正文行高，按内容宽折行（罕见超长 alt）。
    let cap_run = [gpui::TextRun {
        len: alt.len(),
        font: base_font.clone(),
        color: theme.muted,
        ..Default::default()
    }];
    let cap_wrapped = window
        .text_system()
        .shape_text(
            alt.clone(),
            theme.font_size,
            &cap_run,
            Some(px(avail)),
            None,
        )
        .map(|ls| ls.into_vec())
        .unwrap_or_default();
    let cap_lh = theme.para_line();
    let cap_h = cap_wrapped.iter().fold(0f32, |acc, wl| {
        acc + f32::from(cap_lh) * (wl.wrap_boundaries().len() + 1) as f32
    });
    let cap_h = if cap_h <= 0. {
        f32::from(cap_lh)
    } else {
        cap_h
    };
    let mut sub_heights: Vec<Pixels> = cap_wrapped
        .iter()
        .map(|wl| (f32::from(cap_lh) * (wl.wrap_boundaries().len() + 1) as f32).into())
        .collect();
    if sub_heights.is_empty() {
        sub_heights.push(cap_lh);
    }
    let gap = crate::config::markdown::IMAGE_CAPTION_GAP_PX;
    // 题注水平居中：x_off = (内容宽 − 首行宽)/2（折行的后续行仍左对齐，超长罕见）。
    let x_off = cap_wrapped
        .first()
        .map(|wl| ((avail - f32::from(wl.width())) / 2.).max(0.))
        .unwrap_or(0.);
    let height = img_h + px(gap + cap_h);
    let has_non_ascii = cap_wrapped.iter().any(|wl| !wl.text.is_ascii());
    let editable = !r.byte_map.is_empty();
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped: std::rc::Rc::new(cap_wrapped),
        has_non_ascii,
        sub_heights,
        byte_map: std::rc::Rc::new(r.byte_map),
        editable,
        multi_offsets: Vec::new(),
        code_ranges: Vec::new(),
        lh: cap_lh,
        x_off: px(x_off),
        height,
        top: px(0.),
        kind,
        table: None,
        table_edit: None,
        image: Some(ShapedImage {
            src,
            w,
            h: f32::from(img_h),
        }),
        mm: None,
        extra: RowExtra::default(),
    }
}

/// 把一个物化的 `DocRow` 真正 shape 成可视行（仅带内块走这里）——按块类型分派。
pub(crate) fn shape_docrow(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    avail: f32,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    match r.kind {
        IrKind::Code => shape_code_row(r, editor, window, theme, width),
        IrKind::Table => shape_table_row(r, window, theme.font_size, theme, base_font),
        IrKind::Image => shape_image_row(r, editor, window, theme, base_font, avail),
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

/// 数学块卡片高度：**只由源码显示行决定**（`20 + Σ每行排版高估计`）——与排版槽
/// Loading/Ready/Failed 无关，**单值稳定**：替换位图/回退不改卡高（块级公式输入/
/// 滚动时「大块异常↔正常块」两态回弹的根因 = 旧实现 Ready 用 `20+位图高`、回退用
/// `20+n×22` 两个不同高来回跳）。就绪位图按「自然宽 + 块内横向滚动」画（见
/// `paint_card`），纵向在 `height−20` 内等比收容，不会再让卡高被位图顶高。
pub(crate) fn math_card_height(editor: &WyEditor, reg: &MathRegion) -> Pixels {
    px(20. + editor.math_height_estimate(reg.source.as_str()))
}

/// 源码显示行的**排版高估计**：按 `math_display_lines` 同拆分（`\n`/`\\`）逐段，
/// 含高算子（∫/∑/lim/frac/sqrt 等）的段用 `MATH_DISPLAY_LINE_TALL_H`、否则
/// `MATH_DISPLAY_LINE_H`。与 `math_card_height` 单一来源（`mod` 侧缓存复用）。
pub(crate) fn math_line_height_h(src: &str) -> f32 {
    use crate::config::math;
    let mut total = 0.0f32;
    for ln in src.split('\n') {
        for seg in ln.split("\\\\") {
            if seg.trim().is_empty() {
                continue;
            }
            let tall = [
                "\\int",
                "\\sum",
                "\\prod",
                "\\coprod",
                "\\bigcup",
                "\\bigcap",
                "\\bigsqcup",
                "\\bigoplus",
                "\\bigotimes",
                "\\lim",
                "\\frac",
                "\\sqrt",
                "\\binom",
                "\\oint",
                "\\iint",
                "\\iiint",
                "\\left",
                "\\right",
                "\\cases",
                "\\begin",
            ]
            .iter()
            .any(|cmd| seg.contains(cmd));
            total += if tall {
                math::MATH_DISPLAY_LINE_TALL_H
            } else {
                math::MATH_DISPLAY_LINE_H
            };
        }
    }
    total.max(math::MATH_DISPLAY_LINE_H)
}

/// 装配一张数学块卡片行（A1）：整块（含围栏）折叠成一张面板，占据 `height`。
pub(crate) fn build_math_card_row(
    reg: &MathRegion,
    src_line: usize,
    height: Pixels,
    top: Pixels,
) -> ShapedRow {
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
            card: Some(BlockCard::Math {
                align: reg.align,
                latex: reg.source.clone(),
            }),
            ..Default::default()
        },
    }
}
