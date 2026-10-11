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
//   另：M5-B5 P3 前置「量程精确化」把量程从公式族升级为**真实 DocRow shard**：
//   - `unified_window_range_equals_docrow_shard_total`：窗口模式（≤ `HEIGHT_EXACT_SHAPE_MAX`
//     块）量程 == Σ `shape_docrow_shard(真实 DocRow)`（R1，不再 `est_wrap_len` 估算）；
//   - `unified_sampled_model_matches_sampled_docrow_shard`：超大文档抽样档的量程 ==
//     「抽中块真实 shard × 组内块数」的自洽值，并报告对全量精确的相对误差；
//   - `unified_small_doc_range_equals_docrow_shard_total`：小文档统一提示 == 逐行精确；
//   - `unified_window_scroll_to_bottom_reaches_last_block`：滚到底达末块且末块内容可达。
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
                .position(|r| r.docrow.kind == IrKind::Paragraph)
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

/// M5-B5 P3 前置 **unified 量程精确化（R1）**：窗口模式（> `WINDOWED_MIN_BLOCKS`、且
/// ≤ `HEIGHT_EXACT_SHAPE_MAX` 块 = 全量精确档）的滚动总量程 **== Σ shape_docrow_shard
/// over 每个真实 DocRow**（不再 `unified_index_block_height` 的 `est_wrap_len` 估算）。
/// 参考值用独立路径（`Doc::open` + `rows_in_block_window`）直接 shape 每块，与模型完全
/// 解耦——模型增量内容为真（渲染上下文与参考同宽同主题）。
#[gpui::test]
fn unified_window_range_equals_docrow_shard_total(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 2400 组「标题 + 空行」= 4800 块 > WINDOWED_MIN_BLOCKS(4000) 且 < 8192 → 全量精确档。
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

    let (total, expect, n) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let md = host.read(cx).md.read(cx);
        let n = md.index.as_ref().expect("大文档应走窗口模式").blocks.len();
        assert!(n > 4000, "应进入窗口模式（n={n}）");
        assert!(
            n <= HEIGHT_EXACT_SHAPE_MAX,
            "该用例块数应落在全量精确档（n={n}）"
        );
        let avail = (width - 2.0 * CONTENT_EDGE_PAD_X_PX).max(40.0);
        let total = md.vertical_total_px();
        // 独立参考：真实 DocRow → shape，与模型解耦。
        let ts = window.text_system().clone();
        let style = window.text_style();
        let fs = style.font_size.to_pixels(window.rem_size());
        let theme = WyTheme::from_style(&p, style.font(), fs);
        let doc = Doc::open(src.into());
        let rows = doc.rows_in_block_window(0, n - 1);
        let expect: f32 = rows
            .iter()
            .map(|dr| {
                shape_docrow_shard(&ts, dr, &theme, avail, width)
                    .iter()
                    .map(|s| s.h)
                    .sum::<f32>()
            })
            .sum();
        (total, expect, n)
    });
    assert!(
        (total - expect).abs() <= 1.0,
        "unified 量程应 == Σ 真实 DocRow shard 总高：total={total:.1} expect={expect:.1} (n={n})"
    );
}

/// M5-B5 P3 前置 **unified 抽样档自洽**：超大文档（> `HEIGHT_EXACT_SHAPE_MAX`）走
/// 抽样窗口——`block_h[g]` 取**抽中块的真实 DocRow shard 高**、组内其余块沿用。
/// 断言模型量程 == 「抽中块真实 shard × 组内块数」自洽值（抽样在采样点精确、无
/// `est_wrap_len` 估算误差），并打印对「全量精确」的相对误差供取舍留档。
#[gpui::test]
fn unified_sampled_model_matches_sampled_docrow_shard(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 9000 组「标题 + 空行」= 18000 块 > 8192 → 抽样档（stride = 18000/4096 ≈ 4）。
    let mut src = String::with_capacity(1_000_000);
    for i in 0..9_000 {
        src.push_str(&format!("## 标题 {i}\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);

    let (total, sampled_total, exact_total, n, stride) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let md = host.read(cx).md.read(cx);
        let idx = md.index.as_ref().expect("大文档应走窗口模式");
        let n = idx.blocks.len();
        let stride = md.height_stride;
        assert!(n > HEIGHT_EXACT_SHAPE_MAX && stride > 1, "应走抽样档（n={n}, stride={stride}）");
        let avail = (width - 2.0 * CONTENT_EDGE_PAD_X_PX).max(40.0);
        let ts = window.text_system().clone();
        let style = window.text_style();
        let fs = style.font_size.to_pixels(window.rem_size());
        let theme = WyTheme::from_style(&p, style.font(), fs);
        let doc = Doc::open(src.into());
        let rows = doc.rows_in_block_window(0, n - 1);
        let block_h = |k: usize| {
            shape_docrow_shard(&ts, &rows[k], &theme, avail, width)
                .iter()
                .map(|s| s.h)
                .sum::<f32>()
        };
        // 与 `rebuild_height_model` 抽样同式（首块精确 × 组内块数）。
        let mut si = 0usize;
        let mut sampled_total = 0.0f32;
        while si < n {
            let group_len = stride.min(n - si);
            sampled_total += block_h(si) * group_len as f32;
            si += stride;
        }
        let exact_total: f32 = rows.iter().map(|dr| shape_docrow_shard(&ts, dr, &theme, avail, width).iter().map(|s| s.h).sum::<f32>()).sum();
        (md.vertical_total_px(), sampled_total, exact_total, n, stride)
    });
    let rel = (exact_total - sampled_total).abs() / exact_total.max(1.0) * 100.0;
    eprintln!(
        "[guard] 抽样档（n={n}, stride={stride}）量程 {total:.0} == 抽样自洽 {sampled_total:.0}；对全量精确 {exact_total:.0} 的相对误差 {rel:.2}%（组首×组数、组内异构误差，与 legacy 抽样同构）"
    );
    assert!(
        (total - sampled_total).abs() <= 1.0,
        "unified 抽样档量程应 == 抽样点真实 shard 自洽值：total={total:.1} sampled={sampled_total:.1} (n={n})"
    );
    // 抽样采点精确 ⇒ 误差只来自「组首×组数」对周期复用块组的欠/过估（本用例为「标题+空行」
    // 周期，stride=4 组首全为标题 → 系统性 +20% 量级）。这正是 `rebuild_height_model` 既有
    // 注释的「确定性偏移抽样对高度周期复用块有偏差」；全量精确档（≤8192 块）不受影响。
    assert!(rel < 50.0, "抽样档相对误差应 <50%（实测 {rel:.2}%）");
}

/// M5-B5 P3 前置：`docrow_for_height`（预览 Row→DocRow 适配）与真实
/// `Doc::rows_in_block_window` DocRow 的 **shard 高度逐行一致**——钉死「适配器高度 ==
/// 真实 DocRow」这一前提（视图高度模型不持完整 Doc 的合法性依据）。
#[gpui::test]
fn docrow_for_height_matches_doc_docrow_shard(cx: &mut TestAppContext) {
    use crate::markdown::layout::docrow_for_height;
    use crate::markdown::parse::parse_to_rows;
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
    let failures = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let ts = window.text_system().clone();
        let style = window.text_style();
        let fs = style.font_size.to_pixels(window.rem_size());
        let theme = WyTheme::from_style(&p, style.font(), fs);
        let avail = width - 2.0 * PREVIEW_SIDE_GAP_PX - 2.0 * CONTENT_EDGE_PAD_X_PX;
        let doc = Doc::open(src.into());
        let n = doc.block_count();
        let docrows = doc.rows_in_block_window(0, n - 1);
        assert_eq!(docrows.len(), rows.len(), "DocRows 与预览行数应对齐");
        let mut failures = Vec::new();
        for (ix, (r, dr)) in rows.iter().zip(docrows.iter()).enumerate() {
            let via_adapter: f32 = shape_docrow_shard(&ts, &docrow_for_height(r), &theme, avail, width)
                .iter().map(|s| s.h).sum();
            let via_doc: f32 = shape_docrow_shard(&ts, dr, &theme, avail, width)
                .iter().map(|s| s.h).sum();
            if (via_adapter - via_doc).abs() > 0.05 {
                failures.push(format!("row[{ix}] kind={:?} adapter={via_adapter:.2} doc={via_doc:.2}", dr.kind));
            }
        }
        failures
    });
    assert!(failures.is_empty(), "docrow_for_height 与真实 DocRow 高度不一致：\n{}", failures.join("\n"));
}

/// M5-B5 P3-1a：预览渲染行源切共享 `DocRow` 后，预览侧装配的 `DocRow` 与真实
/// `Doc::rows_in_block_window` **逐字段一致**（kind/src_line/lines/text/runs/byte_map/
/// payload/table_rows）——钉死「`DocRow.byte_map` 正常提供、字节反射语义保真」的前提，
/// 且 Str 小文档全量路径与 Rope 大文档窗口路径**双源**都被锁住（渲染/命中/选择不变，
/// 只是行数据源换成共享 Doc 行型）。
#[gpui::test]
fn preview_docrows_match_doc_rows_in_block_window(cx: &mut TestAppContext) {
    use crate::config::limits::WINDOWED_MIN_BLOCKS;
    use crate::markdown::doc::Doc;
    use crate::markdown::parse::scan_blocks_rope;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    // —— ① 小文档全量路径（Str 源）：混合行型；预览 `rows` 的 DocRow == Doc 全量窗口。
    let src = "# 标题 **加粗** 与 `代码`\n正文第二段，含 *斜体* 与 ~~删除~~。\n\n- 列表项\n  - 嵌套\n1. 有序\n> 引述\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 列A | 列B |\n| :- | -: |\n| 1 | 2 |\n\n![图](a.png)\nשלום עולם";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    cx.simulate_resize(size(px(700.), px(500.)));
    settle(cx);
    let doc = Doc::open(src.into());
    let n = doc.block_count();
    let expect_ok = doc.rows_in_block_window(0, n - 1);
    let got = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        assert!(md.index.is_none(), "小文档应走全量路径");
        md.rows.iter().map(|r| r.docrow.clone()).collect::<Vec<_>>()
    });
    assert_eq!(got.len(), expect_ok.len(), "预览行数应与 Doc 块数对齐（全量路径）");
    for (i, (g, e)) in got.iter().zip(expect_ok.iter()).enumerate() {
        assert_eq!(g, e, "全量路径 row[{i}] DocRow 不一致（kind/text/byte_map/payload 应逐位相同）");
    }

    // —— ② 大文档窗口路径（Rope 源）：当前窗口 `win_rows` 的 DocRow == Doc 同窗口。
    let mut big = String::new();
    for i in 0..WINDOWED_MIN_BLOCKS + 600 {
        big.push_str(&format!("# 标题 {i}\n\n正文段落 {i} **粗** `码`\n\n- 项 {i}\n\n```rust\nlet x = {i};\n```\n\n"));
    }
    let rope = ropey::Rope::from(big.as_str());
    let index = std::rc::Rc::new(scan_blocks_rope(&rope));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::Rope(rope.clone()), index, cx);
        });
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);
    let (win_lo, got) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        assert!(md.index.is_some(), "大文档应走窗口模式");
        (
            md.win_lo,
            md.win_rows.iter().map(|r| r.docrow.clone()).collect::<Vec<_>>(),
        )
    });
    let doc = Doc::open_rope(rope);
    let hi = win_lo.saturating_add(got.len()).saturating_sub(1);
    let expect_win = doc.rows_in_block_window(win_lo, hi);
    assert_eq!(got.len(), expect_win.len(), "窗口行数应与 Doc 块数一致（窗口路径）");
    for (i, (g, e)) in got.iter().zip(expect_win.iter()).enumerate() {
        assert_eq!(
            g,
            e,
            "窗口路径 row[{win_lo}+{i}] DocRow 不一致（byte_map 应逐位相同）"
        );
    }

    // —— ③ Rope 窗口路径滚到底：末窗口含**末块**，验证 `line_count()`（ropey
    // `len_lines(LF)` == `split('\n')`）与 Doc 末块 `lines` 上界逐位一致。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| md.scroll_to_px(1e9));
    });
    cx.update(|window, cx| {
        for _ in 0..6 {
            let _ = window.draw(cx);
        }
    });
    cx.run_until_parked();
    let (win_lo, got) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (
            md.win_lo,
            md.win_rows.iter().map(|r| r.docrow.clone()).collect::<Vec<_>>(),
        )
    });
    let hi = win_lo.saturating_add(got.len()).saturating_sub(1);
    let expect_tail = doc.rows_in_block_window(win_lo, hi);
    assert!(hi + 1 == doc.block_count(), "末窗口应收束到末块（hi={hi}）");
    for (i, (g, e)) in got.iter().zip(expect_tail.iter()).enumerate() {
        assert_eq!(
            g,
            e,
            "末窗口 row[{win_lo}+{i}] DocRow 不一致（末块 lines 上界应一致）"
        );
    }
}

/// M5-B5 Phase 2/3 **unified 小文档量程精确化**：统一高度提示（`est_hint`，未偏置）seed
/// 出的列表量程 `n × est_hint` == Σ 逐行真实 DocRow shard（不再 `est_wrap_lines` 估算）；
/// 且小文档滚到底可达末行（ListState 实测路径，防夹量程）。
#[gpui::test]
fn unified_small_doc_range_equals_docrow_shard_total(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let mut src = String::new();
    for i in 0..80 {
        src.push_str(&format!("这是一段足够长、能在 288px 内容宽下折成多行的中文段落文本，标号 {i}。\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);

    let (hint_total, expect, n) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let md = host.read(cx).md.read(cx);
        let n = md.rows.len();
        assert!(md.index.is_none(), "小文档应走全量路径（n={n}）");
        let avail = (width - 2.0 * CONTENT_EDGE_PAD_X_PX).max(40.0);
        let ts = window.text_system().clone();
        let style = window.text_style();
        let fs = style.font_size.to_pixels(window.rem_size());
        let theme = WyTheme::from_style(&p, style.font(), fs);
        let doc = Doc::open(src.into());
        let rows = doc.rows_in_block_window(0, n - 1);
        let expect: f32 = rows
            .iter()
            .map(|dr| shape_docrow_shard(&ts, dr, &theme, avail, width).iter().map(|s| s.h).sum::<f32>())
            .sum();
        (md.est_hint * n as f32, expect, n)
    });
    assert!(
        (hint_total - expect).abs() <= 2.0,
        "unified 小文档量程（n×est_hint）应 == Σ 逐行真实 shard：total={hint_total:.1} expect={expect:.1} (n={n})"
    );
    // 统一高度提示确已被逐行精确值 seed（`update_height_hint` 计量程的前提）。
    let seeded = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        m.hint_applied.is_some_and(|(n2, h)| n2 == n && (h - m.est_hint).abs() < 0.01)
    });
    assert!(seeded, "小文档统一提示应按逐行精确平均高 seed（n={n}）");
}

/// M5-B5 Phase 2/3 **unified 滚到底护栏（R1）**：窗口模式 + unified（量程 = 真实 DocRow
/// shard 总高）下，`scroll_to_px` 滚到最大量程不被旧估算的短量程夹住——绝对块定位可达
/// 文档末块（尺寸对齐），且首屏/末屏**都被窗口物化、内容可达**（`rendered_text` 含首/末
/// 标题文本，防「滚得到块号却渲染空白」的量程/渲染脱节）。
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
    // 首屏：未滚动时第一块内容已被窗口物化（首屏不缺内容）。
    let first_text = cx.update(|_, cx| host.read(cx).md.read(cx).rendered_text());
    assert!(
        first_text.contains("标题 0"),
        "首屏应含第一块内容（缺内容？rendered={first_text:?}）"
    );
    // 滚到最大量程（1e9 被 clamp 到总量−视口）→ 绝对块应落在末块附近，且末块内容可达。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| md.scroll_to_px(1e9));
    });
    // `scroll_to_px` 同步推进 win_base + ListState 滚动；窗口物化（ensure_window）在
    // prepare_render 里做，需真实重绘帧才会跑——强制 `window.draw` 数次让窗口重建到末块。
    cx.update(|window, cx| {
        for _ in 0..6 {
            let _ = window.draw(cx);
        }
    });
    cx.run_until_parked();
    let (abs, total, text) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.test_abs_scroll_top(), m.vertical_total_px(), m.rendered_text())
    });
    assert!(
        abs.0 >= n.saturating_sub(48),
        "滚到底应达末块（abs_block={} n={n}，total={total:.0}）——量程被夹住？",
        abs.0
    );
    assert!(
        text.contains("标题 2399"),
        "滚到底后末块内容应被窗口物化可达（缺内容？rendered 尾部={:?}）",
        &text[text.len().saturating_sub(80)..]
    );
}

