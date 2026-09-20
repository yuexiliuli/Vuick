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

use super::{CaretVis, WyEditor};
use crate::markdown::parser::{BlockMeta, RowKind, RunKind};

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
}

pub(crate) struct ShapedRow {
    pub src_line: usize,
    pub src_lines: Range<usize>,
    /// 该块的可视行（`shape_text` 结果；一个块 = 一个或多个 `WrappedLine`）。
    /// 带外块为空（未 shape，paint 跳过）。
    pub wrapped: Vec<gpui::WrappedLine>,
    pub sub_heights: Vec<Pixels>,
    pub byte_map: std::rc::Rc<Vec<usize>>,
    pub editable: bool,
    pub height: Pixels,
    pub top: Pixels,
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

/// 文本块的高度缓存键：`(内容代数, 源行, 类型, shaping 宽)`。
fn height_cache_key(rgen: u64, src_line: u32, kind: RowKind, width_bits: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rgen.hash(&mut h);
    src_line.hash(&mut h);
    kind_tag(kind).hash(&mut h);
    width_bits.hash(&mut h);
    h.finish()
}

/// 带外（且从未整形过）文本块的高度估算：用**真实字数**（`Doc::line_chars_ascii`）
/// 与近似字符宽（ASCII ≈ 0.5 字高、含 CJK ≈ 0.95 字高）估可视行数，尽量贴合真实
/// shape，使带外/带内高度差 ≈ 0（滚动带边界不再跳「字体大小」）；代码/表格用
/// `aux`（行数）* 行高；空行/图片/分隔线固定一行。只作兜底——整形过的块走缓存真值。
fn estimate_height(
    meta: &BlockMeta,
    chars: usize,
    ascii: bool,
    font_size: Pixels,
    avail: f32,
    line_h: Pixels,
) -> Pixels {
    match meta.kind {
        RowKind::Code | RowKind::Table => line_h * meta.aux.max(1) as f32,
        RowKind::Blank | RowKind::Image | RowKind::Rule => line_h,
        _ => {
            let char_w = if ascii {
                f32::from(font_size) * 0.5
            } else {
                f32::from(font_size) * 0.95
            };
            let cpl = (avail / char_w.max(1.)).max(1.);
            let lines = (chars as f32 / cpl).ceil().max(1.);
            line_h * lines
        }
    }
}

/// 把一个物化的 `DocRow` 真正 shape 成可视行（仅带内块走这里）。
fn shape_docrow(
    r: crate::markdown::doc::DocRow,
    window: &mut Window,
    width: Pixels,
    font_size: Pixels,
    line_h: Pixels,
    theme: &super::WyTheme,
    base_font: &gpui::Font,
) -> ShapedRow {
    let runs = runs_to_text_runs(
        r.text.as_ref(),
        &r.runs,
        base_font,
        theme.text,
        &theme.mono,
        theme.accent,
        theme.code_bg,
    );
    let wrapped = window
        .text_system()
        .shape_text(r.text.clone(), font_size, &runs, Some(width), None)
        .map(|ls| ls.into_vec())
        .unwrap_or_default();
    let mut height = px(0.);
    let mut sub_heights = Vec::with_capacity(wrapped.len());
    for wl in &wrapped {
        let sh = line_h * (wl.wrap_boundaries().len() + 1) as f32;
        height += sh;
        sub_heights.push(sh);
    }
    if height == px(0.) {
        height = line_h;
        sub_heights.push(line_h);
    }
    let editable = !r.byte_map.is_empty();
    ShapedRow {
        src_line: r.src_line,
        src_lines: r.lines.clone(),
        wrapped,
        sub_heights,
        byte_map: std::rc::Rc::new(r.byte_map),
        editable,
        height,
        top: px(0.),
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
    let font_size = theme.font_size;
    let line_h = font_size * 1.5;
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
                height: line_h,
                top: px(0.),
            }],
            total: line_h,
            band,
            width,
            rgen,
        };
    }

    let (band_lo, band_hi) = band;
    let avail = (f32::from(width) - f32::from(padding) * 2.).max(1.);
    let width_bits = f32::from(width).to_bits();

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
            RowKind::Paragraph | RowKind::Heading(_) | RowKind::Quote | RowKind::Bullet => {
                Some(height_cache_key(rgen, meta.src_line, meta.kind, width_bits))
            }
            _ => None,
        };
        let h = match cache_key.and_then(|k| editor.cached_height_lines(k)) {
            Some(lines) => line_h * lines as f32,
            None => {
                let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
                estimate_height(&meta, chars, ascii, font_size, avail, line_h)
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
            let srow = shape_docrow(r, window, width, font_size, line_h, theme, &base_font);
            // 缓存该块真实可视行数（带外再遇此块时沿用真值 → 不跳）。
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
                    height: h,
                    top: total,
                }
            }
        };
        total += sr.height;
        rows.push(sr);
    }
    Frame { padding, line_h, rows, total, band, width, rgen }
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

/// 源 caret → 光标条几何。非文本行停在块首（只读）。
fn caret_geometry(
    editor: &WyEditor,
    rows: &[ShapedRow],
    line_h: Pixels,
) -> (bool, Pixels, Pixels) {
    let (cl, col) = editor.row_col(editor.caret());
    let line_start = editor.line_start(cl);
    let line_end = editor.line_end(cl);
    for r in rows {
        if !r.src_lines.contains(&cl) {
            continue;
        }
        if r.editable {
            let target = col.min(line_end.saturating_sub(line_start));
            // 源列 → 渲染列（`source_to_display`，左界）——与 `source_offset_at`
            // 的反查互为逆映射，光标才落在点击处。
            let dcol = source_to_display(&r.byte_map, target);
            // 折行时 p.y 是子行偏移，叠加到块首才算准缓存所在的可视行。
            let p = render_point_of(&r.wrapped, line_h, dcol);
            return (true, px(12.) + p.x, r.top + p.y);
        } else {
            return (false, px(12.), r.top);
        }
    }
    let y = rows.last().map_or(line_h, |r| r.top + r.height);
    (true, px(12.), y)
}

/// 点命中 → 源字节偏移（帧内 byte_map 反查）。
pub(crate) fn source_offset_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    // `MouseDownEvent.position` 是窗口坐标；`frame.rows[*].top`/行内 x 是元素局部
    // （paint 时以 `origin = bounds.origin` 为起点绘制），先把命中换算回局部——
    // 否则元素被 `live-body` 内边距/滚动/窗口位置偏移后，点击会映射到错误的行/列。
    let origin = editor.last_bounds().map(|b| b.origin).unwrap_or_else(|| point(px(0.), px(0.)));
    let rel_y = f32::from(p.y) - f32::from(origin.y);
    let rel_x = (f32::from(p.x) - f32::from(origin.x) - f32::from(frame.padding)).max(0.);
    let line_h = frame.line_h;
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if rel_y >= top && rel_y < bottom {
            if r.editable {
                if let Some(w0) = r.wrapped.first() {
                    // y 用点击在块内的**子行偏移**（`rel_y - top`）：`closest_index_for_position`
                    // 据此选折行的第几可视行——写成 0 会把第二行的点击映射回第一行。
                    let rel = point(px(rel_x), px(rel_y - top));
                    // `closest_index_for_position` 的 `Err` 是越界钳制后的有效索引
                    // （点过行尾/行首）；把它当 0 会让点行尾时光标跳回行首。
                    let dcol = match w0.closest_index_for_position(rel, line_h) {
                        Ok(i) | Err(i) => i,
                    };
                    let local = display_to_source(&r.byte_map, dcol);
                    let ls = editor.line_start(r.src_line);
                    let le = editor.line_end(r.src_line);
                    return (ls + local).min(le);
                }
            } else {
                return editor.line_end(r.src_line).max(editor.line_start(r.src_line));
            }
        }
    }
    editor.caret()
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
            // 帧缓存：内容/宽度/可视带都没变 → 直接复用总高，不再整篇重建
            // （大文档的每帧“卡死”就是这里：每次 notify 都重排全 16 万行）。
            let rgen = e.content_gen();
            let band = e.shape_band().unwrap_or((0., 6500.));
            let cached_total = e
                .frame
                .borrow()
                .as_ref()
                .filter(|f| f.rgen == rgen && f.width == width && f.band == band)
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

        // 带移动 / 内容 / 宽度变化 → 按新带重建窗口化帧（大文档滚动只补新入带的
        // shape，其余带外块估算占位，O(n) 只有廉价算术）。
        let rgen = self.editor.read(cx).content_gen();
        let width = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map(|f| f.width).unwrap_or(px(800.))
        };
        let stale = {
            let f = self.editor.read(cx).frame.borrow();
            f.as_ref().map_or(true, |f| f.rgen != rgen || f.width != width || f.band != band)
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
            let (on, x, ctop) = caret_geometry(&*editor, &f.rows, f.line_h);
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
            // 带外（未 shape）行不可见：跳过文本与背景，避免为整篇占位块画屏。
            if row.wrapped.is_empty() {
                continue;
            }
            let mut sub_top = row.top;
            for wl in &row.wrapped {
                let o = point(origin.x + f.padding, origin.y + sub_top);
                if !row.editable {
                    window.paint_quad(gpui::PaintQuad {
                        bounds: Bounds::new(o, size(f.padding + wl.width(), f.line_h)),
                        corner_radii: gpui::Corners::all(px(4.)),
                        background: theme.code_bg.into(),
                        border_widths: gpui::Edges::default(),
                        border_color: gpui::Hsla::default(),
                        border_style: gpui::BorderStyle::Solid,
                    });
                }
                let _ = wl.paint(o, f.line_h, gpui::TextAlign::Left, None, window, cx);
                sub_top += f.line_h * (wl.wrap_boundaries().len() + 1) as f32;
            }
        }
        // 光标几何每帧按 caret 现算（帧缓存不含 caret，改 caret 无需整帧重建）。
        let (caret_on, caret_x, caret_top) = {
            let e = self.editor.read(cx);
            caret_geometry(&*e, &f.rows, f.line_h)
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
        WyEditor,
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
            let (on, _, caret_top) = caret_geometry(ed.read(cx), &frame.rows, frame.line_h);
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
}

