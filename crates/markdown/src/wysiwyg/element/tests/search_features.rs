#[gpui::test]
fn wy_search_highlight_resolves_query_range(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    // 预览独有能力搬入统一组件：`highlight_search(line, query)` → 命中的绝对源区间。
    let text = "alpha beta\ngamma delta\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            // 命中第 0 行 "beta"（源字节 6..10），大小写不敏感（内部小写化）。
            e.highlight_search(0, "BETA", cx);
            assert_eq!(
                e.search_highlight_range(),
                Some(6..10),
                "命中词应解析为绝对源区间"
            );
            // 行内无命中 → None。
            e.highlight_search(0, "zzz", cx);
            assert_eq!(e.search_highlight_range(), None, "行内无命中 → None");
            // 清除 → 无高亮。
            e.highlight_search(0, "beta", cx);
            assert_eq!(e.search_highlight_range(), Some(6..10));
            e.clear_highlight();
            assert_eq!(e.search_highlight_range(), None, "清除后应无高亮");
        });
    });
}

#[gpui::test]
fn wy_search_highlight_cleared_on_source_change(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("alpha beta\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.highlight_search(0, "beta", cx);
            assert_eq!(e.search_highlight_range(), Some(6..10));
            // 换源（如打开另一文件）→ 高亮清空。
            e.set_source_text("gamma delta\n");
            assert_eq!(e.search_highlight_range(), None, "换源应清高亮");
        });
    });
}

#[gpui::test]
fn wy_search_highlight_paints_without_panic(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "alpha beta\ncode block\n| A | beta |\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| e.highlight_search(0, "beta", cx));
        // 带高亮状态注入帧并触发真实绘制（paint_search_highlight 走选区几何，
        // 表格/文本分支均不 panic）。
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        });
        let _ = window.draw(cx);
    });
}

#[gpui::test]
fn table_arrow_nav_skips_delimiter(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _cx| {
            // 表头（行 0）down → 首数据行（行 2，跳过分隔行 1）。
            e.set_caret(e.line_start(0) + 2);
            e.move_down();
            assert_eq!(e.row_col(e.caret()).0, 2, "表头 down 应到首数据行");
            // 数据行 up → 上一数据行。
            e.set_caret(e.line_start(3) + 4);
            e.move_up();
            assert_eq!(e.row_col(e.caret()).0, 2, "次数据行 up 应到首数据行");
            // 首数据行 up → 表头（跳过分隔行）。
            e.move_up();
            assert_eq!(e.row_col(e.caret()).0, 0, "首数据行 up 应到表头");
            // 末数据行 down → 出表到尾段（行 4）。
            e.set_caret(e.line_start(3) + 4);
            e.move_down();
            assert_eq!(e.row_col(e.caret()).0, 4, "末数据行 down 应出表到尾段");
            // 单元格内 Left：行首 Left 被钳在格内容（不落管道空白）。
            e.set_caret(e.line_start(2) + 2); // "1"
            e.move_left();
            {
                let (l, c) = e.row_col(e.caret());
                assert_eq!(l, 2, "内容行 Left 不应出表");
                assert_eq!(c, 2, "格内容起点 Left 被钳在格内（col {c}）");
            }
        });
    });
}

#[gpui::test]
fn new_features_shape_gutters_cards_links(cx: &mut TestAppContext) {
    use super::{BlockCard, LinkHit, RowGutter};
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "\
# 标题

- [x] 完成
- [ ] 待办

> [!NOTE]- 提示正文
> 折叠内容

行内链接 [示例](https://example.com) 与数学 $x^2$ 同段

$$
\\int x dx
$$

```mermaid
graph TD
A-->B
```
";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        // C1: 任务行 gutter = Check；未勾选行在 src 行 3。
        let check = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Check { checked: false })))
            .expect("应有未勾选任务行");
        assert_eq!(check.src_line, 3, "`- [ ] 待办` 的源行");
        // C3: 警报 marker 行 gutter = Alert；其下 body 行折叠成 0 高。
        let alert = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Alert { .. })))
            .expect("应有警报行");
        assert_eq!(alert.src_line, 5, "`> [!NOTE]-` marker 源行");
        // 折叠 callout 主体（src 6）0 高不物化，高度经布局断言。
        let folded_ix = frame
            .block_index_for_src_line(6)
            .expect("callout 主体块应存在");
        assert_eq!(
            frame.block_height(folded_ix),
            0.,
            "折叠 callout 主体应 0 高"
        );
        // C5: 有可点击链接的行。
        let linkrow = frame
            .rows
            .iter()
            .find(|r| !r.extra.links.is_empty())
            .expect("应有链接行");
        assert!(
            linkrow
                .extra
                .links
                .iter()
                .any(|l| matches!(l.target, LinkHit::Url(_))),
            "应命中 URL 链接"
        );
        // B2: 行内数学 splice 成 spacer 并翻译成 Unicode 排版（`x^2` → `x²`）。
        let math_row = frame
            .rows
            .iter()
            .find(|r| !r.extra.inline_maths.is_empty())
            .expect("应有行内数学");
        let im = &math_row.extra.inline_maths[0];
        assert!(
            im.latex.as_ref().contains('²') || im.latex.as_ref().contains('π'),
            "行内数学应翻译成 Unicode：「{}」",
            im.latex
        );
        assert!(
            !math_row.extra.links.is_empty(),
            "含 `[text](url)` 的行应为可点击链接"
        );
        // A1: 数学块折叠成 Math 卡片（整块一行）。
        let mathcard = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
            .expect("应有数学卡片");
        assert!(f32::from(mathcard.height) >= 40., "数学卡片高");
        assert_eq!(mathcard.src_lines, 10..13, "数学卡片覆盖 `$$…$$` 块");
        // A2: mermaid 围栏 → Mermaid 卡片。
        let mm = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid)))
            .expect("应有 mermaid 卡片");
        assert!(
            f32::from(mm.height) >= 100.,
            "mermaid 卡片高按层数计算：{}",
            f32::from(mm.height)
        );
        let _ = IrKind::Paragraph;
    });
}

#[gpui::test]
fn mermaid_renders_diagram_to_image(cx: &mut TestAppContext) {
    // mermaid 卡片：层式布局（4 节点 3 边）+ 高度按层数算 + 把**同一套几何**
    // 光栅化成 PNG（盒子+连线+箭头），供外部识图核验“真图”。
    use super::{BlockCard, mm_layout};
    let out_dir = std::env::temp_dir().join("kilo");
    let _ = std::fs::create_dir_all(&out_dir);
    const SRC: &str = "A[开始] --> B{判断}\nB -->|是| C[完成]\nB -->|否| D[返回]";
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "\
# 标题

```mermaid
graph TD
    A[开始] --> B{判断}
    B -->|是| C[完成]
    B -->|否| D[返回 A]
```

尾段
";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (card_h, n_nodes, n_edges, png_path, ink, size) = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let mm = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid)))
            .expect("应有 mermaid 卡片");
        let card_h = f32::from(mm.height);
        let theme = ed.read(cx).theme.clone();
        let layout = mm_layout(window, &theme, SRC, 0., 0., 560.);
        let (placed, edges) = match &layout {
            super::MmLayout::Flow(f) => (&f.placed, &f.edges),
            _ => panic!("应为 flowchart 布局"),
        };
        let (img, ink) = rasterize_mm_png(placed, edges);
        let path = out_dir.join("mermaid_check.png");
        let saved = img.save(&path).is_ok();
        assert!(saved, "应能保存 PNG");
        let size = (img.width(), img.height());
        (
            card_h,
            placed.len(),
            edges.len(),
            path.to_string_lossy().to_string(),
            ink,
            size,
        )
    });
    assert!(card_h >= 100., "卡片高按层数计算：{card_h}");
    assert_eq!(n_nodes, 4, "4 个节点");
    assert_eq!(n_edges, 3, "3 条边");
    assert!(ink > 300, "PNG 应画出盒子/连线（非白像素 {ink}）");
    eprintln!("mermaid PNG 已导出：{png_path}（{size:?}，涂墨 {ink}px）");
}

#[gpui::test]
fn mermaid_sequence_renders_card(cx: &mut TestAppContext) {
    // sequenceDiagram 围栏 → 序列图卡片：参与者在首见顺序、消息行数、激活条。
    use super::BlockCard;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "\
# 标题

```mermaid
sequenceDiagram
    actor 用户
    participant 服务 as API 网关
    用户->>+服务: 登录
    服务-->>-用户: 成功
```

尾段
";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let mm = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid)))
            .expect("应有 mermaid 序列图卡片");
        let laid = mm.mm.as_ref().expect("有缓存布局");
        match &**laid {
            super::MmLayout::Seq(s) => {
                assert_eq!(s.seq.messages.len(), 2, "两条消息");
                assert_eq!(s.columns.len(), 2, "两个参与者");
                assert!(s.columns[0].is_actor, "用户是 actor");
                assert_eq!(s.columns[1].label, "API 网关", "participant … as 别名");
                assert_eq!(s.activations.len(), 1, "一条激活条（含 + / - 配对）");
                assert!(f32::from(mm.height) >= 46. + 2. * 26., "卡高按行数");
                assert!(s.columns[1].x > s.columns[0].x, "列从左到右排");
            }
            other => panic!("应为序列图布局，实际 {other:?}"),
        }
    });
}

#[gpui::test]
fn mermaid_flow_lr_orders_columns(cx: &mut TestAppContext) {
    // LR 方向：层沿 +x 增长 → A.x < B.x < C.x（方向参数化生效）。
    use super::{MmLayout, mm_layout};
    cx.update(crate::test_support::bind_local_keymaps);
    const SRC: &str = "flowchart LR\nA-->B\nB-->C";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed =
            cx.new(|cx| WyEditor::new(window, cx).with_text("```mermaid\ngraph LR\nA-->B\n```"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let theme = ed.read(cx).theme.clone();
        match mm_layout(window, &theme, SRC, 0., 0., 800.) {
            MmLayout::Flow(f) => {
                let node = |id: &str| {
                    f.placed
                        .iter()
                        .find(|p| p.id == id && !p.virtual_node)
                        .expect("节点")
                };
                let (ax, aw, bx, bw, cx, cw_) = (
                    node("A").x,
                    node("A").w,
                    node("B").x,
                    node("B").w,
                    node("C").x,
                    node("C").w,
                );
                assert!(ax < bx && bx < cx, "LR: A<B<C 沿 x");
                // 相邻列不再重叠（05 左挤修复：列宽按该列最宽节点推进）。
                assert!(ax + aw <= bx, "LR: A 右边不与 B 重叠（{ax}+{aw} vs {bx}）");
                assert!(bx + bw <= cx, "LR: B 右边不与 C 重叠");
                let _ = (cx, cw_);
                assert_eq!(f.edges.len(), 2, "两条边");
            }
            other => panic!("应为 flowchart 布局，实际 {other:?}"),
        }
    });
}

#[gpui::test]
fn mermaid_lr_edges_attach_to_node_edges(cx: &mut TestAppContext) {
    // 回归「连线歪斜」：LR 边钉在节点**右/左边缘中心**，同行边为水平直线，
    // 异行边为水平-垂直-水平直角折线（起终点都在边缘、直线段与行对齐）。
    use super::{MmLayout, mm_layout};
    cx.update(crate::test_support::bind_local_keymaps);
    const SRC: &str = "flowchart LR\nX[引擎] --> Y[阶段1] & Z[阶段2]\nY --> W\nZ --> W";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("x"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let theme = ed.read(cx).theme.clone();
        match mm_layout(window, &theme, SRC, 0., 0., 800.) {
            MmLayout::Flow(f) => {
                let node = |id: &str| {
                    f.placed
                        .iter()
                        .find(|p| p.id == id && !p.virtual_node)
                        .unwrap()
                };
                let edge = |from: &str, to: &str| {
                    f.edges
                        .iter()
                        .find(|e| f.placed[e.a].id == from && f.placed[e.b].id == to)
                        .expect("边")
                };
                // X→Y 同行：起点= X 右缘中心，终点= Y 左缘中心，全程同一 y → 水平直线。
                let xy = edge("X", "Y");
                let (xp, yp) = (node("X"), node("Y"));
                assert_eq!(
                    xy.points.first().unwrap().0,
                    xp.x + xp.w,
                    "X→Y 起点钉 X 右缘"
                );
                assert_eq!(
                    xy.points.first().unwrap().1,
                    xp.y + xp.h / 2.,
                    "X→Y 起点钉行中线"
                );
                assert_eq!(xy.points.last().unwrap().0, yp.x, "X→Y 终点钉 Y 左缘");
                assert!(
                    xy.points
                        .iter()
                        .all(|p| (p.1 - (xp.y + xp.h / 2.)).abs() < 0.01),
                    "X→Y 水平直线"
                );
                // X→Z 异行：直角折线，中间两段共享同一 x（竖直段），两端钉边缘。
                let xz = edge("X", "Z");
                let zp = node("Z");
                assert_eq!(
                    xz.points.first().unwrap().0,
                    xp.x + xp.w,
                    "X→Z 起点钉 X 右缘"
                );
                assert_eq!(xz.points.last().unwrap().0, zp.x, "X→Z 终点钉 Z 左缘");
                assert_eq!(xz.points.len(), 4, "X→Z 为 4 段直角折线");
                assert_eq!(xz.points[1].0, xz.points[2].0, "竖直段 x 对齐");
                // Y→W 同行水平直线；Z→W 异行直角。
                let yw = edge("Y", "W");
                let wp = node("W");
                assert_eq!(yw.points.first().unwrap().0, yp.x + yp.w);
                assert_eq!(yw.points.last().unwrap().0, wp.x);
                assert!(
                    yw.points
                        .iter()
                        .all(|p| (p.1 - (yp.y + yp.h / 2.)).abs() < 0.01)
                );
                assert_eq!(
                    xz.points.last().unwrap().1,
                    zp.y + zp.h / 2.,
                    "X→Z 终点水平段在 Z 行"
                );
            }
            other => panic!("应为 flowchart 布局，实际 {other:?}"),
        }
    });
}

#[gpui::test]
fn mermaid_td_branch_user_example_arcs(cx: &mut TestAppContext) {
    // 投诉 .1：`B-->|是|C` / `B-->|否|D` 同源分支边须出**单弓弧线**——smoothed 密点首锚定、
    // 12 点二次贝塞尔、中点横扫（非直角折线）、**无 S 拐点**（横向偏距同号）、弓向各自
    // 目标外侧、彼此不交叉；目标端收短到**箭头基部**（「线到基部、箭头到节点」双连接修复）。
    // 走完整 mm_layout（真实 shaping）。
    use super::{MmLayout, ARROW_LEN_PX, arrow_base, mm_layout};
    cx.update(crate::test_support::bind_local_keymaps);
    const SRC: &str = "graph TD\nA[开始]-->B{判断}\nB-->|是|C[完成]\nB-->|否|D[返回 A]";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("x"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let theme = ed.read(cx).theme.clone();
        match mm_layout(window, &theme, SRC, 0., 0., 800.) {
            MmLayout::Flow(f) => {
                let edge = |from: &str, to: &str| {
                    f.edges
                        .iter()
                        .find(|e| f.placed[e.a].id == from && f.placed[e.b].id == to)
                        .expect("边")
                };
                for (f, t) in [("B", "C"), ("B", "D")] {
                    let e = edge(f, t);
                    let sm = e.smoothed.as_ref().unwrap_or_else(|| panic!("{f}→{t} 同源分支应出弧线"));
                    assert_eq!(sm.len(), 12, "{f}→{t} 弧线 12 点 ≤24");
                    assert_eq!(sm.first(), e.points.first(), "{f}→{t} 首锚定 = 骨架首点");
                    // **箭头基部（双连接修复核心断言）**：smoothed 尾点 = 节点边缘 −
                    // ARROW_LEN_PX·arrow_dir（弧线收短到基部，paint 线到基部、箭头到节点，
                    // 不再「线穿到节点边 + 箭头同点」双连接）。
                    let tip = *e.points.last().unwrap();
                    let base = arrow_base(tip, e.arrow_dir, ARROW_LEN_PX);
                    assert_eq!(*sm.last().unwrap(), base, "{f}→{t} 弧线尾点 = 箭头基部（非节点边缘）");
                    assert_ne!(*sm.last().unwrap(), tip, "{f}→{t} 线末 ≠ 节点边缘（不再双连接）");
                    let (dx, dy) = (sm.last().unwrap().0 - tip.0, sm.last().unwrap().1 - tip.1);
                    let dist = (dx * dx + dy * dy).sqrt();
                    assert!((dist - ARROW_LEN_PX).abs() < 1e-3, "{f}→{t} 基部-尖端距离 = 箭头长：{dist:.2}");
                    assert!(
                        (dx + e.arrow_dir.0 * ARROW_LEN_PX).abs() < 1e-3 && (dy + e.arrow_dir.1 * ARROW_LEN_PX).abs() < 1e-3,
                        "{f}→{t} 基部在 tip 的 arrow_dir 反方向一个箭头长：dir={:?}",
                        e.arrow_dir,
                    );
                    let (x0, x1) = (e.points[0].0, e.points.last().unwrap().0);
                    let (y0, y1) = (e.points[0].1, e.points.last().unwrap().1);
                    assert!(
                        sm.iter().any(|&(x, y)| x > x0.min(x1) && x < x0.max(x1) && y > y0.min(y1) && y < y0.max(y1)),
                        "{f}→{t} 弧线中点横扫（非直角折线）",
                    );
                    // 无 S 拐点硬判据：对直线弦的横向（外侧）偏距全部同号——单弓、不弯
                    // 来弯去。弦 x(y)=x0+(x1-x0)·(y-y0)/(y1-y0)，偏距=x−弦x，逐点断言同号。
                    let chord = |p: (f32, f32)| x0 + (x1 - x0) * (p.1 - y0) / (y1 - y0);
                    let interior: Vec<f32> = sm.iter().skip(1).take(sm.len() - 2).map(|&(x, y)| x - chord((x, y))).collect();
                    assert!(interior.len() >= 2);
                    let s0 = interior[0].signum();
                    assert!(s0 != 0., "{f}→{t} 弧线中段偏距不可为零");
                    for (i, &d) in interior.iter().enumerate() {
                        assert!(
                            d.signum() == s0,
                            "{f}→{t} 第 {} 点偏距 {d:.2} 与首点异号 ⇒ 有 S 拐点（弯来弯去）",
                            i + 1
                        );
                    }
                    // 弓起**带符号方向（向外分叉）**：曲线中点对骨架直线中点的横向偏距——
                    // 左目标(C)必须 <0（弓 −x）、右目标(D)必须 >0（弓 +x），即每分支弓向
                    // 远离源列中心线的外侧。窗口化（真实 shaping）debug 实测：单弓弧线
                    // B→C=−28.1、B→D=+29.2（=弦中点横向偏距，肚皮约过目标入射点 ~3px）。
                    // 阈值 ±10 是「肉眼可辨向外弯」下限，方向断言拒绝把弓弯回内侧/彼此。
                    let signed = sm[sm.len() / 2].0 - (x0 + x1) / 2.;
                    if x1 < x0 {
                        assert!(signed <= -10., "{f}→{t} 应弓向外侧(左)：signed={signed:.1} ≤ −10");
                    } else {
                        assert!(signed >= 10., "{f}→{t} 应弓向外侧(右)：signed={signed:.1} ≥ +10");
                    }
                }
                // 双分支不交叉：同一出射点、同一目标行 ⇒ 采样 y 逐点相同，断言 C 的 x < D 的 x。
                let (sc, sd) = (edge("B", "C"), edge("B", "D"));
                let (smc, smd) = (
                    sc.smoothed.as_ref().expect("B→C 弧线"),
                    sd.smoothed.as_ref().expect("B→D 弧线"),
                );
                assert_eq!(smc.len(), smd.len());
                for i in 0..smc.len() {
                    assert!(
                        smc[i].0 <= smd[i].0,
                        "B→C 与 B→D 弧线第 {i} 点交叉：{:.1} > {:.1}",
                        smc[i].0, smd[i].0
                    );
                }
                // A→B 直干：A 非分支源——即便对齐/偏移都不得误接弧线（对齐直边→None；
                // 若宽度差≥4 也只能是圆角肘，绝无 12 点弧线）。
                let ab = edge("A", "B");
                assert_ne!(ab.smoothed.as_ref().map(|s| s.len()), Some(12), "A→B 非分支不得误接弧线");
            }
            other => panic!("应为 flowchart 布局，实际 {other:?}"),
        }
    });
}

#[gpui::test]
fn mermaid_wheel_zooms_block(cx: &mut TestAppContext) {
    // 滚轮缩放须**先点击聚焦该 mermaid 卡**：未聚焦时滚轮放行（给宿主正常滚动，
    // 不再「往下滚遇到 mermaid 就缩图」）；点击卡置聚焦后才缩放；点击它处解除聚焦。
    // Ctrl 滚轮放行给宿主全局缩放；非卡区域滚轮不改缩放。
    use super::BlockCard;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "# 标题\n\n```mermaid\ngraph TD\n    A[开始] --> B[结束]\n```\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let mrow = frame
            .rows
            .iter()
            .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid)))
            .expect("应有 mermaid 卡片");
        let block = mrow.src_line;
        // 元素 bounds 占满 600×2000（手工置 last_bounds，模拟 paint 期产物）。
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(2000.)),
            )));
        });
        let card_y = f32::from(mrow.top) + 10.;
        let wheel = |ctrl: bool| gpui::ScrollWheelEvent {
            position: point(px(300.), px(card_y)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(120.))),
            modifiers: gpui::Modifiers {
                control: ctrl,
                ..Default::default()
            },
            ..Default::default()
        };
        let down_at = |y: f32| MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(300.), px(y)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        // 1) 未聚焦：悬停卡片滚轮 → 不消费（正常滚动），不改缩放。
        let before = ed.read(cx).mermaid_zoom(block);
        let consumed0 = ed.update(cx, |e, c| e.on_scroll_wheel(&wheel(false), window, c));
        assert!(!consumed0, "未聚焦时滚轮应放行（正常滚动）");
        assert_eq!(
            ed.read(cx).mermaid_zoom(block),
            before,
            "未聚焦滚轮不改缩放"
        );
        // 2) 点击卡片 → 聚焦。
        let d = down_at(card_y);
        ed.update(cx, |e, c| e.on_mouse_down(&d, window, c));
        assert_eq!(
            ed.read(cx).focused_mermaid(),
            Some(block),
            "点击卡片应聚焦该 mermaid"
        );
        // 3) 聚焦后：滚轮 → 消费并放大。
        let consumed1 = ed.update(cx, |e, c| e.on_scroll_wheel(&wheel(false), window, c));
        let after = ed.read(cx).mermaid_zoom(block);
        assert!(consumed1, "聚焦后悬停卡片滚轮应消费");
        assert!(
            after > before,
            "普通滚轮应放大 mermaid 卡：{before} → {after}"
        );
        // 4) Ctrl 滚轮：mermaid 不放行（宿主 zoom_overlay 管全局字号）。
        let consumed_ctrl = ed.update(cx, |e, c| e.on_scroll_wheel(&wheel(true), window, c));
        assert!(!consumed_ctrl, "Ctrl 滚轮不放行给 mermaid");
        assert_eq!(
            ed.read(cx).mermaid_zoom(block),
            after,
            "Ctrl 滚轮不改 mermaid 缩放"
        );
        // 5) 点击卡片外（普通文本区）→ 清除聚焦 → 滚轮再次放行。
        let dm = down_at(8.0);
        ed.update(cx, |e, c| e.on_mouse_down(&dm, window, c));
        assert_eq!(ed.read(cx).focused_mermaid(), None, "点击卡片外应清除聚焦");
        let consumed2 = ed.update(cx, |e, c| e.on_scroll_wheel(&wheel(false), window, c));
        assert!(!consumed2, "清除聚焦后滚轮应再次放行（正常滚动）");
        assert_eq!(
            ed.read(cx).mermaid_zoom(block),
            after,
            "清除聚焦后滚轮不改缩放"
        );
        // 6) 完全没聚焦时的非卡区域滚轮：不消费、不改缩放（历史分支仍成立）。
        let miss = gpui::ScrollWheelEvent {
            position: point(px(300.), px(8.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(120.))),
            ..Default::default()
        };
        let consumed_miss = ed.update(cx, |e, c| e.on_scroll_wheel(&miss, window, c));
        assert!(!consumed_miss, "卡片外滚轮不应消费");
        assert_eq!(ed.read(cx).mermaid_zoom(block), after, "卡片外滚轮不改缩放");
    });
}

/// 把布局几何光栅化成 PNG（盒子边框 + 连线 + 箭头；中文标签不进位图，
/// 由「框 + 箭头」结构本身表明这是一张流程图）。
fn rasterize_mm_png(
    placed: &[super::MmPlaced],
    edges: &[super::MmPolylineEdge],
) -> (raster::RgbaImage, usize) {
    const PAD: i32 = 40;
    if placed.is_empty() {
        return (raster::RgbaImage::new(1, 1), 0);
    }
    let mut x0f = f32::MAX;
    let mut y0f = f32::MAX;
    let mut x1f = f32::MIN;
    let mut y1f = f32::MIN;
    for p in placed {
        x0f = x0f.min(p.x);
        y0f = y0f.min(p.y);
        x1f = x1f.max(p.x + p.w);
        y1f = y1f.max(p.y + p.h);
    }
    let w = ((x1f - x0f) as i32 + 2 * PAD).max(8);
    let h = ((y1f - y0f) as i32 + 2 * PAD).max(8);
    let mut img =
        raster::RgbaImage::from_pixel(w as u32, h as u32, raster::Rgba([255, 255, 255, 255]));
    let ox = -x0f as i32 + PAD;
    let oy = -y0f as i32 + PAD;
    let mut ink = 0usize;
    let dark = [40u8, 45, 60, 255];
    let fill = [235u8, 238, 245, 255];
    // 边：三层折线 + 箭头。
    for e in edges {
        let (pa, pb) = (&placed[e.a], &placed[e.b]);
        let ax = (pa.x + pa.w / 2.) as i32 + ox;
        let ay = (pa.y + pa.h) as i32 + oy;
        let bx = (pb.x + pb.w / 2.) as i32 + ox;
        let by = pb.y as i32 + oy;
        let midy = (ay + by) / 2;
        if (bx - ax).abs() < 2 {
            mm_blit(&mut img, &mut ink, ax, ay, bx, by - 4, dark);
        } else {
            mm_blit(&mut img, &mut ink, ax, ay, ax, midy, dark);
            mm_blit(&mut img, &mut ink, ax, midy, bx, midy, dark);
            mm_blit(&mut img, &mut ink, bx, midy, bx, by - 4, dark);
        }
        mm_blit(&mut img, &mut ink, bx - 4, by - 6, bx, by - 1, dark);
        mm_blit(&mut img, &mut ink, bx + 4, by - 6, bx, by - 1, dark);
    }
    // 节点：填充 + 边框。
    for p in placed {
        let (x0, y0) = (p.x as i32 + ox, p.y as i32 + oy);
        let (x1, y1) = ((p.x + p.w) as i32 + ox, (p.y + p.h) as i32 + oy);
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                if xx >= 0 && yy >= 0 && (xx as u32) < img.width() && (yy as u32) < img.height() {
                    img.put_pixel(xx as u32, yy as u32, fill.into());
                    ink += 1;
                }
            }
        }
        mm_blit(&mut img, &mut ink, x0, y0, x1, y0, dark);
        mm_blit(&mut img, &mut ink, x0, y1, x1, y1, dark);
        mm_blit(&mut img, &mut ink, x0, y0, x0, y1, dark);
        mm_blit(&mut img, &mut ink, x1, y0, x1, y1, dark);
    }
    (img, ink)
}

fn mm_blit(
    img: &mut raster::RgbaImage,
    ink: &mut usize,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    c: [u8; 4],
) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let (mut x0, mut y0) = (x0, y0);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 && x0 < w && y0 < h {
            img.put_pixel(x0 as u32, y0 as u32, c.into());
            *ink += 1;
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

#[gpui::test]
fn mermaid_paint_smoke_accents_and_sequence(cx: &mut TestAppContext) {
    // 换色后的 paint 冒烟：流程图（多形状 + 粗/点边缘 + classDef 作者色节点）+
    // 序列图（loop/else、激活、实/虚/叉箭头）一起真实绘制一遍，断言不 panic；
    // 同时核对分类 accent：普通节点拿到 accent_idx（循环 0..），作者色节点为 None 且保留 hex。
    use super::BlockCard;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "\
# 标题

```mermaid
flowchart TD
    A[开始] ==> B{判断}
    B -.-> C[完成]
    B --> D[返回]
    D ==> E[退出]
    classDef red fill:#ff8888
    class B red
```

```mermaid
sequenceDiagram
    participant U as 用户
    participant S as 服务
    U->>+S: 登录
    loop 重试
        S->>U: 提示
    else 超时
        S--xU: 错误
    end
    S-->>-U: 成功
```

尾段
";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let cards: Vec<_> = frame
            .rows
            .iter()
            .filter(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid)))
            .collect();
        assert_eq!(cards.len(), 2, "两张 mermaid 卡（流程图 + 序列图）");
        // 流程图：普通节点按声明序循环 accent 0..、作者 classDef 着色节点 None。
        let flow = cards
            .iter()
            .find_map(|r| {
                r.mm.as_deref().and_then(|l| match l {
                    super::MmLayout::Flow(f) => Some(f),
                    _ => None,
                })
            })
            .expect("应有流程图布局");
        let accent = |id: &str| {
            flow.placed
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.accent_idx)
        };
        // 声明序 A,B,C,D,E；B 作者着色跳过 → A=0、C=1、D=2、E=3。
        assert_eq!(accent("A"), Some(Some(0)), "A accent 0");
        assert_eq!(accent("B"), Some(None), "classDef 作者色节点不参与循环");
        assert_eq!(accent("C"), Some(Some(1)), "C accent 1");
        assert_eq!(accent("D"), Some(Some(2)), "D accent 2");
        assert_eq!(accent("E"), Some(Some(3)), "E accent 3");
        // 作者色节点保留作者 fill hex；普通节点 fill 在 shape 期预混好（Some tint）。
        let b = flow.placed.iter().find(|p| p.id == "B").unwrap();
        assert_eq!(b.fill, Some(0xff8888), "作者 fill 原样保留");
        // 完整画一遍（measure→prepaint→paint），catch 任何渲染 panic。
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        let _ = window.draw(cx);
    });
}

