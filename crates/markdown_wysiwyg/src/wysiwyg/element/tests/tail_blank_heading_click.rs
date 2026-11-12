// WYSIWYG 点击定位光标的两条回归（用户报告）：
// - 场景 1：点击「文档尾部之后的无内容空白区域」→ 光标应钳到内容末尾且可见
//   （旧行为：命中不到任何行 → `source_offset_at_local` 返回当前光标 = 点空白不动，
//   光标停在别处/像「消失」）。
// - 场景 2：点击「仅含 `#` 标题行、无其它内容」的空标题行 → 光标应落在该行可编辑
//   位置且可见（旧行为：空标题 `# `/`## ` 无正文 → 行 `editable=false`，击中置
//   `caret=行尾` 后老 `caret_geometry` 对非 Blank 的不可编辑行恒回 `on=false`
//   = 光标到了该行却画不出竖条）。
//
// ⚠️ 本文件 include! 进 `element::tests` 共享命名空间：`WyHost`/`build_frame`/
// `caret_geometry`/`source_offset_at_local`/`blank_harness`/`down_at`/`click_and_caret`
// 已由 `byte_map.rs` 系与 `blank_line_hit.rs` 就位，此处不再重复 `use`。

/// 场景 2：点击空标题行（无正文标题，如 `# ` / `## ` / `# 标题` 全文档仅此一行）
/// → 光标落在该行源区间且 `caret_geometry.on == true`（可画）。
#[gpui::test]
fn empty_or_single_heading_click_shows_caret(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    for text in ["# \n", "## \n", "# 标题\n", "## 二级\n"] {
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let ed = host.read_with(vcx, |h, _| h.ed.clone());
        let frame = blank_harness(vcx, &ed);
        let heading = frame
            .rows
            .iter()
            .find(|r| matches!(r.kind, IrKind::Heading(_)))
            .unwrap_or_else(|| panic!("doc {text:?} 应含标题行"));
        // 复现前提：空标题行（无正文）在 shape 后是 `editable=false` 行；
        // 非空标题行（`# 标题`）是 `editable=true`——两条路径都要可画。
        let x = f32::from(frame.padding) + f32::from(heading.x_off) + 2.;
        let y = f32::from(heading.top) + f32::from(heading.height) * 0.5;
        let (caret, row) = click_and_caret(vcx, &ed, x, y);
        assert_eq!(
            row, heading.src_line,
            "doc {text:?} 点击标题行后光标应落在该行，实际 caret={caret} row={row}"
        );
        let ls = ed.read_with(vcx, |e, _| e.line_start(heading.src_line));
        let le = ed.read_with(vcx, |e, _| e.line_end(heading.src_line));
        assert!(
            caret >= ls && caret <= le,
            "doc {text:?} 光标应落标题行源区间 [{ls},{le}]，实际 caret={caret}"
        );
        // 用户可见核心：光标画得出（caret_geometry.on=true）且 top 落在行带内。
        let f2 = blank_harness(vcx, &ed);
        let (on, _, carety) = vcx.update(|_, cx| {
            let g = caret_geometry(ed.read(cx), &f2);
            (g.0, g.1, g.2)
        });
        let top = f32::from(heading.top);
        let h = f32::from(heading.height);
        assert!(
            on && f32::from(carety) >= top && f32::from(carety) < top + h,
            "doc {text:?} 点击标题行后光标应可见（on={on}）且 top 落行带 [{top},{})，实际 {carety:?}",
            top + h
        );
        assert!(
            on,
            "doc {text:?} 标题行应可画光标（caret_geometry.on=true），实际 on=false"
        );
    }
}

/// 场景 1：点击「文档尾部之后的无内容空白区域」（元素内、末行之下）→ 光标钳到
/// **内容末尾**且可见（旧行为：无行命中返回当前光标 = 点空白不动）。
#[gpui::test]
fn click_below_tail_clamps_caret_to_content_end(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    for text in ["# 你好\n正文\n", "正文", "标题\n\n"] {
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let ed = host.read_with(vcx, |h, _| h.ed.clone());
        // 先落一个确定的非末尾光标（行 0 首字节），确保空白点击真实「移动」到末尾。
        ed.update(vcx, |e, _| e.set_caret(0));
        let frame = blank_harness(vcx, &ed);
        let bottom = frame
            .rows
            .last()
            .map(|r| f32::from(r.top) + f32::from(r.height))
            .expect("帧应有末行");
        let x = f32::from(frame.padding) + 40.;
        let y = bottom + 60.;
        assert!(
            y < 600.,
            "点击点须落在元素 bounds 内（blank_harness 高 ≥600）：y={y}"
        );
        let (caret, _) = click_and_caret(vcx, &ed, x, y);
        let len = ed.read_with(vcx, |e, _| e.len());
        assert_eq!(
            caret, len,
            "doc {text:?} 点击末行(底 {bottom:.0})之后的空白 y={y} 光标应钳到内容末尾 \
             len={len}，实际 caret={caret}"
        );
        // 光标可见：caret_geometry.on == true 且 top 落末行带内。
        let f2 = blank_harness(vcx, &ed);
        let last = f2.rows.last().expect("帧应有末行");
        let (on, _, carety) = vcx.update(|_, cx| {
            let g = caret_geometry(ed.read(cx), &f2);
            (g.0, g.1, g.2)
        });
        assert!(
            on,
            "doc {text:?} 光标钳到内容末尾后应可画（caret_geometry.on=true），实际 on=false"
        );
        let t = f32::from(carety);
        let top = f32::from(last.top);
        let h = f32::from(last.height);
        assert!(
            t >= top && t < top + h,
            "doc {text:?} 光标 top 应落末行带 [{top},{})，实际 {t:.2}",
            top + h
        );
    }
}

/// 场景 A：空标题行（`# ` / `## ` 无正文）点击后光标竖条要**垂直居中于行带**（= 该行
/// 文本将出现的带内居中位置，与输入一字后的光标垂直位置一致），而非锚行带顶
/// （偏上 bug）。对可编辑标题行（有正文）同样断言一致性：两条路径 ctop−top 都取
/// `(lh − f.line_h)/2`。
#[gpui::test]
fn empty_heading_caret_vertically_centered(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    for text in ["# \n", "## \n", "# 标题\n", "### 三级\n"] {
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let ed = host.read_with(vcx, |h, _| h.ed.clone());
        let frame = blank_harness(vcx, &ed);
        let heading = frame
            .rows
            .iter()
            .find(|r| matches!(r.kind, IrKind::Heading(_)))
            .unwrap_or_else(|| panic!("doc {text:?} 应含标题行"));
        let top = f32::from(heading.top);
        let line_h = f32::from(frame.line_h);
        let lh = f32::from(heading.lh);
        // 用户路径：点击标题行（文本起点附近）→ 光标落该行 → 读 caret_geometry。
        let x = f32::from(frame.padding) + f32::from(heading.x_off) + 2.;
        let y = top + f32::from(heading.height) * 0.5;
        let (caret, row) = click_and_caret(vcx, &ed, x, y);
        assert_eq!(
            row, heading.src_line,
            "doc {text:?} 点击后光标应落标题行 src_line={}，实际 caret={caret} row={row}",
            heading.src_line
        );
        let f2 = blank_harness(vcx, &ed);
        let (on, _cx_geom, carety) = vcx.update(|_, cx| {
            let g = caret_geometry(ed.read(cx), &f2);
            (g.0, g.1, g.2)
        });
        assert!(on, "doc {text:?} 标题行光标应可画");
        // 带内居中量：与可编辑分支同一推导（`(lh − line_h)/2`）。标题行 lh > line_h →
        // 该量 ≠ 0，旧实现在不可编辑分支锚 `r.top`（偏上）时此断言必败。
        let centered = ((lh - line_h) / 2.).max(0.);
        let d = f32::from(carety) - top;
        assert!(
            (d - centered).abs() < 0.5,
            "doc {text:?} 光标 ctop−top 应 = 带内居中 {centered:.2} \
             （跟手即得：lh={lh:.2} line_h={line_h:.2}），实际 {d:.2}"
        );
        if lh > line_h + 0.5 {
            assert!(
                d > 0.5,
                "doc {text:?} 标题行高更高时 ctop 应离开带顶（偏上 bug 回归锁）：d={d:.2}"
            );
        }
    }
}
