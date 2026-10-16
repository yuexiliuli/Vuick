//! WYSIWYG「Mermaid 图」渲染族：解析（`parse`）、布局（`layout`）、绘制（`paint`）
//! 三分离，独立成 `mermaid/` 文件夹（原与数学同居 `math/mod.rs`，随 Mermaid 专项
//! 支持迁出）。经 [`super::element`] 的 `pub(crate) use super::mermaid::*` 重导出到
//! 上层命名空间，既有 `use super::*` 消费方（frame/shape/paint/tests）代码路径不变。
//!
//! 结构约定与 `math` 一致：解析/布局走**纯函数**（`mermaid_graph`/`mm_layout`），
//! 卡片高 `mermaid_card_height` 无窗口可算（frame 围栏高度估算依赖），绘制只消费
//! shape 期缓存的已排布局（`ShapedRow.mm`），滚动/键入不每帧重排上级。
//!
//! 本轮覆盖：
//! - **flowchart / graph**：方向 TB/TD/BT/RL/LR；14 种节点形状（含 `@{}` 语法）；
//!   10 种边型（实线/虚线/粗线/圆端/叉端/双向 + 链式 `A-->B-->C` + 并行 `&`）；
//!   `%%` 注释；`subgraph` 分组（含嵌套）；`classDef`/`class`/`style` 配色；
//!   布局升级：最长路径分层 + 长边插虚节点 + barycenter 交叉最小化 + 方向参数化。
//! - **sequenceDiagram**：participant/actor/别名、六种消息箭头（含 `+/-` 激活速记）、
//!   `activate`/`deactivate` 激活条、`Note left/right/over`、`loop/alt/else/opt` 片段。
//!
//! `#![allow(dead_code)]`：本模块为自持的数据模型 + 渲染器，多数字段是模块 API（部分
//! 仅在 `#[cfg(test)]`（`element/tests.rs`）消费，如 `MmPolylineEdge.a/b`、`MmPlaced.id`），
//! 非测试构建报 unread 属预期，保留而非删字段。

#![allow(dead_code)]

mod layout;
mod paint;
mod parse;

pub use layout::{MmSeqActivation, MmSeqColumn, MmSeqLayout};
pub(crate) use layout::{mm_layout, mermaid_card_height, scale_mm_layout, SEQ_NOTE_H};
#[cfg(test)]
pub(crate) use layout::accent_idx_for;
pub(crate) use layout::arrow_base;
pub(crate) use paint::paint_mermaid_layout;
pub(crate) use parse::{diagram_kind, mermaid_graph, mermaid_seq, MmDiagramKind};

use gpui::{Pixels, px};
use std::collections::HashMap;

/// 图方向（`TD` 与 `TB` 同）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MmDirection {
    #[default]
    TB,
    BT,
    LR,
    RL,
}

/// mermaid 节点形状。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MmShape {
    /// 普通矩形 `A` / `A[文本]`。
    Rect,
    /// 菱形 `{ … }`。
    Diamond,
    /// 圆角矩形 `( … )`。
    Round,
    /// 圆形 `(( … ))`。
    Circle,
    /// 双圆 `((( … )))`。
    DoubleCircle,
    /// 跑道形 `([ … ])`。
    Stadium,
    /// 子程序双竖线 `[[ … ]]`。
    Subroutine,
    /// 圆柱 `[( … )]`。
    Cylinder,
    /// 六边形 `{{ … }}`。
    Hexagon,
    /// 不对称旗形 `> … ]`。
    Asymmetric,
    /// 平行四边形 `[/ … /]`。
    Parallelogram,
    /// 平行四边形（反）`[\ … \]`。
    ParallelogramAlt,
    /// 梯形 `[/ … \]`。
    Trapezoid,
    /// 梯形（反）`[\ … /]`。
    TrapezoidAlt,
}

/// mermaid 边型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MmEdgeKind {
    /// `-->` 实线箭头。
    Arrow,
    /// `---` 实线（无箭头）。
    Line,
    /// `-.->` / `..>` 虚线箭头。
    DottedArrow,
    /// `-.-` / `...` 虚线。
    DottedLine,
    /// `==>` 粗箭头。
    ThickArrow,
    /// `===` 粗线。
    ThickLine,
    /// `--o` 圆端（聚合）。
    CircleEnd,
    /// `--x` 叉端。
    CrossEnd,
    /// `<-->` 双向箭头。
    BothArrow,
    /// `<==>` 双向粗箭头。
    BothThick,
}

/// 解析后的 flowchart 节点。
#[derive(Clone, Debug)]
pub struct MmNode {
    pub id: String,
    /// 显示标签（缺省回退到 id）。
    pub label: String,
    pub shape: MmShape,
}

/// 解析后的 flowchart 边。
#[derive(Clone, Debug)]
pub struct MmEdge {
    pub from: String,
    pub to: String,
    pub kind: MmEdgeKind,
    pub label: Option<String>,
}

/// subgraph 分组（含嵌套：`nodes` 为直接声明的成员 id）。
/// `id`/`depth` 为语义字段（解析期使用；布局用 `depth` 推算标题带），当前消费在 cfg(test) 之外，
/// 保留为模型 API。
#[derive(Clone, Debug)]
pub struct MmSubgraph {
    pub id: String,
    pub title: String,
    /// 嵌套深度（0 = 顶层分组）。
    pub depth: usize,
    pub nodes: Vec<String>,
}

/// `classDef` / `style` 配色（hex 存 0xRRGGBB）。
#[derive(Clone, Copy, Debug, Default)]
pub struct MmStyle {
    pub fill: Option<u32>,
    pub stroke: Option<u32>,
    pub stroke_width: Option<f32>,
}

impl MmStyle {
    fn merge(&mut self, o: MmStyle) {
        if o.fill.is_some() {
            self.fill = o.fill;
        }
        if o.stroke.is_some() {
            self.stroke = o.stroke;
        }
        if o.stroke_width.is_some() {
            self.stroke_width = o.stroke_width;
        }
    }
}

/// flowchart 解析结果。
#[derive(Clone, Debug, Default)]
pub struct MmGraph {
    pub direction: MmDirection,
    pub nodes: Vec<MmNode>,
    pub edges: Vec<MmEdge>,
    pub subgraphs: Vec<MmSubgraph>,
    /// 每节点最终配色（`class`/`style` 指令解析后的覆盖）。
    pub styles: HashMap<String, MmStyle>,
}

impl MmGraph {
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}

/// 序列图消息箭头。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MmSeqArrow {
    /// `->` 实线开放箭头。
    SolidOpen,
    /// `-->` 虚线开放箭头。
    DottedOpen,
    /// `->>` 实线实心箭头。
    Solid,
    /// `-->>` 虚线实心箭头。
    Dotted,
    /// `-x` 实线叉端。
    SolidCross,
    /// `--x` 虚线叉端。
    DottedCross,
}

/// 序列图参与者（id + 显示标签 + 是否 actor）。
#[derive(Clone, Debug)]
pub struct MmSeqParticipant {
    pub id: String,
    pub label: String,
    pub is_actor: bool,
}

/// 序列图消息。
#[derive(Clone, Debug)]
pub struct MmSeqMessage {
    pub from: String,
    pub to: String,
    pub arrow: MmSeqArrow,
    pub text: String,
    /// 目标参与者的激活层状态变化（`+1` 入栈 / `-1` 出栈 / `0` 无）。
    pub act: i8,
}

/// 序列图 Note 位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MmSeqNotePos {
    Left,
    Right,
    Over,
}

/// 序列图 Note。
#[derive(Clone, Debug)]
pub struct MmSeqNote {
    pub pos: MmSeqNotePos,
    /// `over A` 时 `b` 为 `None`；`over A,B` 时 `b` 为 `Some(B)`。
    pub a: String,
    pub b: Option<String>,
    pub text: String,
}

/// 序列图片段类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MmSeqFragmentKind {
    Loop,
    Alt,
    Opt,
    Par,
}

/// 序列图片段（覆盖一段行区间；`else` 分隔行用 `else_titles`）。
#[derive(Clone, Debug)]
pub struct MmSeqFragment {
    pub kind: MmSeqFragmentKind,
    pub title: String,
    /// 片段的 title/主体 `else` 标题（无则 `None`）。
    pub else_titles: Vec<Option<String>>,
}

/// 序列图扁平行模型（source 顺序；参与者和 activate/deactivate 是状态行，不占高）。
#[derive(Clone, Debug)]
pub enum MmSeqRow {
    Message(usize),
    Note(usize),
    /// 片段开始（占 1 条标题带高）。
    FragOpen(usize),
    /// 片段 `else`（占 1 条分隔带高；`Some(标题)` 或 `None`）。
    FragElse(Option<String>),
    /// 片段结束（不占高，仅标记区间结束）。
    FragClose(usize),
    /// `activate <id>`（状态行，不占高）。
    Activate(String),
    /// `deactivate <id>`（状态行，不占高）。
    Deactivate(String),
}

/// 序列图解析结果。
#[derive(Clone, Debug)]
pub struct MmSeq {
    pub participants: Vec<MmSeqParticipant>,
    pub messages: Vec<MmSeqMessage>,
    pub notes: Vec<MmSeqNote>,
    pub fragments: Vec<MmSeqFragment>,
    pub rows: Vec<MmSeqRow>,
}

/// 已摆放的 flowchart 节点（卡片相对坐标；layout 算好，paint 直接画）。
#[derive(Clone, Debug)]
pub struct MmPlaced {
    pub id: String,
    pub label: String,
    pub shape: MmShape,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub fill: Option<u32>,
    pub stroke: Option<u32>,
    pub stroke_width: Option<f32>,
    /// 分类 accent 色板下标（`Some(i)` = 按出现顺序循环 `WyTheme.mermaid_accents`；
    /// `None` = 作者 classDef/style 显式 hex 着色，按作者色绘制，不参与循环。
    /// 长边插桩虚节点亦为 `None`）。shape 期定死，paint 只查表调色。
    pub accent_idx: Option<usize>,
    /// 长边插桩的虚节点（画点不画框，不计入测试节点数语义验证）。
    pub virtual_node: bool,
}

/// 已排好的 flowchart 边（折线点卡片相对坐标，方向已变换）。
#[derive(Clone, Debug)]
pub struct MmPolylineEdge {
    /// 起/终节点在 `placed` 里的下标（虚节点参与）。
    pub a: usize,
    pub b: usize,
    pub kind: MmEdgeKind,
    pub points: Vec<(f32, f32)>,
    /// 曲线化后的密点（layout 期一次算好缓存；`Some` = 圆角/贝塞尔后的采样，
    /// paint 优先消费，`None` = 骨架即直边直接用 `points`）。
    pub smoothed: Option<Vec<(f32, f32)>>,
    /// 目标端箭头方向 = 目标节点接触边的**垂直内法向**（与弧线切线无关，「垂直指向节点」）：
    /// TB 顶边进 → (0,1) 垂直向下、BT 底边进 → (0,-1) 垂直向上、LR 左进 → (1,0)、RL 右进
    /// → (-1,0)。`route_edges` 按本边实际钉在目标的哪条边定死，paint 直接消费；不再取弧线
    /// 入射切线（旧 `end_tangent` 在 12 采样/大弓高下与垂直方向偏差 5~11.5°，即「箭头随弧线
    /// 角度歪斜」投诉根因）。单位向量，等比缩放下不变（保角，无需随缩放变换）。
    pub arrow_dir: (f32, f32),
    /// 标签锚点（卡片相对坐标，居中在路径中点）。
    pub label: Option<(f32, f32)>,
    /// 边标签文本。
    pub label_text: Option<String>,
}

/// subgraph 绘制盒（卡片相对坐标；点集为所属节点 bbox + 内衬）。
#[derive(Clone, Debug)]
pub struct MmSubgraphBox {
    pub title: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// flowchart 布局结果（卡片相对坐标；shape 期算好缓存，paint 直接画）。
#[derive(Clone, Debug, Default)]
pub struct MmFlowLayout {
    pub placed: Vec<MmPlaced>,
    pub edges: Vec<MmPolylineEdge>,
    pub subgraphs: Vec<MmSubgraphBox>,
    pub content_w: f32,
    pub content_h: f32,
    /// 滚轮缩放（paint 用它等比缩放字号；几何已在 `layout_flow` 后按此整体缩放）。
    pub zoom: f32,
}

/// Mermaid 图布局结果（卡片相对坐标）；`ShapedRow.mm` 携带。
#[derive(Clone, Debug)]
pub enum MmLayout {
    Flow(MmFlowLayout),
    Seq(MmSeqLayout),
    Empty,
}

/// 卡片布局常量（paint 与卡片高度共用，不得分开改）。
pub(crate) const MM_TOP_PAD: f32 = 30.;
/// LR/RL 纵向层内堆叠间距（cross 固定步距，宽度无关 → 卡高无窗口可算）。
pub(crate) const MM_LANE: f32 = 52.;
/// TB/BT 轴向秩距 = 节点高 + 轴间隙（轴间隙容纳边折线与标签，防文字挤成堆）。
pub(crate) const MM_RANK_TB: f32 = 84.;
/// LR/RL 列与列之间的水平间距（容纳箭头/横线段）。
pub(crate) const MM_H_GAP: f32 = 40.;
pub(crate) const MM_NODE_H: f32 = 36.;
pub(crate) const MM_NODE_XPAD: f32 = 18.;
pub(crate) const MM_SEP: f32 = 24.;
pub(crate) const MM_EDGE_X: f32 = 14.;
/// 每个 subgraph 嵌套层加的标题带高。
pub(crate) const MM_SUB_TITLE_H: f32 = 26.;
/// 端箭头长度（px，缩放前规范空间；= `paint::arrow_head` 的 len 9·s 在 s=1 的取值）。
/// 布局据此把弧线目标端收短一个箭头长、paint 据此把线末点回缩到箭头基部——「线到基部、
/// 箭头到节点」双连接修复的**单一几何来源**，layout 与 paint 同源，不得分开改。
pub(crate) const ARROW_LEN_PX: f32 = 9.;

/// 空图的固定卡片高。
pub(crate) fn mermaid_empty_height() -> Pixels {
    px(140.)
}

#[cfg(test)]
mod tests;
