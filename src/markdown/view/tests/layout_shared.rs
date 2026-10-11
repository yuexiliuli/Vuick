// B5 蓝图对照测试：`shape_docrow_shard` 高度累加 vs 预览实测行高。
//
// 本文件经 `view/tests.rs` `include!` 进 `view::tests`（与之同模块的 `viewport.rs` 已
// `use super::*` + `use gpui::{…, px, size, …}`，故 `MarkdownPreview`/`PreviewHost`/
// `settle`/`parse_to_rows`/`RowKind`/`px`/`size` 直接可用，不重复 import 以免 E0252）。
//
// 断言口径（与 `layout` 模块文档一致；M5-B5 Phase 2 双模式）：
// - **legacy（默认，现状逐位一致）**：`shared_shard_height_matches_preview_row` 走
//   「高度带」断言——Code 行精确相等；文本行视觉行数一致、盒高落在
//   `py + max(min_floor, n×[para_line, fs×1.9])` 带内。带能拦下折行数漂移并暴露真实
//   行距系数差（**实测预览 gpui 自然行距 ≈1.79×fs**，shard 用 para_line = 1.45×fs；
//   fs=14 时 25.10 vs 20.30）——这正是 Phase 2「单一 lyh」要消掉的口径差。
// - **unified（开关 `set_preview_backend(PreviewBackend::Unified)`）**：预览文本行
//   显式 `line_height` = 共享 para_line/heading_line、空行/分隔 = para_line，故
//   `unified_shard_height_matches_preview_row` 断言**同一 lyh 下逐行一致**：
//   Code/标题/空行/分隔精确相等；内容主导的段落/引述 `盒高 − py == shard 总高`。
//   另：`unified_window_range_equals_shard_formula_total` 钉窗口模式量程 ==
//   Σ shard 公式块高，`unified_window_scroll_to_bottom_reaches_last_block` 钉滚到底。
// - 已知系统性差（只留档、不断言相等）：Bullet 的 min_h 兜底、列表/引述 gutter 宽、
//   图片题注模型、行内公式 splice（B 独有）——P3 对齐盒模型后复校。

use crate::config::markdown::{CONTENT_EDGE_PAD_X_PX, PREVIEW_SIDE_GAP_PX};
use crate::markdown::doc::Doc;
use crate::markdown::ir::IrKind;
use crate::markdown::layout::shape_docrow_shard;
use crate::markdown::view::style::StyleParams;
use crate::markdown::wysiwyg::WyTheme;

/// 测试期后端守护：进入时切到给定后端，退出（含 panic）自动还原，防串扰后续测试。
struct BackendGuard(PreviewBackend);
impl BackendGuard {
    fn with(b: PreviewBackend) -> Self {
        let prev = preview_backend();
        set_preview_backend(b);
        Self(prev)
    }
}
impl Drop for BackendGuard {
    fn drop(&mut self) {
        set_preview_backend(self.0);
    }
}

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

/// M5-B5 Phase 2 **unified 后端**对照：单一 lyh 落地后（预览文本行显式
/// `line_height` = 共享 para_line/heading_line、空行/分隔 = para_line），
/// 预览实测行高的「可变部分」与 `shape_docrow_shard` **逐行一致**：
/// - Code：精确相等（元素无内衬，式同）；
/// - Blank / Rule：预览盒高 == shard（两边都 = para_line，无内衬差）；
/// - Heading / 多折行 Paragraph / 单行 Quote：`盒高 − py(垂直内衬) == shard 总高`
///   （每视觉行 == 共享 lh；标题 shard 折进首片的 pad(8) 恰被预览 py 吸收）。
/// - 单行 Paragraph / Bullet：`min_h` 兜底主导（盒高 − lh ≠ 0 的系统内衬差），
///   只做宽松带断言，留 P3 对齐盒模型。
#[gpui::test]
fn unified_shard_height_matches_preview_row(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let src = concat!(
        "### 标题\n",
        "\n",
        "主干长段落，这段中文内容必须足够长，在 288px 真实内容宽下强制折成多个视觉行，",
        "用于对拍共享整形核心与预览行高的逐行一致。再补若干字符稳稳超过一行的宽度。\n",
        "\n",
        "```rs\nlet a = 1;\nlet b = 2;\n```\n",
        "\n",
        "- 列表项\n",
        "\n",
        "> 引述行\n",
        "\n",
        "---\n",
    );
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

    let (failures, notes) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let theme = WyTheme::from_style(&p, window.text_style().font(), p.para_size);
        let md = host.read(cx).md.read(cx);
        // 与预览同一内容宽度口径：宿主侧边 `PREVIEW_SIDE_GAP_PX`×2 + 行内左右边距×2。
        let avail = width - 2.0 * PREVIEW_SIDE_GAP_PX - 2.0 * CONTENT_EDGE_PAD_X_PX;
        let wts = window.text_system().clone();
        let mut failures = Vec::new();
        let mut notes = Vec::new();
        // 垂直内衬 py：从第一个 **内容主导**（≥2 折行、min_h 不遮蔽）的段落测出
        // （unified 下行盒 = py + n_visual×lh）。标题的 shard pad(8) 被预览 py_1 吸收、
        // 空行/分隔无 py，故这些行型不走 py 减法（见各自分支）。
        let mut py: Option<f32> = None;
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
            match dr.kind {
                IrKind::Code | IrKind::Heading(_) | IrKind::Blank | IrKind::Rule => {
                    // 代码：元素无内衬，精确相等；标题：shard pad(8) == 预览 py_1(8) 抵消，
                    // 盒高直接相等；空行/分隔：两边都 = para_line。均无 py 差。
                    if (sum - h_preview).abs() > 0.9 {
                        failures.push(format!(
                            "{tag}: 无内衬差行型应相等 got={sum:.2} preview={h_preview:.2}"
                        ));
                    }
                    notes.push(format!(
                        "{tag}: n_visual={n_visual:.0} shard={sum:.2} preview={h_preview:.2}"
                    ));
                }
                IrKind::Paragraph => {
                    if n_visual >= 2. {
                        // 内容主导：盒高 − py == shard（每视觉行 == 共享 lh）。
                        let ipy = *py.get_or_insert(h_preview - sum);
                        if (h_preview - ipy - sum).abs() > 0.9 {
                            failures.push(format!(
                                "{tag}: 多折行段 height−py={:.2} 应 == shard={sum:.2} (py={ipy:.2})",
                                h_preview - ipy
                            ));
                        }
                    }
                    notes.push(format!(
                        "{tag}: n_visual={n_visual:.0} shard={sum:.2} preview={h_preview:.2}"
                    ));
                }
                IrKind::Quote => {
                    // 单行引述：带内 py_1 与段落同源 → 盒高 − py == shard。
                    let ipy = py.unwrap_or(0.0);
                    if (h_preview - ipy - sum).abs() > 1.0 {
                        failures.push(format!(
                            "{tag}: 引述 height−py={:.2} 应 == shard={sum:.2} (py={ipy:.2})",
                            h_preview - ipy
                        ));
                    }
                    notes.push(format!(
                        "{tag}: n_visual={n_visual:.0} shard={sum:.2} preview={h_preview:.2}"
                    ));
                }
                _ => {
                    // Bullet 等 min_h 兜底主导，留档（P3 对齐盒模型）。
                    notes.push(format!("{tag}: 留档 shard={sum:.2} preview={h_preview:.2}"));
                }
            }
        }
        (failures, notes)
    });
    for note in &notes {
        eprintln!("{note}");
    }
    assert!(failures.is_empty(), "unified 对照失败：\n{}", failures.join("\n"));
}

/// M5-B5 Phase 2 **unified 量程**：窗口模式（> `WINDOWED_MIN_BLOCKS`）的滚动总量程 ==
/// Σ 共享 shard 公式块高（`unified_index_block_height`；块数 < 抽样阈值 → stride=1
/// 逐块精确，确定性）。legacy 下该口径就是旧抽样公式（默认行为不动）。
#[gpui::test]
fn unified_window_range_equals_shard_formula_total(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 2400 组「标题 + 空行」= 4800 块 > WINDOWED_MIN_BLOCKS(4000) → 窗口模式。
    let mut src = String::with_capacity(512_000);
    for i in 0..2_400 {
        src.push_str(&format!("## 标题 {i}\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);

    let (total, expect, n) = cx.update(|_window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let md = host.read(cx).md.read(cx);
        let idx = md.index.as_ref().expect("大文档应走窗口模式");
        let n = idx.blocks.len();
        assert!(n > 4000, "应进入窗口模式（n={n}）");
        let avail = (width - 2.0 * CONTENT_EDGE_PAD_X_PX).max(40.0);
        let total = md.vertical_total_px();
        let expect = (0..n)
            .map(|i| unified_index_block_height(&idx.blocks.at(i), &p, avail))
            .sum::<f32>();
        (total, expect, n)
    });
    assert!(
        (total - expect).abs() <= 1.0,
        "unified 量程应 == Σ shard 公式块高：total={total:.1} expect={expect:.1} (n={n})"
    );
}

/// M5-B5 Phase 2 **unified 滚到底护栏**（R1）：窗口模式 + unified（量程 = shard 口径）
/// 下，`scroll_to_px` 滚到最大量程不被旧估算的短量程夹住，绝对块定位可达文档末块。
#[gpui::test]
fn unified_window_scroll_to_bottom_reaches_last_block(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let mut src = String::with_capacity(512_000);
    for i in 0..2_400 {
        src.push_str(&format!("## 标题 {i}\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);

    let n = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.read(cx).item_count()
    });
    assert!(n > 4000, "应进入窗口模式（n={n}）");
    // 滚到最大量程（1e9 被 clamp 到总量−视口）→ 绝对块应落在末块附近。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| md.scroll_to_px(1e9));
    });
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    let (abs, total) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.test_abs_scroll_top(), m.vertical_total_px())
    });
    assert!(
        abs.0 >= n.saturating_sub(48),
        "滚到底应达末块（abs_block={} n={n}，total={total:.0}）——量程被夹住？",
        abs.0
    );
}

