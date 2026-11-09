//! WYSIWYG **行整形层（shape）**（自宿主 `wysiwyg/shape/mod.rs` 迁入引擎）：把
//! [`element::DocRow`]（`markdown::doc::DocRow`）按块类型整形为 [`crate::element::ShapedRow`]
//! （文本/代码/表格/图片），含行内装饰（链接/行内图/脚注）与高度估计、数学卡片行构造。
//! 纯整形、无绘制/命中——下游由 [`crate::frame`]（build_frame）消费，绘制在
//! [`crate::paint`]。
//!
//! 迁移 seam（宿主 `WyEditor` 原入参 → 引擎）：凡需「宿主只读态」的函数改持
//! `rd: &RefCell<WyRenderModel>` + `host: &dyn WyRenderHost` 成对入参；纯函数
//! （`estimate_height`/`math_display_lines` 等）保留原签名随迁。行为/语义/失效时序逐字保持。
//!
//! 依赖方向：`types ← math（独立 crate `markdown_wysiwyg_math`）← shape ← frame ← element(胶水)`。

use std::cell::RefCell;
use std::ops::Range;

use gpui::{Pixels, Window, px};

use crate::element::*;
use crate::code_parse::CodeParseLookup;
use crate::layout::{text_runs, TextRunDeco};
use crate::model::WyRenderModel;
use crate::{WyRenderHost, docutil, theme::WyTheme, theme::math_color};
use markdown::doc::{DocPayload, DocRow};
use markdown::ir::IrKind;
use markdown::parse::inline_style::{self, MathRegion};
use markdown::parse::recognition;
use markdown::parse::{BlockMeta, RowAlign, RowKind, RunKind, TableAlign};
use markdown_wysiwyg_math::{MathSlot, splice_math_text, visual_block_line_for_index};
use markdown_wysiwyg_mermaid::{MmTheme, mermaid_card_height, mm_layout, scale_mm_layout};
use syntax::{CodeLineRuns, github_theme_cached, parse_code_lines, resolve_line_styles_with};

/// 引述（非警报）正文的**左缘缩进**（px，`x_off`）：
/// 内容边距(12) + 左侧指示条(3) + 带内左内衬(12) − 内容边距(12) = 15。
/// 正文起点在内容盒内再缩进，文字折行宽必须一并扣除（`gutter_x` 与
/// `shape_text_row` 的行 `x_off` 同源），否则右缘越出内容盒。
const QUOTE_TEXT_INDENT_PX: f32 = 15.0;

/// `DocRow.runs` → gpui `TextRun`（`RunKind` → 视觉样式）。
/// 行内代码：等宽字体 + `code_inline_fg` 文字色（**无 run 背景**——圆角 pill 由
/// [`crate::paint::paint_text_row`] 在 paint 阶段按像素几何自绘，与预览 [`TextTrack::paint_code_ranges`]
/// 同式；`code_inline_*` 单一源 [`crate::theme::WyTheme`] ← [`StyleParams`]）。
/// 收敛自 `layout::text_runs` 单一 run 构造核心（字体/颜色/划线经 `TextRunDeco` 参数化；
/// 本调用点传完整视觉装饰，`layout::font_runs` 只传字形维度——两处对拍由
/// `layout::tests` parity 钉死，见 [`crate::layout::tests::parity_shard_height_sum_matches_shape_docrow`]）。
#[allow(clippy::too_many_arguments)]
pub fn runs_to_text_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    color: gpui::Hsla,
    mono: &gpui::Font,
    accent: gpui::Hsla,
    link: gpui::Hsla,
    code_inline_fg: gpui::Hsla,
) -> Vec<gpui::TextRun> {
    use gpui::{StrikethroughStyle, UnderlineStyle};
    text_runs(text, runs, base, color, move |kind| {
        let mut f = base.clone();
        let mut run_color = color;
        let mut strikethrough = None;
        let mut underline = None;
        match kind {
            RunKind::Bold => f.weight = gpui::FontWeight::BOLD,
            RunKind::Italic => f.style = gpui::FontStyle::Italic,
            RunKind::InlineCode => {
                f = mono.clone();
                run_color = code_inline_fg;
            }
            RunKind::Strikethrough => {
                strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                });
            }
            RunKind::Link => {
                run_color = link;
                underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(link),
                    wavy: false,
                });
            }
            RunKind::Image => {}
            RunKind::NoteMarker => run_color = accent,
        }
        TextRunDeco {
            font: f,
            color: Some(run_color),
            strikethrough,
            underline,
        }
    })
}

/// 表格块的总高（px）：与预览**实际渲染**同式 `(1 + 数据行数) × 行高 + 行分隔数 + 2`
/// （行高 = 正文行高 `para_line` + 12，外边框 2px；见 `view::table::render_table`）。
/// `estimate_height` 与 `shape_docrow` 用同一公式 → 带内/带外高度一致（滚动不跳）。
pub fn table_block_height(theme: &WyTheme, n_rows: usize) -> Pixels {
    let row_h = f32::from(theme.para_line()) + 12.;
    px((1. + n_rows as f32) * row_h + n_rows as f32 + 2.)
}

/// 表格单行高（表头/数据行同高）＝正文行高 + 12（预览 `render_table` 的 `row_h`）。
pub fn table_row_h(theme: &WyTheme) -> Pixels {
    px(f32::from(theme.para_line()) + 12.)
}

/// 带外（且从未整形过）块的高度估算：用**真实字数**（`Doc::line_chars_ascii`）
/// 与近似字符宽（ASCII ≈ 0.5 字高、含 CJK ≈ 0.95 字高）估可视行数，尽量贴合真实
/// shape，使带外/带内高度差 ≈ 0（滚动带边界不再跳「字体大小」）；代码用
/// `(行数+2) × code_line`（与预览同式）、表格用 [`table_block_height`]、
/// 标题用各级行高 + 底部留白；图片用占位高度；空行/分隔线固定一行。
/// 只作兜底——整形过的块走缓存真值。
pub fn estimate_height(
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
        RowKind::Table => {
            table_block_height(theme, meta.aux.max(1) as usize)
                + px(config::markdown::TABLE_SCROLLBAR_RESERVE_PX)
        }
        RowKind::Image => px(config::markdown::IMAGE_ROW_DEFAULT_H),
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
pub struct RowTextStyle {
    pub fs: Pixels,
    pub lh: Pixels,
    pub color: gpui::Hsla,
    pub bold: bool,
    /// 块底部额外留白（标题 py_1）。
    pub pad: Pixels,
}

pub fn text_style_for(kind: IrKind, theme: &WyTheme) -> RowTextStyle {
    match kind {
        IrKind::Heading(n) => RowTextStyle {
            fs: theme.heading_fs(n),
            lh: theme.heading_line(n),
            color: theme.text,
            bold: true,
            pad: px(8.),
        },
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
pub fn shape_text_row(
    _rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
    let code_ranges: Vec<Range<usize>> = r
        .runs
        .iter()
        .filter(|(_, k)| *k == RunKind::InlineCode)
        .map(|(rg, _)| crate::layout::expand_bg_over_whitespace(r.text.as_ref(), rg))
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
    let mut math_font = theme.font.clone();
    math_font.style = gpui::FontStyle::Italic;
    let caret_disp = {
        let (cl, col) = docutil::row_col(host.doc(), host.caret());
        if cl == r.src_line { Some(col) } else { None }
    };
    let line_start = docutil::line_start(host.doc(), r.src_line);
    let (rtext, runs2, map2, mut inline_maths) = splice_math_text(
        line_start,
        window,
        caret_disp,
        r.text.as_ref(),
        runs,
        r.byte_map.clone(),
        st.fs,
        st.lh,
        wrap_w.max(1.),
        &base,
        &math_font,
        math_color(theme),
        &code_ranges,
        &|key| match host.math_slot(key) {
            Some(MathSlot::Ready(img, w, h)) => Some((img, w, h)),
            _ => None,
        },
    );
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
        align_offsets: Vec::new(),
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

/// EPUB 行对齐的逐可视行水平移量（px）：相对**内容盒**（`avail`，= 帧宽 − 两内衬）
/// 按行宽右/中推，`row.x_off`（额外左缩进，标题 gutter 等）已计入，使齐平的几何以
/// 「内容盒中心/右缘」为基准：`Right` = (avail − 行宽) − x_off、`Center` =
/// (avail − 行宽)/2 − x_off；不足（行过期宽/行外缩进吞噬）时钳到 0（保持左对齐）。
/// 对齐来源 = `row.align`（`%%ALIGN:X%%` 指令）优先；EPUB 阅读模式下**标题**默认居中
/// （对齐 A 侧 `row.rs` 的 `align.or(if epub { 标题居中 })`）。普通行（无对齐）恒空。
pub fn align_offsets_for(
    kind: IrKind,
    row_align: Option<RowAlign>,
    epub: bool,
    wrapped: &std::rc::Rc<Vec<gpui::WrappedLine>>,
    avail: f32,
    x_off: f32,
) -> Vec<Pixels> {
    let align = match row_align {
        Some(a) => Some(a),
        None if epub && matches!(kind, IrKind::Heading(_)) => Some(RowAlign::Center),
        None => None,
    };
    let factor = match align {
        Some(RowAlign::Right) => 1.0f32,
        Some(RowAlign::Center) => 0.5f32,
        _ => return Vec::new(),
    };
    if avail <= 0. {
        return Vec::new();
    }
    wrapped
        .iter()
        .map(|wl| {
            let slack = ((avail - f32::from(wl.width())) * factor - x_off).max(0.);
            px(slack)
        })
        .collect()
}

/// 文本行的装饰化入口（在 [`shape_text_row`] 之上叠加：警报标记剥除、列表/任务
/// gutter、可点击链接、行内图片）。需要源文本以识别构造，故把 `host` 穿进来。
pub fn shape_text_row_decorated(
    mut r: DocRow,
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    window: &mut Window,
    width: Pixels,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    let raw = host.doc().line_str(r.src_line);
    style_links_in_runs(&mut r, &raw);
    let mut alert: Option<RowGutter> = None;
    if r.kind == IrKind::Quote
        && let Some(p) = inline_style::blockquote_prefix(&raw)
    {
        let body = &raw[p..];
        let trimmed = body.trim_start();
        if let Some((akind, consumed, fold)) = inline_style::alert_prefix(trimmed) {
            let n = consumed.min(r.text.len());
            strip_prefix_bytes(&mut r, n);
            alert = Some(RowGutter::Alert { kind: akind, fold });
        }
    }
    let mut bullet_g: Option<RowGutter> = None;
    if r.kind == IrKind::Bullet
        && let Some(g) = bullet_gutter(&raw)
    {
        bullet_g = Some(g);
    }
    let pad = f32::from(px(config::markdown::CONTENT_EDGE_PAD_X_PX));
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
        QUOTE_TEXT_INDENT_PX
    } else {
        0.
    };
    let wrap_w = (f32::from(width) - 2. * pad - gutter_x).max(1.);
    let mut srow = shape_text_row(rd, host, r.clone(), window, width, theme, base_font, wrap_w);
    if let Some(g @ RowGutter::Alert { .. }) = &alert {
        srow.extra.gutter = Some(g.clone());
        srow.x_off = px(84.);
    }
    if let Some(g) = bullet_g {
        let is_check = matches!(g, RowGutter::Check { .. });
        srow.extra.gutter = Some(g);
        srow.x_off = if is_check { px(24.) } else { px(18.) };
    }
    if matches!(r.kind, IrKind::Heading(_)) {
        srow.x_off = px(HEADING_GUTTER_W);
    }
    if !srow.align_offsets.is_empty() || r.align.is_some() || host.is_epub() {
        let avail = wrap_w + gutter_x;
        srow.align_offsets =
            align_offsets_for(r.kind, r.align, host.is_epub(), &srow.wrapped, avail, f32::from(srow.x_off));
    }
    if srow.editable {
        srow.extra.links = link_hits(&raw, srow.byte_map.as_slice());
    }
    if !r.notes.is_empty() || !r.note_defs.is_empty() {
        srow.extra.note_markers = note_marker_hits(&r);
    }
    srow.extra.inline_images = inline_image_hits(&r, &srow, &raw);
    srow
}

/// 把渲染文本/样式/映射的前 `n` 个字节剥掉（警报标记行用；`n` 是剥掉后的字节数）。
pub fn strip_prefix_bytes(r: &mut DocRow, n: usize) {
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
pub fn bullet_gutter(raw: &str) -> Option<RowGutter> {
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
pub fn style_links_in_runs(r: &mut DocRow, raw: &str) {
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
pub fn link_hits(raw: &str, byte_map: &[usize]) -> Vec<RowLink> {
    use crate::element::geom::source_to_display;
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

/// 脚注标记 `[n]` 的显示几何（P4-2 注记迁入 WYSIWYG）：区间取 `r.notes`/`r.note_defs`
/// （`extract_notes` 已把 `[^n]`/`[!n]` 剥成 `[n]`，记录的是**相对 `r.text` 的显示
/// 字节范围**）。越界区间直接丢弃，`position_for_index` 也有界外 None 兜底，不会 panic。
pub fn note_marker_hits(r: &DocRow) -> Vec<NoteMarker> {
    let n = r.text.len();
    let mut out = Vec::new();
    for (range, nm) in &r.notes {
        if range.start < range.end && range.end <= n {
            out.push(NoteMarker {
                disp: range.clone(),
                n: *nm,
                is_def: false,
            });
        }
    }
    for (range, nm) in &r.note_defs {
        if range.start < range.end && range.end <= n {
            out.push(NoteMarker {
                disp: range.clone(),
                n: *nm,
                is_def: true,
            });
        }
    }
    out
}

/// 行内图片：`RunKind::Image`（alt 显示区）反向解析源 src（经 byte_map 找 `!` 位，
/// 再在原文尾随 `![…](…)` 取 src）。`r.byte_map` 是「剥标记后、数学 splice 前」的
/// display→源（源值不变）；`srow.byte_map` 是 splice 后的——显示区间统一以**源偏移**
/// 为准重算（两种 map 都成立）。
pub fn inline_image_hits(r: &DocRow, srow: &ShapedRow, raw: &str) -> Vec<InlineImageHit> {
    use crate::element::geom::source_to_display;
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
pub fn shape_code_row(
    r: DocRow,
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
    if lang.as_deref() == Some("mermaid") {
        let source = code_lines.join("\n");
        let zoom = rd
            .borrow()
            .mermaid_zoom
            .borrow()
            .get(&r.src_line)
            .copied()
            .unwrap_or(1.0);
        let height = px(f32::from(mermaid_card_height(&source)) * zoom);
        let area_w = (f32::from(width)
            - 2. * f32::from(px(config::markdown::CONTENT_EDGE_PAD_X_PX)))
        .max(1.);
        let mut mm = mm_layout(window, &MmTheme::from(theme), &source, 0., 0., area_w);
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
            align_offsets: Vec::new(),
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
    let t_parse = crate::prof::begin();
    let code_join = code_lines
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    let hl_runs: Option<CodeLineRuns> = match lang.as_deref() {
        Some(l) if !l.is_empty() && !code_lines.is_empty() => {
            if code_lines.len() > 8 && !l.eq_ignore_ascii_case("toml") {
                match host.code_parse_lookup(l, &code_join) {
                    CodeParseLookup::Ready(runs) => runs,
                    CodeParseLookup::NotReady => {
                        host.request_code_parse(l, &code_join, r.src_line);
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
        x_off: px(config::markdown::CONTENT_EDGE_PAD_X_PX),
        align_offsets: Vec::new(),
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
pub fn shape_table_row(
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
    let height = table_block_height(theme, rows2.len())
        + px(config::markdown::TABLE_SCROLLBAR_RESERVE_PX);
    let mut aligns2 = aligns.clone();
    aligns2.resize(ncols, TableAlign::Left);
    let mappings: Option<Vec<markdown::doc::TableRowMap>> =
        r.table_rows.as_ref().map(|m| m.as_ref().clone());

    let mut trow_edit: Vec<TableRowCellsLayout> = Vec::new();
    let mk_row = |window: &mut Window,
                  src_line: usize,
                  texts: Vec<gpui::SharedString>,
                  maps: &[markdown::doc::TableCellMap],
                  lines: Option<Vec<gpui::ShapedLine>>| {
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
                take_data_line_or_placeholder(window, &mut lines, font_size)
            };
            let w = f32::from(line.width());
            let text_x = match aligns2.get(ci).copied().unwrap_or(TableAlign::Left) {
                TableAlign::Center => ((col_w - w) / 2.).max(0.),
                TableAlign::Right => (col_w - w - 10.).max(0.),
                _ => 10.,
            };
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
        align_offsets: Vec::new(),
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

/// 数据行逐格搬用 shape 期唯一一次产物（`rows_shapes` 与 `rows2` 同构）。
pub fn take_data_line_or_placeholder(
    window: &mut Window,
    lines: &mut dyn Iterator<Item = gpui::ShapedLine>,
    font_size: Pixels,
) -> gpui::ShapedLine {
    match lines.next() {
        Some(line) => line,
        None => window
            .text_system()
            .shape_line(gpui::SharedString::default(), font_size, &[], None),
    }
}

/// 独立图片行的显示宽（px）：有用户拖拽调整（`image_width` 覆写）→ 覆写值钳在
/// `[IMAGE_MIN_W, IMAGE_MAX_W]` 内且不超可用宽；否则自然宽 = `min(avail, IMAGE_MAX_W)`。
pub fn effective_image_w(rd: &RefCell<WyRenderModel>, src: &str, avail: f32) -> f32 {
    let natural = avail.min(config::markdown::IMAGE_MAX_W);
    match rd.borrow().image_widths.borrow().get(src).copied() {
        Some(w) => w
            .clamp(
                config::markdown::IMAGE_MIN_W,
                config::markdown::IMAGE_MAX_W,
            )
            .min(avail)
            .max(1.),
        None => natural,
    }
}

/// 独立图片行的显示高（px）：已加载按真实宽高比换算（宽 = [`effective_image_w`]），
/// 未加载/失败用占位高。
pub fn image_row_height(rd: &RefCell<WyRenderModel>, src: &str, avail: f32) -> Pixels {
    let w = effective_image_w(rd, src, avail);
    match rd.borrow().image_sizes.borrow().get(src).copied() {
        Some(Some((iw, ih))) if iw > 0. && ih > 0. => px((w * ih / iw.max(1.)).max(1.)),
        _ => px(config::markdown::IMAGE_ROW_DEFAULT_H),
    }
}

/// 独立图片块的**基础布局高**（px）：图片盒高（[`image_row_height`]）+ 题注间隙 +
/// 题注行高（`line_h` 按单行估算，整形后 Pass2 以真实折行高回写）。
pub fn image_block_base_height(
    rd: &RefCell<WyRenderModel>,
    src: &str,
    avail: f32,
    line_h: Pixels,
) -> f32 {
    f32::from(image_row_height(rd, src, avail))
        + config::markdown::IMAGE_CAPTION_GAP_PX
        + f32::from(line_h)
}

/// 独立图片行：**可编辑**。布局 = 图片盒（顶部，按真实宽高比/占位高）+
/// 题注（alt 文本行，置于图片之下、水平居中）。图片内容在 paint 期异步加载后画出。
pub fn shape_image_row(
    r: DocRow,
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
    lazy_ensure_image_size(rd, host, src.as_ref());
    let w = effective_image_w(rd, src.as_ref(), avail);
    let img_h = image_row_height(rd, src.as_ref(), avail);
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
    let gap = config::markdown::IMAGE_CAPTION_GAP_PX;
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
        align_offsets: Vec::new(),
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

/// 只读图片头部固有尺寸懒装载（A-4 冷开惰性化）：`x_image_sizes` 已含该 src → 短路；
/// 否则经 `host.image_intrinsic_size`（EPUB 内存图字节 / 本地文件头只读）拿一次真实
/// 宽高比并写入 `rd.image_sizes`。宿主原 `WyEditor::lazy_ensure_image_size` 逐字语义。
pub fn lazy_ensure_image_size(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    src: &str,
) {
    if rd.borrow().image_sizes.borrow().get(src).is_some() {
        return;
    }
    if let Some((w, h)) = host.image_intrinsic_size(src) {
        rd.borrow_mut()
            .image_sizes
            .borrow_mut()
            .insert(src.into(), Some((w, h)));
    }
}

/// 把一个物化的 `DocRow` 真正 shape 成可视行（仅带内块走这里）——按块类型分派。
pub fn shape_docrow(
    r: DocRow,
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    window: &mut Window,
    width: Pixels,
    avail: f32,
    theme: &WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    match r.kind {
        IrKind::Code => shape_code_row(r, rd, host, window, theme, width),
        IrKind::Table => shape_table_row(r, window, theme.font_size, theme, base_font),
        IrKind::Image => shape_image_row(r, rd, host, window, theme, base_font, avail),
        _ => shape_text_row_decorated(r, rd, host, window, width, theme, base_font),
    }
}

/// 数学块卡片的显示行：源码按换行 + LaTeX `\\`（行断）切成多行，逐段做 Unicode
/// 排版。`math_card_height` 与 `paint_card` 必须用同一函数 → 高与绘制逐行对应。
pub fn math_display_lines(src: &str) -> Vec<String> {
    use markdown_wysiwyg_math::math_to_unicode;
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
/// Loading/Ready/Failed 无关，**单值稳定**。就绪位图按「自然宽 + 块内横向滚动」画。
pub fn math_card_height(rd: &RefCell<WyRenderModel>, reg: &MathRegion) -> Pixels {
    px(20. + math_height_estimate(rd, reg.source.as_str()))
}

/// 源码显示行的**排版高估计**：按 `math_display_lines` 同拆分（`\n`/`\\`）逐段，
/// 含高算子（∫/∑/lim/frac/sqrt 等）的段用 `MATH_DISPLAY_LINE_TALL_H`、否则
/// `MATH_DISPLAY_LINE_H`。
pub fn math_line_height_h(src: &str) -> f32 {
    use config::math;
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

/// 数学源码**显示行→卡高估计**缓存（键=源）：`math_line_height_h` 对每源做
/// LaTeX→Unicode 转换与高算子扫描，3000 块每次 `fold_plan` 重建重复 ≈38ms；
/// 按源缓存后 O(1) 复用。自宿主 `WyEditor::math_height_estimate` 迁入（纯 rd 侧）。
pub fn math_height_estimate(rd: &RefCell<WyRenderModel>, src: &str) -> f32 {
    let rd = rd.borrow();
    if let Some(&h) = rd.math_line_map.borrow().get(src) {
        return h;
    }
    let h = math_line_height_h(src);
    let cap = config::limits::MATH_LINE_MAP_CAP;
    if rd.math_line_map.borrow().len() < cap {
        rd.math_line_map.borrow_mut().insert(src.to_string(), h);
    } else if !rd.math_line_map_warned.get() {
        rd.math_line_map_warned.set(true);
        eprintln!(
            "[WY-MATH-LINE-MAP] 互异公式源超过缓存上限 {cap}：甲方为性能回退（每轮重建重算 \
             LaTeX→Unicode 转换），不丢正确性；换源后复位可再加缓存"
        );
    }
    h
}

/// 装配一张数学块卡片行（A1）：整块（含围栏）折叠成一张面板，占据 `height`。
pub fn build_math_card_row(
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
        align_offsets: Vec::new(),
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
