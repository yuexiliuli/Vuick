//! WYSIWYG 绘制层：把 [`super::Frame`] 里已 shape 的行画到窗口（文本/代码面板/表格网格/图片/卡片/选区/横向滚动条）。与 `element` 共用几何，超宽裁剪 + 横向滚动平移同源。
use super::*;
use gpui::{App, Bounds, Hsla, Point, SharedString, TextRun, Window, size, point, px};
use crate::markdown::recognition::AlertKind;
use crate::markdown::inline_style::MathAlign;


/// 画 **已排好** 的 mermaid 布局：`placed`/`edges` 为**卡片相对坐标**（shape 期以
/// area=(0,0) 排好并存进 `row.mm`——paint 不再每帧重排/重测标签宽），绘制时统一加
/// 卡片原点 `(ox, oy)`。生产路径恒由 `row.mm` 携带此布局（`paint_card` 唯一入口）。
pub(crate) fn paint_mermaid_placed(
    window: &mut Window,
    theme: &WyTheme,
    placed: &[MmPlaced],
    edges: &[(usize, usize, Option<String>)],
    ox: f32,
    oy: f32,
    area_w: f32,
    cx: &mut App,
) {
    if placed.is_empty() {
        // 无可解析节点 → 弱化提示。
        let hint: SharedString = "空图 / 渲染器未接入".into();
        let mut reduced = theme.muted;
        reduced.a = 0.55;
        let hrun = [TextRun { len: hint.len(), font: theme.mono.clone(), color: reduced, ..Default::default() }];
        let hline = window.text_system().shape_line(hint, px(12.), &hrun, None);
        let hw = f32::from(hline.width());
        let _ = hline.paint(
            point(px(ox + (area_w - hw) / 2.), px(oy + 40.)),
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
    for (ia, ib, elab) in edges {
        let (pa, pb) = (&placed[*ia], &placed[*ib]);
        let ax = ox + pa.x + pa.w / 2.;
        let ay = oy + pa.y + pa.h;
        let bx = ox + pb.x + pb.w / 2.;
        let by = oy + pb.y;
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
    for p in placed {
        let r = match p.shape {
            MmShape::Circle => p.h / 2.,
            MmShape::Round => 10.,
            MmShape::Diamond => 4.,
            MmShape::Rect => 6.,
        };
        let b = Bounds::new(point(px(ox + p.x), px(oy + p.y)), size(px(p.w), px(p.h)));
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
            point(px(ox + p.x + (p.w - tw) / 2.), px(oy + p.y + (p.h - f32::from(label_h)) / 2.)),
            label_h,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}



/// paint 版表格单元格绘制（复用 [`super::TableCellLayout`] 在 shape 期排好的单行；
/// 几何与 `shape_table_row` 的 `text_x`/纵向居中一致）——**不再每帧重 shape 单元格**。
fn draw_table_row_cached(
    window: &mut Window,
    cx: &mut App,
    cells: &[super::TableCellLayout],
    widths: &[f32],
    x0: f32,
    row_top_y: f32,
    row_h: f32,
    theme: &WyTheme,
) {
    let text_lh = f32::from(theme.para_line());
    let ty = row_top_y + ((row_h - text_lh) / 2.).max(0.);
    let mut acc = 0f32;
    for (ci, cell) in cells.iter().enumerate() {
        let col_w = widths.get(ci).copied().unwrap_or(0.);
        let _ = cell.line.paint(
            point(px(x0 + acc + cell.text_x), px(ty)),
            px(text_lh),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
        acc += col_w;
    }
}

/// 画表格块网格：外框 + 表头背景 + 行/列分隔线 + 单元格文本（配色对齐预览）。
/// 内容超宽（`total_w` > 内容区宽）时裁剪在内容区内并按横向滚动偏移平移
/// （`table_h_scroll`，paint 平移 ↔ 命中/光标反向加回），底部覆横向滚动条。
pub(crate) fn paint_table_row(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    t: &ShapedTable,
    window: &mut Window,
    cx: &mut App,
) {
    let (hgeo, scroll) = {
        let ed = editor.read(cx);
        (table_hscroll_geo(ed, f, row), table_hscroll_clamped(ed, f, row))
    };
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let total_w = t.total_w.max(1.);
    let x0 = f32::from(origin.x) + f32::from(f.padding) - scroll;
    let top0 = f32::from(origin.y) + f32::from(row.top);
    let row_h_f = f32::from(t.row_h);
    let ncols = t.header_texts.len().max(1);
    let n_rows = t.rows_texts.len();
    let block_h = f32::from(table_block_height(theme, n_rows));
    // 网格裁剪在内容区内（横向滚动时把超出的网格/文字藏进内容区，不再溢到页面背景上），
    // 只裁**网格高度**——底部滚动条预留带（`row.height − block_h`）不属于表格内容，
    // 滚动条画在那里、不遮内容（preview `markdown/view` 的 `.pb` 同款）。
    let content_rect = Bounds::new(
        point(origin.x + f.padding, origin.y + row.top),
        size(px(viewport), px(block_h)),
    );
    let border: gpui::Hsla = theme.divider;
    // 超宽表格裁剪在内容区内（横向滚动时把超出的网格/文字藏进内容区，不再溢到
    // 页面背景上），与代码块 `paint_code_row` 的 `with_content_mask` 同手法。
    window.with_content_mask(Some(gpui::ContentMask { bounds: content_rect }), |window| {
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
        // 单元格文本画 shape 期排好的 `table_edit` 单行（免每帧重 shape）。`shape_table_row`
        // 恒推表头行 → `table_edit` 恒 Some（不变量，无兜底重推对齐公式的漂移路径）。
        let te = row.table_edit.as_ref().expect("表格行恒有逐单元格编辑排版");
        if let Some(hrow) = te.rows.first() {
            draw_table_row_cached(window, cx, &hrow.cells, &t.widths, x0, cy, row_h_f, theme);
        }
        cy += row_h_f;
        for (ri, _cells) in t.rows_texts.iter().enumerate() {
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
            if let Some(drow) = te.rows.get(1 + ri) {
                draw_table_row_cached(window, cx, &drow.cells, &t.widths, x0, cy, row_h_f, theme);
            }
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
    });
    // 内容超宽 → 底部横向滚动条（与代码块同款）。
    if let Some(geo) = hgeo {
        paint_hscroll_thumb(origin, theme, row, geo, window);
    }
}



/// 画底部横向滚动条滑块（view 的 gpui-component 同款：圆角滑块 8 高/最小 48 宽；
/// 轨道可点击跳转、滑块可拖）。代码块/表格共用。
/// `geo` = `code_hscroll_geo`/`table_hscroll_geo` 的结果。
/// **只画滑块，不画整条轨道背景**（用户要求滚动条区域无背景色——轨道区保持透明，
/// 只有滑块/可交互带；轨道跳转命中仍按整条 `CODE_HSCROLL_BAND` 带判定）。
pub(crate) fn paint_hscroll_thumb(
    origin: Point<Pixels>,
    theme: &WyTheme,
    row: &ShapedRow,
    geo: (f32, f32, f32, f32, f32),
    window: &mut Window,
) {
    let (_bar_x, _bar_w, thumb_x, thumb_w, _) = geo;
    let track_h = crate::config::layout::CODE_HSCROLL_BAND;
    let track_top = f32::from(row.top) + f32::from(row.height) - track_h;
    let inset = 4.;
    let th = crate::config::layout::CODE_HSCROLL_THUMB_H;
    let ty = track_top + inset;
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



/// 代码块：圆角面板（对齐预览：外层 `px_3` → 面板贴**内容区左缘**（=content 边距，
/// 与正文对齐），而非再缩进）；行文本再 `pl_3`（面板内 12px 内衬，走 `row.x_off`，
/// 命中/光标/选区同 x）。首/末各留一个 code_line 空白带。
/// 长代码行（nowrap 超宽）**裁剪在面板内** + 底部横向滚动条（视图本地，可拖动）。
pub(crate) fn paint_code_row(
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
        for wl in row.wrapped.iter() {
            let o = point(text_x - px(scroll), origin.y + sub_top);
            let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
            sub_top += row.lh * (wl.wrap_boundaries().len() + 1) as f32;
        }
    });
    // 内容超宽 → 底部横向滚动条（view 的 gpui-component 同款：轨道 16 高 +
    // 内衬 4 + 圆角滑块 8 高/最小 48 宽；点击轨道跳转、滑块可拖）。覆在末空白带上。
    if let Some(geo) = hgeo {
        paint_hscroll_thumb(origin, theme, row, geo, window);
    }
}



/// 图片行占位/失败兜底：浅底细边框盒 + 居中 alt 文字（加载完成前的占位与加载失败体验）。
pub(crate) fn paint_image_fallback(
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
pub(crate) fn paint_image_row(
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
pub(crate) fn paint_inline_code_pills(
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
pub(crate) fn paint_text_row(
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
pub(crate) fn paint_quote_band(
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
pub(crate) fn alert_color(theme: &WyTheme, kind: AlertKind) -> Hsla {
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
pub(crate) fn paint_row_gutter(
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
pub(crate) fn paint_alert_chrome(
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
pub(crate) fn paint_heading_chevron(
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
    let key = editor.read(cx).line_text(row.src_line).trim().to_string();
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
pub(crate) fn paint_inline_maths(
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
    let mcolor = crate::markdown::wysiwyg::math_color(theme);
    for im in &row.extra.inline_maths {
        let Some(p) = w0.position_for_index(im.display_off, row.lh) else { continue };
        let active = match editor
            .read(cx)
            .math_slots()
            .borrow()
            .get(crate::markdown::wysiwyg::inline_math_key(im.latex.trim()).as_ref())
            .cloned()
        {
            Some(crate::markdown::wysiwyg::MathSlot::Ready(img, w, h)) => Some((img, w, h)),
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
pub(crate) fn paint_inline_images(
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
pub(crate) fn paint_card(
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
            if let Some(crate::markdown::wysiwyg::MathSlot::Ready(img, w, h)) = slot {
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
            let mcolor = crate::markdown::wysiwyg::math_color(theme);
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
        BlockCard::Mermaid { .. } => {
            // 左上角小标签（弱化）。
            let tag: SharedString = "mermaid".into();
            let mut muted = theme.muted;
            muted.a = 0.7;
            let trun = [TextRun { len: tag.len(), font: theme.font.clone(), color: muted, ..Default::default() }];
            let tline = window.text_system().shape_line(tag, px(11.), &trun, None);
            let _ = tline.paint(point(px(x0 + 12.), px(top0 + 6.)), px(14.), TextAlign::Left, None, window, cx);
            // 矢量图渲染：圆角节点框 + 正交连线/箭头/边标。`row.mm` 恒为 shape 期算好的
            // 布局（卡片相对坐标，mermaid 卡片唯一构造点 `shape_code_row` 设之）——
            // paint 不再每帧重排/重测全部标签宽，只加卡片原点绘制。
            let mm = row.mm.as_ref().expect("mermaid 卡片恒有缓存的已排布局");
            paint_mermaid_placed(window, theme, &mm.0, &mm.1, x0, top0, content_w, cx);
        }
    }
}



/// 当前选区（源字节归一化区间）；无选区返回 `None`。
pub(crate) fn editor_selection(editor: &gpui::Entity<WyEditor>, cx: &mut App) -> Option<Range<usize>> {
    editor.read(cx).selection()
}



/// 选区高亮：对可编辑且与源选区相交的行，按字形几何逐可视行画半透明 accent 矩形。
/// 与光标共用 `x_off`/`lh`，因此引述缩进里的选中文字也高亮对齐。
pub(crate) fn paint_selection(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    _theme: &WyTheme,
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
