//! layout 模块测试：
//! 1. **单元健壮性**：空输入 / 单行 / 极窄宽 / 多折行边界不 panic、每片高 > 0、宽有界。
//! 2. **与 WYSIWYG `shape_docrow` 逐块对拍**：`Σ shard.h == frame.block_height(k)`
//!    （真实窗口 + `build_frame`），覆盖 纯文本 / 标题 / 折行长行 / 代码 / 表格 / 列表 / 引述。

use gpui::{App, AppContext, Pixels, SharedString, TestAppContext, Window, WindowTextSystem, px};

use crate::config::markdown::CONTENT_EDGE_PAD_X_PX;
use crate::markdown::doc::{Doc, DocPayload, DocRow};
use crate::markdown::ir::IrKind;
use crate::markdown::layout::{Shard, shape_docrow_shard, shape_docrow_shard_with_image};
use crate::markdown::parse::TableAlign;
use crate::markdown::view::style::StyleParams;
use crate::markdown::wysiwyg::{Frame, WyEditor, WyTheme, build_frame};

/// 编辑宿主（复用 wysiwyg 测试惯例：封装 `WyEditor` 为根元素）。
struct WyHost {
    ed: gpui::Entity<WyEditor>,
}
impl gpui::Render for WyHost {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        self.ed.clone()
    }
}

/// 直接构造一个文本 `DocRow`（跳过解析，聚焦几何路径）。
fn text_row(kind: IrKind, text: &str) -> DocRow {
    let n = text.len();
    DocRow {
        kind,
        src_line: 0,
        lines: 0..1,
        text: text.into(),
        runs: vec![],
        byte_map: (0..=n).collect(),
        payload: DocPayload::Text,
        table_rows: None,
    }
}

/// 默认排版主题（body = 系统 UI 字体；对拍路径用真实窗口主题，见 [`build_frame_for`]）。
fn theme_from_font(cx: &mut App, font: gpui::Font, fs: f32) -> WyTheme {
    let p = StyleParams::from_theme_with_font(cx, Some(fs));
    WyTheme::from_style(&p, font, px(fs))
}

/// 断言：每片高>0、宽度非负、折行序号连续、byte_map 段半开有序、宽累加有界。
fn assert_shard_sane(shards: &[Shard], note: &str) {
    assert!(!shards.is_empty(), "{note}: 应至少有一片");
    let mut total_w = 0f32;
    for (i, s) in shards.iter().enumerate() {
        assert_eq!(s.line_ix, i, "{note}: line_ix 应连续 0 基");
        assert!(s.h > 0., "{note}: 每片高应 > 0，第 {i} 片 h={}", s.h);
        assert!(s.w >= 0. && s.w.is_finite(), "{note}: 宽应非负有界");
        assert!(s.lh > 0., "{note}: 行高系数应 > 0");
        assert!(s.byte_map.start <= s.byte_map.end, "{note}: byte_map 段应半开有序");
        total_w += s.w;
    }
    assert!(total_w.is_finite(), "{note}: 宽累加应有界");
}

#[gpui::test]
fn empty_text_shard_is_single_line_no_panic(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    let shards = shape_docrow_shard(&ts, &text_row(IrKind::Paragraph, ""), &theme, 400., 400.);
    assert_shard_sane(&shards, "空行");
    assert_eq!(shards[0].text.as_ref(), "", "空行片应无文本");
}

#[gpui::test]
fn single_line_paragraph_one_shard_no_panic(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    let shards =
        shape_docrow_shard(&ts, &text_row(IrKind::Paragraph, "hello world"), &theme, 400., 400.);
    assert_shard_sane(&shards, "单行");
}

#[gpui::test]
fn very_narrow_width_no_panic_bounded(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    let long = "the quick brown fox jumps over the lazy dog ".repeat(10);
    for &avail in &[1f32, 5., 20., 60.] {
        let shards =
            shape_docrow_shard(&ts, &text_row(IrKind::Paragraph, &long), &theme, avail, avail);
        assert_shard_sane(&shards, &format!("极窄宽 {avail}"));
        assert!(
            shards.iter().all(|s| s.w <= avail + 1.),
            "窄宽下每片宽不应显著越界：avail={avail} 实际 {:?}",
            shards.iter().map(|s| s.w).collect::<Vec<_>>()
        );
    }
}

#[gpui::test]
fn multi_fold_line_boundary_no_panic(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    // 经典折行点密集文本（空格/连字符换行点）。
    let text = "alpha-beta-gamma delta epsilon zeta-eta theta iota kappa lambda mu";
    for &avail in &[80f32, 120., 200.] {
        let shards = shape_docrow_shard(&ts, &text_row(IrKind::Paragraph, text), &theme, avail, avail);
        assert_shard_sane(&shards, &format!("多折行 {avail}"));
        // B 语义：每片高 = 行高系数 × 该片视觉子行数（折行段数+1）。
        for s in &shards {
            assert!((s.h / s.lh).round() >= 1., "每片至少一个视觉子行");
        }
    }
}

#[gpui::test]
fn heading_code_table_all_produce_sane_shards(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    let heading = text_row(IrKind::Heading(1), "标题行");
    let h = shape_docrow_shard(&ts, &heading, &theme, 400., 400.);
    assert_shard_sane(&h, "标题");
    assert!(
        h.iter().map(|s| s.h).sum::<f32>() > f32::from(theme.heading_line(1)),
        "标题总高应 ≥ 标题行高"
    );
    let code = DocRow {
        kind: IrKind::Code,
        src_line: 0,
        lines: 0..3,
        text: SharedString::default(),
        runs: vec![],
        byte_map: (0..10).collect(),
        payload: DocPayload::Code {
            code_lines: vec!["a".into(), "bb".into()],
            language: None,
        },
        table_rows: None,
    };
    let c = shape_docrow_shard(&ts, &code, &theme, 400., 400.);
    assert_shard_sane(&c, "代码");
    assert_eq!(c.len(), 4, "首/末空白带 + 2 代码行 = 4 片");
    let table = DocRow {
        kind: IrKind::Table,
        src_line: 0,
        lines: 0..3,
        text: SharedString::default(),
        runs: vec![],
        byte_map: vec![],
        payload: DocPayload::Table(crate::markdown::parse::TableData {
            header: vec!["a".into()],
            rows: vec![vec!["b".into()], vec!["c".into()]],
            aligns: vec![],
        }),
        table_rows: None,
    };
    let t = shape_docrow_shard(&ts, &table, &theme, 400., 400.);
    assert_shard_sane(&t, "表格");
    assert_eq!(t.len(), 1, "表格为单片");
}

/// P3-1c-2：表格 shard 必须携带**真实网格绘制几何**（[`crate::markdown::layout::ShardTable`]）
/// ——列宽按字形实测 + 20 / 下限 48（与 WYSIWYG `shape_table_row` 同式）、单元格高 =
/// `para_line + 12`、表头/数据/对齐；`shard.h` 仍 = 网格高 + 底部滚动条预留带
/// （量程与渲染同源，R2；`parity_*` 的对拍保持）。
#[gpui::test]
fn table_shard_carries_grid_geometry(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    let table = DocRow {
        kind: IrKind::Table,
        src_line: 0,
        lines: 0..3,
        text: SharedString::default(),
        runs: vec![],
        byte_map: vec![],
        payload: DocPayload::Table(crate::markdown::parse::TableData {
            header: vec!["a".into(), "很长的一列标题".into()],
            rows: vec![vec!["b".into(), "x".into()], vec!["c".into(), "y".into()]],
            aligns: vec![TableAlign::Left, TableAlign::Right],
        }),
        table_rows: None,
    };
    let shards = shape_docrow_shard(&ts, &table, &theme, 400., 400.);
    assert_shard_sane(&shards, "表格网格");
    let geo = shards[0]
        .table
        .as_ref()
        .expect("表格 shard 应携带网格几何（P3-1c-2 切 shard 承载）");
    assert_eq!(geo.header.len(), 2, "表头应随几何携带");
    assert_eq!(geo.rows.len(), 2, "数据行应随几何携带");
    assert!(
        geo.widths.iter().all(|w| *w >= 48.0),
        "列宽下限 48：{geo:?}"
    );
    assert!(
        (geo.total_w - geo.widths.iter().sum::<f32>()).abs() < 0.01,
        "total_w 应 = Σ widths"
    );
    assert!(
        geo.widths[1] > geo.widths[0],
        "长文本列应明显宽于短列（字形实测 + 20）：{:?}",
        geo.widths
    );
    let expect_row_h = f32::from(theme.para_line()) + 12.;
    assert!(
        (geo.row_h - expect_row_h).abs() < 0.01,
        "单元格高应 = para_line + 12：{} vs {}",
        geo.row_h,
        expect_row_h
    );
    assert_eq!(geo.aligns[0], TableAlign::Left, "对齐应补齐为 Left");
    assert_eq!(geo.aligns[1], TableAlign::Right, "对齐应随原表保留");
    // 高度公式保持：网格高 + 底部滚动条预留带（与 `parity_*` 对拍的 WYSIWYG 同式）。
    let n = geo.rows.len() as f32;
    let expect_h = (1. + n) * geo.row_h
        + n
        + 2.
        + crate::config::markdown::TABLE_SCROLLBAR_RESERVE_PX;
    assert!(
        (shards[0].h - expect_h).abs() < 0.01,
        "表格 shard 高应 = 网格高 + 预留带：{} vs {}",
        shards[0].h,
        expect_h
    );
}

/// P3-1c-2：图片 shard 必须携带**真实盒几何**（[`crate::markdown::layout::ShardImage`]）；
/// 给固尺寸后盒高按 `box_w * ih / iw` 等比换算、随可用宽缩放；不给尺寸用占位高
/// （与现状一致；量程与渲染同源，R2）。
#[gpui::test]
fn image_shard_box_height_follows_aspect(cx: &mut TestAppContext) {
    let ts = WindowTextSystem::new(cx.text_system().clone());
    let theme = cx.update(|cx| theme_from_font(cx, gpui::Font::default(), 14.));
    let img = DocRow {
        kind: IrKind::Image,
        src_line: 0,
        lines: 0..1,
        text: "题注文字".into(),
        runs: vec![],
        byte_map: vec![],
        payload: DocPayload::Image {
            src: "img/a.png".into(),
        },
        table_rows: None,
    };
    // 无固尺寸 → 占位盒高（`IMAGE_ROW_DEFAULT_H`）。
    let ph = shape_docrow_shard(&ts, &img, &theme, 400., 400.);
    assert_shard_sane(&ph, "图片占位");
    let geo0 = ph[0].image.as_ref().expect("图片 shard 应携带盒几何");
    assert_eq!(geo0.box_w, 400f32.min(crate::config::markdown::IMAGE_MAX_W), "盒宽 = min(avail, IMAGE_MAX_W)");
    assert_eq!(geo0.box_h, crate::config::markdown::IMAGE_ROW_DEFAULT_H, "无尺寸用占位高");
    assert_eq!(geo0.src.as_ref(), "img/a.png", "盒几何应携带源");

    // 给固尺寸（宽 2 高 1）→ 盒高 = box_w × 0.5（等比换算）。
    let sized = shape_docrow_shard_with_image(&ts, &img, &theme, 400., 400., Some((2, 1)));
    let geo1 = sized[0].image.as_ref().expect("尺寸版应仍携带盒几何");
    assert!(
        (geo1.box_h - geo1.box_w * 0.5).abs() < 0.01,
        "盒高按固尺寸比例换算：{:?}",
        geo1
    );
    // 窄 avail → 盒宽/高随宽同比缩小（随宽缩放，量程跟随）。
    let narrow = shape_docrow_shard_with_image(&ts, &img, &theme, 100., 100., Some((2, 1)));
    let geo2 = narrow[0].image.as_ref().expect("窄宽应仍携带盒几何");
    assert!((geo2.box_w - 100.).abs() < 0.01, "盒宽随 avail 缩小：{}", geo2.box_w);
    assert!((geo2.box_h - 50.).abs() < 0.01, "盒高随宽同比：{}", geo2.box_h);
    // 总高 = 盒高 + 题注间隙 + 题注折行高（与 `shape_image_row` 同式）。
    let cap_lh = f32::from(theme.para_line());
    let expect_total = geo2.box_h
        + crate::config::markdown::IMAGE_CAPTION_GAP_PX
        + cap_lh;
    let total: f32 = narrow.iter().map(|s| s.h).sum();
    assert!(
        (total - expect_total).abs() < 1.0,
        "图片块总高 = 盒高 + 题注间隙 + 题注高：{total:.1} vs {expect_total:.1}"
    );
}

/// 真实窗口 + `WyEditor`：主题取自 `window.text_style()`（与 `WyEditor::new` 同一口径）。
/// 返回 `(docrows, frame, theme, avail, ts)`——`frame` 全带宽整形，`block_height(k)` 即真值。
fn build_frame_for(
    cx: &mut TestAppContext,
    src: &str,
    width: f32,
) -> (Vec<DocRow>, Frame, WyTheme, f32, std::sync::Arc<WindowTextSystem>) {
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(src));
        WyHost { ed }
    });
    let vcx: &mut gpui::VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let doc: &Doc = ed.read(cx).doc();
        let n = doc.block_count();
        let rows = doc.rows_in_block_window(0, n.saturating_sub(1));
        let frame = build_frame(ed.read(cx), window, px(width), (0., 1_000_000.));
        let ts = window.text_style();
        let fs: Pixels = ts.font_size.to_pixels(window.rem_size());
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        let theme = WyTheme::from_style(&p, ts.font(), fs);
        (rows, frame, theme, width - 2. * CONTENT_EDGE_PAD_X_PX, window.text_system().clone())
    })
}

/// 与 WYSIWYG `shape_docrow` 的几何**逐块对拍**：`Σ shard.h == frame.block_height(k)`。
#[gpui::test]
fn parity_shard_height_sum_matches_shape_docrow(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let src = concat!(
        "# 标题\n",
        "\n",
        "普通短段落。\n",
        "\n",
        "the quick brown fox jumps over the lazy dog, and keeps jumping and ",
        "jumping over the fence again and again to make this line wrap into ",
        "several visual lines for sure.\n",
        "\n",
        "- 列表项\n",
        "\n",
        "> 引述行\n",
        "\n",
        "```rust\nfn main() {}\nlet x = 1;\n```\n",
        "\n",
        "| a | b |\n",
        "|---|---|\n",
        "| 1 | 2 |\n",
    );
    let width = 400f32;
    let (docrows, frame, theme, avail, ts) = build_frame_for(cx, src, width);
    assert_eq!(
        docrows.len(),
        frame.block_count(),
        "DocRows 与块计数应对齐"
    );
    let mut failures = Vec::new();
    for (k, dr) in docrows.iter().enumerate() {
        let expect = frame.block_height(k);
        let shards = shape_docrow_shard(&ts, dr, &theme, avail, width);
        let got: f32 = shards.iter().map(|s| s.h).sum();
        if (got - expect).abs() > 0.05 {
            failures.push(format!(
                "k={k} kind={:?} got={got:.2} expect={expect:.2} shards={}",
                dr.kind,
                shards.len()
            ));
        }
    }
    assert!(failures.is_empty(), "逐块对拍失败：\n{}", failures.join("\n"));
}
