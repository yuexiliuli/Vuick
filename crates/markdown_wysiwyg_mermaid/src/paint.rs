//! Mermaid 绘制：把 shape 期缓存的已排布局（[`MmFlowLayout`]/[`MmSeqLayout`]，卡片相对
//! 坐标）画到窗口。生产路径恒由 `paint_mermaid_layout` 入口（唯一构造点
//! `shape_code_row` 设 `row.mm`）→ paint 不每帧重排/重测标签宽，只加卡片原点绘制。
//!
//! 绘制原语：`paint_quad`（圆角矩形/卡片）、`PathBuilder::fill/stroke`（多边形节点、
//! 椭圆近似）、虚线用短线段仿制（GPUI 无 native dashed path）。

use super::{MmEdgeKind, MmFlowLayout, MmLayout, MmPlaced, MmSeqArrow, MmSeqColumn, MmSeqFragmentKind, MmSeqLayout, MmSeqNotePos, MmSeqRow, MmShape, SEQ_NOTE_H, ARROW_LEN_PX, arrow_base};
use crate::MmTheme;
use gpui::{App, Bounds, Corners, Edges, Hsla, PathBuilder, Pixels, Point, SharedString, TextRun, Window, point, px};

/// 入口：按布局类型分发绘制（含空图占位提示）。`ox/oy` = 卡片左上角（宿主坐标）。
#[allow(clippy::too_many_arguments)] // paint 图元：画布(window/theme/cx) + 几何参数互为正交
pub fn paint_mermaid_layout(
    window: &mut Window,
    theme: &MmTheme,
    mm: &MmLayout,
    ox: f32,
    oy: f32,
    area_w: f32,
    cx: &mut App,
) {
    match mm {
        MmLayout::Flow(f) => paint_mermaid_placed(window, theme, f, ox, oy, area_w, cx),
        MmLayout::Seq(s) => paint_mermaid_seq(window, theme, s, ox, oy, area_w, cx),
        MmLayout::Empty => paint_empty_hint(window, theme, ox, oy, area_w, cx),
    }
}

fn paint_empty_hint(window: &mut Window, theme: &MmTheme, ox: f32, oy: f32, area_w: f32, cx: &mut App) {
    let hint: SharedString = "空图 / 暂不支持的类型".into();
    let mut reduced = theme.muted;
    reduced.a = 0.55;
    let hrun = [TextRun { len: hint.len(), font: theme.mono.clone(), color: reduced, ..Default::default() }];
    let hline = window.text_system().shape_line(hint, px(12.), &hrun, None);
    let hw = f32::from(hline.width());
    let _ = hline.paint(
        point(px(ox + (area_w - hw) / 2.), px(oy + 40.)),
        px(16.),
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// 画 flowchart 布局。
#[allow(clippy::too_many_arguments)] // paint 图元：画布(window/theme/cx) + 几何参数互为正交
pub(crate) fn paint_mermaid_placed(
    window: &mut Window,
    theme: &MmTheme,
    flow: &MmFlowLayout,
    ox: f32,
    oy: f32,
    area_w: f32,
    cx: &mut App,
) {
    if flow.placed.iter().all(|p| p.virtual_node) {
        paint_empty_hint(window, theme, ox, oy, area_w, cx);
        return;
    }
    let z = flow.zoom;
    // 1) subgraph 分组（底色盒先画）。
    for sg in &flow.subgraphs {
        paint_quad(window, sg.x + ox, sg.y + oy, sg.w, sg.h, 8., soft_bg(theme), border_soft(theme), 1.);
        if !sg.title.is_empty() {
            text(window, theme, sg.x + ox + 8., sg.y + oy + (18. - 12.) / 2. * z, &sg.title, theme.font.clone(), 11. * z, theme.muted, 17. * z, cx);
        }
    }
    // 2) 边（折线 + 端点标记 + 箭头，**不含标签**——标签在节点之后单独顶层画）。
    let edge_color = { let mut c = theme.mermaid_border; c.a = 0.85; c };
    for e in &flow.edges {
        paint_edge(window, theme, e, ox, oy, edge_color, z);
    }
    // 3) 节点。
    let box_bg = theme.code_bg;
    let box_border = { let mut c = theme.muted; c.a = 0.6; c };
    for p in &flow.placed {
        if p.virtual_node {
            // 长边插桩点。
            let r = 3. * z;
            let b = Bounds::new(point(px(ox + p.x - r), px(oy + p.y - r)), size_wh(2. * r, 2. * r));
            window.paint_quad(gpui::PaintQuad {
                bounds: b,
                corner_radii: Corners::all(px(r)),
                background: edge_color.into(),
                border_widths: Edges::default(),
                border_color: Hsla::default(),
                border_style: gpui::BorderStyle::Solid,
            });
            continue;
        }
        let fill = p.fill.map(to_hsla).unwrap_or(box_bg);
        let border = p.stroke.map(to_hsla).unwrap_or(box_border);
        let bw = p.stroke_width.unwrap_or(1.5);
        paint_node_shape(window, theme, p, ox, oy, fill, border, bw, z, cx);
    }
    // 4) 边标签顶层（任何节点/折线都盖不住文字）。
    for e in &flow.edges {
        paint_edge_label(window, theme, e, ox, oy, z, cx);
    }
}

fn size_wh(w: f32, h: f32) -> gpui::Size<Pixels> {
    gpui::size(px(w), px(h))
}

fn to_hsla(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

/// 分组底色：`mermaid_border` 极浅 tint（panel 语义，比卡底 show 一层浅灰分组）。
fn soft_bg(theme: &MmTheme) -> Hsla {
    let mut c = theme.mermaid_border;
    c.a = 0.12;
    c
}

/// 分组边框。
fn border_soft(theme: &MmTheme) -> Hsla {
    let mut c = theme.mermaid_border;
    c.a = 0.40;
    c
}

#[allow(clippy::too_many_arguments)] // 低层画图原语：显式标量参数比打包结构更直白
fn paint_quad(window: &mut Window, x: f32, y: f32, w: f32, h: f32, r: f32, fill: Hsla, border: Hsla, bw: f32) {
    window.paint_quad(gpui::PaintQuad {
        bounds: Bounds::new(point(px(x), px(y)), size_wh(w, h)),
        corner_radii: Corners::all(px(r)),
        background: fill.into(),
        border_widths: Edges::all(px(bw)),
        border_color: border,
        border_style: gpui::BorderStyle::Solid,
    });
}

fn path_pts(window: &mut Window, pts: &[Point<Pixels>], fill: Option<Hsla>, stroke: Option<Hsla>, width: f32) {
    if let Some(c) = stroke {
        let mut b = PathBuilder::stroke(px(width));
        b.move_to(pts[0]);
        for p in &pts[1..] {
            b.line_to(*p);
        }
        b.close();
        if let Ok(path) = b.build() {
            window.paint_path(path, c);
        }
    }
    if let Some(c) = fill {
        let mut fb = PathBuilder::fill();
        fb.move_to(pts[0]);
        for p in &pts[1..] {
            fb.line_to(*p);
        }
        fb.close();
        if let Ok(path) = fb.build() {
            window.paint_path(path, c);
        }
    }
}

fn polygon(window: &mut Window, pts: &[(f32, f32)], fill: Hsla, stroke: Hsla, width: f32) {
    let pts: Vec<Point<Pixels>> = pts.iter().map(|&(x, y)| point(px(x), px(y))).collect();
    path_pts(window, &pts, Some(fill), Some(stroke), width);
}

/// 椭圆/圆（多边形近似）。
#[allow(clippy::too_many_arguments)] // 低层画图原语：显式标量参数比打包结构更直白
fn circle(window: &mut Window, cx_: f32, cy_: f32, rx: f32, ry: f32, fill: Hsla, stroke: Hsla, width: f32) {
    let n = 28;
    let pts: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / n as f32;
            (cx_ + rx * a.cos(), cy_ + ry * a.sin())
        })
        .collect();
    polygon(window, &pts, fill, stroke, width);
}

/// 虚线（沿折线短线段画短划；GPUI 无 native dashed path，逐段仿制）。
fn dashed_path(window: &mut Window, pts: &[(f32, f32)], color: Hsla, width: f32, dash: f32, gap: f32) {
    for seg in pts.windows(2) {
        let (a, b) = (seg[0], seg[1]);
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        let len = (dx * dx + dy * dy).sqrt();
        let (nx, ny) = if len > 0. { (dx / len, dy / len) } else { (0., 0.) };
        let mut s = 0.0_f32;
        let mut on = true;
        while s < len {
            let step = (dash + gap).min(len - s);
            if on {
                let e0 = (a.0 + nx * s, a.1 + ny * s);
                let e1 = (a.0 + nx * (s + step.min(dash)), a.1 + ny * (s + step.min(dash)));
                stroke_seg(window, e0, e1, color, width);
            }
            on = !on;
            s += step;
        }
    }
}

fn stroke_seg(window: &mut Window, a: (f32, f32), b: (f32, f32), color: Hsla, width: f32) {
    let mut pb = PathBuilder::stroke(px(width));
    pb.move_to(point(px(a.0), px(a.1)));
    pb.line_to(point(px(b.0), px(b.1)));
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

fn stroke_poly(window: &mut Window, pts: &[(f32, f32)], color: Hsla, width: f32) {
    if pts.is_empty() {
        return;
    }
    let mut pb = PathBuilder::stroke(px(width));
    pb.move_to(point(px(pts[0].0), px(pts[0].1)));
    for &(x, y) in &pts[1..] {
        pb.line_to(point(px(x), px(y)));
    }
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

/// 画一条边（虚线/实线/粗线 + 首尾箭头 + 标签）。统一规则「线到基部、箭头到节点」：
/// - **箭头 tip 恒为逻辑锚点 `e.points.last()` = 骨架末端 = 节点边缘接触点**（与 smoothed
///   是否已收短无关，缩放/平移不变）——paint 用 `<tip>` 落实心箭头尖；
/// - **线末点回缩到箭头基部** `base = tip − ARROW_LEN_PX·z·arrow_dir`（与 layout 的弧线
///   [`arrow_base`] 同函数同常量）：弧线已在 layout 收短到基部（末点 == base，此处 no-op）；
///   直边/圆角肘末段沿 `arrow_dir` 轴对齐，此处精确回缩 9·z——不再「线穿到节点边 + 箭头
///   同点」双连接；
/// - **双端**（Both）源端对称：线首点沿首段方向回缩一个箭头长。
fn paint_edge(
    window: &mut Window,
    theme: &MmTheme,
    e: &super::MmPolylineEdge,
    ox: f32,
    oy: f32,
    color: Hsla,
    z: f32,
) {
    // 曲线化密点优先（layout 期缓存）；无则用直角骨架。首锚定不变。
    let src: &Vec<(f32, f32)> = e.smoothed.as_ref().unwrap_or(&e.points);
    let pts: Vec<(f32, f32)> = src.iter().map(|&(x, y)| (x + ox, y + oy)).collect();
    if pts.len() < 2 {
        return;
    }
    let width = match e.kind {
        MmEdgeKind::ThickArrow | MmEdgeKind::ThickLine | MmEdgeKind::BothThick => 2.8,
        _ => 1.5,
    };
    // 目标端箭头方向 = 目标接触边垂直内法向（`e.arrow_dir`，layout 期按边实际钉在目标的
    // 哪条边定死）——垂直指向节点、直插所入边，不随弧线切线歪斜（旧 `end_tangent` 偏差
    // 5~11.5° 是投诉 .2 根因）。
    let tip = (e.points.last().unwrap().0 + ox, e.points.last().unwrap().1 + oy);
    let dir = e.arrow_dir;
    let draw = edge_draw_points(e, &pts, tip, z);
    // 边标签文字开口：把绘制折线按「文字矩形 + 2.5·z margin」拆成两段——head 收到入矩形
    // 边界、tail 从出矩形边界续到箭头基部，中间跳过文字区（线在文字两侧断开、不穿字），
    // 替代旧「断线盒盖上电线」（盒已删、纯文字）。`split_poly_at_rect` 精确线性插值到矩形
    // 边界作断点，不靠采样粗切。
    let (head, tail) = if let (Some((lx, ly)), Some(lt)) = (e.label, &e.label_text)
        && !lt.is_empty()
    {
        let (tw, th) = measure_edge_label(window, theme, lt, z);
        if tw > 0. {
            let margin = 2.5 * z;
            let cx = lx + ox;
            let cy = ly + oy + th / 2.;
            split_poly_at_rect(
                &draw,
                cx - tw / 2. - margin,
                cy - th / 2. - margin,
                cx + tw / 2. + margin,
                cy + th / 2. + margin,
            )
        } else {
            (draw.clone(), None)
        }
    } else {
        (draw.clone(), None)
    };
    match e.kind {
        MmEdgeKind::DottedArrow | MmEdgeKind::DottedLine => {
            if head.len() >= 2 {
                dashed_path(window, &head, color, width, 4., 4.);
            }
            if let Some(t) = &tail
                && t.len() >= 2
            {
                dashed_path(window, t, color, width, 4., 4.);
            }
        }
        _ => {
            if head.len() >= 2 {
                stroke_poly(window, &head, color, width);
            }
            if let Some(t) = &tail
                && t.len() >= 2
            {
                stroke_poly(window, t, color, width);
            }
        }
    }
    // 端点标记：tip = 节点边缘接触点；箭头实心填充从基部到 tip。
    match e.kind {
        MmEdgeKind::Arrow | MmEdgeKind::DottedArrow | MmEdgeKind::ThickArrow | MmEdgeKind::BothArrow | MmEdgeKind::BothThick => {
            arrow_head(window, tip, dir, color, z);
        }
        MmEdgeKind::CircleEnd => {
            circle(window, tip.0, tip.1, 3.5 * z, 3.5 * z, color, color, width);
        }
        MmEdgeKind::CrossEnd => {
            let (dx, dy) = (dir.0 * 4. * z, dir.1 * 4. * z);
            let (pxx, py) = (dir.1 * 4. * z, -dir.0 * 4. * z);
            stroke_seg(window, (tip.0 - dx + pxx, tip.1 - dy + py), (tip.0 + dx - pxx, tip.1 + dy - py), color, width);
            stroke_seg(window, (tip.0 - dx - pxx, tip.1 - dy - py), (tip.0 + dx + pxx, tip.1 + dy + py), color, width);
        }
        _ => {}
    }
    if matches!(e.kind, MmEdgeKind::BothArrow | MmEdgeKind::BothThick) {
        let first = *pts.first().unwrap();
        let d = dir_of(&pts);
        arrow_head(window, first, (-d.0, -d.1), color, z);
    }
}

fn dir_of(pts: &[(f32, f32)]) -> (f32, f32) {
    let a = pts[pts.len() - 2];
    let b = pts[pts.len() - 1];
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let l = (dx * dx + dy * dy).sqrt().max(1e-3);
    (dx / l, dy / l)
}

/// 绘制用折线（宿主坐标）：复刻「线到基部、箭头到节点」的端点回缩，`paint_edge` 与单测
/// 共用同一几何——有箭头（含双向）末点回缩到箭头基部 `tip − ARROW_LEN_PX·z·arrow_dir`
/// （与 layout 收短弧线尾点同函数同常量严格重合；`tip` 恒为逻辑锚点 `e.points.last()` 的
/// 宿主坐标，弧线 `pts.last()` 已收短到基部、不能当 tip 用）、双向源端对称回缩；无箭头
/// （Line/圆端/叉端）原样连到节点边缘。`pts[..]` 为已加卡原点的绘制点。
pub(crate) fn edge_draw_points(
    e: &super::MmPolylineEdge,
    pts: &[(f32, f32)],
    tip: (f32, f32),
    z: f32,
) -> Vec<(f32, f32)> {
    if pts.len() < 2 {
        return pts.to_vec();
    }
    let has_arrow = matches!(
        e.kind,
        MmEdgeKind::Arrow
            | MmEdgeKind::DottedArrow
            | MmEdgeKind::ThickArrow
            | MmEdgeKind::BothArrow
            | MmEdgeKind::BothThick
    );
    if !has_arrow {
        return pts.to_vec();
    }
    let base = arrow_base(tip, e.arrow_dir, ARROW_LEN_PX * z);
    let mut v = pts.to_vec();
    *v.last_mut().unwrap() = base;
    if matches!(e.kind, MmEdgeKind::BothArrow | MmEdgeKind::BothThick) {
        // 双端：源端同样把线首点回缩一个箭头长（沿首段方向进入线内），与目标端对称，
        // 两条半箭头各占两端一个箭头长、「线到两端基部」。
        let (a, b) = (v[0], v[1]);
        let l = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt().max(1e-3);
        let (ux, uy) = ((b.0 - a.0) / l, (b.1 - a.1) / l);
        v[0] = (a.0 + ARROW_LEN_PX * z * ux, a.1 + ARROW_LEN_PX * z * uy);
    }
    v
}

/// 线段 (a,b) 与轴对齐矩形（x0,y0,x1,y1）求交：返回落在矩形内的参数区间 `[t0,t1]`
/// （a↔t=0、b↔t=1）；`None` = 完全不相交（相切单点由调用方以 `t1-t0` 阈值过滤）。
fn seg_rect_t(a: (f32, f32), b: (f32, f32), x0: f32, y0: f32, x1: f32, y1: f32) -> Option<(f32, f32)> {
    let mut t0 = 0.;
    let mut t1 = 1.;
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    for (p, q) in [(-dx, a.0 - x0), (dx, x1 - a.0), (-dy, a.1 - y0), (dy, y1 - a.1)] {
        if p.abs() < 1e-9 {
            if q < 0. {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0. {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }
    (t1 >= t0).then_some((t0, t1))
}

/// 按文字矩形把折线「开口」拆两段：`head` = 起点→入矩形边界、`tail` = 出矩形边界→终点
/// （`None` = 折线未穿矩形），矩形内一段跳过——线在文字两侧断开、不穿字。进出点用
/// 线性插值精确落在矩形边界（`seg_rect_t`，避免采样粗导致断口毛糙）。单文字区（标签
/// 锚点在路径中点，折线至多穿入/穿出各一次）为设计前提。
pub(crate) fn split_poly_at_rect(
    pts: &[(f32, f32)],
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> (Vec<(f32, f32)>, Option<Vec<(f32, f32)>>) {
    let inside = |p: (f32, f32)| x0 <= p.0 && p.0 <= x1 && y0 <= p.1 && p.1 <= y1;
    let lerp = |a: (f32, f32), b: (f32, f32), t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
    let mut head = vec![pts[0]];
    let mut tail: Option<Vec<(f32, f32)>> = None;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        match seg_rect_t(a, b, x0, y0, x1, y1) {
            Some((t0, t1)) if t1 - t0 > 1e-5 => match (inside(a), inside(b)) {
                // 出矩形：出点是尾段首点（精确落矩形边界），随后续到 b（在外）。
                (true, false) => {
                    let exit = lerp(a, b, t1.min(1.));
                    match &mut tail {
                        Some(t) => t.push(exit),
                        None => tail = Some(vec![exit]),
                    }
                    tail.as_mut().unwrap().push(b);
                }
                // 入矩形：head 收在入点（精确落矩形边界）；b 在矩形内跳过。
                (false, true) => {
                    let enter = lerp(a, b, t0);
                    match &mut tail {
                        Some(t) => t.push(enter),
                        None => head.push(enter),
                    }
                }
                // 段直穿（双端在外切角）：入口接 head、出口接 tail 并续 b。
                (false, false) => {
                    let enter = lerp(a, b, t0);
                    let exit = lerp(a, b, t1);
                    match &mut tail {
                        Some(t) => t.push(enter),
                        None => head.push(enter),
                    }
                    match &mut tail {
                        Some(t) => t.push(exit),
                        None => tail = Some(vec![exit]),
                    }
                    tail.as_mut().unwrap().push(b);
                }
                // 段全在矩形内：整段跳过。
                (true, true) => {}
            },
            // 段与矩形不相交：全部在外，续到当前活动段（tail 已开 → tail，否则 head）。
            _ => match &mut tail {
                Some(t) => t.push(b),
                None => head.push(b),
            },
        }
    }
    (head, tail)
}

/// 边标签：**单独一层最后画**（在节点框/连线之上）——无盒纯文字居中（连线已在
/// [`paint_edge`] 按文字矩形开口断开，不再有背景盒盖线）。
fn paint_edge_label(
    window: &mut Window,
    theme: &MmTheme,
    e: &super::MmPolylineEdge,
    ox: f32,
    oy: f32,
    z: f32,
    cx: &mut App,
) {
    if let (Some((lx, ly)), Some(lt)) = (e.label, &e.label_text)
        && !lt.is_empty()
    {
        text_centered(window, lx + ox, ly + oy, lt, theme.font.clone(), 12. * z, theme.text, 14. * z, cx);
    }
}

/// 测量边标签文字盒：返回 `(tw, th)`——`tw` = 文字宽（`shape_line` 于 `theme.font`、12·z），
/// `th` = 文本行盒高（= [`text_centered`] 的行高 14·z）。供 [`paint_edge`] 文字开口几何用。
fn measure_edge_label(window: &mut Window, theme: &MmTheme, text: &str, z: f32) -> (f32, f32) {
    if text.is_empty() {
        return (0., 0.);
    }
    let run = [TextRun { len: text.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
    let l = window.text_system().shape_line(SharedString::from(text), px(12. * z), &run, None);
    (f32::from(l.width()), 14. * z)
}

/// 实心三角形箭头：len=ARROW_LEN_PX·s、半宽 5·s，尖端落在线终点（=节点边缘/消息线端），
/// 颜色=边色（`Some(色)` 填充、无描边）。流程/序列共用——`->>`/`-->`/`==>` 等实心箭头
/// 同款；len 与 layout 收短弧线/ line 回缩用同一 [`ARROW_LEN_PX`] 常量，保证「线末=基部」。
fn arrow_head(window: &mut Window, tip: (f32, f32), dir: (f32, f32), color: Hsla, s: f32) {
    let base = (tip.0 - dir.0 * ARROW_LEN_PX * s, tip.1 - dir.1 * ARROW_LEN_PX * s);
    let (px_, py_) = (dir.1 * 5. * s, -dir.0 * 5. * s);
    let p1 = (base.0 + px_, base.1 + py_);
    let p2 = (base.0 - px_, base.1 - py_);
    let pts: Vec<Point<Pixels>> = [(p1.0, p1.1), (tip.0, tip.1), (p2.0, p2.1)]
        .into_iter()
        .map(|(x, y)| point(px(x), px(y)))
        .collect();
    path_pts(window, &pts, Some(color), None, 0.);
}

#[allow(clippy::too_many_arguments)] // paint 文本图元：画布 + 字体/坐标互为正交
fn text(
    window: &mut Window,
    _theme: &MmTheme,
    x: f32,
    y: f32,
    s: &str,
    font: gpui::Font,
    fs: f32,
    color: Hsla,
    lh: f32,
    cx: &mut App,
) {
    if s.is_empty() {
        return;
    }
    let run = [TextRun { len: s.len(), font, color, ..Default::default() }];
    let line = window.text_system().shape_line(SharedString::from(s), px(fs), &run, None);
    let _ = line.paint(point(px(x), px(y)), px(lh), gpui::TextAlign::Left, None, window, cx);
}

/// 居中文本（按整行宽把起点左移一半；边标签/序列消息用）。
#[allow(clippy::too_many_arguments)] // 低层画图原语：显式标量参数比打包结构更直白
fn text_centered(
    window: &mut Window,
    x: f32,
    y: f32,
    s: &str,
    font: gpui::Font,
    fs: f32,
    color: Hsla,
    lh: f32,
    cx: &mut App,
) {
    if s.is_empty() {
        return;
    }
    let run = [TextRun { len: s.len(), font, color, ..Default::default() }];
    let line = window.text_system().shape_line(SharedString::from(s), px(fs), &run, None);
    let w = f32::from(line.width());
    let _ = line.paint(point(px(x - w / 2.), px(y)), px(lh), gpui::TextAlign::Center, None, window, cx);
}

/// 画一个节点形状（卡片相对坐标 + 原点）。
#[allow(clippy::too_many_arguments)] // 节点图元：画布 + 已排版几何/样式互为正交
fn paint_node_shape(
    window: &mut Window,
    theme: &MmTheme,
    p: &MmPlaced,
    ox: f32,
    oy: f32,
    fill: Hsla,
    border: Hsla,
    bw: f32,
    z: f32,
    cx: &mut App,
) {
    let x = p.x + ox;
    let y = p.y + oy;
    let w = p.w;
    let h = p.h;
    match p.shape {
        MmShape::Rect => paint_quad(window, x, y, w, h, 6., fill, border, bw),
        MmShape::Round => paint_quad(window, x, y, w, h, 10., fill, border, bw),
        MmShape::Stadium => paint_quad(window, x, y, w, h, h / 2., fill, border, bw),
        MmShape::Subroutine => {
            paint_quad(window, x, y, w, h, 6., fill, border, bw);
            let inner = border;
            stroke_seg(window, (x + 8., y + 2.), (x + 8., y + h - 2.), inner, 1.);
            stroke_seg(window, (x + w - 8., y + 2.), (x + w - 8., y + h - 2.), inner, 1.);
        }
        MmShape::Circle => {
            circle(window, x + w / 2., y + h / 2., w / 2., h / 2., fill, border, bw);
        }
        MmShape::DoubleCircle => {
            circle(window, x + w / 2., y + h / 2., w / 2., h / 2., fill, border, bw);
            circle(window, x + w / 2., y + h / 2., w / 2. - 5., h / 2. - 5., Hsla::default(), border, 1.2);
        }
        MmShape::Diamond => {
            let pts = [(x + w / 2., y), (x + w, y + h / 2.), (x + w / 2., y + h), (x, y + h / 2.)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::Hexagon => {
            let sx = w * 0.18;
            let pts = [(x + sx, y), (x + w - sx, y), (x + w, y + h / 2.), (x + w - sx, y + h), (x + sx, y + h), (x, y + h / 2.)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::Parallelogram => {
            let sk = h * 0.45;
            let pts = [(x + sk, y), (x + w, y), (x + w - sk, y + h), (x, y + h)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::ParallelogramAlt => {
            let sk = h * 0.45;
            let pts = [(x, y), (x + w - sk, y), (x + w, y + h), (x + sk, y + h)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::Trapezoid => {
            let sk = h * 0.45;
            let pts = [(x + sk, y), (x + w, y), (x + w, y + h), (x, y + h)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::TrapezoidAlt => {
            let sk = h * 0.45;
            let pts = [(x, y), (x + w - sk, y), (x + w, y + h), (x + sk, y + h)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::Asymmetric => {
            let pts = [(x, y), (x + w - 10., y), (x + w, y + h / 2.), (x + w - 10., y + h), (x, y + h)];
            polygon(window, &pts, fill, border, bw);
        }
        MmShape::Cylinder => {
            // 柱身（上至上椭圆的中心线，下到下椭圆）。
            let r = 6.;
            paint_quad(window, x, y + r, w, h - 2. * r, 0., fill, Hsla::default(), 0.);
            // 上/下椭圆线。
            circle(window, x + w / 2., y + r, w / 2., r, fill, border, bw);
            circle(window, x + w / 2., y + h - r, w / 2., r, Hsla::default(), border, bw);
            // 柱身两侧。
            stroke_seg(window, (x, y + r), (x, y + h - r), border, bw);
            stroke_seg(window, (x + w, y + r), (x + w, y + h - r), border, bw);
        }
    }
    // 标签（字号随缩放等比）。
    if p.label.is_empty() {
        return;
    }
    let fs = f32::from(theme.font_size) * z;
    let run = [TextRun { len: p.label.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
    let l = window.text_system().shape_line(SharedString::from(p.label.as_str()), px(fs), &run, None);
    let tw = f32::from(l.width());
    let lh = 18. * z;
    let ty = y + (h - lh) / 2.;
    let _ = l.paint(point(px(x + (w - tw) / 2.), px(ty)), px(lh), gpui::TextAlign::Left, None, window, cx);
}

// ---------------------------------------------------------------------------
// 序列图绘制
// ---------------------------------------------------------------------------

/// 画序列图布局。
#[allow(clippy::too_many_arguments)] // paint 图元：画布(window/theme/cx) + 几何参数互为正交
pub(crate) fn paint_mermaid_seq(
    window: &mut Window,
    theme: &MmTheme,
    seq: &MmSeqLayout,
    ox: f32,
    oy: f32,
    _area_w: f32,
    cx: &mut App,
) {
    let cols = &seq.columns;
    let z = seq.zoom;
    let header_h = 30. * z;
    let body_top = oy + seq.body_top;
    // 参与者头部盒 + 生命线（虚线）。
    let line_color = { let mut c = theme.mermaid_border; c.a = 0.5; c };
    for c in cols {
        let x = c.x + ox;
        // 生命线。
        dashed_path(window, &[(x + c.w / 2., oy + header_h + 6. * z), (x + c.w / 2., body_top + (seq.height - header_h))], line_color, 1. * z, 3. * z, 3. * z);
        // 头部盒。
        let head_border = { let mut c = theme.mermaid_border; c.a = 0.7; c };
        paint_quad(window, x + 4. * z, oy + 4. * z, c.w - 8. * z, header_h - 8. * z, 6. * z, theme.table_head_bg, head_border, 1. * z);
        text(window, theme, x + 6. * z, oy + 6. * z + 2. * z, &c.label, theme.font.clone(), 13. * z, theme.text, 18. * z, cx);
    }

    // 片段盒（先扫出区间再画底层）。
    let frag_boxes: Vec<(usize, f32, f32)> = scan_fragments(&seq.rows);
    let band_color = { let mut c = theme.mermaid_border; c.a = 0.10; c };
    let band_border = { let mut c = theme.mermaid_border; c.a = 0.55; c };
    for (fi, y0, y1) in &frag_boxes {
        let bx = ox + 6.;
        let by = oy + y0;
        let bw = seq.content_w - 12.;
        let bh = (y1 - y0).max(14. * z);
        paint_quad(window, bx, by, bw, bh, 6., band_color, band_border, 1.);
        if let Some(frg) = seq.seq.fragments.get(*fi) {
            let label = frag_title(&frg.kind, &frg.title);
            if !label.is_empty() {
                text(window, theme, bx + 10., by + (14. - 10.) / 2. * z, &label, theme.font.clone(), 11. * z, theme.muted, 15. * z, cx);
            }
        }
    }

    // 激活条。
    let act_color = { let mut c = theme.accent; c.a = 0.75; c };
    for a in &seq.activations {
        if let Some(c) = cols.iter().find(|c| c.id == a.id) {
            let cx_ = c.x + ox + c.w / 2. - 3. * z;
            paint_quad(window, cx_, oy + a.y0, 6. * z, (a.y1 - a.y0).max(4. * z), 3. * z, act_color, Hsla::default(), 0.);
        }
    }

    // 逐行画消息/Note。
    for (ry, row) in &seq.rows {
        match row {
            MmSeqRow::Message(mi) => {
                if let Some(m) = seq.seq.messages.get(*mi)
                    && let (Some(fc), Some(tc)) = (col_of(cols, &m.from), col_of(cols, &m.to)) {
                        paint_message(window, theme, fc, tc, m.arrow, &m.text, ox, oy + ry, z, cx);
                    }
            }
            MmSeqRow::Note(ni) => {
                if let Some(no) = seq.seq.notes.get(*ni) {
                    paint_note(window, theme, cols, no.pos, &no.a, no.b.as_deref(), &no.text, ox, oy + ry, z, cx);
                }
            }
            MmSeqRow::FragElse(title) => {
                let by = oy + ry;
                let bx = ox + 6.;
                stroke_seg(window, (bx, by), (bx + seq.content_w - 12., by), { let mut c = band_border; c.a = 0.8; c }, 1.);
                if let Some(t) = title
                    && !t.is_empty() {
                        text(window, theme, bx + 10., by + 4. * z, t, theme.font.clone(), 11. * z, theme.muted, 15. * z, cx);
                    }
            }
            _ => {}
        }
    }
}

fn frag_title(kind: &MmSeqFragmentKind, title: &str) -> String {
    let name = match kind {
        MmSeqFragmentKind::Loop => "loop",
        MmSeqFragmentKind::Alt => "alt",
        MmSeqFragmentKind::Opt => "opt",
        MmSeqFragmentKind::Par => "par",
    };
    format!("{name} {}", title).trim_end().to_string()
}

/// 扫片段盒：`(片段下标, 开行 y, 结束行 y)`。
fn scan_fragments(rows: &[(f32, MmSeqRow)]) -> Vec<(usize, f32, f32)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, f32)> = Vec::new();
    for (y, r) in rows {
        match r {
            MmSeqRow::FragOpen(fi) => stack.push((*fi, *y)),
            MmSeqRow::FragClose(fi) => {
                if let Some(idx) = stack.iter().rposition(|(f, _)| f == fi) {
                    let (fi, y0) = stack.remove(idx);
                    out.push((fi, y0, *y));
                }
            }
            _ => {}
        }
    }
    out
}

fn col_of<'a>(cols: &'a [MmSeqColumn], id: &str) -> Option<&'a MmSeqColumn> {
    cols.iter().find(|c| c.id == id)
}

/// 画一条消息（同列自环/异列横线）。
#[allow(clippy::too_many_arguments)] // 消息图元：画布 + 列几何/箭头/文本互为正交
fn paint_message(
    window: &mut Window,
    theme: &MmTheme,
    fc: &MmSeqColumn,
    tc: &MmSeqColumn,
    arrow: MmSeqArrow,
    text: &str,
    ox: f32,
    y: f32,
    z: f32,
    cx: &mut App,
) {
    let col = |c: &MmSeqColumn| -> f32 { c.x + ox + c.w / 2. };
    let ax = col(fc);
    let bx = col(tc);
    let dotted = matches!(arrow, MmSeqArrow::Dotted | MmSeqArrow::DottedOpen | MmSeqArrow::DottedCross);
    let head = matches!(arrow, MmSeqArrow::Solid | MmSeqArrow::Dotted | MmSeqArrow::SolidCross | MmSeqArrow::DottedCross);
    let color = { let mut c = theme.mermaid_border; c.a = 0.9; c };
    let width = 1.2 * z;
    if (ax - bx).abs() < 1. {
        // 自环：向右画一个小折环。
        let r = 14. * z;
        let pts = [(ax, y - 2. * z), (ax + r, y - 2. * z), (ax + r, y + 10. * z), (ax, y + 10. * z)];
        if dotted {
            dashed_path(window, &[pts[0], pts[1], pts[2], pts[3]], color, width, 3. * z, 3. * z);
        } else {
            stroke_poly(window, &pts, color, width);
        }
    } else {
        let lo = ax.min(bx);
        let hi = ax.max(bx);
        let (x0, x1) = if ax < bx { (lo + 4. * z, hi - 4. * z) } else { (hi + 4. * z, lo - 4. * z) };
        let dir: (f32, f32) = if ax < bx { (1., 0.) } else { (-1., 0.) };
        // 线到箭头基部、箭头到线端（tip=x1，4·z 处生命线停靠）：与流程图端箭头同一规则
        // （`ARROW_LEN_PX` 同源）——不再「线穿到线端 + 箭头同点」双连接。
        let base = arrow_base((x1, y), dir, ARROW_LEN_PX * z);
        if dotted {
            dashed_path(window, &[(x0, y), (base.0, base.1)], color, width, 4. * z, 4. * z);
        } else {
            stroke_seg(window, (x0, y), base, color, width);
        }
        // 箭头在目标端（实心填充三角形，尖端落线端 tip=x1）。
        if head {
            arrow_head(window, (x1, y), dir, color, z);
        }
        stride_label(window, theme, (x0 + x1) / 2., y, text, z, cx);
    }
}

fn stride_label(window: &mut Window, theme: &MmTheme, x: f32, y: f32, s: &str, z: f32, cx: &mut App) {
    if s.is_empty() {
        return;
    }
    // 文字浮在线上方。
    let run = [TextRun { len: s.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
    let l = window.text_system().shape_line(SharedString::from(s), px(12. * z), &run, None);
    let w = f32::from(l.width());
    let _ = l.paint(point(px(x - w / 2.), px(y - 17. * z)), px(15. * z), gpui::TextAlign::Center, None, window, cx);
}

#[allow(clippy::too_many_arguments)] // Note 图元：画布 + 列几何/位置互为正交
fn paint_note(
    window: &mut Window,
    theme: &MmTheme,
    cols: &[MmSeqColumn],
    pos: MmSeqNotePos,
    a: &str,
    b: Option<&str>,
    note_text: &str,
    ox: f32,
    y: f32,
    z: f32,
    cx: &mut App,
) {
    let ac = col_of(cols, a);
    if ac.is_none() {
        return;
    }
    let ac = ac.unwrap();
    let cx0 = ac.x + ox + ac.w / 2.;
    let (x0, x1) = match pos {
        MmSeqNotePos::Left => {
            // 参与者左侧。
            let w = (text_w(window, theme, note_text, z) + 20. * z).clamp(60. * z, 200. * z);
            (cx0 - 12. * z - w, cx0 - 12. * z)
        }
        MmSeqNotePos::Right => {
            let w = (text_w(window, theme, note_text, z) + 20. * z).clamp(60. * z, 200. * z);
            (cx0 + 12. * z, cx0 + 12. * z + w)
        }
        MmSeqNotePos::Over => {
            let x_end = b.and_then(|bb| col_of(cols, bb)).map(|bc| bc.x + ox + bc.w / 2.).unwrap_or(cx0);
            let lo = cx0.min(x_end);
            let hi = cx0.max(x_end);
            (lo - 8. * z, hi + 8. * z)
        }
    };
    let w = (x1 - x0).max(40. * z);
    let h = 26. * z;
    let y0 = y + (SEQ_NOTE_H * z - h) / 2.;
    let note_bg = { let mut c = theme.code_inline_bg; c.a = 0.8; c };
    let border = { let mut c = theme.mermaid_border; c.a = 0.7; c };
    paint_quad(window, x0, y0, w, h, 5. * z, note_bg, border, 1. * z);
    text(window, theme, x0 + 6. * z, y0 + 4. * z, note_text, theme.font.clone(), 12. * z, theme.text, 16. * z, cx);
}

fn text_w(window: &mut Window, theme: &MmTheme, s: &str, z: f32) -> f32 {
    if s.is_empty() {
        return 0.;
    }
    let run = [TextRun { len: s.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
    f32::from(window.text_system().shape_line(SharedString::from(s), px(12. * z), &run, None).width())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 点 p 到文字矩形（无 margin）的最近距离。
    fn dist_pt_rect(p: (f32, f32), x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
        let dx = if p.0 < x0 { x0 - p.0 } else if p.0 > x1 { p.0 - x1 } else { 0. };
        let dy = if p.1 < y0 { y0 - p.1 } else if p.1 > y1 { p.1 - y1 } else { 0. };
        (dx * dx + dy * dy).sqrt()
    }

    /// 直边（LR 横向）穿文字矩形：整段被开口切成 head/tail，断点精确落在矩形边界、
    /// 无任何点落在文字矩形内。
    #[test]
    fn split_poly_gaps_straight_edge_at_label() {
        let pts = [(0., 100.), (40., 100.), (200., 100.)];
        let (head, tail) = split_poly_at_rect(&pts, 80., 90., 160., 110.);
        assert_eq!(head, vec![(0., 100.), (40., 100.), (80., 100.)], "head 收在入矩形点（精确到边）");
        let tail = tail.expect("直边穿文字矩形应拆出尾段");
        assert_eq!(tail, vec![(160., 100.), (200., 100.)], "tail 从出矩形点续到终点");
        for p in head.iter().chain(tail.iter()) {
            assert!(!(80. < p.0 && p.0 < 160.), "点 {p:?} 不应落在文字矩形内");
        }
    }

    /// 弧线采样（模拟用户例图 arc 12 点）穿文字矩形：入/出点精确落边界，矩形内段被完全
    /// 移除（线不穿字），两侧段各自连续。
    #[test]
    fn split_poly_gaps_arc_like_polyline() {
        let pts = [(60., 90.), (90., 100.), (120., 110.), (150., 100.), (180., 90.)];
        let (head, tail) = split_poly_at_rect(&pts, 80., 90., 160., 110.);
        assert_eq!(head.len(), 2, "head 应止于入矩形点");
        let te = 1e-2;
        assert!((head[1].0 - 80.).abs() < te && (head[1].1 - 96.6667).abs() < te, "head 末点应在左边界：{head:?}");
        let tail = tail.expect("弧线穿文字矩形应拆出尾段");
        assert_eq!(tail.len(), 2, "tail 应从出矩形点续到终点：{tail:?}");
        assert!((tail[0].0 - 160.).abs() < te && (tail[0].1 - 96.6667).abs() < te, "tail 首点应在右边界：{tail:?}");
        assert_eq!(tail[1], (180., 90.));
        for p in head.iter().chain(tail.iter()) {
            assert!(!(80. < p.0 && p.0 < 160. && 90. < p.1 && p.1 < 110.), "点 {p:?} 不应落在文字矩形内");
        }
    }

    /// 折线不穿矩形：原样单段返回（无标签时行为不变）。
    #[test]
    fn split_poly_keeps_polyline_when_missing_rect() {
        let pts = [(0., 0.), (10., 0.), (20., 0.)];
        let (head, tail) = split_poly_at_rect(&pts, 30., -5., 60., 5.);
        assert_eq!(head, pts.to_vec());
        assert!(tail.is_none(), "未穿矩形不应拆尾段");
    }
}
