//! WYSIWYG 绘制层（自宿主 `wysiwyg/element/paint.rs` 迁入引擎）：把 [`crate::element::Frame`]
//! 里已 shape 的行画到窗口（文本/代码面板/表格网格/图片/卡片/选区/横向滚动条）。
//!
//! 迁移 seam：原 `editor: &gpui::Entity<WyEditor>` 一律改持 `rd: &RefCell<WyRenderModel>` +
//! `host: &dyn WyRenderHost` 成对入参（原 `editor.read(cx).X` → `host.X()`/`rd.borrow()`；
//! 原 `editor.update(cx, |e,_| e.set_*())` → 直接经 `rd` 内写，语义逐字保持：`images_gen`/
//! `math_gen` 自增 → 下帧整帧重建）。纯图元函数（`paint_text_row` 等）保留原签名随迁。

// 绘制内核逐参数传参 → 放行
#![allow(clippy::too_many_arguments)]

use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

use gpui::{App, Bounds, Corners, Edges, Hsla, Point, Pixels, RenderImage, SharedString, TextAlign, TextRun, Window, fill, point, px, size};

use crate::element::*;
use crate::element::geom::{first_band_center_y, text_row_shift, text_y_off};
use crate::frame::{set_image_size, set_math_natural_w, set_math_pin};
use crate::hit::{
    code_hscroll_clamped, code_hscroll_geom_inner, image_box_local, image_edit_button_local,
    image_resize_handle_window, inline_image_box_local, inline_image_edit_button_local,
    math_hscroll_clamped, math_hscroll_geom_inner, math_natural_w, note_marker_src_range,
    table_hscroll_clamped, table_hscroll_geom_inner, HScrollGeom,
};
use crate::model::WyRenderModel;
use crate::shape::{math_display_lines, table_block_height};
use crate::{WyRenderHost, docutil, theme::WyTheme, theme::math_color};
use markdown::parse::inline_style::MathAlign;
use markdown::parse::recognition::AlertKind;
use markdown_wysiwyg_math::{MathPos, inline_math_key, math_to_segments};
use markdown_wysiwyg_mermaid::{MmTheme, paint_mermaid_layout};

// Mermaid 绘制已迁至独立 crate `markdown_wysiwyg_mermaid` 的 `paint`。

/// paint 版表格单元格绘制（复用 [`super::TableCellLayout`] 在 shape 期排好的单行）。
#[allow(clippy::too_many_arguments)]
fn draw_table_row_cached(
    window: &mut Window,
    cx: &mut App,
    cells: &[TableCellLayout],
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
#[allow(clippy::too_many_arguments)]
pub fn paint_table_row(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    t: &ShapedTable,
    window: &mut Window,
    cx: &mut App,
) {
    let hgeo = table_hscroll_geom_inner(rd, host, f, row);
    let scroll = table_hscroll_clamped(rd, host, f, row);
    // 网格视口以**表格内容左缘**起算（`padding + x_off`，引号内表格缩进）：遮罩
    // 同步左移并收窄，保证①滚动条对齐表格边缘②内容拖拽不盖引号竖条③右缘可达
    // （2026-11-13 真机复查 #1/#2/#3，与 `table_hscroll_geom_inner` 同口径）。
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2. - f32::from(row.x_off)).max(1.);
    let total_w = t.total_w.max(1.);
    let x0 = f32::from(origin.x) + f32::from(f.padding) + f32::from(row.x_off) - scroll;
    let top0 = f32::from(origin.y) + f32::from(row.top);
    let row_h_f = f32::from(t.row_h);
    let ncols = t.header_texts.len().max(1);
    let n_rows = t.rows_texts.len();
    let block_h = f32::from(table_block_height(theme, n_rows));
    let content_rect = Bounds::new(
        point(origin.x + f.padding + row.x_off, origin.y + row.top),
        size(px(viewport), px(block_h)),
    );
    let border: gpui::Hsla = theme.divider;
    window.with_content_mask(
        Some(gpui::ContentMask {
            bounds: content_rect,
        }),
        |window| {
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(point(px(x0), px(top0)), size(px(total_w), px(block_h))),
                corner_radii: gpui::Corners::all(px(6.)),
                background: gpui::Hsla {
                    h: 0.,
                    s: 0.,
                    l: 0.,
                    a: 0.,
                }
                .into(),
                border_widths: gpui::Edges::all(px(1.)),
                border_color: border,
                border_style: gpui::BorderStyle::Solid,
            });
            let inner_lo = top0 + 1.;
            let inner_hi = top0 + block_h - 1.;
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
            debug_assert!(row.table_edit.is_some(), "表格行恒有逐单元格编辑排版");
            let Some(te) = row.table_edit.as_ref() else {
                return;
            };
            if let Some(hrow) = te.rows.first() {
                draw_table_row_cached(window, cx, &hrow.cells, &t.widths, x0, cy, row_h_f, theme);
            }
            cy += row_h_f;
            for (ri, _cells) in t.rows_texts.iter().enumerate() {
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
                    draw_table_row_cached(
                        window,
                        cx,
                        &drow.cells,
                        &t.widths,
                        x0,
                        cy,
                        row_h_f,
                        theme,
                    );
                }
                cy += row_h_f;
            }
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
        },
    );
    if let Some(geo) = hgeo {
        paint_hscroll_thumb(origin, theme, row, geo, window);
    }
}

/// 画底部横向滚动条滑块（代码块/表格共用）。
pub fn paint_hscroll_thumb(
    origin: Point<Pixels>,
    theme: &WyTheme,
    row: &ShapedRow,
    geo: HScrollGeom,
    window: &mut Window,
) {
    let track_h = config::layout::CODE_HSCROLL_BAND;
    let track_top = f32::from(row.top) + f32::from(row.height) - track_h;
    let inset = 4.;
    let th = config::layout::CODE_HSCROLL_THUMB_H;
    let ty = track_top + inset;
    let mut thumb = theme.muted;
    thumb.a = 0.6;
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(
            point(origin.x + px(geo.thumb_x), origin.y + px(ty)),
            size(px(geo.thumb_w), px(th)),
        ),
        corner_radii: gpui::Corners::all(px(th / 2.)),
        background: thumb.into(),
        border_widths: gpui::Edges::default(),
        border_color: gpui::Hsla::default(),
        border_style: gpui::BorderStyle::Solid,
    });
}

/// 代码块：圆角面板（对齐预览）。长代码行 nowrap 超宽裁剪 + 底部横向滚动条。
pub fn paint_code_row(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
        corner_radii: gpui::Corners::all(px(8.)),
        background: theme.code_bg.into(),
        border_widths: gpui::Edges::default(),
        border_color: gpui::Hsla::default(),
        border_style: gpui::BorderStyle::Solid,
    });
    let text_x = panel_x + row.x_off;
    let hgeo = code_hscroll_geom_inner(rd, host, f, row);
    let scroll = code_hscroll_clamped(rd, host, f, row);
    window.with_content_mask(Some(gpui::ContentMask { bounds: panel_rect }), |window| {
        let mut sub_top = row.top + theme.code_line();
        for wl in row.wrapped.iter() {
            let o = point(text_x - px(scroll), origin.y + sub_top);
            let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
            sub_top += row.lh * (wl.wrap_boundaries().len() + 1) as f32;
        }
    });
    if let Some(geo) = hgeo {
        paint_hscroll_thumb(origin, theme, row, geo, window);
    }
}

/// 图片行占位/失败兜底：浅底细边框盒。
pub fn paint_image_fallback(theme: &WyTheme, bounds: Bounds<Pixels>, window: &mut Window) {
    let border: gpui::Hsla = {
        let mut c = theme.divider;
        c.a = 0.6;
        c
    };
    window.paint_quad(gpui::PaintQuad {
        bounds,
        corner_radii: gpui::Corners::all(px(4.)),
        background: theme.code_bg.into(),
        border_widths: gpui::Edges::all(px(1.)),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
}

/// 行内图「加载中/失败」的占位框（**窗口坐标**）：把行内图显示盒 `b`（元素局部）平移到
/// 窗口坐标、尺寸不变（与独立图 [`paint_image_fallback`] 同视觉；P1-9 修复前失败静默
/// 不画占位）。
fn inline_image_placeholder_bounds(
    b: Bounds<Pixels>,
    origin: Point<Pixels>,
) -> Bounds<Pixels> {
    Bounds::new(point(origin.x + b.left(), origin.y + b.top()), b.size)
}

/// 图片 `image_sizes` 状态 → 是否**跳过**重新加载（直接画占位、不碰 cache）。
///
/// P1-9 修复前：`Some(None)`（一次加载已失败）被短路 → 失败「一次性、永不复测」。
/// 修复后：**任何状态都不跳过**——失败标记只用于布局占位高（`image_row_height` 走
/// 占位分支），paint 每帧都会重新走 `AnyImageCache::load`（成功图命中缓存重画、
/// 失败图重试）。
///
/// **现状修正（2026-11-16 评审 K173 口径）**：「每帧重试」当下**不可达**——`load`
/// 的失败结果被 gpui `CachedLoad`（vendor asset_cache.rs「只完成一次」的状态机）
/// **永久缓存**，且 wysiwyg 侧无 `cache.remove`/`clear` 失效路径 → 失败后每帧 `load`
/// 均同步返回缓存 Err、从不真正重取。本函数保留为将来「节流/重试」的策略集中点
/// （要节流时在此返回 true 并画占位；失效路径打开后此处即成恢复探测开关）。
fn image_should_skip_reload(_state: Option<Option<(f32, f32)>>) -> bool {
    false
}

/// 画图片行：经 `AnyImageCache` 异步加载已就绪的 `RenderImage` 后画出；
/// 加载中/失败画占位框。加载成功即把真实宽高比尺寸写回 `rd`（`images_gen` 自增 →
/// 下帧重建帧以真实行高布局）。
///
/// 失败路径现状（K173 口径）：`CachedLoad` 永久缓存 Err（见 [`image_should_skip_reload`]
/// 文档），失败后每帧 `load` 同步返回缓存 Err，直接画占位、不真正重取；成功图每帧
/// `load` 命中缓存重画，开销可忽略。
#[allow(clippy::too_many_arguments)]
pub fn paint_image_row(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
    let state = rd.borrow().image_sizes.borrow().get(img.src.as_ref()).copied();
    if image_should_skip_reload(state) {
        paint_image_fallback(theme, bounds, window);
    } else {
        let loaded: Option<std::sync::Arc<gpui::RenderImage>> = {
            let mem = host.memory_image(img.src.as_ref());
            match &mem {
                Some(image) => image.clone().get_render_image(window, cx),
                None => {
                    let resource = host.image_load_resource(img.src.as_ref());
                    let cache = host.image_cache();
                    let anycache: gpui::AnyImageCache = cache.clone().into();
                    match anycache.load(&resource, window, cx) {
                        Some(Ok(data)) => Some(data),
                        Some(Err(_)) => {
                            set_image_size(rd, host.doc(), img.src.clone(), None);
                            None
                        }
                        None => None,
                    }
                }
            }
        };
        match loaded {
            Some(data) => {
                let img_size = data.size(0);
                let iw = img_size.width.0 as f32;
                let ih = img_size.height.0 as f32;
                if iw > 0. && ih > 0. {
                    let w = img.w.max(1.);
                    let h = (w * ih / iw).max(1.);
                    // 仅在「从未写大小」时回写（`Some(None)` 失败标记/`Some(Some(..))`
                    // 已知尺寸均跳过，防每帧重复写 `images_gen` 自增）。
                    // ⚠️ 潜伏隐患留档：当且仅当未来失效路径打开、先前失败（`Some(None)`）
                    // 的图恢复加载成功时，此处 `state.is_none()` 为 false → 不回写真实
                    // 尺寸 → 行高滞留占位高/旧失败高（`gently` 下无自愈）。当前失败被
                    // `CachedLoad` 永久缓存、不会恢复成功（见 [`image_should_skip_reload`]
                    // 文档），故不实现恢复逻辑（K174 口径：实现前须先开失效路径 + 红测）。
                    if state.is_none() {
                        set_image_size(rd, host.doc(), img.src.clone(), Some((w, h)));
                    }
                    let b = Bounds::new(point(px(x0), px(top0)), size(px(w), px(h)));
                    let _ = window.paint_image(b, b, gpui::Corners::all(px(4.)), data, 0, false);
                } else {
                    paint_image_fallback(theme, bounds, window);
                }
            }
            None => paint_image_fallback(theme, bounds, window),
        }
    }
    let hovered_or_selected = host.hover_image() == Some(row.src_line)
        || host.selected_image() == Some(row.src_line);
    if host.is_editable()
        && hovered_or_selected
        && let Some(bb) = image_box_local(f, row)
    {
        paint_image_edit_button(origin, image_edit_button_local(&bb), window);
        let hb = image_resize_handle_window(origin, &bb);
        window.paint_quad(gpui::PaintQuad {
            bounds: hb,
            corner_radii: gpui::Corners::all(px(3.)),
            // 图片缩放柄 = 黑玻璃 × 白细边（浮在图上、专属控件色；token 见
            // `ui::content_colors::WY_GLASS_*`）
            background: ui::content_colors::WY_GLASS_HANDLE_BG.into(),
            border_widths: gpui::Edges::all(px(1.)),
            border_color: ui::content_colors::WY_GLASS_BORDER,
            border_style: gpui::BorderStyle::Solid,
        });
    }
    if host.selected_image() == Some(row.src_line)
        && let Some(bb) = image_box_local(f, row)
    {
        paint_image_selection_outline(origin, &bb, theme, window);
    }
    paint_text_row(origin, theme, f, row, window, cx);
}

/// 画图片编辑按钮（「⋮」三点，图片右上角）。
fn paint_image_edit_button(
    origin: Point<Pixels>,
    btn_local: Bounds<Pixels>,
    window: &mut Window,
) {
    let btn = Bounds::new(
        point(origin.x + btn_local.left(), origin.y + btn_local.top()),
        btn_local.size,
    );
    window.paint_quad(gpui::PaintQuad {
        bounds: btn,
        corner_radii: gpui::Corners::all(px((f32::from(btn.size.width) * 0.25).min(6.))),
        // 图片编辑钮 = 黑玻璃 × 白细边（专属控件色；token 见 `ui::content_colors::WY_GLASS_*`）
        background: ui::content_colors::WY_GLASS_BTN_BG.into(),
        border_widths: gpui::Edges::all(px(1.)),
        border_color: ui::content_colors::WY_GLASS_BORDER,
        border_style: gpui::BorderStyle::Solid,
    });
    let dot_size: f32 = 3.;
    let dot_gap: f32 = 2.;
    let dot_count: usize = 3;
    let span = dot_count as f32 * dot_size + (dot_count - 1) as f32 * dot_gap;
    let step = dot_size + dot_gap;
    let dot = size(px(dot_size), px(dot_size));
    let btn_size = f32::from(btn.size.width);
    let dot_x = btn.left() + px((btn_size - dot_size) / 2.);
    let dot_y0 = btn.top() + px((btn_size - span) / 2.);
    for k in 0..dot_count {
        window.paint_quad(gpui::PaintQuad {
            bounds: Bounds::new(point(dot_x, dot_y0 + px(k as f32 * step)), dot),
            corner_radii: gpui::Corners::all(px(dot_size / 2.)),
            // 编辑钮三点 = 纯黑不透明（专属控件色；token 见 `ui::content_colors::WY_GLASS_DOT`）
            background: ui::content_colors::WY_GLASS_DOT.into(),
            border_widths: gpui::Edges::default(),
            border_color: gpui::Hsla::default(),
            border_style: gpui::BorderStyle::Solid,
        });
    }
}

/// 画图片**选中描边**（accent 2px 圆角）。
fn paint_image_selection_outline(
    origin: Point<Pixels>,
    bb_local: &Bounds<Pixels>,
    theme: &WyTheme,
    window: &mut Window,
) {
    let sel_b = Bounds::new(
        point(origin.x + bb_local.left(), origin.y + bb_local.top()),
        bb_local.size,
    );
    window.paint_quad(gpui::PaintQuad {
        bounds: sel_b,
        corner_radii: gpui::Corners::all(px(4.)),
        background: gpui::Hsla::default().into(),
        border_widths: gpui::Edges::all(px(2.)),
        border_color: theme.accent,
        border_style: gpui::BorderStyle::Solid,
    });
}

/// 行内代码（反引号）圆角 pill 底。
pub fn paint_inline_code_pills(
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
            let b = Bounds::new(
                point(
                    px(text_origin_x + p0.x.as_f32()),
                    px(text_origin_y + p0.y.as_f32()),
                ),
                size(px((p1.x.as_f32() - p0.x.as_f32()).max(0.0)), px(line_h)),
            );
            window.paint_quad(
                gpui::fill(b, theme.code_inline_bg).corner_radii(gpui::Corners::all(px(5.))),
            );
        } else {
            for real_ri in y0..=y1 {
                let (rx0, rx1) = if real_ri == y0 {
                    (p0.x.as_f32(), wl.width().as_f32())
                } else if real_ri == y1 {
                    (0.0, p1.x.as_f32())
                } else {
                    (0.0, wl.width().as_f32())
                };
                let b = Bounds::new(
                    point(
                        px(text_origin_x + rx0),
                        px(text_origin_y + real_ri as f32 * line_h),
                    ),
                    size(px((rx1 - rx0).max(0.0)), px(line_h)),
                );
                window.paint_quad(
                    gpui::fill(b, theme.code_inline_bg).corner_radii(gpui::Corners::all(px(5.))),
                );
            }
        }
    }
}

/// 文本块（段落/标题/引述等可编辑行）逐可视行绘制。
pub fn paint_text_row(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    paint_inline_code_pills(origin, theme, f, row, window);
    let mut band_top = row.top + px(text_y_off(row));
    for (k, wl) in row.wrapped.iter().enumerate() {
        let band_h = row.sub_heights.get(k).copied().unwrap_or(row.lh);
        let orig = row.lh * (wl.wrap_boundaries().len() + 1) as f32;
        let shift = ((f32::from(band_h) - f32::from(orig)) / 2.).max(0.);
        let aln = row.align_offsets.get(k).copied().unwrap_or(px(0.));
        let o = point(
            origin.x + f.padding + row.x_off + aln,
            origin.y + band_top + px(shift),
        );
        let _ = wl.paint(o, row.lh, gpui::TextAlign::Left, None, window, cx);
        band_top += band_h;
    }
}

/// 引述/警报背景带宽度（px）= 内容盒宽。
pub fn quote_band_w(f: &Frame) -> f32 {
    (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.)
}

/// 引述块的灰底带 + 左侧指示条（背景层）。
pub fn paint_quote_band(
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    alert_kinds: &rustc_hash::FxHashMap<usize, AlertKind>,
    window: &mut Window,
) {
    let x0 = origin.x + f.padding;
    let y0 = origin.y + row.top - px(4.);
    let band_w = quote_band_w(f);
    let band_h = row.height + px(8.);
    let border: gpui::Hsla = {
        let mut c = theme.divider;
        c.a = 0.2;
        c
    };
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(point(x0, y0), size(px(band_w), band_h)),
        corner_radii: gpui::Corners::default(),
        background: theme.quote_bg.into(),
        border_widths: gpui::Edges::default(),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
    let bar: gpui::Hsla = match alert_kinds.get(&row.src_line) {
        Some(kind) => alert_color(theme, *kind),
        None => gpui::rgb(config::markdown::QUOTE_BAR_COLOR).into(),
    };
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(
            point(x0, y0),
            size(px(config::markdown::QUOTE_BAR_WIDTH_PX), band_h),
        ),
        corner_radii: gpui::Corners::default(),
        background: bar.into(),
        border_widths: gpui::Edges::default(),
        border_color: bar,
        border_style: gpui::BorderStyle::Solid,
    });
}

/// 每个警报（callout）对应的条色（明/暗主题微调，区分五类警报）。
pub fn alert_color(theme: &WyTheme, kind: AlertKind) -> Hsla {
    // 5 类警报条 base 色（表单内容色、仅 callout 条消费；WyTheme 无此栏，base 值与既有
    // theme token 不同 —— token 集中在 `ui::content_colors::WY_ALERT_*_BASE`）
    let mut c: Hsla = match kind {
        AlertKind::Note => ui::content_colors::WY_ALERT_NOTE_BASE,
        AlertKind::Tip => ui::content_colors::WY_ALERT_TIP_BASE,
        AlertKind::Important => ui::content_colors::WY_ALERT_IMPORTANT_BASE,
        AlertKind::Warning => ui::content_colors::WY_ALERT_WARNING_BASE,
        AlertKind::Caution => ui::content_colors::WY_ALERT_CAUTION_BASE,
    };
    if theme.is_dark {
        c.l = (c.l + 0.12).clamp(0., 1.);
        c.s = (c.s + 0.05).min(1.);
    } else {
        c.l = (c.l - 0.06).max(0.05);
    }
    c
}

/// 行首装饰：列表项目符号 / 有序号 / 任务框（画在内容区左缘 gutter）。
pub fn paint_row_gutter(
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
    let center_y = f32::from(origin.y) + first_band_center_y(row);
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
    let rline = window
        .text_system()
        .shape_line(glyph, theme.font_size, &rrun, None);
    let _ = rline.paint(
        point(
            px(x0 + GUTTER_MARK_X),
            px(center_y - f32::from(row.lh) / 2.),
        ),
        row.lh,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// 警报（callout）行 chrome：左侧彩条（覆盖引述灰条）+ 粗体标签 + 可选折叠箭头。
pub fn paint_alert_chrome(
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
    window.paint_quad({
        let b = Bounds::new(point(x0, top0 - px(4.)), size(px(3.), row.height + px(8.)));
        fill(b, color)
    });
    let label: SharedString = kind.label().into();
    let mut lfont = theme.font.clone();
    lfont.weight = gpui::FontWeight::BOLD;
    let lrun = [TextRun {
        len: label.len(),
        font: lfont,
        color,
        ..Default::default()
    }];
    let lline = window
        .text_system()
        .shape_line(label.clone(), theme.font_size, &lrun, None);
    let _ = lline.paint(
        point(x0 + px(6.), top0),
        row.lh,
        TextAlign::Left,
        None,
        window,
        cx,
    );
    if let Some(folded) = fold {
        let gx = x0 + px(6.) + lline.width() + px(4.);
        let cy = f32::from(top0) + f32::from(row.lh) * 0.5;
        let w = 9.0_f32;
        let h = 7.0_f32;
        let ax = f32::from(gx) + 1.0;
        let ay = cy - h * 0.5;
        let mut p = gpui::PathBuilder::fill();
        if *folded {
            p.move_to(point(px(ax), px(ay)));
            p.line_to(point(px(ax), px(ay + h)));
            p.line_to(point(px(ax + w), px(cy)));
        } else {
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

/// 标题折叠箭头（`▾` 展开 / `▸` 已折叠）：画在**左缘 gutter**。自绘实心小三角。
pub fn paint_heading_chevron(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    _cx: &mut App,
) {
    if row.wrapped.is_empty() {
        return;
    }
    let key = docutil::line_text(host.doc(), row.src_line).trim().to_string();
    let folded = rd.borrow().folded_headings.borrow().contains(&key);
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
        p.move_to(point(px(ax), px(ay)));
        p.line_to(point(px(ax), px(ay + h)));
        p.line_to(point(px(ax + w), px(cy)));
    } else {
        p.move_to(point(px(ax + w * 0.5), px(ay + h)));
        p.line_to(point(px(ax), px(ay)));
        p.line_to(point(px(ax + w), px(ay)));
    }
    p.close();
    if let Ok(path) = p.build() {
        window.paint_path(path, col);
    }
}

/// 行内 `$…$` 数学：在 splice 出的 spacer 位置画排版位图 / 斜体 LaTeX。
pub fn paint_inline_maths(
    _rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
    let tx = origin.x + f.padding + row.x_off
        + row.align_offsets.first().copied().unwrap_or(px(0.));
    let ty = origin.y + row.top;
    let fs = w0.unwrapped_layout.font_size;
    let lh = row.lh;
    let mut mfont = theme.font.clone();
    mfont.style = gpui::FontStyle::Italic;
    let mcolor = math_color(theme);
    for im in &row.extra.inline_maths {
        let Some(p) = w0.position_for_index(im.display_off, row.lh) else {
            continue;
        };
        let active = match host.math_slot(inline_math_key(im.latex.trim()).as_ref()) {
            Some(markdown_wysiwyg_math::MathSlot::Ready(img, _, _)) => Some(img),
            _ => im.ready.as_ref().map(|(img, _, _)| img.clone()),
        };
        let li = im.line.min(row.sub_heights.len().saturating_sub(1));
        let band_top = row.sub_heights[..li]
            .iter()
            .map(|b| f32::from(*b))
            .sum::<f32>();
        let band_h = row.sub_heights.get(li).copied().unwrap_or(row.lh);
        let orig = lh * (w0.wrap_boundaries().len() + 1) as f32;
        let shift = ((f32::from(band_h) - f32::from(orig)) / 2.).max(0.);
        let line_y = ty + px(band_top + shift) + p.y;
        if let Some(img) = active {
            let pw = im.disp_w.max(1.);
            let ph = im.disp_h.max(1.);
            let by = line_y + px((f32::from(row.lh) - ph) / 2.);
            let b = Bounds::new(point(tx + p.x, by), size(px(pw), px(ph)));
            let _ = window.paint_image(b, b, gpui::Corners::all(px(2.)), img, 0, false);
            continue;
        }
        let sfs = (f32::from(fs) * im.fit).max(1.);
        let run = [TextRun {
            len: im.latex.len(),
            font: mfont.clone(),
            color: mcolor,
            ..Default::default()
        }];
        let line = window
            .text_system()
            .shape_line(im.latex.clone(), px(sfs), &run, None);
        let _ = line.paint(
            point(tx + p.x, line_y),
            row.lh,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// 行内 `![alt](src)`：在 alt 文本（显示区）上方画等比缩小的缩略图（B1）。
pub fn paint_inline_images(
    _rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    if row.extra.inline_images.is_empty() || row.wrapped.is_empty() {
        return;
    }
    for ii in &row.extra.inline_images {
        let Some(b) = inline_image_box_local(f, row, ii) else {
            continue;
        };
        let x = origin.x + b.left();
        let y = origin.y + b.top();
        let show_w = b.size.width;
        let loaded: Option<std::sync::Arc<gpui::RenderImage>> = {
            let mem = host.memory_image(ii.src.as_ref());
            match &mem {
                Some(image) => image.clone().get_render_image(window, cx),
                None => {
                    let resource = host.image_load_resource(ii.src.as_ref());
                    let cache = host.image_cache();
                    let anycache: gpui::AnyImageCache = cache.into();
                    anycache.load(&resource, window, cx).and_then(|r| r.ok())
                }
            }
        };
        if let Some(data) = loaded {
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
        } else {
            // 行内图失败/未就绪也画占位框（P1-9：修复前静默不画，用户看不到行内图位置）。
            paint_image_fallback(
                theme,
                inline_image_placeholder_bounds(b, origin),
                window,
            );
        }
        let hovered_or_selected = {
            let h = host.hover_inline_image();
            let s = host.selected_inline_image();
            let at = |r: &Option<crate::model::InlineImageRef>| {
                r.is_some_and(|r| r.src_line == row.src_line && r.display_off == ii.display_off)
            };
            at(&h) || at(&s)
        };
        if host.is_editable() && hovered_or_selected {
            paint_image_edit_button(origin, inline_image_edit_button_local(&b), window);
        }
        if host
            .selected_inline_image()
            .is_some_and(|r| r.src_line == row.src_line && r.display_off == ii.display_off)
        {
            paint_image_selection_outline(origin, &b, theme, window);
        }
    }
}

/// 数学卡内容 x 定位（元素局部，未减滚动偏移；调用方再 `− scroll` 平移）。
#[allow(clippy::too_many_arguments)]
fn math_card_x(
    x0: f32,
    viewport: f32,
    natural_w: f32,
    item_w: f32,
    fits: bool,
    align: MathAlign,
) -> f32 {
    if fits {
        match align {
            MathAlign::Left => x0 + 16.,
            MathAlign::Center => x0 + ((viewport - item_w) / 2.).max(8.),
            MathAlign::Right => x0 + (viewport - item_w - 16.).max(8.),
        }
    } else {
        let ref_w = natural_w.max(1.);
        match align {
            MathAlign::Left => x0 + 16.,
            MathAlign::Center => x0 + ((ref_w - item_w) / 2.).max(0.),
            MathAlign::Right => x0 + (ref_w - item_w - 16.).max(0.),
        }
    }
}

/// 一块 `$$` 卡当前该画什么（「渲染表示」的单一决策）。
#[derive(Debug)]
pub enum MathCardDisplay {
    Ready(Arc<RenderImage>, f32, f32),
    Pinned(Arc<RenderImage>, f32, f32),
    Fallback,
}

/// 决策一块 `$$` 卡的显示：槽 `Ready` → 就绪位图；`Loading`/缺位 + 有钉 → 钉住最近
/// 旧位图；`Failed` / 无钉 → Unicode 回退。
pub fn math_card_display(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    src_line: usize,
    key: &str,
) -> MathCardDisplay {
    use markdown_wysiwyg_math::MathSlot as S;
    match host.math_slot(key) {
        Some(S::Ready(img, w, h)) => MathCardDisplay::Ready(img, w, h),
        Some(S::Failed) => MathCardDisplay::Fallback,
        _ => match rd.borrow().math_pins.borrow().get(&src_line).cloned() {
            Some(p) => MathCardDisplay::Pinned(p.image, p.w, p.h),
            None => MathCardDisplay::Fallback,
        },
    }
}

/// 块级卡片（数学块 / Mermaid 图）的绘制：圆角面板 + 居中排版 / 占位提示。
#[allow(clippy::too_many_arguments)]
pub fn paint_card(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
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
            let tag: SharedString = "公式".into();
            let mut muted = theme.muted;
            muted.a = 0.7;
            let trun = [TextRun {
                len: tag.len(),
                font: theme.font.clone(),
                color: muted,
                ..Default::default()
            }];
            let tline = window.text_system().shape_line(tag, px(11.), &trun, None);
            let _ = tline.paint(
                point(px(x0 + 12.), px(top0 + 6.)),
                px(14.),
                TextAlign::Left,
                None,
                window,
                cx,
            );
            let viewport = content_w;
            let avail_h = (f32::from(row.height) - 20.).max(1.);
            let natural_w = math_natural_w(rd, host, row);
            let scroll = math_hscroll_clamped(rd, host, f, row);
            let content_rect = Bounds::new(point(px(x0), px(top0)), size(px(viewport), row.height));
            window.with_content_mask(
                Some(gpui::ContentMask {
                    bounds: content_rect,
                }),
                |window| {
                    let key = latex.trim();
                    let ready_img: Option<(Arc<RenderImage>, f32, f32)> =
                        match math_card_display(rd, host, row.src_line, key) {
                            MathCardDisplay::Ready(img, w, h) => {
                                set_math_pin(rd, row.src_line, img.clone(), w, h);
                                Some((img, w, h))
                            }
                            MathCardDisplay::Pinned(img, w, h) => Some((img, w, h)),
                            MathCardDisplay::Fallback => None,
                        };
                    if let Some((img, w, h)) = ready_img {
                        let iw = w.max(1.);
                        let ih = h.max(1.);
                        let vs = (avail_h / ih).min(1.0);
                        let d_w = iw * vs;
                        let d_h = ih * vs;
                        let x = math_card_x(x0, viewport, natural_w, d_w, d_w <= viewport, *align)
                            - scroll;
                        let y = top0 + 20. + ((avail_h - d_h) / 2.).max(0.);
                        let b = Bounds::new(point(px(x), px(y)), size(px(d_w), px(d_h)));
                        let _ = window.paint_image(b, b, gpui::Corners::all(px(2.)), img, 0, false);
                        return;
                    }
                    let mut mfont = theme.font.clone();
                    mfont.style = gpui::FontStyle::Italic;
                    let lh = px(22.);
                    let fs = (f32::from(theme.font_size) * 1.15).max(14.);
                    let ss_fs = (fs * 0.78).max(10.);
                    let sup_dy = (fs * 0.34).max(2.);
                    let sub_dy = (fs * 0.18).max(2.);
                    let mcolor = math_color(theme);
                    let lines = math_display_lines(latex);
                    let n = lines.len().max(1);
                    let mut plans: Vec<Vec<(SharedString, f32, f32, MathPos)>> =
                        Vec::with_capacity(lines.len());
                    let mut max_line_w = 0.0f32;
                    for shown in &lines {
                        let segs = math_to_segments(shown);
                        let mut plan: Vec<(SharedString, f32, f32, MathPos)> =
                            Vec::with_capacity(segs.len());
                        let mut total = 0.0_f32;
                        for s in &segs {
                            let sfs = if s.pos == MathPos::Normal { fs } else { ss_fs };
                            let run = [TextRun {
                                len: s.text.len(),
                                font: mfont.clone(),
                                color: mcolor,
                                ..Default::default()
                            }];
                            let sh = window.text_system().shape_line(
                                s.text.clone().into(),
                                px(sfs),
                                &run,
                                None,
                            );
                            let w = f32::from(sh.width());
                            total += w;
                            plan.push((s.text.clone().into(), sfs, w, s.pos));
                        }
                        max_line_w = max_line_w.max(total);
                        plans.push(plan);
                    }
                    set_math_natural_w(rd, row.src_line, max_line_w);
                    let block_h = f32::from(row.height);
                    let mut y = top0 + 10. + ((block_h - 20. - n as f32 * 22.) / 2.).max(0.);
                    for plan in plans {
                        let total: f32 = plan.iter().map(|(_, _, w, _)| w).sum();
                        let start_x =
                            math_card_x(x0, viewport, max_line_w, total, total <= viewport, *align)
                                - scroll;
                        let mut cursor = start_x;
                        for (text, sfs, w, pos) in plan {
                            let run = [TextRun {
                                len: text.len(),
                                font: mfont.clone(),
                                color: mcolor,
                                ..Default::default()
                            }];
                            let sh = window.text_system().shape_line(text, px(sfs), &run, None);
                            let paint_y = match pos {
                                MathPos::Normal => y,
                                MathPos::Sup => y - sup_dy,
                                MathPos::Sub => y + sub_dy,
                            };
                            let _ = sh.paint(
                                point(px(cursor), px(paint_y)),
                                lh,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                            cursor += w;
                        }
                        y += 22.;
                    }
                },
            );
            if let Some(geo) = math_hscroll_geom_inner(rd, host, f, row) {
                paint_hscroll_thumb(origin, theme, row, geo, window);
            }
        }
        BlockCard::Mermaid => {
            if host.focused_mermaid() == Some(row.src_line) {
                window.paint_quad(gpui::PaintQuad {
                    bounds: Bounds::new(point(px(x0), px(top0)), size(px(content_w), row.height)),
                    corner_radii: Corners::all(px(8.)),
                    background: Hsla::default().into(),
                    border_widths: gpui::Edges::all(px(2.)),
                    border_color: theme.accent,
                    border_style: gpui::BorderStyle::Solid,
                });
            }
            let tag: SharedString = "mermaid".into();
            let mut muted = theme.muted;
            muted.a = 0.7;
            let trun = [TextRun {
                len: tag.len(),
                font: theme.font.clone(),
                color: muted,
                ..Default::default()
            }];
            let tline = window.text_system().shape_line(tag, px(11.), &trun, None);
            let _ = tline.paint(
                point(px(x0 + 12.), px(top0 + 6.)),
                px(14.),
                TextAlign::Left,
                None,
                window,
                cx,
            );
            debug_assert!(row.mm.is_some(), "mermaid 卡片恒有缓存的已排布局");
            let Some(mm) = row.mm.as_ref() else {
                return;
            };
            paint_mermaid_layout(window, &MmTheme::from(theme), mm, x0, top0, content_w, cx);
        }
    }
}

/// 当前选区（源字节归一化区间）；无选区返回 `None`。
pub fn editor_selection(host: &dyn WyRenderHost) -> Option<Range<usize>> {
    host.selection()
}

/// 选区高亮：对可编辑且与源选区相交的行，按字形几何逐可视行画 `hl` 背景矩形。
#[allow(clippy::too_many_arguments)]
pub fn paint_selection(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    range: &Range<usize>,
    hl: Hsla,
    window: &mut Window,
    _cx: &mut App,
) {
    for r in &f.rows {
        if r.table_edit.is_some() && r.table.is_some() {
            paint_table_selection_hl(rd, host, origin, theme, f, r, range, hl, window);
            continue;
        }
        if !r.editable || r.wrapped.is_empty() {
            continue;
        }
        if !r.multi_offsets.is_empty() {
            paint_code_selection_hl(rd, host, origin, f, r, range, hl, window);
        } else {
            paint_text_selection_hl(host, origin, f, r, range, hl, window);
        }
    }
}

/// 表格选区高亮矩形的 x（**元素局部**，未含 `origin.x`）：内容左缘 = 帧内衬 `padding`
/// + 行 `x_off`（引号内表格缩进）+ 单元格列起点 + 格内文本缩进 + 选区字形偏移 − 水平
///
/// 滚动偏移。与 `paint_table_row`/`hit.rs` 的表格内容左缘口径一致——修复前缺 `x_off`，
/// 引号内表格选区整体左偏 15px 且盖引号竖条（P1-11）。
fn table_selection_hl_x(
    padding: f32,
    x_off: f32,
    cell_x0: f32,
    text_x: f32,
    p0: f32,
    scroll: f32,
) -> f32 {
    padding + x_off + cell_x0 + text_x + p0 - scroll
}

/// 表格行选区高亮：逐可见行 × 逐格与选区求交 → 格内字形几何背景。
#[allow(clippy::too_many_arguments)]
fn paint_table_selection_hl(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    r: &ShapedRow,
    range: &Range<usize>,
    hl: Hsla,
    window: &mut Window,
) {
    let (Some(te), Some(t)) = (&r.table_edit, &r.table) else {
        return;
    };
    if !r.editable {
        return;
    }
    let scroll = table_hscroll_clamped(rd, host, f, r);
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2. - f32::from(r.x_off)).max(1.);
    let grid_h = f32::from(table_block_height(theme, t.rows_texts.len()));
    let content_rect = Bounds::new(
        point(origin.x + f.padding + r.x_off, origin.y + r.top),
        size(px(viewport), px(grid_h)),
    );
    let text_lh = f32::from(theme.para_line());
    use crate::element::geom::{cell_selection_display, table_cell_x0, table_row_top};
    window.with_content_mask(
        Some(gpui::ContentMask {
            bounds: content_rect,
        }),
        |window| {
            for (row_ix, row_l) in te.rows.iter().enumerate() {
                let ls = docutil::line_start(host.doc(), row_l.src_line);
                let row_top = table_row_top(t, row_ix);
                let ty = f32::from(r.top) + row_top + ((f32::from(t.row_h) - text_lh) / 2.).max(0.);
                for (cell_ix, cell) in row_l.cells.iter().enumerate() {
                    let Some((d0, d1)) = cell_selection_display(
                        &cell.map,
                        cell.line.len(),
                        cell.src.clone(),
                        ls,
                        range,
                    ) else {
                        continue;
                    };
                    let p0 = cell.line.x_for_index(d0);
                    let p1 = cell.line.x_for_index(d1);
                    let x = f32::from(origin.x)
                        + table_selection_hl_x(
                            f32::from(f.padding),
                            f32::from(r.x_off),
                            table_cell_x0(&t.widths, cell_ix),
                            cell.text_x,
                            f32::from(p0),
                            scroll,
                        );
                    let w = (f32::from(p1) - f32::from(p0)).max(2.);
                    window.paint_quad(gpui::PaintQuad {
                        bounds: Bounds::new(
                            point(px(x), origin.y + px(ty)),
                            size(px(w), px(text_lh)),
                        ),
                        corner_radii: gpui::Corners::default(),
                        background: hl.into(),
                        border_widths: gpui::Edges::default(),
                        border_color: gpui::Hsla::default(),
                        border_style: gpui::BorderStyle::Solid,
                    });
                }
            }
        },
    );
}

/// 代码块行选区高亮：逐源码行求与选区的交集，按字形几何画高亮（每行一条）。
fn paint_code_selection_hl(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    f: &Frame,
    r: &ShapedRow,
    range: &Range<usize>,
    hl: Hsla,
    window: &mut Window,
) {
    use crate::element::geom::{code_group_top, group_len, source_to_display};
    let block_start = docutil::line_start(host.doc(), r.src_line);
    let g_count = r.multi_offsets.len();
    let scroll = code_hscroll_clamped(rd, host, f, r);
    let panel_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let panel_rect = Bounds::new(
        point(origin.x + f.padding, origin.y + r.top),
        size(px(panel_w), r.height),
    );
    window.with_content_mask(Some(gpui::ContentMask { bounds: panel_rect }), |window| {
        for g in 0..g_count {
            let ls = docutil::line_start(host.doc(), r.src_line + 1 + g);
            let le = docutil::line_end(host.doc(), r.src_line + 1 + g).max(ls);
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
                    let y1 = r.top + px(code_group_top(r, g));
                    let h = r.sub_heights.get(g).copied().unwrap_or(r.lh);
                    window.paint_quad(gpui::PaintQuad {
                        bounds: Bounds::new(
                            point(
                                origin.x + f.padding + r.x_off + p0.x - px(scroll),
                                origin.y + y1,
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
}

/// 普通文本行选区高亮：与源选区求交后按字形几何逐可视行画背景。
fn paint_text_selection_hl(
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    f: &Frame,
    r: &ShapedRow,
    range: &Range<usize>,
    hl: Hsla,
    window: &mut Window,
) {
    let aln0 = f32::from(r.align_offsets.first().copied().unwrap_or(px(0.)));
    let rects = text_selection_hl_rects(host, r, range, f32::from(f.padding), aln0);
    for (x, y, w) in rects {
        window.paint_quad(gpui::PaintQuad {
            bounds: Bounds::new(
                point(origin.x + px(x), origin.y + px(y)),
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

/// 普通文本行选区高亮的逐可视行背景矩形（几何纯函数，paint 与回归测试共用）。
pub fn text_selection_hl_rects(
    host: &dyn WyRenderHost,
    r: &ShapedRow,
    range: &Range<usize>,
    pad: f32,
    aln0: f32,
) -> Vec<(f32, f32, f32)> {
    use crate::element::geom::source_to_display;
    let ls = docutil::line_start(host.doc(), r.src_line);
    let le = docutil::line_end(host.doc(), r.src_line).max(ls);
    let from = range.start.max(ls).min(le);
    let to = range.end.max(ls).min(le);
    if to <= from {
        return Vec::new();
    }
    let d0 = source_to_display(&r.byte_map, from - ls);
    let d1 = source_to_display(&r.byte_map, to - ls);
    let Some(w0) = r.wrapped.first() else {
        return Vec::new();
    };
    let len = w0.len().max(1);
    let d0 = d0.min(len);
    let d1 = d0.max(d1.min(len));
    if d1 <= d0 {
        return Vec::new();
    }
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
    let y_base = f32::from(r.top) + text_y_off(r) + text_row_shift(r);
    let mut out = Vec::with_capacity(rows.len());
    for (a, b2) in rows {
        let c0 = d0.max(a);
        let c1 = d1.min(b2);
        if c1 <= c0 {
            continue;
        }
        let p1 = w0.position_for_index(c1, r.lh).unwrap_or(point(px(0.), px(0.)));
        let x_left = if c0 == a {
            0.
        } else {
            w0.position_for_index(c0, r.lh).map_or(0., |p| f32::from(p.x))
        };
        let w = f32::from(p1.x - px(x_left)).max(2.);
        out.push((
            pad + f32::from(r.x_off) + aln0 + x_left,
            y_base + f32::from(p1.y),
            w,
        ));
    }
    out
}

/// 注记跳转加亮（P4-2）：把 `rd.marker_hl` 的「目标源码行的注记标记」解析成绝对源区间，
/// 复用 [`paint_selection`] 的几何用 accent 淡色画该标记。
pub fn paint_note_marker_highlight(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    window: &mut Window,
    cx: &mut App,
) {
    // 注记跳转加亮 = 蓝系 accent 淡色半透明（仅预览引擎消费；token 见
    // `ui::content_colors::WY_NOTE_JUMP_HL_{DARK,LIGHT}`，与 doc_selection_bg 值不同，非 A/B）
    let hl: Hsla = if ui::theme::Theme::global(cx).is_dark() {
        ui::content_colors::WY_NOTE_JUMP_HL_DARK
    } else {
        ui::content_colors::WY_NOTE_JUMP_HL_LIGHT
    };
    let range = rd.borrow().marker_hl.get().and_then(|(src_line, n, is_def)| {
        note_marker_src_range(rd, host, f, src_line, n, is_def)
    });
    let Some(range) = range else { return };
    paint_selection(rd, host, origin, theme, f, &range, hl, window, cx);
}

/// 搜索跳转高亮（预览独有能力）：把搜索命中解析成**绝对源区间**，以命中色复用
/// [`paint_selection`] 的几何绘制。
pub fn paint_search_highlight(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(range) = host.search_highlight_range() else {
        return;
    };
    // 预览搜索命中 = 淡黄半透明（仅本引擎消费；token 见
    // `ui::content_colors::WY_SEARCH_HIT_HL_{DARK,LIGHT}`，与 editor_find_* 值不同，非 A/B）
    let hl: Hsla = if ui::theme::Theme::global(cx).is_dark() {
        ui::content_colors::WY_SEARCH_HIT_HL_DARK
    } else {
        ui::content_colors::WY_SEARCH_HIT_HL_LIGHT
    };
    paint_selection(rd, host, origin, theme, f, &range, hl, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-9 状态机红测：`image_should_skip_reload` 对**任何**状态（含失败标记 `Some(None)`）
    /// 都必须返回 false——失败不再被 paint 短路跳过（每帧仍会走 `load`、画占位可重试）。
    /// 修复前短路判断是 `matches!(state, Some(None))`（失败恒跳过、永不复测）。
    /// 注：因 `CachedLoad` 永久缓存失败结果（见 `image_should_skip_reload` 文档），当前
    /// 每帧 `load` 实际同步返回缓存 Err 不真正重取；本红测只锁「paint 不短路跳过」这层。
    #[test]
    fn image_failed_state_never_skips_reload() {
        let cases: [Option<Option<(f32, f32)>>; 3] = [
            None,                     // 未知（未试 / 失败未写标记）
            Some(None),               // 失败标记（P1-9 关键：不得被短路）
            Some(Some((200., 150.))), // 已成功
        ];
        for s in cases {
            assert!(
                !image_should_skip_reload(s),
                "状态 {s:?} 不应跳过重新加载（失败可重试）"
            );
        }
    }

    /// P1-9 行内图占位几何：占位框 = 行内图显示盒 `b` 平移到窗口坐标、尺寸不变
    /// （与独立图 `paint_image_fallback` 同视觉，锁真实对位量）。
    #[test]
    fn inline_image_placeholder_bounds_aligns_to_display_box() {
        let origin = point(px(10.), px(24.));
        let b = Bounds::new(point(px(3.), px(5.)), size(px(60.), px(18.)));
        let ph = inline_image_placeholder_bounds(b, origin);
        assert_eq!(ph.left(), origin.x + b.left());
        assert_eq!(ph.top(), origin.y + b.top());
        assert_eq!(ph.size, b.size);
    }

    /// P1-11 几何：表格选区高亮矩形 x 必须纳入行 `x_off`（引号内表格缩进）——期望由
    /// `paint_table_row` 的内容左缘口径推导（padding + x_off + 单元格 + 文本 + 字形 − 滚动）。
    #[test]
    fn table_selection_hl_x_includes_row_x_off() {
        let padding = 12.0f32;
        let x_off = 15.0f32;
        let cell_x0 = 3.0f32;
        let text_x = 2.0f32;
        let p0 = 5.0f32;
        let scroll = 4.0f32;
        let x = table_selection_hl_x(padding, x_off, cell_x0, text_x, p0, scroll);
        assert_eq!(x, padding + x_off + cell_x0 + text_x + p0 - scroll);
        assert!(x >= padding + x_off, "引号内表格选区左缘不能左偏盖竖条, 实得 {x}");
    }
}
