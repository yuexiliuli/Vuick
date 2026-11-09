//! Mermaid 模块单测：纯解析（`parse`）与无窗口卡高（`layout::mermaid_card_height`）。
//! 走完整 `mm_layout`（需窗口/主题）的集成用例在 `element/tests.rs`（复用其 harness）。

use super::*;
use gpui::px;

/// 图类型判定：flowchart / sequence / 空。
#[test]
fn detects_diagram_kind() {
    assert_eq!(diagram_kind("graph TD\nA-->B"), MmDiagramKind::Flow);
    assert_eq!(diagram_kind("flowchart LR\nA-->B"), MmDiagramKind::Flow);
    assert_eq!(diagram_kind("sequenceDiagram\nA->>B: x"), MmDiagramKind::Seq);
    assert_eq!(diagram_kind("%% 注释\n   \n"), MmDiagramKind::Empty);
}

/// 方向解析（TD 与 TB 同向；LR/RL/BT 各识别）。
#[test]
fn parses_direction() {
    use MmDirection::*;
    assert_eq!(mermaid_graph("graph TD\nA-->B").direction, TB);
    assert_eq!(mermaid_graph("graph TB\nA-->B").direction, TB);
    assert_eq!(mermaid_graph("flowchart LR\nA-->B").direction, LR);
    assert_eq!(mermaid_graph("flowchart RL\nA-->B").direction, RL);
    assert_eq!(mermaid_graph("graph BT\nA-->B").direction, BT);
}

/// 节点形状全覆盖。
#[test]
fn parses_node_shapes() {
    let src = "flowchart TD\n\
        a[矩形]\n\
        b(圆角)\n\
        c{菱形}\n\
        d((圆))\n\
        e([跑道])\n\
        f[[子程序]]\n\
        g[(库)]\n\
        h{{六边形}}\n\
        i>旗形]\n\
        j[/平行/]\n\
        k[\\平行反\\]\n\
        l[/梯形\\]\n\
        m[\\梯形反/]\n\
        n(((双圆)))\n\
        o@{ shape: rect, label: \"@形状\" }\n";
    let g = mermaid_graph(src);
    let g = &g;
    for (id, want) in [
        ("a", MmShape::Rect),
        ("b", MmShape::Round),
        ("c", MmShape::Diamond),
        ("d", MmShape::Circle),
        ("e", MmShape::Stadium),
        ("f", MmShape::Subroutine),
        ("g", MmShape::Cylinder),
        ("h", MmShape::Hexagon),
        ("i", MmShape::Asymmetric),
        ("j", MmShape::Parallelogram),
        ("k", MmShape::ParallelogramAlt),
        ("l", MmShape::Trapezoid),
        ("m", MmShape::TrapezoidAlt),
        ("n", MmShape::DoubleCircle),
        ("o", MmShape::Rect),
    ] {
        let n = t_nodes(g, id);
        assert_eq!(n.shape, want, "节点 {id} 形状");
    }
    let o = t_nodes(g, "o");
    assert_eq!(o.label, "@形状", "@{{}} 语法取 label");
}

fn t_nodes<'a>(g: &'a MmGraph, id: &str) -> &'a MmNode {
    g.nodes.iter().find(|n| n.id == id).unwrap_or_else(|| panic!("无节点 {id}"))
}

/// 边型全覆盖（粗线/虚线/圆端/叉端/双向/实线）。
#[test]
fn parses_edge_kinds_and_labels() {
    let src = "flowchart TD\n\
        A ==> B\n\
        B -.-> C\n\
        C --o D\n\
        D --x E\n\
        E <--> F\n\
        F -- 文字 --> G\n\
        G --- H";
    let g = mermaid_graph(src);
    let expect: Vec<(MmEdgeKind, Option<&str>)> = vec![
        (MmEdgeKind::ThickArrow, None),
        (MmEdgeKind::DottedArrow, None),
        (MmEdgeKind::CircleEnd, None),
        (MmEdgeKind::CrossEnd, None),
        (MmEdgeKind::BothArrow, None),
        (MmEdgeKind::Arrow, Some("文字")),
        (MmEdgeKind::Line, None),
    ];
    assert_eq!(g.edges.len(), expect.len(), "边数 {}", g.edges.len());
    for (i, (kind, lab)) in expect.iter().enumerate() {
        assert_eq!(g.edges[i].kind, *kind, "边[{i}] 类型");
        assert_eq!(g.edges[i].label.as_deref(), *lab, "边[{i}] 标签");
    }
}

/// 链式边 `A-->B-->C` 展开成两条；并行 `&` 拆成两条链路；`&` 后裸节点续接同一 source。
#[test]
fn parses_chained_and_parallel_edges() {
    let g = mermaid_graph("flowchart TD\nA-->B-->C");
    assert_eq!(g.nodes.len(), 3);
    assert_eq!(g.edges.len(), 2);
    assert_eq!((g.edges[0].from.as_str(), g.edges[0].to.as_str()), ("A", "B"));
    assert_eq!((g.edges[1].from.as_str(), g.edges[1].to.as_str()), ("B", "C"));

    let g2 = mermaid_graph("graph TD\nA --> B & C --> D");
    assert_eq!(g2.edges.len(), 2);
    assert!(g2.edges.iter().any(|e| e.from == "A" && e.to == "B"));
    assert!(g2.edges.iter().any(|e| e.from == "C" && e.to == "D"));

    // `X --> Y & Z`：`&` 后裸节点 Z 复用前一条边的起点 X（X→Y 与 X→Z）。
    let g3 = mermaid_graph("flowchart LR\nX[引擎] --> Y[阶段1] & Z[阶段2]\nY --> W\nZ --> W");
    assert_eq!(g3.nodes.len(), 4);
    assert_eq!(g3.edges.len(), 4);
    assert!(g3.edges.iter().any(|e| e.from == "X" && e.to == "Y"), "X→Y");
    assert!(g3.edges.iter().any(|e| e.from == "X" && e.to == "Z"), "X→Z");
    assert!(g3.edges.iter().any(|e| e.from == "Y" && e.to == "W"), "Y→W");
    assert!(g3.edges.iter().any(|e| e.from == "Z" && e.to == "W"), "Z→W");
    let l = |id: &str| g3.nodes.iter().find(|n| n.id == id).unwrap().label.clone();
    assert_eq!(l("X"), "引擎");
    assert_eq!(l("Y"), "阶段1");
    assert_eq!(l("Z"), "阶段2");
}

/// 注释 `%%` 不解析成节点/边。
#[test]
fn ignores_comments() {
    let g = mermaid_graph("graph TD\n%% 标题注释\nA[开始] %% 行尾注释\nB --> C");
    assert_eq!(g.nodes.len(), 3, "A/B/C，注释不产节点");
    assert_eq!(g.edges.len(), 1);
    assert_eq!((g.edges[0].from.as_str(), g.edges[0].to.as_str()), ("B", "C"));
    assert_eq!(t_nodes(&g, "A").label, "开始");
}

/// subgraph 分组：成员登记 + 嵌套深度。
#[test]
fn parses_subgraphs() {
    let src = "flowchart TD\n\
        subgraph 外[外层]\n\
        A-->B\n\
        subgraph 内[内层]\n\
        C-->D\n\
        end\n\
        E-->A\n\
        end\n\
        F-->G\n";
    let g = mermaid_graph(src);
    assert_eq!(g.subgraphs.len(), 2, "两个 subgraph");
    let outer = g.subgraphs.iter().find(|s| s.title == "外层").unwrap();
    assert_eq!(outer.depth, 0);
    assert_eq!(outer.nodes.len(), 5, "外层含 A/B/C/D/E（F/G 在分组外）");
    let inner = g.subgraphs.iter().find(|s| s.title == "内层").unwrap();
    assert_eq!(inner.depth, 1);
    assert_eq!(inner.nodes.len(), 2);
    assert!(inner.nodes.iter().all(|id| outer.nodes.contains(id)), "内层成员也在外层");
}

/// classDef/class/style → 每节点最终配色（hex 简写 `#9f6`→`#99ff66`；style 覆盖 class）。
#[test]
fn parses_class_styles() {
    let src = "flowchart TD\n\
        classDef green fill:#9f6,stroke:#333,stroke-width:2px\n\
        classDef red fill:#f66\n\
        A[启动] --> B[处理]\n\
        class A green\n\
        class B green\n\
        style B fill:#4488ff\n";
    let g = mermaid_graph(src);
    let a = g.styles.get("A").unwrap();
    assert_eq!(a.fill, Some(0x99ff66), "`#9f6` 展开成 #99ff66");
    assert_eq!(a.stroke, Some(0x333333), "stroke:#333 → #333333");
    assert_eq!(a.stroke_width, Some(2.0), "stroke-width:2px");
    let b = g.styles.get("B").unwrap();
    assert_eq!(b.fill, Some(0x4488ff), "`style B` 覆盖 `class B` 的 fill");
}

/// 序列图：participant/actor 别名、六种箭头、激活速记、Note、loop/alt/else 片段。
#[test]
fn parses_sequence_basics() {
    let src = "sequenceDiagram\n\
        participant U as 用户\n\
        actor A\n\
        U->>+A: 登录\n\
        Note over U,A: 校验\n\
        A-->>-U: 成功\n\
        alt 失败\n\
        A--xU: 错误\n\
        else 超时\n\
        U->A: 重试\n\
        end\n";
    let seq = mermaid_seq(src);
    assert_eq!(seq.participants.len(), 2);
    assert_eq!(seq.participants[0].label, "用户");
    assert!(seq.participants[1].is_actor);
    assert_eq!(seq.messages.len(), 4);
    assert_eq!(seq.messages[0].text, "登录");
    assert_eq!(seq.messages[0].act, 1, "`->>+` 激活速记");
    assert_eq!(seq.messages[1].arrow, MmSeqArrow::Dotted, "`-->>` 虚线实心");
    assert_eq!(seq.messages[1].act, -1, "`-->>-` 收激活");
    assert_eq!(seq.messages[2].arrow, MmSeqArrow::DottedCross, "`--x` 虚叉");
    assert_eq!(seq.notes.len(), 1);
    assert_eq!(seq.notes[0].pos, MmSeqNotePos::Over);
    assert_eq!(seq.fragments.len(), 1);
    assert_eq!(seq.fragments[0].kind, MmSeqFragmentKind::Alt);
    assert_eq!(seq.fragments[0].else_titles.len(), 1);
    assert_eq!(seq.fragments[0].else_titles[0].as_deref(), Some("超时"));
    // 行模型：开/片/last/关。
    assert!(seq.rows.iter().any(|r| matches!(r, MmSeqRow::FragOpen(_))));
    assert!(seq.rows.iter().any(|r| matches!(r, MmSeqRow::FragElse(Some(_)))));
    assert!(seq.rows.iter().any(|r| matches!(r, MmSeqRow::FragClose(_))));
}

/// 无窗口卡高：flowchart 按层数 / 方向不同；sequence 按行数；空图固定。
#[test]
fn card_height_window_free() {
    // TB：3 层 → 30 + 3*84（秩距） + 8 = 290（留足容纳边线+标签）。
    assert_eq!(mermaid_card_height("graph TD\nA-->B-->C"), px(290.));
    // LR：按单层最多节点数（max_row=1）→ 30 + 1*52 + 8 = 90。
    assert_eq!(mermaid_card_height("flowchart LR\nA-->B-->C"), px(90.));
    // 空图。
    assert_eq!(mermaid_card_height("%% nothing\n"), px(140.));
    // sequence：46 头 + 2 消息 52 + 16 = 114。
    assert_eq!(mermaid_card_height("sequenceDiagram\nA->>B: x\nB-->>A: y"), px(46. + 2. * 26. + 16.));
}

/// 卡高不含窗口：与 `layout_flow` 几何一致（LR 固定纵向步距）。
#[test]
fn flow_layout_extents_match_card_height() {
    // 两层、每层 2 节点：TB 高 30+2*84+8=206；LR 高 30+2*52+8=142（max_row=2）。
    let tb = "flowchart TD\nA-->C\nB-->C";
    let lr = "flowchart LR\nA-->C\nB-->C";
    assert_eq!(mermaid_card_height(tb), px(206.));
    assert_eq!(mermaid_card_height(lr), px(142.));
}

/// 分类 accent 循环（纯函数）：普通节点按声明序循环 0..5；作者 classDef/style 显式
/// hex 着色节点不参与循环、映射 None；第 7 个普通节点回绕到 0。
#[test]
fn accent_idx_cycles_and_skips_author_colors() {
    let g = mermaid_graph("flowchart TD\nA-->B\nB-->C\nC-->D\nD-->E\nE-->F");
    let m = accent_idx_for(&g);
    for (id, want) in [("A", 0), ("B", 1), ("C", 2), ("D", 3), ("E", 4), ("F", 5)] {
        assert_eq!(m[id], Some(want), "节点 {id} 应循环到 accent {want}");
    }

    // 作者 classDef/style hex 节点不占循环位（None）；其后普通节点从 0 续起。
    let g2 = mermaid_graph(
        "flowchart TD\n\
         X[甲] --> Y[乙]\n\
         Z[丙] --> Y\n\
         classDef red fill:#f66\n\
         class X red\n\
         style Z stroke:#4488ff",
    );
    let m2 = accent_idx_for(&g2);
    assert_eq!(m2["X"], None, "classDef 填色 → 作者色不参与循环");
    assert_eq!(m2["Z"], None, "style 描边色 → 同样视为作者着色");
    assert_eq!(m2["Y"], Some(0), "作者色节点不占位，Y 从 0 起");

    // 第 7 个普通节点回绕到 0（0..5 后循环）。
    let g3 = mermaid_graph("flowchart TD\nA1-->A2\nA2-->A3\nA3-->A4\nA4-->A5\nA5-->A6\nA6-->A7");
    let m3 = accent_idx_for(&g3);
    assert_eq!(m3["A1"], Some(0));
    assert_eq!(m3["A7"], Some(0), "7 个普通节点回绕到 0");
}

/// `scale_mm_layout`：flowchart 整体等比缩放（x 绕中线、y 贴顶），paint 按 zoom 缩放字号。
#[test]
fn scale_layout_zooms_flow_geometries() {
    // 直接构造一个 mini flow 布局来验证变换（不依赖窗口）。
    let flow = MmFlowLayout {
        placed: vec![MmPlaced {
            id: "A".into(), label: "A".into(), shape: MmShape::Rect,
            x: 100., y: 20., w: 40., h: 30., fill: None, stroke: None, stroke_width: None, accent_idx: None, virtual_node: false,
        }],
        edges: vec![MmPolylineEdge {
            a: 0, b: 0, kind: MmEdgeKind::Arrow,
            points: vec![(0., 0.), (10., 10.)], smoothed: None, arrow_dir: (0., 1.), label: Some((5., 2.)), label_text: Some("x".into()),
        }],
        subgraphs: vec![MmSubgraphBox { title: "g".into(), x: 80., y: 10., w: 80., h: 60. }],
        content_w: 0., content_h: 0., zoom: 1.,
    };
    let mut layout = MmLayout::Flow(flow);
    scale_mm_layout(&mut layout, 2.0, 200.);
    match &layout {
        MmLayout::Flow(f) => {
            assert_eq!(f.zoom, 2.0);
            // x 绕 card 中线(100)缩放：100→100；y 贴顶：20→40；宽高×2。
            assert_eq!(f.placed[0].x, 100.);
            assert_eq!(f.placed[0].y, 40.);
            assert_eq!(f.placed[0].w, 80.);
            assert_eq!(f.placed[0].h, 60.);
            // 边点 (0,0)→x=100+(0-100)*2=-100，y=0*2=0。
            assert_eq!(f.edges[0].points[0].0, -100.);
            assert_eq!(f.edges[0].points[0].1, 0.);
            // 标签锚点 (5,2)→x=100+(5-100)*2=-90，y=4。
            assert_eq!(f.edges[0].label, Some((-90., 4.)));
            // 目标端箭头方向是**单位方向**（目标接触边垂直内法向），均匀缩放（x/y 同倍）+
            // 平移保角 ⇒ 原样保留；tip 落点 = 变换后的骨架末端（节点边缘接触点）。箭头
            // 方向不随缩放歪。
            assert_eq!(f.edges[0].arrow_dir, (0., 1.), "arrow_dir 不随缩放变换（保角）");
            assert_eq!(f.edges[0].points.last(), Some(&(-80., 20.)), "tip 落点=缩放后的骨架末端");
            // 箭头基部随缩放正确：paint 在 zoom z 下 `base = tip − ARROW_LEN_PX·z·arrow_dir`
            // （直边/肘边回缩）；弧线收短的基点在 scale_mm_layout 中随 smoothed 密点同式
            // 缩放，两处共用 `arrow_base` + `ARROW_LEN_PX` 严格重合。此处 z=2 → (−80,20)−18·(0,1)。
            let zoom_tip = *f.edges[0].points.last().unwrap();
            assert_eq!(
                arrow_base(zoom_tip, f.edges[0].arrow_dir, ARROW_LEN_PX * f.zoom),
                (-80., 2.),
                "缩放后箭头基部 = tip − ARROW_LEN_PX·zoom·arrow_dir"
            );
            assert_eq!(f.subgraphs[0].w, 160.);
        }
        _ => panic!("应为 Flow"),
    }
}
