//! Mermaid 布局（shape 期一次算好缓存进 `ShapedRow.mm`；paint 只消费）：
//! flowchart 走「最长路径分层 + 长边插虚节点 + barycenter 交叉最小化 + 方向参数化」，
//! sequence 走泳道列布局。`mermaid_card_height` 保持**无窗口**可算（frame 围栏高度
//! 估算依赖）——所有几何常数在此与 paint 共用。

use super::{MmDirection, MmEdgeKind, MmFlowLayout, MmGraph, MmLayout, MmPlaced, MmPolylineEdge, MmSeq, MmSeqRow, MmShape, MmStyle, MmSubgraphBox, MM_EDGE_X, MM_H_GAP, MM_LANE, MM_NODE_H, MM_NODE_XPAD, MM_RANK_TB, MM_SEP, MM_SUB_TITLE_H, MM_TOP_PAD, mermaid_empty_height};
use crate::markdown::wysiwyg::WyTheme;
use gpui::{Pixels, SharedString, TextRun, Window, px};
use std::collections::HashMap;

/// 序列图列（卡片相对坐标）。
#[derive(Clone, Debug)]
pub(crate) struct MmSeqColumn {
    pub id: String,
    pub label: String,
    pub is_actor: bool,
    pub x: f32,
    pub w: f32,
}

/// 序列图激活条（参与者 id + 卡片相对 y 区间）。
#[derive(Clone, Debug)]
pub(crate) struct MmSeqActivation {
    pub id: String,
    pub y0: f32,
    pub y1: f32,
}

/// 序列图布局结果。
#[derive(Clone, Debug)]
pub(crate) struct MmSeqLayout {
    pub seq: MmSeq,
    pub columns: Vec<MmSeqColumn>,
    /// (卡片相对 y, 行)。
    pub rows: Vec<(f32, MmSeqRow)>,
    pub activations: Vec<MmSeqActivation>,
    /// 参与者头部区域底（生命线从这开始）。
    pub body_top: f32,
    pub height: f32,
    pub content_w: f32,
    /// 滚轮缩放（paint 用它等比缩放字号；几何已在 `layout_seq` 后按此整体缩放）。
    pub zoom: f32,
}

/// 卡片高度：由图类型分发（无窗口）。
pub(crate) fn mermaid_card_height(source: &str) -> Pixels {
    use super::MmDiagramKind;
    match super::diagram_kind(source) {
        MmDiagramKind::Seq => seq_card_height(source),
        MmDiagramKind::Empty => mermaid_empty_height(),
        MmDiagramKind::Flow => flow_card_height(source),
    }
}

/// 滚轮缩放：把已排布局整体等比缩放 `zoom`（x 相对卡片水平中线、y 相对顶，宽/高同倍），
/// paint 用同 `zoom` 缩放字号保持比例（节点/边/分组/序列列坐标统一走此变换）。
/// 由 `shape_code_row` 在 `mm_layout` 之后调用；`zoom == 1` 直接原样返回。
pub(crate) fn scale_mm_layout(mm: &mut MmLayout, zoom: f32, area_w: f32) {
    if zoom <= 0. || (zoom - 1.).abs() < 1e-4 {
        return;
    }
    let cx = area_w / 2.;
    let tx = |x: f32| cx + (x - cx) * zoom;
    match mm {
        MmLayout::Flow(f) => {
            for p in &mut f.placed {
                p.x = tx(p.x);
                p.y *= zoom;
                p.w *= zoom;
                p.h *= zoom;
            }
            for e in &mut f.edges {
                for pt in &mut e.points {
                    pt.0 = tx(pt.0);
                    pt.1 *= zoom;
                }
                if let Some((lx, ly)) = &mut e.label {
                    *lx = tx(*lx);
                    *ly *= zoom;
                }
            }
            for sg in &mut f.subgraphs {
                sg.x = tx(sg.x);
                sg.y *= zoom;
                sg.w *= zoom;
                sg.h *= zoom;
            }
            f.zoom = zoom;
        }
        MmLayout::Seq(s) => {
            for c in &mut s.columns {
                c.x = tx(c.x);
                c.w *= zoom;
            }
            for (y, _) in &mut s.rows {
                *y *= zoom;
            }
            for a in &mut s.activations {
                a.y0 *= zoom;
                a.y1 *= zoom;
            }
            s.body_top *= zoom;
            s.height *= zoom;
            s.zoom = zoom;
        }
        MmLayout::Empty => {}
    }
}

/// 入口：shape 期计算并缓存整张图布局。
pub(crate) fn mm_layout(
    window: &Window,
    theme: &WyTheme,
    source: &str,
    area_x: f32,
    area_y: f32,
    area_w: f32,
) -> MmLayout {
    use super::MmDiagramKind;
    match super::diagram_kind(source) {
        MmDiagramKind::Seq => {
            let seq = super::mermaid_seq(source);
            if seq.participants.is_empty() {
                return MmLayout::Empty;
            }
            MmLayout::Seq(layout_seq(window, theme, seq, area_x, area_y, area_w))
        }
        MmDiagramKind::Empty => MmLayout::Empty,
        MmDiagramKind::Flow => {
            let g = super::mermaid_graph(source);
            if g.is_empty() {
                return MmLayout::Empty;
            }
            MmLayout::Flow(layout_flow(window, theme, &g, area_x, area_y, area_w))
        }
    }
}

// ---------------------------------------------------------------------------
// flowchart 布局
// ---------------------------------------------------------------------------

fn node_sub_depth(g: &MmGraph, id: &str) -> usize {
    g.subgraphs.iter().filter(|s| s.nodes.iter().any(|x| x == id)).count()
}

/// 最长路径分层。
fn layer_map(g: &MmGraph) -> HashMap<String, usize> {
    let mut layer: HashMap<String, usize> = HashMap::new();
    for n in &g.nodes {
        layer.insert(n.id.clone(), 0);
    }
    for _ in 0..g.nodes.len().max(1) {
        let mut changed = false;
        for e in &g.edges {
            let la = layer.get(&e.from).copied().unwrap_or(0);
            if layer.get(&e.to).copied().unwrap_or(0) < la + 1 {
                layer.insert(e.to.clone(), la + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    layer
}

/// 无窗口可算的 flowchart 几何特征：(层数, 单层最多节点数, 最深 subgraph 嵌套)。
fn flow_extents(g: &MmGraph) -> (usize, usize, usize) {
    let layer = layer_map(g);
    let n_layers = layer.values().copied().max().unwrap_or(0) + 1;
    let mut row = vec![0usize; n_layers];
    for &l in layer.values() {
        row[l] += 1;
    }
    let max_row = row.iter().copied().max().unwrap_or(0);
    let max_sub = g.nodes.iter().map(|n| node_sub_depth(g, &n.id)).max().unwrap_or(0);
    (n_layers.max(1), max_row, max_sub)
}

/// 无窗口 flowchart 卡高（与 `layout_flow` 的几何一致）。
fn flow_card_height(source: &str) -> Pixels {
    let g = super::mermaid_graph(source);
    if g.is_empty() {
        return mermaid_empty_height();
    }
    let (n_layers, max_row, max_sub) = flow_extents(&g);
    let h = match g.direction {
        MmDirection::LR | MmDirection::RL => {
            MM_TOP_PAD + max_row as f32 * MM_LANE + max_sub as f32 * MM_SUB_TITLE_H + 8.
        }
        MmDirection::TB | MmDirection::BT => {
            MM_TOP_PAD + n_layers as f32 * MM_RANK_TB + max_sub as f32 * MM_SUB_TITLE_H + 8.
        }
    };
    px(h)
}

/// 测量标签宽（虚节点给固定小宽）。
fn measure_width(window: &Window, theme: &WyTheme, label: &str, virtual_node: bool) -> f32 {
    if virtual_node {
        return 10.;
    }
    if label.is_empty() {
        return 10.;
    }
    let run = [TextRun { len: label.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
    f32::from(window.text_system().shape_line(SharedString::from(label), theme.font_size, &run, None).width()) + 2. * MM_NODE_XPAD
}

/// 方向变换（节点：带 w/h 偏移；点：无）。
fn orient(
    dir: MmDirection,
    px_: f32,
    py_: f32,
    axis_ref: f32,
) -> (f32, f32) {
    match dir {
        MmDirection::TB => (px_, py_),
        MmDirection::BT => (px_, axis_ref - py_),
        MmDirection::LR => (py_, px_),
        MmDirection::RL => (axis_ref - py_, px_),
    }
}

fn layout_flow(
    window: &Window,
    theme: &WyTheme,
    g: &MmGraph,
    area_x: f32,
    area_y: f32,
    area_w: f32,
) -> MmFlowLayout {
    let layer_of = layer_map(g);
    let n_layers = layer_of.values().copied().max().unwrap_or(0) + 1;
    let avail = (area_w - 2. * MM_EDGE_X).max(1.);
    let xc = area_x + MM_EDGE_X;

    // 元素 = 实节点 + 长边插桩虚节点。
    let mut elements: Vec<(String, String, MmShape)> = g.nodes.iter().map(|n| (n.id.clone(), n.label.clone(), n.shape)).collect();
    let real_index: HashMap<String, usize> = g.nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();
    let mut virtual_layers: Vec<Option<usize>> = vec![None; elements.len()];
    let mut chain: Vec<(usize, usize, MmEdgeKind, Option<String>)> = Vec::new();
    for e in &g.edges {
        let a = real_index[&e.from];
        let b = real_index[&e.to];
        let la = layer_of[&e.from];
        let lb = layer_of[&e.to];
        if lb >= la + 2 {
            let mut cur = a;
            for l in la + 1..lb {
                elements.push((format!("~v{}", elements.len()), String::new(), MmShape::Circle));
                virtual_layers.push(Some(l));
                chain.push((cur, elements.len() - 1, MmEdgeKind::Line, None));
                cur = elements.len() - 1;
            }
            chain.push((cur, b, e.kind, e.label.clone()));
        } else {
            chain.push((a, b, e.kind, e.label.clone()));
        }
    }
    let n_elem = elements.len();
    let el_layer = |ei: usize| virtual_layers[ei].unwrap_or_else(|| layer_of.get(&elements[ei].0).copied().unwrap_or(0));
    let el_sub = |ei: usize| node_sub_depth(g, &elements[ei].0);

    // 逐层元素序（barycenter 交叉最小化）。
    let mut order: Vec<Vec<usize>> = vec![Vec::new(); n_layers];
    for ei in 0..n_elem {
        order[el_layer(ei)].push(ei);
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n_elem];
    for (a, b, _, _) in &chain {
        let (a, b) = (*a, *b);
        if !adj[a].contains(&b) {
            adj[a].push(b);
        }
        if !adj[b].contains(&a) {
            adj[b].push(a);
        }
    }
    // barycenter（下/上交替 4 轮）。
    let mut pos_in_layer: HashMap<usize, usize> = HashMap::new();
    let refresh_pos = |order: &[Vec<usize>], pos: &mut HashMap<usize, usize>| {
        pos.clear();
        for (li, o) in order.iter().enumerate() {
            for (j, &ei) in o.iter().enumerate() {
                pos.insert(ei, li * 100_000 + j);
            }
        }
    };
    for _ in 0..4 {
        // 下向
        for li in 1..n_layers {
            let below: Vec<usize> = order[li].to_vec();
            let mut with_bary: Vec<(usize, usize)> = Vec::new();
            for &ei in &below {
                let mut sum = 0usize;
                let mut cnt = 0usize;
                for &nb in &adj[ei] {
                    if let Some(p) = pos_in_layer.get(&nb) {
                        sum += *p;
                        cnt += 1;
                    }
                }
                let bary = if cnt > 0 { sum.checked_div(cnt).unwrap_or(0) } else { usize::MAX };
                with_bary.push((ei, bary));
            }
            // 稳定排序（含 `MAX` 垫底）。
            let mut sorted: Vec<usize> = below.clone();
            sorted.sort_by_key(|&ei| with_bary.iter().find(|(e, _)| *e == ei).map(|(_, b)| *b).unwrap_or(usize::MAX));
            order[li] = sorted;
        }
        refresh_pos(&order, &mut pos_in_layer);
        // 上向
        for li in (0..n_layers.saturating_sub(1)).rev() {
            let above: Vec<usize> = order[li].to_vec();
            let mut with_bary: Vec<(usize, usize)> = Vec::new();
            for &ei in &above {
                let mut sum = 0usize;
                let mut cnt = 0usize;
                for &nb in &adj[ei] {
                    if let Some(p) = pos_in_layer.get(&nb) {
                        sum += *p;
                        cnt += 1;
                    }
                }
                let bary = if cnt > 0 { sum.checked_div(cnt).unwrap_or(0) } else { usize::MAX };
                with_bary.push((ei, bary));
            }
            let mut sorted: Vec<usize> = above.clone();
            sorted.sort_by_key(|&ei| with_bary.iter().find(|(e, _)| *e == ei).map(|(_, b)| *b).unwrap_or(usize::MAX));
            order[li] = sorted;
        }
        refresh_pos(&order, &mut pos_in_layer);
    }

    // 节点位置（先在 TB 规范空间：axis=+y 层下标，cross=+x 行内居中）。
    let max_sub = g.nodes.iter().map(|n| node_sub_depth(g, &n.id)).max().unwrap_or(0);
    let mut cx: Vec<f32> = vec![0.; n_elem];
    let mut cy: Vec<f32> = vec![0.; n_elem];
    let mut cw: Vec<f32> = vec![0.; n_elem];
    let mut ch: Vec<f32> = vec![MM_NODE_H; n_elem];
    let mut widths: Vec<f32> = vec![0.; n_elem];
    let mut virtual_flag: Vec<bool> = vec![false; n_elem];
    for (ei, (_, lab, _)) in elements.iter().enumerate() {
        let is_v = virtual_layers[ei].is_some();
        virtual_flag[ei] = is_v;
        widths[ei] = measure_width(window, theme, lab, is_v);
        if is_v {
            cw[ei] = 8.;
            ch[ei] = 8.;
        } else {
            cw[ei] = widths[ei];
        }
    }
    // 层下标→轴线位（flow axis，规范空间 +y）：TB/BT 固定秩距；LR/RL 按每列最宽节点
    // 推进——宽节点不再挤进下一列（02/03 实测的左挤症状）。
    let mut axis_pos: Vec<f32> = vec![0.; n_layers];
    match g.direction {
        MmDirection::TB | MmDirection::BT => {
            for li in 0..n_layers {
                axis_pos[li] = MM_TOP_PAD + li as f32 * MM_RANK_TB;
            }
        }
        MmDirection::LR | MmDirection::RL => {
            let mut acc = MM_TOP_PAD;
            for li in 0..n_layers {
                axis_pos[li] = acc;
                let maxw = order[li].iter().map(|&ei| widths[ei]).fold(0.0_f32, f32::max);
                acc += maxw + MM_H_GAP;
            }
        }
    }
    for li in 0..n_layers {
        let row = &order[li];
        if matches!(g.direction, MmDirection::LR | MmDirection::RL) {
            // 横向图：规范 cross（+x）= 纵向固定步距堆叠；层下标沿 +y 轴线（列位置）。
            for (j, &ei) in row.iter().enumerate() {
                let sub = el_sub(ei);
                cx[ei] = MM_TOP_PAD + j as f32 * MM_LANE;
                cy[ei] = axis_pos[li] + sub as f32 * MM_SUB_TITLE_H;
            }
        } else {
            let row_total: f32 = row.iter().map(|&ei| cw[ei]).sum::<f32>() + MM_SEP * (row.len().saturating_sub(1)) as f32;
            let mut x = xc + ((avail - row_total) / 2.).max(0.);
            for &ei in row {
                let sub = el_sub(ei);
                cx[ei] = x;
                cy[ei] = axis_pos[li] + sub as f32 * MM_SUB_TITLE_H;
                x += cw[ei] + MM_SEP;
            }
        }
    }

    // 方向变换（整体坐标系）。
    let axis_ref_or: f32 = match g.direction {
        MmDirection::BT => {
            axis_pos[n_layers - 1] + MM_RANK_TB + max_sub as f32 * MM_SUB_TITLE_H + MM_NODE_H + 8.
        }
        MmDirection::RL => {
            let last_w = order[n_layers - 1].iter().map(|&ei| widths[ei]).fold(0.0_f32, f32::max);
            axis_pos[n_layers - 1] + last_w + MM_H_GAP
        }
        _ => 0.,
    };

    // 最终 placed。
    let mut placed: Vec<MmPlaced> = Vec::with_capacity(n_elem);
    for (ei, (id, lab, shp)) in elements.iter().enumerate() {
        let ew = cw[ei];
        let eh = ch[ei];
        let (fx, fy) = orient(g.direction, cx[ei], cy[ei], axis_ref_or);
        let st = g.styles.get(id).copied().unwrap_or(MmStyle::default());
        placed.push(MmPlaced {
            id: id.clone(),
            label: lab.clone(),
            shape: *shp,
            x: area_x + fx,
            y: area_y + fy,
            w: ew,
            h: eh,
            fill: st.fill,
            stroke: st.stroke,
            stroke_width: st.stroke_width,
            virtual_node: virtual_flag[ei],
        });
        let _ = (ew, eh);
    }

    // 边折线：**在最终坐标**里按方向装配——起终点钉在**节点边缘中心**（TB/BT 顶/底、
    // LR/RL 右/左），生成横/纵直角折线。旧的「规范空间纵向路由 + 方向变换」在 LR/RL 下
    // 起终点钉在节点中心，节点宽度不同就歪斜。
    let vertical_flow = matches!(g.direction, MmDirection::TB | MmDirection::BT);
    let mut edges_out: Vec<MmPolylineEdge> = Vec::with_capacity(chain.len());
    for (a, b, kind, label) in &chain {
        let pa = &placed[*a];
        let pb = &placed[*b];
        let (pts, label_anchor) = if vertical_flow {
            let (ay0, by0) = if pb.y + pb.h <= pa.y {
                (pa.y, pb.y + pb.h) // BT：B 在 A 之上，A 顶出、B 底进
            } else {
                (pa.y + pa.h, pb.y) // TB：B 在 A 之下，A 底出、B 顶进
            };
            let ax = pa.x + pa.w / 2.;
            let bx = pb.x + pb.w / 2.;
            let mid_y = (ay0 + by0) / 2.;
            if (bx - ax).abs() < 4. {
                (vec![(ax, ay0), (bx, by0)], Some((ax + 8., mid_y - 4.)))
            } else {
                (
                    vec![(ax, ay0), (ax, mid_y), (bx, mid_y), (bx, by0)],
                    Some(((ax + bx) / 2., mid_y - 8.)),
                )
            }
        } else {
            let (ax0, bx0) = if pb.x + pb.w <= pa.x {
                (pa.x, pb.x + pb.w) // RL：B 在 A 之左，A 左出、B 右进
            } else {
                (pa.x + pa.w, pb.x) // LR：B 在 A 之右，A 右出、B 左进
            };
            let ay = pa.y + pa.h / 2.;
            let by = pb.y + pb.h / 2.;
            let mid_x = (ax0 + bx0) / 2.;
            if (by - ay).abs() < 4. {
                (vec![(ax0, ay), (bx0, by)], Some((mid_x + 6., ay - 8.)))
            } else {
                (vec![(ax0, ay), (mid_x, ay), (mid_x, by), (bx0, by)], Some((mid_x + 8., (ay + by) / 2.)))
            }
        };
        edges_out.push(MmPolylineEdge { a: *a, b: *b, kind: *kind, points: pts, label: label_anchor, label_text: label.clone() });
    }

    // subgraph 盒（最终坐标）。
    let mut sub_out: Vec<MmSubgraphBox> = Vec::new();
    for sg in &g.subgraphs {
        let members: Vec<usize> = sg.nodes.iter().filter_map(|id| real_index.get(id).copied()).collect();
        if members.is_empty() {
            continue;
        }
        let mut x0 = f32::MAX;
        let mut y0 = f32::MAX;
        let mut x1 = f32::MIN;
        let mut y1 = f32::MIN;
        for &mi in &members {
            let p = &placed[mi];
            if p.virtual_node {
                continue;
            }
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x + p.w);
            y1 = y1.max(p.y + p.h);
        }
        if x0 > x1 {
            continue;
        }
        let pad = 8.;
        let bx = (x0 - pad).max(area_x + MM_EDGE_X);
        let by = (y0 - pad).max(area_y + MM_TOP_PAD);
        sub_out.push(MmSubgraphBox { title: sg.title.clone(), x: bx, y: by, w: (x1 - x0 + 2. * pad).max(0.), h: (y1 - y0 + 2. * pad).max(0.) });
    }

    let content_h = placed.iter().map(|p| p.y + p.h).fold(area_y, f32::max) - area_y;
    MmFlowLayout { placed, edges: edges_out, subgraphs: sub_out, content_w: area_w, content_h, zoom: 1. }
}

// ---------------------------------------------------------------------------
// 序列图布局
// ---------------------------------------------------------------------------

/// 序列图行高常数（与 paint / 卡高共用）。
pub(crate) const SEQ_HEADER_H: f32 = 46.;
pub(crate) const SEQ_MSG_H: f32 = 26.;
pub(crate) const SEQ_NOTE_H: f32 = 34.;
pub(crate) const SEQ_BAND_H: f32 = 26.;
pub(crate) const SEQ_GAP: f32 = 26.;
pub(crate) const SEQ_COL_MIN: f32 = 60.;

fn seq_row_h(r: &MmSeqRow) -> f32 {
    match r {
        MmSeqRow::Message(_) => SEQ_MSG_H,
        MmSeqRow::Note(_) => SEQ_NOTE_H,
        MmSeqRow::FragOpen(..) => SEQ_BAND_H,
        MmSeqRow::FragElse(..) => SEQ_BAND_H,
        MmSeqRow::FragClose(..) | MmSeqRow::Activate(..) | MmSeqRow::Deactivate(..) => 0.,
    }
}

/// 无窗口序列图卡高。
fn seq_card_height(source: &str) -> Pixels {
    let seq = super::mermaid_seq(source);
    if seq.participants.is_empty() {
        return mermaid_empty_height();
    }
    let mut h = SEQ_HEADER_H;
    for r in &seq.rows {
        h += seq_row_h(r);
    }
    px(h + 16.)
}

fn layout_seq(window: &Window, theme: &WyTheme, seq: MmSeq, area_x: f32, area_y: f32, area_w: f32) -> MmSeqLayout {
    // 列宽（shaping 标签）。
    let mut cols: Vec<MmSeqColumn> = Vec::new();
    for p in &seq.participants {
        let run = [TextRun { len: p.label.len(), font: theme.font.clone(), color: theme.text, ..Default::default() }];
        let w = f32::from(window.text_system().shape_line(p.label.clone().into(), theme.font_size, &run, None).width());
        cols.push(MmSeqColumn { id: p.id.clone(), label: p.label.clone(), is_actor: p.is_actor, x: 0., w: (w + 24.).max(SEQ_COL_MIN) });
    }
    let n = cols.len();
    let total: f32 = cols.iter().map(|c| c.w).sum::<f32>() + SEQ_GAP * n.saturating_sub(1) as f32;
    let start = area_x + MM_EDGE_X + ((area_w - 2. * MM_EDGE_X) - total).max(0.) / 2.;
    let mut x = start;
    for c in &mut cols {
        c.x = x;
        x += c.w + SEQ_GAP;
    }
    // 行 y。
    let body_top = area_y + SEQ_HEADER_H;
    let mut rows: Vec<(f32, MmSeqRow)> = Vec::new();
    let mut y = body_top;
    for r in &seq.rows {
        let h = seq_row_h(r);
        rows.push((y, r.clone()));
        y += h;
    }
    let height = (y - area_y) + 16.;

    // 激活条：遍历状态行推算每个参与者深度区间。
    let mut depth: HashMap<String, usize> = HashMap::new();
    let mut active_start: HashMap<String, f32> = HashMap::new();
    let mut activations: Vec<MmSeqActivation> = Vec::new();
    for (ry, r) in &rows {
        match r {
            MmSeqRow::Message(mi) => {
                let m = &seq.messages[*mi];
                if m.act != 0 && !m.to.is_empty() {
                    let d = depth.entry(m.to.clone()).or_insert(0);
                    *d = ((*d as i32) + m.act as i32).max(0) as usize;
                    let cur = *d;
                    if m.act > 0 {
                        if !active_start.contains_key(&m.to) {
                            active_start.insert(m.to.clone(), *ry);
                        }
                    } else if cur == 0 {
                        close_activation(&mut active_start, &m.to, *ry, &mut activations);
                    }
                }
            }
            MmSeqRow::Activate(id) => {
                *depth.entry(id.clone()).or_insert(0) += 1;
                if depth.get(id).copied().unwrap_or(0) == 1 && !active_start.contains_key(id) {
                    active_start.insert(id.clone(), *ry);
                }
            }
            MmSeqRow::Deactivate(id) => {
                if let Some(d) = depth.get_mut(id) {
                    *d = d.saturating_sub(1);
                }
                if depth.get(id).copied().unwrap_or(0) == 0 {
                    close_activation(&mut active_start, id, *ry, &mut activations);
                }
            }
            _ => {}
        }
    }
    // 未关闭的补到末尾。
    for (id, sy0) in active_start {
        if y > sy0 + 1. {
            activations.push(MmSeqActivation { id, y0: sy0, y1: y });
        }
    }
    MmSeqLayout { seq, columns: cols, rows, activations, body_top, height, content_w: area_w, zoom: 1. }
}

/// 激活区间收口（深度归 0 时把 `[start, y_now)` 攒成一个激活条）。
fn close_activation(
    active_start: &mut HashMap<String, f32>,
    id: &str,
    y_now: f32,
    activations: &mut Vec<MmSeqActivation>,
) {
    if let Some(start) = active_start.remove(id) {
        if y_now > start + 1. {
            activations.push(MmSeqActivation { id: id.to_string(), y0: start, y1: y_now });
        }
    }
}
