// B5 蓝图对照测试：`shape_docrow_shard` 高度累加 vs 预览实测行高。
//
// 本文件经 `view/tests.rs` `include!` 进 `view::tests`（与之同模块的 `viewport.rs` 已
// `use super::*` + `use gpui::{…, px, size, …}`，故 `MarkdownPreview`/`PreviewHost`/
// `settle`/`parse_to_rows`/`RowKind`/`px`/`size` 直接可用，不重复 import 以免 E0252）。
//
// 断言口径（与 `layout` 模块文档一致；**M5-B5 P3-1b-2 起默认翻转 unified、单一 shard
// 真源，legacy 元素树/抽样量程已删除**）：
// - `shared_shard_height_matches_preview_row`（默认 unified）：预览文本/代码/空行/
//   分隔线实测行高 == Σ `shape_docrow_shard`（render_text_shard_rows 显式 `h = Σ shard.h`，
//   逐行 tight 相等；R2 排布与量程同真源）。
// - `unified_shard_height_matches_preview_row`：Code/标题/空行/分隔精确相等；
//   内容主导的段落/引述 `盒高 − py == shard 总高`。
//   另：量程精确化（R1，不再 `est_wrap_len` 估算）：
//   - `unified_window_range_equals_docrow_shard_total`：窗口模式（≤ `HEIGHT_EXACT_SHAPE_MAX`
//     块）量程 == Σ `shape_docrow_shard(真实 DocRow)`（R1，不再 `est_wrap_len` 估算）；
//   - `unified_sampled_model_matches_sampled_docrow_shard`：超大文档抽样档的量程 ==
//     「抽中块真实 shard × 组内块数」的自洽值，并报告对全量精确的相对误差；
//   - `unified_small_doc_range_equals_docrow_shard_total`：小文档统一提示 == 逐行精确；
//   - `unified_window_scroll_to_bottom_reaches_last_block`：滚到底达末块且末块内容可达。
// - 已知系统性差（只留档、不断言相等）：列表/引述 gutter 宽、图片题注模型、行内公式
//   splice（B 独有）——P3 对齐盒模型后复校。

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
/// 主对照测试：`shared_shard_height_matches_preview_row`。
/// P3-1b-2（默认翻转 unified）后断言 **逐行 `预览实测行高 == Σ shape_docrow_shard`**
/// （文本/代码/空行/分隔线均由 `render_text_shard_rows`/`render_code_shard_rows` 以
/// 显式 `h = Σ shard.h` 渲染 → tight 相等；legacy 自然行距带断言已删）。
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
    // 追加收敛帧：unified 渲染的条目测量在换宽后多一帧落定（shard 固定高重测），
    // 测试读取 `bounds_for_item` 前需等收敛（与其余 unified 对照测试同款）。
    cx.update(|window, cx| {
        for _ in 0..4 {
            let _ = window.draw(cx);
        }
    });
    cx.run_until_parked();

    let doc = Doc::open(src.into());
    let n = doc.block_count();
    let docrows = doc.rows_in_block_window(0, n.saturating_sub(1));
    assert_eq!(docrows.len(), rows.len(), "DocRows 与预览行数应对齐");

    let (failures, notes): (Vec<String>, Vec<String>) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let theme = WyTheme::from_style(&p, window.text_style().font(), p.para_size);
        let md = host.read(cx).md.read(cx);
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
            if (sum - h_preview).abs() > 1.0 {
                failures.push(format!(
                    "{tag}: 预览实测行高应 == shard 总高 got={sum:.2} preview={h_preview:.2} (n_visual={n_visual:.0})"
                ));
            }
            notes.push(format!("{tag}: n_visual={n_visual:.0} shard={sum:.2} preview={h_preview:.2}"));
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
    // 追加收敛帧：unified 渲染的条目测量在换宽后多一帧落定（shard 固定高重测）。
    cx.update(|window, cx| {
        for _ in 0..4 {
            let _ = window.draw(cx);
        }
    });
    cx.run_until_parked();

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
        // 与统一高度模型同一口径：内容可用宽 = `preview_text_avail`（渲染同源，R2）；
        // 正文整形字号 = `p.para_size`（预览实际渲染字号，非窗口默认 16）。
        let avail = super::preview_text_avail(&p, width);
        let total = md.vertical_total_px();
        // 独立参考：真实 DocRow → shape，与模型解耦。
        let ts = window.text_system().clone();
        let style = window.text_style();
        let theme = WyTheme::from_style(&p, style.font(), p.para_size);
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
        let avail = super::preview_text_avail(&p, width);
        let ts = window.text_system().clone();
        let style = window.text_style();
        let theme = WyTheme::from_style(&p, style.font(), p.para_size);
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
        let theme = WyTheme::from_style(&p, style.font(), p.para_size);
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
    // 换宽后平均行高重算有 120ms 节流（`HINT_REAPPLY_INTERVAL_MS`），测试瞬间跑完、
    // 第一次估算落在「换宽前」窄文档宽度上 → 强制一次确定性重算，让 `est_hint` 落在
    // 最终宽度（与下面的参考同口径），量程护栏与节流时序解耦。
    // P3-1b-2 删 `invalidate_height_hint` 助手后直接失效标志位（同模块私有字段可达）。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |m, _| m.est_hint_width = -1.0);
    });
    cx.update(|window, cx| {
        for _ in 0..3 {
            let _ = window.draw(cx);
        }
    });
    cx.run_until_parked();

    let (hint_total, expect, n) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let md = host.read(cx).md.read(cx);
        let n = md.rows.len();
        assert!(md.index.is_none(), "小文档应走全量路径（n={n}）");
        // 与统一高度模型同一口径（渲染同源，R2）：avail = `preview_text_avail`、字号 = para_size。
        let avail = super::preview_text_avail(&p, width);
        let ts = window.text_system().clone();
        let style = window.text_style();
        let theme = WyTheme::from_style(&p, style.font(), p.para_size);
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

/// M5-B5 P3-1b-1：**unified 渲染几何 == shard 几何**（渲染与量程同真源，R2）。
/// 逐行断言 `bounds_for_item` 高 == Σ `shape_docrow_shard`（同一 `preview_text_avail`）；
/// 另用静态选择器断言代码块首/末空白带跨度 == 该行 shard 总高（`md-code-blank-top` →
/// `md-code-blank-bottom`，唯一的代码块）。
#[gpui::test]
fn unified_render_bounds_equal_shard_geometry(cx: &mut TestAppContext) {
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
        "用于对拍共享整形核心与预览渲染的逐行一致。再补若干字符稳稳超过一行的宽度。\n",
        "\n",
        "```rs\nlet a = 1;\nlet b = 2;\n```\n",
        "\n",
        "> 引述行\n",
        "\n",
        "---\n",
    );
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);
    // 追加收敛帧：unified 渲染的条目测量在换宽后多一帧才落定（shard 固定高重测），
    // 测试读取 `bounds_for_item` 前需等收敛。
    cx.update(|window, cx| {
        for _ in 0..4 {
            let _ = window.draw(cx);
        }
    });
    cx.run_until_parked();

    let (failures, code_sum) = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let theme = WyTheme::from_style(&p, window.text_style().font(), p.para_size);
        let md = host.read(cx).md.read(cx);
        let avail = super::preview_text_avail(&p, width);
        let ts = window.text_system().clone();
        let doc = Doc::open(src.into());
        let n = doc.block_count();
        let docrows = doc.rows_in_block_window(0, n - 1);
        let mut failures = Vec::new();
        let mut code_sum = 0.0f32;
        for (ix, dr) in docrows.iter().enumerate() {
            let b = md.list_state.bounds_for_item(ix);
            let h_preview = b.map(|b| b.size.height.as_f32()).unwrap_or(0.);
            let w_preview = b.map(|b| b.size.width.as_f32()).unwrap_or(0.);
            let shards = shape_docrow_shard(&ts, dr, &theme, avail, width);
            let sum: f32 = shards.iter().map(|s| s.h).sum();
            let tag = format!("row[{ix}] kind={:?}", dr.kind);
            if (h_preview - sum).abs() > 1.0 {
                failures.push(format!("{tag}: 渲染高 ≠ shard 总高 got={h_preview:.2} shard={sum:.2}"));
            }
            if matches!(dr.kind, IrKind::Code) {
                code_sum = sum;
            }
            eprintln!("{tag}: h_preview={h_preview:.2} w_preview={w_preview:.2} shard={sum:.2}");
        }
        (failures, code_sum)
    });
    // 代码块（唯一）：整块跨度（顶空白带顶 → 底空白带底）== shard 总高
    //（= 首空白 + 2 代码行 + 末空白，共 4×code_line）。
    if code_sum > 0.0 {
        let top = cx
            .debug_bounds("md-code-blank-top")
            .expect("代码块顶部空白带应存在");
        let bottom = cx
            .debug_bounds("md-code-blank-bottom")
            .expect("代码块底部空白带应存在");
        let span =
            (bottom.origin.y.as_f32() + bottom.size.height.as_f32()) - top.origin.y.as_f32();
        assert!(
            (span - code_sum).abs() < 1.5,
            "代码块整块跨度应 == shard 总高 span={span:.1} shard={code_sum:.1}"
        );
    }
    assert!(
        failures.is_empty(),
        "unified 渲染几何 ≠ shard 几何：\n{}",
        failures.join("\n")
    );
}

/// M5-B5 P3-1b-1：unified 文本命中分片与 shard 折行一致。多折行段落整段是一条
/// `WrappedLine`（1 个 shard、1 条 `TextHit`）：其 layout 行高 == `shard.lh`、总高 ==
/// `shard.h`、行数 == `shard.h / lh`；且分片命中文本拼回 == `docrow.text`（`byte_offset`
/// 抬到整行不丢/不重字节——「命中区域」与 shard 几何一致）。
#[gpui::test]
fn unified_text_hits_match_shard_wrapping(cx: &mut TestAppContext) {
    let _g = BackendGuard::with(PreviewBackend::Unified);
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 两段长中文强制多折行；同宽下 shard 与渲染应同折行点。
    let src = "第一段足够长，在 288px 真实内容宽下折成三个视觉行的中文文本。再来一些内容稳稳超出宽度折到第二行第三行去。\n\n第二段同样长，确保多段逐段可达。这是一段中文测试文字，必须稳稳超过一行的可用宽度。\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    let width = 400f32;
    cx.simulate_resize(size(px(width), px(600.)));
    settle(cx);

    let failures = cx.update(|window, cx| {
        let p = StyleParams::from_theme_with_font(cx, None);
        let theme = WyTheme::from_style(&p, window.text_style().font(), p.para_size);
        let md = host.read(cx).md.read(cx);
        let avail = super::preview_text_avail(&p, width);
        let ts = window.text_system().clone();
        let doc = Doc::open(src.into());
        let docrows = doc.rows_in_block_window(0, doc.block_count() - 1);
        let mut failures = Vec::new();
        let hits = md.text_ctx.hits.borrow();
        for (ix, dr) in docrows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.kind, IrKind::Paragraph))
        {
            let shards = shape_docrow_shard(&ts, dr, &theme, avail, width);
            let sum: f32 = shards.iter().map(|s| s.h).sum();
            let n_visual: usize = shards.iter().map(|s| (s.h / s.lh).round() as usize).sum();
            let row_hits: Vec<_> = hits.iter().filter(|h| h.row_ix == ix).collect();
            let tag = format!("row[{ix}]");
            if row_hits.is_empty() {
                failures.push(format!("{tag}: 应有一条分片文本命中"));
                continue;
            }
            // 分片命中拼回整行文本 == docrow.text（字节可达，不丢不重）。
            let joined: String = {
                let mut sorted = row_hits.to_vec();
                sorted.sort_by_key(|h| h.byte_offset);
                sorted.iter().map(|h| h.text.as_ref()).collect()
            };
            if joined != dr.text.as_ref() {
                failures.push(format!("{tag}: 分片命中文本拼回 ≠ docrow.text"));
            }
            for h in &row_hits {
                let line_h = h.layout.line_height().as_f32();
                let n = h
                    .layout
                    .line_layouts()
                    .first()
                    .map(|l| l.wrap_boundaries().len() + 1)
                    .unwrap_or(0);
                let hit_w = h
                    .layout
                    .line_layouts()
                    .first()
                    .map(|l| l.width().as_f32())
                    .unwrap_or(0.);
                let hit_h = h
                    .layout
                    .line_layouts()
                    .first()
                    .map(|l| l.size(h.layout.line_height()).height.as_f32())
                    .unwrap_or(0.);
                eprintln!("{tag}: wrap = {hit_w:.1}px, n={n}, line_h={line_h:.2}, hit_h={hit_h:.2}");
                // 行高按设备像素取整（scale 2.0 下 20.3→20.5）：命中 layout 行高可能被
                // `pixel_snap` 抬半像素，与 shard 理想行高差 ≤ 0.5 为同一设备栅格。
                if (line_h - shards[0].lh).abs() > 0.5 {
                    failures.push(format!(
                        "{tag}: 命中行高 {line_h:.2} ≠ shard.lh {:.2}",
                        shards[0].lh
                    ));
                }
                if (hit_h - sum).abs() > 1.0 {
                    failures.push(format!("{tag}: 命中总高 {hit_h:.2} ≠ shard 总高 {sum:.2}"));
                }
                if n != n_visual {
                    failures.push(format!("{tag}: 命中折行数 {n} ≠ shard 折行数 {n_visual}"));
                }
            }
            eprintln!("{tag}: lh={:.2} n_visual={n_visual} hit_h={sum:.2}", shards[0].lh);
        }
        failures
    });
    assert!(
        failures.is_empty(),
        "unified 命中折行 ≠ shard：\n{}",
        failures.join("\n")
    );
}

