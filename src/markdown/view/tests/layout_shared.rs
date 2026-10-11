// B5 蓝图 Phase 1 对照测试：`shape_docrow_shard` 高度累加 == 现行预览实测行高。
//
// 本文件经 `view/tests.rs` `include!` 进 `view::tests`（与之同模块的 `viewport.rs` 已
// `use super::*` + `use gpui::{…, px, size, …}`，故 `MarkdownPreview`/`PreviewHost`/
// `settle`/`parse_to_rows`/`RowKind`/`px`/`size` 直接可用，不重复 import 以免 E0252）。
//
// 断言口径（与 `layout` 模块文档一致）：
// - Code 行：两引擎语义完全一致（`(行数+2)×code_line`，元素无内衬）→ 精确相等。
// - 文本行（段落，无 gutter）：同一 `avail` 折行 → 视觉行数一致；预览盒高落在
//   `py + max(min_floor, n×natural)` 带内（n = shard 视觉子行数，natural ∈ [fs×1.2, para_line]）。
//   这个「高度带」能拦下折行数漂移（n 偏大/偏小都会把带外），并暴露真实的行距系数差
//   （预览用 gpui 自然行距 ≈1.35×fs，shard 用 para_line = 1.45×fs）——Phase 2 统一量程时消掉。
// - 已知系统性差（只留档、不断言相等）：列表/引述 gutter 宽、图片题注模型、
//   空行/分隔行高（shard=para_line vs 预览=blank_h/rule_h）、行内公式 splice（B 独有）——
//   对照样例仅覆盖「两引擎折行语义一致」的行型。

use crate::config::markdown::{CONTENT_EDGE_PAD_X_PX, PREVIEW_SIDE_GAP_PX};
use crate::markdown::doc::Doc;
use crate::markdown::ir::IrKind;
use crate::markdown::layout::shape_docrow_shard;
use crate::markdown::view::style::StyleParams;
use crate::markdown::wysiwyg::WyTheme;

/// 对照样例：纯文本段、代码块、折行长段、含行内图的段、含（中性）行内公式的段。
/// 全部行型都满足「预览与 shard 折行语义一致」。
fn sample_src() -> &'static str {
    "纯文本短段。\n\
    \n\
    这是一段很长的中文折行文本，用来验证在同一个 avail 宽度下，共享整形核心与现行预览\
    元素树是否在相同位置折行、并给出一致的行高。再补一些文字让它稳稳超过一行。\n\
    \n\
    ```rs\nlet a = 1;\nlet b = 2;\n```\n\
    \n\
    含行内图 ![logo](img/logo.png) 的段落。\n\
    \n\
    含行内公式 $x^2 + y = 1$ 的段落。\n"
}

/// 文本行对照（段落，无 gutter）：shard 折行行数 vs 预览盒高。
/// - `h_preview`：预览 `bounds_for_item` 盒高（含垂直内衬 `py`、`min_h` 兜底）。
/// - `n_visual`：`Σ shard.h/shard.lh` = shard 的视觉子行数（折行语义对拍）。
/// - 预览每行实际行距 = gpui 自然行距 `natural`（实测 ≈1.8×fs），shard 用 `para_line`
///   （1.45×fs）——两者行数一致，逐行行距系数不同（Phase 2 统一量程时以单一 lyh 消掉）。
fn check_text_row(note: &str, h_preview: f32, n_visual: f32, p: &StyleParams, py: f32) {
    let fs = p.para_size.as_f32();
    let para = p.para_line.as_f32();
    let min_floor = para + p.para_gap.as_f32() + 6.0;
    // 预览每行 ∈ [para_line, fs×1.9]（gpui 自然行距实测 ≈1.8fs，留裕量）
    // → 盒高 ∈ py + max(min_floor, n×[para_line, fs×1.9])。
    let lo = py + min_floor.max(n_visual * para);
    let hi = py + min_floor.max(n_visual * fs * 1.9);
    assert!(
        h_preview >= lo - 0.6,
        "{note}: 预览盒高低于下界 h={h_preview:.2} lo={lo:.2}（shard 折行数过大？n={n_visual:.0}）"
    );
    assert!(
        h_preview <= hi + 0.6,
        "{note}: 预览盒高高于上界 h={h_preview:.2} hi={hi:.2}（shard 折行数过小？n={n_visual:.0}）"
    );
}

/// 主对照测试：`shared_shard_height_matches_preview_row`。
/// 断言 shard 高度累加与预览实测行高的关系（见模块注释口径）。
#[gpui::test]
fn shared_shard_height_matches_preview_row(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let src = sample_src();
    let rows = parse_to_rows(src);
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);

    let doc = Doc::open(src.into());
    let n = doc.block_count();
    let docrows = doc.rows_in_block_window(0, n.saturating_sub(1));
    assert_eq!(docrows.len(), rows.len(), "DocRows 与预览行数应对齐");

    let (failures, notes): (Vec<String>, Vec<String>) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let theme = WyTheme::from_style(&p, window.text_style().font(), p.para_size);
        let md = host.read(cx).md.read(cx);
        // 垂直内衬 = 首个段落盒高 − min_h 兜底（空文本行仅为兜底+内衬）。
        let min_floor = p.para_line.as_f32() + p.para_gap.as_f32() + 6.0;
        let py = {
            let ix = md
                .rows
                .iter()
                .position(|r| r.kind == RowKind::Paragraph)
                .expect("应有段落行");
            let h = md
                .list_state
                .bounds_for_item(ix)
                .map(|b| b.size.height.as_f32())
                .unwrap_or(0.);
            (h - min_floor).max(0.)
        };
    // 与预览同一内容宽度口径：宿主侧边 `PREVIEW_SIDE_GAP_PX`×2 + 行内左右边距×2。
    let avail = width - 2.0 * PREVIEW_SIDE_GAP_PX - 2.0 * CONTENT_EDGE_PAD_X_PX;
    let wts = window.text_system().clone();
        let mut failures = Vec::new();
        let mut notes = Vec::new();
        for (ix, dr) in docrows.iter().enumerate() {
            let h_preview = md
                .list_state
                .bounds_for_item(ix)
                .map(|b| b.size.height.as_f32())
                .unwrap_or(0.);
            let shards = shape_docrow_shard(&wts, dr, &theme, avail, width);
            let sum: f32 = shards.iter().map(|s| s.h).sum();
            let n_visual: f32 = shards.iter().map(|s| s.h / s.lh).sum();
            let tag = format!("row[{ix}] kind={:?}", dr.kind);
            if matches!(dr.kind, IrKind::Code) {
                // 代码行：两引擎语义完全一致 → 精确相等。
                if (sum - h_preview).abs() > 0.5 {
                    failures.push(format!("{tag}: 代码行应精确相等 got={sum:.2} preview={h_preview:.2}"));
                }
            } else if matches!(dr.kind, IrKind::Paragraph) {
                check_text_row(&tag, h_preview, n_visual, &p, py);
                // 留档每行行距系数差：已能算出的预览自然行距 = (盒高−py)/n（仅 n≥2 时不受
                // min_floor 遮蔽，准确），与 shard 的 para_line 对比。
                if n_visual >= 2. {
                    let natural = (h_preview - py) / n_visual;
                    notes.push(format!(
                        "{tag}: shard para_line={:.2} vs 预览 natural={natural:.2} (Δ/行 {:.2})",
                        p.para_line.as_f32(),
                        p.para_line.as_f32() - natural
                    ));
                }
            } else {
                // 其它行型（空行等）已知系统性差，仅记录不断言（见模块注释）。
                notes.push(format!("{tag}: 留档 shard={sum:.2} preview={h_preview:.2}"));
            }
        }
        (failures, notes)
    });
    for note in &notes {
        eprintln!("{note}");
    }
    assert!(failures.is_empty(), "对照失败：\n{}", failures.join("\n"));
}

