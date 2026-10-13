//! Mermaid 布局（shape 期一次算好缓存进 `ShapedRow.mm`；paint 只消费）：
//! flowchart 走「最长路径分层 + 长边插虚节点 + barycenter 交叉最小化 + 方向参数化」，
//! sequence 走泳道列布局。`mermaid_card_height` 保持**无窗口**可算（frame 围栏高度
//! 估算依赖）——所有几何常数在此与 paint 共用。

use super::{MmDirection, MmEdgeKind, MmFlowLayout, MmGraph, MmLayout, MmPlaced, MmPolylineEdge, MmSeq, MmSeqRow, MmShape, MmStyle, MmSubgraphBox, ARROW_LEN_PX, MM_EDGE_X, MM_H_GAP, MM_LANE, MM_NODE_H, MM_NODE_XPAD, MM_RANK_TB, MM_SEP, MM_SUB_TITLE_H, MM_TOP_PAD, mermaid_empty_height};
use crate::markdown::wysiwyg::WyTheme;
use gpui::{Hsla, Pixels, SharedString, TextRun, Window, px};
use std::collections::HashMap;

/// Mermaid 节点分类色板长度（=`ColorScheme.mermaid_accent_palette` 的 6 色；zed 用
/// 8 色玩家板，本仓库给 6 即可——循环取色下标只在此推导，paint 只查表）。
pub(crate) const MM_ACCENT_COUNT: usize = 6;

/// accent 填充不透明度：把 accent 以 15% 混到卡底（仿 zed `blend_over_background`）。
const ACCENT_FILL_OPACITY: f32 = 0.15;

/// 分类 accent 下标分配（**纯函数**，供 `assemble_placed` 与单测共用）：按 `g.nodes`
/// 声明序循环 `0..MM_ACCENT_COUNT`；作者 classDef/style 显式 hex（fill/stroke 任一）
/// 着色节点不参与循环、映射 `None`（按作者色绘制）。`assemble_placed` 据此再补
/// 虚节点（id 不在表内）的 `None`。
pub(crate) fn accent_idx_for(g: &MmGraph) -> HashMap<String, Option<usize>> {
    let mut out = HashMap::new();
    let mut acc = 0usize;
    for n in &g.nodes {
        let st = g.styles.get(&n.id);
        let author_colored = st.map_or(false, |s| s.fill.is_some() || s.stroke.is_some());
        out.insert(
            n.id.clone(),
            if author_colored {
                None
            } else {
                let i = acc % MM_ACCENT_COUNT;
                acc += 1;
                Some(i)
            },
        );
    }
    out
}

/// accent 填充 hex：把 accent 以 `ACCENT_FILL_OPACITY` 不透明度在 RGB 通道线性混到
/// 卡底 `card_bg`（zed `blend_over_background` 同款）。**shape 期算好锁进
/// `MmPlaced.fill`** → paint 零计算（不逐帧 blend/重算）。
fn accent_fill_hex(accent: Hsla, card_bg: Hsla) -> u32 {
    let fg = gpui::Rgba::from(accent);
    let bg = gpui::Rgba::from(card_bg);
    let t = ACCENT_FILL_OPACITY;
    let blended = gpui::Rgba {
        r: fg.r * t + bg.r * (1.0 - t),
        g: fg.g * t + bg.g * (1.0 - t),
        b: fg.b * t + bg.b * (1.0 - t),
        a: 1.0,
    };
    hsla_to_hex(blended.into())
}

/// Hsla → 0xRRGGBB（与 `parse.rs` 的 hex / `paint::to_hsla` 同式，丢 alpha 字节）。
fn hsla_to_hex(c: Hsla) -> u32 {
    (u32::from(gpui::Rgba::from(c)) >> 8) & 0xFF_FF_FF
}

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
                if let Some(sm) = &mut e.smoothed {
                    for pt in sm.iter_mut() {
                        pt.0 = tx(pt.0);
                        pt.1 *= zoom;
                    }
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
    // 分阶段装配：元素链 → 交叉最小化次序 → 尺寸/坐标 → placed → 边 → 分组盒。
    let fe = flow_elements(g);
    let order = barycenter_order(&fe.elements, &fe.virtual_layers, &fe.layer_of, &fe.chain, fe.n_layers);
    let sizes = flow_sizes(window, theme, &fe.elements, &fe.virtual_layers);
    let geom = flow_positions(g, &fe.elements, &order, sizes, area_x, area_w);
    let placed = assemble_placed(g, theme, &fe.elements, area_x, area_y, geom);
    let edges = route_edges(g, &fe.chain, &placed);
    let subgraphs = flow_subgraph_boxes(g, &fe.real_index, &placed, area_x, area_y);

    let content_h = placed.iter().map(|p| p.y + p.h).fold(area_y, f32::max) - area_y;
    MmFlowLayout { placed, edges, subgraphs, content_w: area_w, content_h, zoom: 1. }
}

/// 元素链装配结果：元素表、实节点索引、虚节点层、链、分层、层数。
struct FlowElements {
    elements: Vec<(String, String, MmShape)>,
    real_index: HashMap<String, usize>,
    virtual_layers: Vec<Option<usize>>,
    chain: Vec<(usize, usize, MmEdgeKind, Option<String>)>,
    layer_of: HashMap<String, usize>,
    n_layers: usize,
}

/// 元素装配：实节点 + 长边插桩虚节点，产出链、分层与索引。
fn flow_elements(g: &MmGraph) -> FlowElements {
    let layer_of = layer_map(g);
    let n_layers = layer_of.values().copied().max().unwrap_or(0) + 1;
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
    FlowElements { elements, real_index, virtual_layers, chain, layer_of, n_layers }
}

/// 重算 `(层下标, 行内下标)` 编码位置。
fn refresh_positions(order: &[Vec<usize>], pos: &mut HashMap<usize, usize>) {
    pos.clear();
    for (li, o) in order.iter().enumerate() {
        for (j, &ei) in o.iter().enumerate() {
            pos.insert(ei, li * 100_000 + j);
        }
    }
}

/// 单层按邻点 barycenter 稳定重排（无邻点 `usize::MAX` 垫底）。
fn sort_layer_by_barycenter(
    layer: &[usize],
    adj: &[Vec<usize>],
    pos_in_layer: &HashMap<usize, usize>,
) -> Vec<usize> {
    let mut with_bary: Vec<(usize, usize)> = Vec::new();
    for &ei in layer {
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
    let mut sorted: Vec<usize> = layer.to_vec();
    sorted.sort_by_key(|&ei| with_bary.iter().find(|(e, _)| *e == ei).map(|(_, b)| *b).unwrap_or(usize::MAX));
    sorted
}

/// 逐层元素序 + barycenter 交叉最小化（下/上交替 4 轮）。
fn barycenter_order(
    elements: &[(String, String, MmShape)],
    virtual_layers: &[Option<usize>],
    layer_of: &HashMap<String, usize>,
    chain: &[(usize, usize, MmEdgeKind, Option<String>)],
    n_layers: usize,
) -> Vec<Vec<usize>> {
    let n_elem = elements.len();
    let el_layer = |ei: usize| virtual_layers[ei].unwrap_or_else(|| layer_of.get(&elements[ei].0).copied().unwrap_or(0));
    let mut order: Vec<Vec<usize>> = vec![Vec::new(); n_layers];
    for ei in 0..n_elem {
        order[el_layer(ei)].push(ei);
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n_elem];
    for (a, b, _, _) in chain {
        let (a, b) = (*a, *b);
        if !adj[a].contains(&b) {
            adj[a].push(b);
        }
        if !adj[b].contains(&a) {
            adj[b].push(a);
        }
    }
    let mut pos_in_layer: HashMap<usize, usize> = HashMap::new();
    for _ in 0..4 {
        // 下向
        for layer in order.iter_mut().skip(1) {
            let sorted = sort_layer_by_barycenter(layer, &adj, &pos_in_layer);
            *layer = sorted;
        }
        refresh_positions(&order, &mut pos_in_layer);
        // 上向
        for layer in order.iter_mut().rev().skip(1) {
            let sorted = sort_layer_by_barycenter(layer, &adj, &pos_in_layer);
            *layer = sorted;
        }
        refresh_positions(&order, &mut pos_in_layer);
    }
    order
}

/// 节点尺寸（TB 规范空间前的宽度度量；虚节点给固定小尺寸）。
struct FlowSizes {
    cw: Vec<f32>,
    ch: Vec<f32>,
    widths: Vec<f32>,
    virtual_flag: Vec<bool>,
}

fn flow_sizes(
    window: &Window,
    theme: &WyTheme,
    elements: &[(String, String, MmShape)],
    virtual_layers: &[Option<usize>],
) -> FlowSizes {
    let n_elem = elements.len();
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
    FlowSizes { cw, ch, widths, virtual_flag }
}

/// TB 规范空间节点几何（方向变换前）：层轴位、行内居中坐标与变换基准。
struct FlowGeometry {
    cx: Vec<f32>,
    cy: Vec<f32>,
    cw: Vec<f32>,
    ch: Vec<f32>,
    virtual_flag: Vec<bool>,
    axis_ref: f32,
}

fn flow_positions(
    g: &MmGraph,
    elements: &[(String, String, MmShape)],
    order: &[Vec<usize>],
    sizes: FlowSizes,
    area_x: f32,
    area_w: f32,
) -> FlowGeometry {
    let n_layers = order.len();
    let n_elem = elements.len();
    let el_sub = |ei: usize| node_sub_depth(g, &elements[ei].0);
    let avail = (area_w - 2. * MM_EDGE_X).max(1.);
    let xc = area_x + MM_EDGE_X;
    let max_sub = g.nodes.iter().map(|n| node_sub_depth(g, &n.id)).max().unwrap_or(0);

    let mut cx: Vec<f32> = vec![0.; n_elem];
    let mut cy: Vec<f32> = vec![0.; n_elem];
    // 层下标→轴线位（flow axis，规范空间 +y）：TB/BT 固定秩距；LR/RL 按每列最宽节点
    // 推进——宽节点不再挤进下一列（02/03 实测的左挤症状）。
    let mut axis_pos: Vec<f32> = vec![0.; n_layers];
    match g.direction {
        MmDirection::TB | MmDirection::BT => {
            for (li, slot) in axis_pos.iter_mut().enumerate() {
                *slot = MM_TOP_PAD + li as f32 * MM_RANK_TB;
            }
        }
        MmDirection::LR | MmDirection::RL => {
            let mut acc = MM_TOP_PAD;
            for (li, slot) in axis_pos.iter_mut().enumerate() {
                *slot = acc;
                let maxw = order[li].iter().map(|&ei| sizes.widths[ei]).fold(0.0_f32, f32::max);
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
            let row_total: f32 = row.iter().map(|&ei| sizes.cw[ei]).sum::<f32>() + MM_SEP * (row.len().saturating_sub(1)) as f32;
            let mut x = xc + ((avail - row_total) / 2.).max(0.);
            for &ei in row {
                let sub = el_sub(ei);
                cx[ei] = x;
                cy[ei] = axis_pos[li] + sub as f32 * MM_SUB_TITLE_H;
                x += sizes.cw[ei] + MM_SEP;
            }
        }
    }

    // 方向变换（整体坐标系）基准。
    let axis_ref: f32 = match g.direction {
        MmDirection::BT => {
            axis_pos[n_layers - 1] + MM_RANK_TB + max_sub as f32 * MM_SUB_TITLE_H + MM_NODE_H + 8.
        }
        MmDirection::RL => {
            let last_w = order[n_layers - 1].iter().map(|&ei| sizes.widths[ei]).fold(0.0_f32, f32::max);
            axis_pos[n_layers - 1] + last_w + MM_H_GAP
        }
        _ => 0.,
    };

    FlowGeometry { cx, cy, cw: sizes.cw, ch: sizes.ch, virtual_flag: sizes.virtual_flag, axis_ref }
}

/// 方向变换并装配 placed（整体坐标系）。`accent_idx` 在此按节点序循环调色板
/// （作者 classDef/style 显式 hex 节点为 None）；accent 的填充/描边也在 **shape 期**
/// 预混好锁进 `fill`/`stroke`，paint 只查表零计算。
fn assemble_placed(
    g: &MmGraph,
    theme: &WyTheme,
    elements: &[(String, String, MmShape)],
    area_x: f32,
    area_y: f32,
    geom: FlowGeometry,
) -> Vec<MmPlaced> {
    let accents = accent_idx_for(g);
    let mut placed: Vec<MmPlaced> = Vec::with_capacity(elements.len());
    for (ei, (id, lab, shp)) in elements.iter().enumerate() {
        let (fx, fy) = orient(g.direction, geom.cx[ei], geom.cy[ei], geom.axis_ref);
        let st = g.styles.get(id).copied().unwrap_or(MmStyle::default());
        let accent_idx = accents.get(id.as_str()).copied().flatten();
        // accent 节点的填充 = 混色 tint、描边 = accent 实色（shape 期定死，paint 零计算）；
        // author 节点原样保留作者 hex；虚节点（不在映射表）走 None → paint 兜底。
        let (fill, stroke) = match accent_idx {
            Some(i) => {
                let a = theme.mermaid_accents[i];
                (Some(accent_fill_hex(a, theme.code_bg)), Some(hsla_to_hex(a)))
            }
            None => (st.fill, st.stroke),
        };
        placed.push(MmPlaced {
            id: id.clone(),
            label: lab.clone(),
            shape: *shp,
            x: area_x + fx,
            y: area_y + fy,
            w: geom.cw[ei],
            h: geom.ch[ei],
            fill,
            stroke,
            stroke_width: st.stroke_width,
            accent_idx,
            virtual_node: geom.virtual_flag[ei],
        });
    }
    placed
}

/// 目标端箭头方向 = 目标节点接触边的**垂直内法向**（与弧线切线无关，用户「垂直指向节点」）：
/// 按本边实际钉在目标的哪条边定（与 `route_edges` 的钉边规则同源）：目标在源之下（顶边进）
/// → (0,1) 垂直向下；在源之上（底边进）→ (0,-1) 垂直向上；在源之右（左边进）→ (1,0) 垂直
/// 向右；在源之左（右边进）→ (-1,0) 垂直向左。返回单位向量。
fn edge_arrow_dir(vertical_flow: bool, pa: &MmPlaced, pb: &MmPlaced) -> (f32, f32) {
    if vertical_flow {
        if pb.y + pb.h <= pa.y {
            (0., -1.)
        } else {
            (0., 1.)
        }
    } else if pb.x + pb.w <= pa.x {
        (-1., 0.)
    } else {
        (1., 0.)
    }
}

/// 箭头基部：节点边缘接触点（tip）沿连接外侧回缩 `len`（=端箭头长）。布局把弧线目标端
/// 收短到基部（`len = ARROW_LEN_PX`，规范空间 z=1）；paint 把线末点回缩到基部（`len =
/// ARROW_LEN_PX·zoom`，缩放后同公式）——两处**共用此函数**则「线末=基部、箭头 tip=节点
/// 边缘」在任意缩放下严格重合（均匀缩放 + 平移保角，`arrow_dir` 为单位向量不变）。
pub(crate) fn arrow_base(tip: (f32, f32), dir: (f32, f32), len: f32) -> (f32, f32) {
    (tip.0 - dir.0 * len, tip.1 - dir.1 * len)
}

/// 边折线：**在最终坐标**里按方向装配——起终点钉在**节点边缘中心**（TB/BT 顶/底、
/// LR/RL 右/左），生成横/纵直角折线。旧的「规范空间纵向路由 + 方向变换」在 LR/RL 下
/// 起终点钉在节点中心，节点宽度不同就歪斜。标签锚点一律居中**路径中点**（直边/肘边
/// 同式），交给断线盒盖住连线；`smoothed` 在此一次算好缓存（同源分支单弯肘直接 S
/// 曲线 / 其余 4 点肘圆角 / 2 点直边箭头 S 曲线，见 [`smoothed_edge`]）。弧线目标端额外
/// **收短到箭头基部**（`points.last()` 仍保留节点边缘作逻辑锚点）——「线到基部、箭头到
/// 节点」双连接修复；paint 对直边/圆角肘再按 `arrow_dir` 轴对齐回缩线末点（同一常量）。
fn route_edges(
    g: &MmGraph,
    chain: &[(usize, usize, MmEdgeKind, Option<String>)],
    placed: &[MmPlaced],
) -> Vec<MmPolylineEdge> {
    let vertical_flow = matches!(g.direction, MmDirection::TB | MmDirection::BT);
    // 每个源的出边数 + 目标列表：出度 ≥2 且横向偏移 → 同源分支边直接 S 曲线（用户 .1 分支曲线），
    // 否则 4 点肘保留圆角正交折线。
    let mut out_t: HashMap<usize, Vec<usize>> = HashMap::new();
    for (a, b, _, _) in chain {
        out_t.entry(*a).or_default().push(*b);
    }
    let mut edges_out: Vec<MmPolylineEdge> = Vec::with_capacity(chain.len());
    for (a, b, kind, label) in chain {
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
                (vec![(ax, ay0), (bx, by0)], Some(((ax + bx) / 2., mid_y)))
            } else {
                (
                    vec![(ax, ay0), (ax, mid_y), (bx, mid_y), (bx, by0)],
                    Some(((ax + bx) / 2., mid_y)),
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
                (vec![(ax0, ay), (bx0, by)], Some((mid_x, (ay + by) / 2.)))
            } else {
                (
                    vec![(ax0, ay), (mid_x, ay), (mid_x, by), (bx0, by)],
                    Some((mid_x, (ay + by) / 2.)),
                )
            }
        };
        let branch = out_t.get(a).map_or(0, Vec::len) >= 2;
        // 弓向限制（分支边才受限）：以「源盒 + 本源全部子节点盒」的联合骨架 bbox 为界，
        // 每分支只许朝目标那一侧的外侧空间弓——二次贝塞尔整体落在控制点凸包内 ⇒ 只要
        // 控制点 Q 不越 bbox，整条弧线就不越界（「肚皮」至多与目标列外侧齐平，绝不压
        // 同侧相邻子盒/源盒）。目标即最外侧兄弟时上限 ≈ 外侧整段空距（放手外弓分叉）；
        // 内侧兄弟因幅度随横向距离单调（见 q_pref）天然在外侧兄弟之内，不会被压。
        // 单出边（无兄弟）无此约束（None）。
        let bounds = out_t.get(a).filter(|t| t.len() >= 2).map(|t| {
            let (pk0, pk1) = if vertical_flow { (pa.x, pa.x + pa.w) } else { (pa.y, pa.y + pa.h) };
            let (mut lo, mut hi) = (pk0, pk1);
            for &k in t {
                let (k0, k1) = if vertical_flow {
                    (placed[k].x, placed[k].x + placed[k].w)
                } else {
                    (placed[k].y, placed[k].y + placed[k].h)
                };
                lo = lo.min(k0);
                hi = hi.max(k1);
            }
            (lo, hi)
        });
        // 单弓弧线的弓高上限：控制点 Q=弦中点+外侧·q，Q 不越联合 bbox ⇒ q ≤ 弦中点到
        // 目标侧 bbox 外沿的距离。内侧兄弟（更外侧还有同向兄弟）此限更宽，但 q_pref
        // 的「幅度随 lat 单调」保证它仍小于外兄弟的幅度，分支彼此永不交叉。
        // 横向轴：TB/BT 用 x（节点宽）、LR/RL 用 y（节点高）。
        let (src_c, tgt_c) = if vertical_flow {
            (pa.x + pa.w / 2., pb.x + pb.w / 2.)
        } else {
            (pa.y + pa.h / 2., pb.y + pb.h / 2.)
        };
        let ch_mid = (src_c + tgt_c) / 2.;
        let q_max = bounds.map(|(lo, hi)| {
            if tgt_c >= src_c {
                hi - ch_mid
            } else {
                ch_mid - lo
            }
        });
        // 目标横向半尺寸（弧线「肚皮」落到目标列外侧一点的依据；LR/RL 用节点高）。
        let tgt_w = if vertical_flow { pb.w } else { pb.h };
        // 目标端箭头方向 = 目标接触边垂直内法向（[`edge_arrow_dir`]）：按本边实际钉在目标
        // 的哪条边定死（TB 顶边→(0,1)、BT 底边→(0,-1)、LR 左→(1,0)、RL 右→(-1,0)），与
        // 弧线切线无关——箭头「垂直指向节点」而非跟随弧线入射角度（旧 `end_tangent` 解析
        // 切线在 12 采样/大弓高下与垂直方向偏差 5~11.5°，肉眼歪斜，用户投诉根因）。
        // 先算 `arrow_dir`：弧线要按它把目标端收短到箭头基部（见下 `base`）。
        let arrow_dir = edge_arrow_dir(vertical_flow, pa, pb);
        // 箭头基部（规范空间 z=1）= 节点边缘接触点沿连接外侧回缩一个**箭头长**：弧线目标
        // 端收短到基部（paint 最后一段线到基部、再画实心箭头从基部到 tip）。`points.last()`
        // 逻辑锚点仍保留节点边缘（标签锚点 / 缩放 / paint 箭头 tip 的单一来源，不随弧线
        // 尾部收短而变）——「线到基部、箭头到节点」双连接修复的 layout 侧落点。
        let base = arrow_base(*pts.last().unwrap(), arrow_dir, ARROW_LEN_PX);
        let smoothed = smoothed_edge(g.direction, *kind, &pts, branch, Some(tgt_w), q_max, Some(base));
        // 弧线（smoothed=12 点）弓起后中点偏离骨架中线 → 标签锚点改取曲线中点，
        // 断线盒仍盖住连线；圆角肘/直线仍用原路径中点（骨架中线）。
        let label_anchor = match &smoothed {
            Some(sm) if sm.len() == 12 => Some(sm[sm.len() / 2]),
            _ => label_anchor,
        };
        edges_out.push(MmPolylineEdge { a: *a, b: *b, kind: *kind, points: pts, smoothed, label: label_anchor, label_text: label.clone(), arrow_dir });
    }
    edges_out
}

/// 曲线化纯函数（layout 期一次算好缓存进 `MmPolylineEdge.smoothed`，paint 只消费）：
/// - **2 点直边**（骨架无中间点）：`Arrow` 且端点有横向（轴）偏移 → 二次贝塞尔**单弓弧线**
///   （见 [`arc_curve`]）；端点横/纵对齐或非 Arrow（虚节点直腿）→ 退化为直线（返回
///   `None`，paint 直接用骨架）。
/// - **4 点正交肘**：*同源分支*（`branch`，源出度 ≥2）且 `Arrow` → 直接按端点出
///   [弧线](`arc_curve`)（用户 .1「分支边曲线」；单弯肘横/纵偏移必 ≥4 由路由保证）；
///   其余每个内角二次贝塞尔圆角（切点 A=Pk−in_u·rk、B=Pk+out_u·rk、控制点=Pk，
///   rk=clamp(min(前段,后段)·0.3, 3, 10)，采样 8 点），中间水平段保留。
/// 每边采样 ≤24 点（骨架恒 ≤4 点 ⇒ 至多 2 个内角）。`base`（Optional）= 箭头基部：
/// 弧线目标端收短到基部（`None` = 直调单测时退化为原节点边缘，保持旧行为）；圆角肘
/// 不在此收短（paint 侧按 `arrow_dir` 轴对齐精确回缩，见 [`paint_edge`]）。
fn smoothed_edge(
    dir: MmDirection,
    kind: MmEdgeKind,
    pts: &[(f32, f32)],
    branch: bool,
    tgt_w: Option<f32>,
    q_max: Option<f32>,
    base: Option<(f32, f32)>,
) -> Option<Vec<(f32, f32)>> {
    match pts.len() {
        2 => {
            let (p0, p3) = (pts[0], pts[1]);
            // 横向（垂直于行进方向）偏移量；对齐则不曲。
            let lateral = match dir {
                MmDirection::TB | MmDirection::BT => (p3.0 - p0.0).abs(),
                MmDirection::LR | MmDirection::RL => (p3.1 - p0.1).abs(),
            };
            if kind != MmEdgeKind::Arrow || lateral < 4. {
                return None;
            }
            // 弧线目标端 = 箭头基部（节点边缘向连接外侧回缩一个箭头长）；无 base（直调
            // 单测）则退化为原节点边缘，保持旧行为。
            Some(arc_curve(p0, p3, dir, tgt_w, q_max, base.unwrap_or(p3)))
        }
        // 同源分支边单弯肘（4 点：源直出 → 一段横/竖 → 目标直入）：端点横向偏移已由
        // 路由保证 ≥4，直接按端点出弧线（出射端沿行进方向 offset、单弓向外）。目标端
        // 同样收短到箭头基部。
        4 if branch && kind == MmEdgeKind::Arrow => {
            let p0 = pts[0];
            let p3 = *pts.last().unwrap();
            Some(arc_curve(p0, p3, dir, tgt_w, q_max, base.unwrap_or(p3)))
        }
        n if n >= 3 => Some(round_polyline(pts)),
        _ => None,
    }
}

/// 二次贝塞尔**单弓弧线**（弓向目标外侧 = 远离源列中心线的外侧分叉，TB/BT/RL/LR
/// 任意方向自洽，BT/RL 反向推进仍正确）：
/// P0=出射点、P3=入射点，Q=骨架直线中点 + n·q（n=弓起方向单位向量，指向目标所在侧）。
///
/// 解析性质（二次贝塞尔 + 控制点取弦中点的结构）：
/// - **行进坐标线性**：Q 的行进坐标（TB/BT 的 y、LR/RL 的 x）恰为 P0/P3 均值 ⇒
///   行进坐标随 t **线性**推进，曲线只占据「源出射边 → 目标入射边」之间的带，绝无
///   回折/穿回源或目标盒；
/// - **对直线弦的横向（外侧）偏距 = 2·q·t(1−t)**（代入 x(t)=(1−t)²x0+2t(1−t)(弦中+q)+t²x3
///   展开即得）：符号恒为「目标侧」、中点（t=½）**最大 = q/2**（肚皮）⇒ **无 S 拐点、
///   无回折、不越弦**（这正是用户「别弯来弯去」的几何落点）；端点精确穿过 P0/P3；
/// - **凸包界**：整条曲线落在 {P0,Q,P3} 凸包内 ⇒ 凡 Q 不越的 bbox，弧线也不越。
///
/// q 的取值（`q_pref` 为主，双保险钳制）：
/// - 目标节点宽度已知（`tgt_w`，路由期传入）→ `q_pref=clamp(0.5·lat+0.5·tgt_w+3,14,84)`，
///   肚皮（q/2）落在目标列中区偏外侧、约过目标入射点 ~3px（窗口化实测），同源分支幅度
///   随 lat 单调 ⇒ 分支彼此不交叉（同侧两支 D−C=(Δlat)·t+2(q_D−q_C)t(1−t) ≥ 0）；
/// - 无宽度信息（单元直调）→ 退化为 `q_pref=clamp(1.35·lat,16,84)`；
/// - 再取 `min(1.15·adv)`（弓高不超过行进步，避免横摆过度）与 `min(q_max)`（见
///   [`route_edges`]，控制点 Q 不越联合 bbox ⇒ 整条弧线亦然）。
/// 12 采样含两端点，paint 逐段画弧。箭头方向**不再取弧线切线**——改由目标接触边的垂直
/// 内法向决定（见 [`edge_arrow_dir`]），弧线本体只负责路径形状（用户「垂直指向节点」）。
/// `end` = 弧线实际终点 = 箭头基部（节点边缘 `p3` 向连接外侧回缩一个箭头长，见
/// [`arrow_base`]）；控制点仍由真实 `p3`（节点边缘）推导，故弧线的弓幅/入射切线几何不变，
/// 只是尾部 9px 收短到基部——入射段仍沿原切线方向进入基部（用户「线到基部、箭头到节点」）。
fn arc_curve(p0: (f32, f32), p3: (f32, f32), dir: MmDirection, tgt_w: Option<f32>, q_max: Option<f32>, end: (f32, f32)) -> Vec<(f32, f32)> {
    let ctrl = arc_ctrl(p0, p3, dir, tgt_w, q_max);
    sample_quad_full(p0, ctrl, end, 12)
}

/// 弧线控制点 Q = 弦中点 + 外侧 · q。`arc_curve` 采样整条弧线；箭头方向不随弧线切线走，
/// 由目标接触边垂直内法向决定（见 [`edge_arrow_dir`]）。
fn arc_ctrl(p0: (f32, f32), p3: (f32, f32), dir: MmDirection, tgt_w: Option<f32>, q_max: Option<f32>) -> (f32, f32) {
    let (nx, ny) = match dir {
        MmDirection::TB | MmDirection::BT => (if p3.0 >= p0.0 { 1. } else { -1. }, 0.),
        MmDirection::LR | MmDirection::RL => (0., if p3.1 >= p0.1 { 1. } else { -1. }),
    };
    let (lat, adv) = match dir {
        MmDirection::TB | MmDirection::BT => ((p3.0 - p0.0).abs(), (p3.1 - p0.1).abs()),
        MmDirection::LR | MmDirection::RL => ((p3.1 - p0.1).abs(), (p3.0 - p0.0).abs()),
    };
    let w = tgt_w.unwrap_or(0.);
    let q_pref = if w > 0. {
        (0.5 * lat + 0.5 * w + 3.).clamp(14., 84.)
    } else {
        (1.35 * lat).clamp(16., 84.)
    };
    let q = q_pref.min(1.15 * adv).min(q_max.unwrap_or(f32::INFINITY));
    let mid = ((p0.0 + p3.0) / 2., (p0.1 + p3.1) / 2.);
    (mid.0 + nx * q, mid.1 + ny * q)
}

/// 二次贝塞尔采样（含两端点；n 点，中点当 n 奇为 t=0.5）。
fn sample_quad_full(a: (f32, f32), ctrl: (f32, f32), b: (f32, f32), n: usize) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / (n - 1).max(1) as f32;
        let mt = 1. - t;
        let x = mt * mt * a.0 + 2. * mt * t * ctrl.0 + t * t * b.0;
        let y = mt * mt * a.1 + 2. * mt * t * ctrl.1 + t * t * b.1;
        out.push((x, y));
    }
    out
}

/// 二次贝塞尔采样（`a`→`b`，控制点 `ctrl`；n 点，含 `b` 不含 `a`）。
fn sample_quad(a: (f32, f32), ctrl: (f32, f32), b: (f32, f32), n: usize) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(n);
    for i in 1..=n {
        let t = i as f32 / n as f32;
        let mt = 1. - t;
        let x = mt * mt * a.0 + 2. * mt * t * ctrl.0 + t * t * b.0;
        let y = mt * mt * a.1 + 2. * mt * t * ctrl.1 + t * t * b.1;
        out.push((x, y));
    }
    out
}

fn pt_len(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    (dx * dx + dy * dy).sqrt()
}

/// 正交肘圆角：对骨架每个内角切掉 rk，用切点 A/B + 控制点=Pk 的二次贝塞尔 8 点采样
/// 替代直角拐点；中间（水平）直段由相邻切点间的连线段保留。首尾锚定与骨架一致。
fn round_polyline(pts: &[(f32, f32)]) -> Vec<(f32, f32)> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out: Vec<(f32, f32)> = vec![pts[0]];
    for k in 1..pts.len() - 1 {
        let prev = pts[k - 1];
        let cur = pts[k];
        let next = pts[k + 1];
        let l1 = pt_len(prev, cur);
        let l2 = pt_len(cur, next);
        let rk = (l1.min(l2) * 0.3).clamp(3., 10.).min(l1).min(l2);
        let in_len = l1.max(1e-6);
        let out_len = l2.max(1e-6);
        let in_u = ((prev.0 - cur.0) / in_len, (prev.1 - cur.1) / in_len);
        let out_u = ((next.0 - cur.0) / out_len, (next.1 - cur.1) / out_len);
        let a = (cur.0 + in_u.0 * rk, cur.1 + in_u.1 * rk);
        let b = (cur.0 + out_u.0 * rk, cur.1 + out_u.1 * rk);
        let last = *out.last().unwrap();
        if pt_len(last, a) > 1e-3 {
            out.push(a);
        }
        out.extend(sample_quad(a, cur, b, 8));
    }
    let end = *pts.last().unwrap();
    if pt_len(*out.last().unwrap(), end) > 1e-3 {
        out.push(end);
    }
    out
}

/// subgraph 盒（最终坐标，取成员包围盒加内边距）。
fn flow_subgraph_boxes(
    g: &MmGraph,
    real_index: &HashMap<String, usize>,
    placed: &[MmPlaced],
    area_x: f32,
    area_y: f32,
) -> Vec<MmSubgraphBox> {
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
    sub_out
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
    if let Some(start) = active_start.remove(id)
        && y_now > start + 1. {
            activations.push(MmSeqActivation { id: id.to_string(), y0: start, y1: y_now });
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::wysiwyg::mermaid::{MmEdge, MmNode, MmSubgraph};
    use std::collections::HashMap;

    /// 手工构造最小 flowchart 图（方向 TB，节点/边按给定序——序号即元素下标）。
    fn mini_flow(nodes: &[&str], edges: &[(&str, &str)]) -> MmGraph {
        MmGraph {
            direction: MmDirection::TB,
            nodes: nodes
                .iter()
                .map(|id| MmNode {
                    id: id.to_string(),
                    label: id.to_string(),
                    shape: MmShape::Rect,
                })
                .collect(),
            edges: edges
                .iter()
                .map(|(f, t)| MmEdge {
                    from: f.to_string(),
                    to: t.to_string(),
                    kind: MmEdgeKind::Arrow,
                    label: None,
                })
                .collect(),
            subgraphs: vec![],
            styles: HashMap::new(),
        }
    }

    fn placed_node(id: &str, x: f32, y: f32) -> MmPlaced {
        MmPlaced {
            id: id.to_string(),
            label: id.to_string(),
            shape: MmShape::Rect,
            x,
            y,
            w: 40.,
            h: 30.,
            fill: None,
            stroke: None,
            stroke_width: None,
            accent_idx: None,
            virtual_node: false,
        }
    }

    /// `flow_elements`：跨层长边（层差 ≥2）插虚节点到中间层并钉进链；相邻层直边不插。
    #[test]
    fn flow_elements_inserts_virtual_nodes_for_long_edges() {
        let g = mini_flow(&["A", "B", "C"], &[("A", "B"), ("B", "C"), ("A", "C")]);
        let fe = flow_elements(&g);
        assert_eq!(fe.n_layers, 3);
        // 3 实节点 + 1 虚节点（A→C 跨 2 层）。
        assert_eq!(fe.elements.len(), 4);
        let (vi, v_layer) = fe
            .elements
            .iter()
            .enumerate()
            .find(|(_, (id, _, _))| id.starts_with("~v"))
            .map(|(i, _)| (i, fe.virtual_layers[i]))
            .expect("应有虚节点");
        assert_eq!(v_layer, Some(1), "虚节点落在中间层");
        // 链把 A→v→C 串起来，另两条直边原样入链。
        let idx = |id: &str| fe.elements.iter().position(|(e, _, _)| e.as_str() == id).unwrap();
        assert!(fe.chain.contains(&(idx("A"), vi, MmEdgeKind::Line, None)));
        assert!(fe.chain.contains(&(vi, idx("C"), MmEdgeKind::Arrow, None)));
        assert!(fe.chain.contains(&(idx("A"), idx("B"), MmEdgeKind::Arrow, None)));
    }

    /// `flow_elements`：全是相邻层边不产生虚节点。
    #[test]
    fn flow_elements_adds_no_virtual_for_adjacent_layers() {
        let g = mini_flow(&["A", "B", "C"], &[("A", "B"), ("B", "C")]);
        let fe = flow_elements(&g);
        assert_eq!(fe.elements.len(), 3);
        assert!(!fe.elements.iter().any(|(id, _, _)| id.starts_with("~v")));
    }

    /// `barycenter_order`：每元素恰在其层一次；起步空位置时首轮下向为稳定 no-op，
    /// 上向把 layer0 镜像到 layer1 源码序消化跨层逆序配对（交叉数降为 0）并收敛为固定点。
    #[test]
    fn barycenter_order_minimizes_layer_crossings() {
        // 声明序 a1 a2 b1 b2：layer0=[a1,a2]、layer1=[b1,b2]，边 a1→b2 / a2→b1 初始 1 交叉。
        let g = mini_flow(&["a1", "a2", "b1", "b2"], &[("a1", "b2"), ("a2", "b1")]);
        let fe = flow_elements(&g);
        let order = barycenter_order(&fe.elements, &fe.virtual_layers, &fe.layer_of, &fe.chain, fe.n_layers);
        assert_eq!(order.len(), 2);
        // 每元素恰好出现一次且落在自己的层。
        let mut seen = 0usize;
        for (li, layer) in order.iter().enumerate() {
            for &ei in layer {
                seen += 1;
                assert_eq!(fe.layer_of.get(&fe.elements[ei].0).copied().unwrap_or(0), li);
            }
        }
        assert_eq!(seen, fe.elements.len());
        // 固定点：layer0 镜像为 [a2,a1]、layer1 保持 [b1,b2] → 两配对同向、交叉数 0。
        let ids = |layer: &[usize]| layer.iter().map(|&ei| fe.elements[ei].0.as_str()).collect::<Vec<_>>();
        assert_eq!(ids(&order[0]), vec!["a2", "a1"]);
        assert_eq!(ids(&order[1]), vec!["b1", "b2"]);
        // 交叉判定：a1→b2 与 a2→b1 的层内相对序同向 → 0 交叉。
        let p = |id: &str| order[0].iter().position(|&ei| fe.elements[ei].0 == id).unwrap();
        let q = |id: &str| order[1].iter().position(|&ei| fe.elements[ei].0 == id).unwrap();
        assert_eq!((p("a1") < p("a2")), (q("b2") < q("b1")), "不应有反向交叉");
    }

    /// `sort_layer_by_barycenter`：按邻点位置 barycenter 升序稳定排序，无邻点垫底。
    #[test]
    fn sort_layer_by_barycenter_orders_by_neighbor_positions() {
        let adj: Vec<Vec<usize>> = vec![vec![4], vec![5], vec![4, 5], vec![], vec![], vec![]];
        let pos = HashMap::from([(4usize, 100usize), (5usize, 200usize)]);
        // 0→bary100；1→bary200；2→bary150；3 无邻 → MAX 垫底。
        assert_eq!(sort_layer_by_barycenter(&[0, 1, 2, 3], &adj, &pos), vec![0, 2, 1, 3]);
        // 稳定：同 bary 保持原相对序。
        let mut adj2: Vec<Vec<usize>> = vec![Vec::new(); 9];
        adj2[7] = vec![4];
        adj2[6] = vec![4, 5];
        adj2[8] = vec![5];
        let pos2 = HashMap::from([(4usize, 10usize), (5usize, 10usize)]);
        assert_eq!(sort_layer_by_barycenter(&[7, 6, 8], &adj2, &pos2), vec![7, 6, 8]);
    }

    /// `refresh_positions`：编码 `层下标*100_000 + 行内下标`。
    #[test]
    fn refresh_positions_encodes_layer_and_index() {
        let order = vec![vec![1, 0], vec![3]];
        let mut pos = HashMap::new();
        refresh_positions(&order, &mut pos);
        assert_eq!(pos.get(&1), Some(&0));
        assert_eq!(pos.get(&0), Some(&1));
        assert_eq!(pos.get(&3), Some(&100_000));
    }

    /// `flow_positions`：TB 层轴位固定秩距、单节点行内居中。
    #[test]
    fn flow_positions_centers_single_nodes_per_layer() {
        let g = mini_flow(&["A", "B"], &[("A", "B")]);
        let order = vec![vec![0usize], vec![1usize]];
        let sizes = FlowSizes {
            cw: vec![40., 40.],
            ch: vec![30., 30.],
            widths: vec![40., 40.],
            virtual_flag: vec![false, false],
        };
        let geom = flow_positions(
            &g,
            &[("A".into(), "A".into(), MmShape::Rect), ("B".into(), "B".into(), MmShape::Rect)],
            &order,
            sizes,
            0.,
            200.,
        );
        // TB：层0 轴位 30、层1 轴位 114（MM_TOP_PAD + li*MM_RANK_TB 84，均无 sub 偏移）。
        assert_eq!(geom.cx, vec![80., 80.]);
        assert_eq!(geom.cy, vec![30., 114.]);
        assert_eq!(geom.axis_ref, 0.);
    }

    /// `route_edges`：TB 垂直流——A 底出 B 顶进，直角折线 + 标签锚点。
    #[test]
    fn route_edges_tb_stays_vertical() {
        let g = mini_flow(&["A", "B"], &[("A", "B")]);
        let fe = flow_elements(&g);
        let placed = vec![placed_node("A", 10., 20.), placed_node("B", 70., 80.)];
        let edges = route_edges(&g, &fe.chain, &placed);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!((e.a, e.b), (0, 1));
        // B 在 A 之下：A 底(50)出、B 顶(80)进，中转折 65 → 4 点直角折线。
        assert_eq!(e.points, vec![(30., 50.), (30., 65.), (90., 65.), (90., 80.)]);
        // 标签锚点居中路径中点（肘边水平段中点）。
        assert_eq!(e.label, Some((60., 65.)));
        assert_eq!(e.kind, MmEdgeKind::Arrow);
        // 4 点肘 → 圆角密点已缓存在 smoothed，首尾锚定与骨架一致。
        let sm = e.smoothed.as_ref().expect("4 点肘应产出 smoothed");
        assert_eq!(sm.first(), e.points.first());
        assert_eq!(sm.last(), e.points.last());
        assert!((sm.len() as isize) <= 24, "采样点 {} ≤24", sm.len());
    }

    /// `route_edges`：LR 水平流——A 右出 B 左进，先水平再竖直。
    #[test]
    fn route_edges_lr_stays_horizontal() {
        let mut g = mini_flow(&["A", "B"], &[("A", "B")]);
        g.direction = MmDirection::LR;
        let fe = flow_elements(&g);
        let placed = vec![placed_node("A", 10., 20.), placed_node("B", 70., 80.)];
        let edges = route_edges(&g, &fe.chain, &placed);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        // 水平流：A 右(50)出、B 左(70)进；A/B 纵中线 35/95 差大 → 中转折 60。
        assert_eq!(e.points, vec![(50., 35.), (60., 35.), (60., 95.), (70., 95.)]);
        // 标签锚点居中路径中点（肘边竖段中点）。
        assert_eq!(e.label, Some((60., 65.)));
    }

    /// 弧线「无拐点」判据：对弧线采样点按「行进方向」对直线弦取横向（外侧）偏距，
    /// 须全部同号且非零（⇒ 单弓、无 S、不越弦）。`axis=0` 按 x 横向（TB/BT）、
    /// `axis=1` 按 y 横向（LR/RL）。返回各采样点偏距（含两端 0）。
    fn arc_no_s_offsets(sm: &[(f32, f32)], axis: usize, p0: (f32, f32), p3: (f32, f32)) -> Vec<f32> {
        let chord = |p: (f32, f32)| -> f32 {
            if axis == 0 {
                let span = p3.1 - p0.1;
                if span.abs() < 1e-6 {
                    return p0.0;
                }
                p0.0 + (p3.0 - p0.0) * (p.1 - p0.1) / span
            } else {
                let span = p3.0 - p0.0;
                if span.abs() < 1e-6 {
                    return p0.1;
                }
                p0.1 + (p3.1 - p0.1) * (p.0 - p0.0) / span
            }
        };
        sm.iter().map(|&p| if axis == 0 { p.0 - chord(p) } else { p.1 - chord(p) }).collect()
    }

    /// 断言弧线无 S：内点横向偏距全部同号（排除端点 0 值）。
    fn assert_arc_no_s(sm: &[(f32, f32)], axis: usize, p0: (f32, f32), p3: (f32, f32)) {
        let offs = arc_no_s_offsets(sm, axis, p0, p3);
        let interior: Vec<f32> = offs.iter().skip(1).take(sm.len().saturating_sub(2)).copied().collect();
        assert!(interior.len() >= 2, "采样点太少无法判 S");
        let s0 = interior[0].signum();
        assert!(s0 != 0., "弧线中段偏距不可为零（端点除外）");
        for (i, &d) in interior.iter().enumerate() {
            assert!(
                d.signum() == s0,
                "弧线第 {} 点偏距 {d:.2} 与首点 {s0:.0} 异号 ⇒ 有 S 拐点",
                i + 1
            );
        }
    }

    /// 两单位方向向量夹角（度，[0,180)）。
    fn ang_between(a: (f32, f32), b: (f32, f32)) -> f32 {
        let dot = (a.0 * b.0 + a.1 * b.1).clamp(-1., 1.);
        dot.acos().to_degrees()
    }

    /// 二次贝塞尔采样点重建控制点 Q（独立于 `arc_ctrl` 的数值反演）：取 t=1/(n-1) 的
    /// 首内样点，`B(t)=(1-t)²P0+2(1-t)t·Q+t²P3` → 解出 Q。用于独立反演弧线真实入射
    /// 切线（∝ P3−Q），证明「垂直内法向」箭头与弧线入射方向确实不同，避免「用同一实现
    /// 验证同一实现」。
    fn reconstruct_quad_ctrl(sm: &[(f32, f32)], p0: (f32, f32), p3: (f32, f32)) -> (f32, f32) {
        let n = sm.len();
        assert!(n >= 3);
        let t = 1. / (n - 1) as f32;
        let mt = 1. - t;
        let s = sm[1];
        let qx = (s.0 - mt * mt * p0.0 - t * t * p3.0) / (2. * mt * t);
        let qy = (s.1 - mt * mt * p0.1 - t * t * p3.1) / (2. * mt * t);
        (qx, qy)
    }

    fn unit(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        let l = (dx * dx + dy * dy).sqrt().max(1e-6);
        (dx / l, dy / l)
    }

    /// `smoothed_edge`：4 点肘**非分支**每内角二次贝塞尔圆角（smoothed 非空、首尾锚定
    /// 不变、中间水平段保留）；**同源分支**单弯肘直接出**单弓弧线**（端点锚定、12 点密点、
    /// 无 S 拐点、弓向外侧）；2 点 Arrow 直边有横向偏移出弧线；对齐/非 Arrow 直边退化
    /// 为 `None`（paint 直接吃骨架）；Line（虚节点直腿）即使分支也保持圆角肘。
    #[test]
    fn smoothed_edge_rounds_elbows_and_curves_offset_arrows() {
        use MmEdgeKind::{Arrow, Line};
        // 4 点肘（非分支）：骨架竖-横-竖，圆角后中点水平段仍在 y=10 上。
        let elbow = smoothed_edge(MmDirection::TB, Arrow, &[(0., 0.), (0., 10.), (50., 10.), (50., 20.)], false, None, None, None)
            .expect("非分支 4 点肘应圆角");
        assert_eq!(elbow.first(), Some(&(0., 0.)), "首锚定不变");
        assert_eq!(elbow.last(), Some(&(50., 20.)), "尾锚定不变");
        assert!(elbow.len() > 4 && elbow.len() <= 24, "圆角采样点 {} ∈ (4,24]", elbow.len());
        assert!(elbow.iter().any(|&(_, y)| y == 10.), "中间水平段保留");
        // 4 点肘（同源分支）：直接出单弓弧线（12 点、首尾锚定、无直角横段、中点横扫）。
        let sbr = smoothed_edge(MmDirection::TB, Arrow, &[(0., 0.), (0., 10.), (50., 10.), (50., 20.)], true, None, None, None)
            .expect("同源分支 4 点肘应出弧线");
        assert_eq!(sbr.len(), 12, "弧线采样 12 点 ≤24");
        assert_eq!(sbr.first(), Some(&(0., 0.)));
        assert_eq!(sbr.last(), Some(&(50., 20.)));
        assert!(sbr.iter().any(|&(x, y)| x > 5. && x < 45. && y > 2. && y < 18.), "弧线中点横扫中带（非直角肘）");
        // 无 S 拐点：对直线弦横向偏距全部同号（单弓）。
        assert_arc_no_s(&sbr, 0, (0., 0.), (50., 20.));
        // 弓起：曲线中点（sm[6]）对骨架直线中点的横向偏距 ≥ 4px（旧对称版≈0）。
        let sbr_ldev = (sbr[sbr.len() / 2].0 - 25.).abs();
        assert!(sbr_ldev >= 4., "分支弧线中点弓起 {sbr_ldev:.1} ≥ 4px（横向偏距，旧对称版≈0）");
        // BT 反向分支（目标在源之上）：方向处理对称，同样弓向目标侧、无 S。
        let sbt = smoothed_edge(MmDirection::BT, Arrow, &[(0., 30.), (0., 10.), (50., 10.), (50., 2.)], true, None, None, None)
            .expect("BT 同源分支应出弧线");
        assert_eq!(sbt.first(), Some(&(0., 30.)));
        assert_eq!(sbt.last(), Some(&(50., 2.)));
        assert_arc_no_s(&sbt, 0, (0., 30.), (50., 2.));
        let sbt_ldev = (sbt[sbt.len() / 2].0 - 25.).abs();
        assert!(sbt_ldev >= 5., "BT 分支弧线反向弓起 {sbt_ldev:.1} ≥ 5px");
        // 2 点 Arrow + 横向偏移 → 弧线（端点钉在骨架首尾，密点在中间）。
        let s = smoothed_edge(MmDirection::TB, Arrow, &[(0., 0.), (30., 40.)], false, None, None, None)
            .expect("2 点 Arrow 横向偏移应出弧线");
        assert_eq!(s.first(), Some(&(0., 0.)));
        assert_eq!(s.last(), Some(&(30., 40.)));
        assert_eq!(s.len(), 12, "弧线采样 12 点 ≤24");
        assert!(s.iter().any(|&(x, y)| (x - 15.).abs() < 20. && y > 10. && y < 30.), "弧线中点扫过中带");
        assert_arc_no_s(&s, 0, (0., 0.), (30., 40.));
        let s_ldev = (s[s.len() / 2].0 - 15.).abs();
        assert!(s_ldev >= 5., "2 点弧线中点弓起 {s_ldev:.1} ≥ 5px（旧对称版≈0）");
        // 对齐直边 → None（退化为直线）。
        assert!(smoothed_edge(MmDirection::TB, Arrow, &[(0., 0.), (0., 40.)], false, None, None, None).is_none());
        // 非 Arrow（虚节点直腿）偏移直边 → 不加弧。
        assert!(smoothed_edge(MmDirection::TB, Line, &[(0., 0.), (30., 40.)], false, None, None, None).is_none());
        // Line 分支直腿（4 点肘）：仍圆角，不接弧线。
        let lline = smoothed_edge(MmDirection::TB, Line, &[(0., 0.), (0., 10.), (50., 10.), (50., 20.)], true, None, None, None)
            .expect("Line 分支直腿应保持圆角肘");
        assert!(lline.len() > 4 && lline.len() <= 24, "Line 直腿不接弧线");
        // LR 肘同样圆角。
        let lr = smoothed_edge(MmDirection::LR, Arrow, &[(0., 0.), (10., 0.), (10., 60.), (20., 60.)], false, None, None, None)
            .expect("LR 4 点肘应圆角");
        assert_eq!(lr.first(), Some(&(0., 0.)));
        assert_eq!(lr.last(), Some(&(20., 60.)));
    }

    /// `route_edges` 真实接线：横向偏移的相邻层边（**单出边非分支**）→ 4 点肘圆角
    /// + smoothed 密点已缓存，首尾锚定与骨架一致。
    #[test]
    fn route_edges_smoothed_cached_for_offset_edges() {
        let g = mini_flow(&["A", "B"], &[("A", "B")]);
        let fe = flow_elements(&g);
        let placed = vec![placed_node("A", 10., 20.), placed_node("B", 70., 80.)];
        let edges = route_edges(&g, &fe.chain, &placed);
        let e = &edges[0];
        assert_eq!(e.points.len(), 4);
        let sm = e.smoothed.as_ref().expect("偏移边应有 smoothed（圆角肘）");
        assert!(sm.len() > 4, "非分支偏移边为圆角肘（>4 点）");
        assert_eq!(sm.first(), e.points.first(), "smoothed 首锚定 = 骨架首点");
        assert_eq!(sm.last(), e.points.last(), "smoothed 尾锚定 = 骨架尾点");
    }

    /// **用户例图真实装配链**（parse → flow_elements → barycenter_order → flow_positions
    /// → route_edges）：`graph TD A[开始]-->B{判断} B-->|是|C[完成] B-->|否|D[返回 A]`
    /// 中 A→B 对齐直边退化为直线；B→C/B→D 为**同源分支**且横向偏移 → smoothed 落**单弓
    /// 弧线**（12 点、首锚定、尾锚定=**箭头基部**、中点横扫非直角、无 S 拐点、弓向各自
    /// 目标外侧、彼此不交叉）——即投诉 .1「分支边曲线」真实命中。另锁箭头方向：目标接触
    /// 边**垂直内法向**（TD 正向顶边 → (0,1) 垂直向下）与边界法向夹角 =0°、tip=目标顶边
    /// 接触点（= `points.last()` 逻辑锚点，不随弧线收短而变）；且弧线真实入射切线偏离垂直
    /// 方向显著（弧线并非竖直入射）——证明「垂直指向节点」（投诉 .2「箭头随弧线角度歪斜」
    /// 的修复）是真实改动而非弧线本就竖直。弧线目标端**收短到箭头基部**（尾点=节点边缘−
    /// ARROW_LEN_PX·arrow_dir ≠ 节点边缘）：paint 线到基部、实心箭头从基部到节点边缘。
    #[test]
    fn route_edges_td_branch_user_example_arc() {
        let g = crate::markdown::wysiwyg::mermaid_graph("graph TD\nA[开始]-->B{判断}\nB-->|是|C[完成]\nB-->|否|D[返回 A]");
        assert_eq!(g.direction, MmDirection::TB);
        assert_eq!(g.nodes.len(), 4);
        let fe = flow_elements(&g);
        let order = barycenter_order(&fe.elements, &fe.virtual_layers, &fe.layer_of, &fe.chain, fe.n_layers);
        // 无窗口测试不可真实 shaping：宽度按「字符数×8 + 2·XPAD」近似——A/B/C 两字同宽、
        // D 四字更宽 → B→C/B→D 横向偏移、A/B 对齐（几何只随 cw 推，不影响判定）。
        let w = |lab: &str| lab.chars().count() as f32 * 8. + 2. * MM_NODE_XPAD;
        let sizes = FlowSizes {
            cw: fe.elements.iter().map(|(_, l, _)| w(l)).collect(),
            ch: vec![MM_NODE_H; fe.elements.len()],
            widths: fe.elements.iter().map(|(_, l, _)| w(l)).collect(),
            virtual_flag: vec![false; fe.elements.len()],
        };
        let geom = flow_positions(&g, &fe.elements, &order, sizes, 0., 800.);
        let placed: Vec<MmPlaced> = fe
            .elements
            .iter()
            .enumerate()
            .map(|(i, (id, lab, shp))| MmPlaced {
                id: id.clone(),
                label: lab.clone(),
                shape: *shp,
                x: geom.cx[i] - geom.cw[i] / 2.,
                y: geom.cy[i],
                w: geom.cw[i],
                h: geom.ch[i],
                fill: None,
                stroke: None,
                stroke_width: None,
                accent_idx: None,
                virtual_node: false,
            })
            .collect();
        let edges = route_edges(&g, &fe.chain, &placed);
        let edge_pts = |from: &str, to: &str| {
            let f = fe.elements.iter().position(|(id, _, _)| id == from).unwrap();
            let t = fe.elements.iter().position(|(id, _, _)| id == to).unwrap();
            edges.iter().find(|e| e.a == f && e.b == t).unwrap()
        };
        let ab = edge_pts("A", "B");
        assert_eq!(ab.points.len(), 2, "A/B 同宽对齐 → 直边骨架");
        assert!(ab.smoothed.is_none(), "对齐直边退化为直线（不接弧线）");
        for (f, t) in [("B", "C"), ("B", "D")] {
            let e = edge_pts(f, t);
            assert_eq!(e.points.len(), 4, "{f}→{t} 横向偏移 → 4 点肘骨架");
            let sm = e.smoothed.as_ref().unwrap_or_else(|| panic!("{f}→{t} 同源分支应接弧线"));
            assert_eq!(sm.len(), 12, "{f}→{t} 弧线 12 点 ≤24");
            assert_eq!(sm.first(), e.points.first(), "{f}→{t} smoothed 首锚定 = 骨架首点");
            // **箭头基部（双连接修复核心断言）**：弧线尾点 = 节点边缘 − ARROW_LEN_PX·arrow_dir
            // （弧线目标端收短到基部、不再穿到节点边）；`points.last()` 逻辑锚点仍 = 节点边缘。
            let tip = *e.points.last().unwrap();
            let base = arrow_base(tip, e.arrow_dir, ARROW_LEN_PX);
            assert_eq!(*sm.last().unwrap(), base, "{f}→{t} 弧线尾点 = 箭头基部（非节点边缘）");
            assert_ne!(*sm.last().unwrap(), tip, "{f}→{t} 线末 ≠ 节点边缘（不再双连接）");
            let (x0, x1) = (e.points[0].0, e.points[3].0);
            let (y0, y1) = (e.points[0].1, e.points[3].1);
            assert!(
                sm.iter().any(|&(x, y)| x > x0.min(x1) && x < x0.max(x1) && y > y0.min(y1) && y < y0.max(y1)),
                "{f}→{t} 弧线有中间横扫点（非直角折线）",
            );
            // 无 S 拐点硬判据：对直线弦横向偏距（外侧）符号恒定。
            assert_arc_no_s(sm, 0, e.points[0], e.points[3]);
            // 弓起**带符号方向**：左目标(C)必须弓 −x、右目标(D)必须弓 +x（远离源列
            // 中心线的外侧分叉）。带符号偏距即弓幅 q（弧线中点=Q）。阈值 8 是「肉眼可辨
            // 弯向」下限，方向断言拒绝把弓弯回内侧/彼此。
            let signed = sm[sm.len() / 2].0 - (x0 + x1) / 2.;
            let expect = if x1 < x0 { "左目标弓 −x" } else { "右目标弓 +x" };
            if x1 < x0 {
                assert!(signed <= -8., "{f}→{t} 应弓向外侧(左{expect})：signed={signed:.1} ≤ −8");
            } else {
                assert!(signed >= 8., "{f}→{t} 应弓向外侧(右{expect})：signed={signed:.1} ≥ +8");
            }
            // 箭头方向（新语义，paint 取 `arrow_dir` ——目标接触边垂直内法向）：
            // 1. = (0,1)：TD 正向目标顶边 → 垂直向下直插节点，不与边界法向夹角（=0°）；
            // 2. 且该弧线**真实入射切线**（独立重建控制点反演）偏离垂直方向显著 —— 弧线
            //    并非竖直入射，证明「垂直指向」是真实改动（旧实现取真切线/割线即那个歪角，
            //    正是投诉 .2 根因；若未来把箭头接回切线须重写本断言）；
            // 3. tip 落点 = 骨架末端 = 目标顶边接触点（`points.last()` 逻辑锚点不因弧线收短
            //    到基部而变；上方已锁 sm.last()=基部、且基部≠节点边缘）。
            assert_eq!(e.arrow_dir, (0., 1.), "{f}→{t} TD 顶边进应垂直向下 (0,1)");
            assert_eq!(ang_between(e.arrow_dir, (0., 1.)), 0., "{f}→{t} 与边界法向夹角应 0°");
            let q = reconstruct_quad_ctrl(sm, e.points[0], e.points[3]);
            let real = unit(q, e.points[3]);
            let dev = ang_between(real, e.arrow_dir);
            assert!(dev > 2., "{f}→{t} 弧线真实切线偏离垂直法向 {dev:.2}° 应显著（垂直指向是真实修复）");
            // 基部-尖端闭合几何：sm.last()(基部) 到 points.last()(节点边缘) 恰沿 arrow_dir
            // 一个 ARROW_LEN_PX——paint 最后一段线到基部、实心箭头从基部填充到节点边缘。
            let (dx, dy) = (sm.last().unwrap().0 - tip.0, sm.last().unwrap().1 - tip.1);
            let dist = (dx * dx + dy * dy).sqrt();
            assert!((dist - ARROW_LEN_PX).abs() < 1e-3, "{f}→{t} 基部-尖端距离 = 箭头长：{dist:.2} ≠ {ARROW_LEN_PX}");
            assert!(
                (dx + e.arrow_dir.0 * ARROW_LEN_PX).abs() < 1e-3 && (dy + e.arrow_dir.1 * ARROW_LEN_PX).abs() < 1e-3,
                "{f}→{t} 基部在 tip 的 arrow_dir 反方向一个箭头长：dx={dx:.2} dy={dy:.2} dir={:?}",
                e.arrow_dir,
            );
        }
        // 双分支**彼此不交叉**：B→C（左）与 B→D（右）同源、同一出射点、同一目标行 ⇒
        // 采样 y 逐点相同，逐点断言 C 的 x < D 的 x（弧线各自向外分叉，永不交）。
        let (sc, sd) = (edge_pts("B", "C"), edge_pts("B", "D"));
        let (smc, smd) = (
            sc.smoothed.as_ref().expect("B→C 弧线"),
            sd.smoothed.as_ref().expect("B→D 弧线"),
        );
        assert_eq!(smc.len(), smd.len());
        for i in 0..smc.len() {
            assert!(
                smc[i].1 == smd[i].1,
                "两分支采样行应一致：smc[i].1={:.1} vs smd[i].1={:.1}",
                smc[i].1, smd[i].1
            );
            assert!(
                smc[i].0 <= smd[i].0,
                "B→C 与 B→D 弧线在第 {i} 点交叉：{:.1} > {:.1}",
                smc[i].0, smd[i].0
            );
        }
    }

    /// 分支**单弓弧线**带符号弓向回归：弓向 = 远离源列中心线的外侧（= 目标所在侧）。
    /// 对每个流向断言曲线中点相对骨架直线中点的**带符号**横向（外侧）偏距方向正确：
    /// TB/BT 右目标 → 弓 +x（ldev 显著>+阈值）、左目标 → −x（显著<−阈值）；
    /// LR/RL 同理按 y 轴（下方目标 → +y、上方目标 → −y）。阈值取 8~10px 级并留足
    /// 余量。同时锁：端点锚定、**无 S 拐点**（横向偏距同号）、行进坐标单调无回折、
    /// 入射段始终朝目标面（不穿回目标）。
    #[test]
    fn arc_bows_outward_signed_directions() {
        use MmDirection::{BT, LR, RL, TB};
        use MmEdgeKind::Arrow;
        let signed = |dir: MmDirection, p0: (f32, f32), p3: (f32, f32)| -> f32 {
            // 用同源分支 4 点肘触发弧线（smoothed_edge 只看首尾）。
            let mid_y = (p0.1 + p3.1) / 2.;
            let sm = smoothed_edge(dir, Arrow, &[p0, (p0.0, mid_y), (p3.0, mid_y), p3], true, None, None, None)
                .expect(&format!("{dir:?} 分支应出弧线"));
            assert_eq!(sm.first(), Some(&p0), "首锚定不变");
            assert_eq!(sm.last(), Some(&p3), "尾锚定不变");
            assert_eq!(sm.len(), 12);
            // 无 S 拐点：横向（外侧）偏距同号（axis=TB/BT→0、LR/RL→1）。
            let axis = match dir { TB | BT => 0, LR | RL => 1 };
            assert_arc_no_s(&sm, axis, p0, p3);
            // 行进坐标单调（无回折）：TB/BT 看 y、LR/RL 看 x，按方向单调推进。
            let travel = |p: (f32, f32)| match dir { TB | BT => p.1, LR | RL => p.0 };
            let dir_sign: f32 = match dir { BT | RL => -1., _ => 1. };
            for w in sm.windows(2) {
                let d = (travel(w[1]) - travel(w[0])) * dir_sign;
                assert!(d >= -1e-3, "弧线行进坐标应单调无回折：{dir:?} 段 {w:?}");
            }
            // 入射段朝目标面（不穿回目标）：末两点的行进坐标继续推进。
            let (a, b) = (sm[sm.len() - 2], sm[sm.len() - 1]);
            assert!(
                (travel(b) - travel(a)) * dir_sign > 0.,
                "{dir:?} 入射段应朝目标行进：{a:?} → {b:?}"
            );
            let mid = sm[sm.len() / 2];
            match dir {
                TB | BT => mid.0 - (p0.0 + p3.0) / 2.,
                LR | RL => mid.1 - (p0.1 + p3.1) / 2.,
            }
        };
        // TB：右目标（→弓 +x），左目标（→弓 −x）。
        assert!(signed(TB, (0., 0.), (50., 20.)) >= 8., "TB 右目标弓 +x");
        assert!(signed(TB, (60., 0.), (10., 20.)) <= -8., "TB 左目标弓 −x");
        // BT：源在下目标在上——右目标（→弓 +x）左目标（→弓 −x）。
        assert!(signed(BT, (0., 30.), (50., 2.)) >= 8., "BT 右目标(上)弓 +x");
        assert!(signed(BT, (60., 30.), (10., 2.)) <= -8., "BT 左目标(上)弓 −x");
        // LR：右行源，目标下方 → 弓 +y；目标上方 → 弓 −y。
        assert!(signed(LR, (0., 0.), (20., 50.)) >= 8., "LR 下方目标弓 +y");
        assert!(signed(LR, (0., 60.), (20., 10.)) <= -8., "LR 上方目标弓 −y");
        // RL：左行源（镜像），下方目标 → 弓 +y；上方目标 → 弓 −y。
        assert!(signed(RL, (60., 0.), (20., 50.)) >= 8., "RL 下方目标弓 +y");
        assert!(signed(RL, (60., 60.), (20., 10.)) <= -8., "RL 上方目标弓 −y");
    }

    /// 箭头方向 = 目标接触边**垂直内法向**（与弧线切线无关，用户「垂直指向节点」语义落点）。
    /// 旧实现取弧线入射端点真切线（`arc_end_tangent` 存进 `end_tangent`），在 12 采样/大弓高
    /// 下与垂直方向偏差 5~11.5°——即投诉 .2「箭头随弧线角度歪斜」根因。本测试锁新语义：
    /// 1. `edge_arrow_dir` 纯函数对四类钉边（顶进/底进/左进/右进）→ (0,1)/(0,-1)/(1,0)/(-1,0)；
    /// 2. `route_edges` 端到端把方向钉进边：`arrow_dir` = 目标接触边垂直内法向、tip（`points`
    ///    末点）= 该边缘接触点——箭头与目标边界法向夹角恒 0°（垂直直插）。
    #[test]
    fn edge_arrow_dir_is_vertical_inward_normal() {
        use MmDirection::{BT, LR, RL, TB};
        // (流向, 源锚点, 目标锚点, 期望箭头方向, 期望 tip=目标边缘接触点)。
        // 钉边规则与 `route_edges` 同源：目标在源之下→顶边进、之上→底边进、
        // 之右→左边进、之左→右边进；方向 = 该边的垂直内法向。
        let cases: [(MmDirection, (f32, f32), (f32, f32), (f32, f32), (f32, f32)); 8] = [
            (TB, (10., 20.), (70., 80.), (0., 1.), (90., 80.)),  // 目标顶边进 → 垂直向下
            (TB, (10., 80.), (70., 20.), (0., -1.), (90., 50.)), // 目标底边进 → 垂直向上
            (BT, (10., 20.), (70., 80.), (0., 1.), (90., 80.)),
            (BT, (10., 80.), (70., 20.), (0., -1.), (90., 50.)),
            (LR, (10., 20.), (130., 40.), (1., 0.), (130., 55.)), // 目标左边进 → 垂直向右
            (LR, (130., 40.), (10., 80.), (-1., 0.), (50., 95.)), // 目标右边进 → 垂直向左
            (RL, (10., 20.), (130., 40.), (1., 0.), (130., 55.)),
            (RL, (130., 40.), (10., 80.), (-1., 0.), (50., 95.)),
        ];
        for (dir, pa, pb, want, tip) in cases {
            let (pn, pexp) = (placed_node("A", pa.0, pa.1), placed_node("B", pb.0, pb.1));
            // 纯函数锁「钉边 → 垂直内法向」映射。
            let vertical = matches!(dir, TB | BT);
            assert_eq!(
                edge_arrow_dir(vertical, &pn, &pexp),
                want,
                "{dir:?}{pa:?}→{pb:?} 纯函数应给该接触边垂直内法向 {want:?}",
            );
            // route_edges 端到端：方向钉进边 + tip = 该边缘接触点。
            let mut g = mini_flow(&["A", "B"], &[("A", "B")]);
            g.direction = dir;
            let fe = flow_elements(&g);
            let placed = vec![pn.clone(), pexp.clone()];
            let edges = route_edges(&g, &fe.chain, &placed);
            assert_eq!(edges.len(), 1);
            let e = &edges[0];
            assert_eq!(e.arrow_dir, want, "{dir:?}{pa:?}→{pb:?} route_edges 应把垂直内法向钉进边");
            assert_eq!(e.points.last(), Some(&tip), "{dir:?} tip=目标边缘接触点 {tip:?}");
            // 箭头与「tip 所在边」的边界法向夹角 = 0°（垂直直插，不随弧线角度偏斜）。
            let cx = pexp.x + pexp.w / 2.;
            let edge_normal = if (tip.0 - cx).abs() < 1e-3 && (tip.1 - pexp.y).abs() < 1e-3 {
                (0., 1.)
            } else if (tip.0 - cx).abs() < 1e-3 && (tip.1 - (pexp.y + pexp.h)).abs() < 1e-3 {
                (0., -1.)
            } else if (tip.0 - pexp.x).abs() < 1e-3 {
                (1., 0.)
            } else {
                (-1., 0.)
            };
            assert_eq!(
                ang_between(e.arrow_dir, edge_normal),
                0.,
                "{dir:?} 箭头与目标边界法向夹角应 0°：dir={:?} normal={edge_normal:?}",
                e.arrow_dir,
            );
        }
    }

    /// `flow_subgraph_boxes`：成员包围盒加 8px 内边距，并夹到内容区。
    #[test]
    fn flow_subgraph_boxes_bounds_members_with_pad() {
        let mut g = mini_flow(&["A", "B"], &[("A", "B")]);
        g.subgraphs = vec![MmSubgraph { id: "s0".into(), title: "s0".into(), depth: 0, nodes: vec!["A".into(), "B".into()] }];
        let fe = flow_elements(&g);
        let placed = vec![placed_node("A", 100., 100.), placed_node("B", 180., 100.)];
        let boxes = flow_subgraph_boxes(&g, &fe.real_index, &placed, 0., 0.);
        assert_eq!(boxes.len(), 1);
        let b = &boxes[0];
        assert_eq!(b.title, "s0");
        // A(100,100)、B 右缘 220、下缘 130。内边距 8，夹内容区（MM_EDGE_X=14 / MM_TOP_PAD=30）。
        assert_eq!((b.x, b.y, b.w, b.h), (92., 92., 136., 46.));
    }
}
