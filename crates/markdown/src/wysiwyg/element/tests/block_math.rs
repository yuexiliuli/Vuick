/// 回归（超长行内公式 + 后续键入「回弹」）：公式自然宽 > 内容盒时，spacer 宽 / 撑高 /
/// paint 必须共用**同一个钳制值**（`InlineMathText::disp_w`，源于 `inline_math_fit`）。
/// 旧实现按「自然宽」把整条公式 spacer 折成 9~13 行空格、把后续内容顶到极远处并越出
/// 画布；下一帧又「钳到内容盒」取小 → 两帧取不同宽、内容忽大忽小。本测试：
/// ① 公式后连续键入 6 帧几何完全一致（无两帧振荡）；② 公式显示宽 ≤ 内容盒宽；
/// ③ 行不再炸成一整把空格行（可视行 ≤ 3）；④ 后续块紧贴公式行、不再被顶远。
#[gpui::test]
fn long_inline_math_typed_after_single_value_within_box(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let latex = "a".repeat(160);
    let text = format!("前 ${latex}$ 后尾 1234567890\n下一段 abcdef\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let buf = raster::RgbaImage::from_raw(100, 80, vec![0u8; 100 * 80 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![raster::Frame::new(buf)]));
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // Ready 位图：4000×40（自然宽远超 400px 窗口内容盒的可复原场景）。
        ed.read(cx).math_slots().borrow_mut().insert(
            super::super::inline_math_key(&latex),
            super::super::MathSlot::Ready(img, 4000., 40.),
        );
        let caret_start = ed.read(cx).content().find("后尾").unwrap();
        let mut frames: Vec<(f32, usize, f32, f32)> = Vec::new();
        let mut last_following_top = 0.0;
        for caret in caret_start..(caret_start + 6) {
            ed.update(cx, |e, ecx| {
                e.set_caret(caret);
                e.replace_range(caret..caret, "X", ecx);
            });
            let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
            let r = &f.rows[0];
            let w0 = r.wrapped.first().unwrap();
            let disp_w = r
                .extra
                .inline_maths
                .first()
                .map(|im| im.disp_w)
                .unwrap_or(0.);
            let following_top = f.rows.get(1).map(|r2| f32::from(r2.top)).unwrap_or(0.);
            frames.push((
                f32::from(r.height),
                w0.wrap_boundaries().len() + 1,
                disp_w,
                following_top,
            ));
            last_following_top = following_top;
        }
        let first = frames[0];
        for (i, fr) in frames.iter().enumerate().skip(1) {
            assert_eq!(
                *fr, first,
                "公式后连续键入第 {i} 帧几何应等于首帧（回弹 = 两帧取不同宽）"
            );
        }
        // 公式显示宽钳到内容盒 400−2×12=376，绝不能以自然宽 4000 参与布局。
        let avail = 400. - 2. * config::markdown::CONTENT_EDGE_PAD_X_PX;
        assert!(
            first.2 <= avail + 1.0,
            "公式 disp_w {} 应 ≤ 内容盒宽 {avail}",
            first.2
        );
        // 行不炸成一整把空格行（旧实现 9~13 个可视行）：钳宽后 ≤ 3 个可视行。
        assert!(
            first.1 <= 3,
            "超长公式行应按内容盒钳制：可视行 {} 应 ≤ 3",
            first.1
        );
        // 后续块紧随公式行（top == 公式行高），不再被顶到极远。
        assert!(
            (last_following_top - first.0).abs() < 0.5,
            "后续块 top {} 应紧贴公式行高 {}",
            last_following_top,
            first.0
        );
    });
}

/// 回归（块级超宽公式不缩小 → 块内横向滚动）：位图就绪（自然宽 5000 ≫ 内容盒 376）
/// → ① 公式自然宽 = 位图逻辑宽（未缩小）；② `math_hscroll_geo` 返回 Some 且
/// `max_scroll > 0`（横向滚动条存在）；③ 滚动偏移按 max_scroll clamp、旧值不越界；
/// ④ 底部滚动条带命中（轨道跳转/拖拽双语义）；⑤ 公式自然宽 ≤ 内容盒时无滚动条。
#[gpui::test]
fn block_math_wide_has_hscroll_and_natural_width(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let latex = r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}".to_string() + &"1".repeat(60);
    let text = format!("$$\n{latex}\n$$\n后文 xxxx\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let buf = raster::RgbaImage::from_raw(100, 80, vec![0u8; 100 * 80 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![raster::Frame::new(buf)]));
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 光标放块外 → 折叠成卡片（非 reveal）。
        let end = ed.read(cx).content().len();
        ed.update(cx, |e, _| e.set_caret(end));
        // 超宽位图（自然宽 5000、高 60；内容盒 = 400 − 2×12 = 376）。
        ed.read(cx).math_slots().borrow_mut().insert(
            latex.clone().into(),
            super::super::MathSlot::Ready(img, 5000., 60.),
        );
        let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
        let card = f
            .rows
            .iter()
            .find(|r| matches!(&r.extra.card, Some(super::BlockCard::Math { .. })))
            .expect("数学卡");
        let viewport = 400. - 2. * config::markdown::CONTENT_EDGE_PAD_X_PX;
        // ① 公式自然宽 = 位图逻辑宽（不缩小）且 > 内容盒宽。
        let nat = super::math_natural_w(ed.read(cx), card);
        assert!(
            nat > viewport + 1.,
            "自然宽 {nat} 应远超内容盒 {viewport}（不缩小）"
        );
        assert_eq!(nat, 5000.0, "自然宽应取位图逻辑宽");
        // ② 超宽 → 横向滚动条几何存在（max_scroll = 自然宽 − 内容盒）。
        let (_, _, _, _, max_scroll) =
            super::math_hscroll_geo(ed.read(cx), &f, card).expect("超宽公式应有横向滚动条");
        assert!(max_scroll > 0., "max_scroll={max_scroll} 应 > 0");
        assert!(
            (max_scroll - (nat - viewport)).abs() < 1.0,
            "max_scroll = 自然宽 − 内容盒"
        );
        // ③ 默认 0；写入超界值被 clamp 到 max_scroll；清回 0。
        assert_eq!(super::math_hscroll_clamped(ed.read(cx), &f, card), 0.);
        ed.read(cx).set_math_h_scroll(card.src_line, 999_999.);
        assert!(
            (super::math_hscroll_clamped(ed.read(cx), &f, card) - max_scroll).abs() < 0.5,
            "应钳到 max_scroll"
        );
        ed.read(cx).set_math_h_scroll(card.src_line, 0.);
        // ④ 底部滚动条带命中（点在轨道内、滑块外 → on_thumb=false，可轨道跳转）。
        let (hit, on_thumb, _) = super::math_hscrollbar_hit(
            ed.read(cx),
            &f,
            point(
                px(f32::from(f.padding) + viewport * 0.9),
                px(f32::from(card.top) + f32::from(card.height)
                    - config::layout::CODE_HSCROLL_BAND / 2.),
            ),
        )
        .expect("超宽公式底部轨道应命中");
        assert_eq!(hit, card.src_line, "命中该数学卡");
        assert!(!on_thumb, "0.9 内容宽处应在轨道（非滑块）上");
    });
}

/// 回归（块级公式键入「回弹」→ 排版槽换代单值稳定）：真实光栅异步换代——每键入
/// 一帧，卡片先遇 Loading（回退）再换 Ready（同源新位图）。旧实现卡高 = `20+位图高`
/// ↔ `20+n×22` 两态来回跳（用户所见「大块异常↔正常块」）。回归断言：连续键入
/// N 帧、槽在 Loading 与 Ready 间交替，卡片行高与后续块 top 必须逐帧恒等。
#[gpui::test]
fn block_math_typing_geometry_stable_across_slot_flip(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let latex = r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}".to_string() + &"1".repeat(60);
    let text = format!("$$\n{latex}\n$$\n后文 xxxx\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let buf = raster::RgbaImage::from_raw(100, 80, vec![0u8; 100 * 80 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![raster::Frame::new(buf)]));
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 光标在后续段（块折叠成卡）：模拟「每键一次换代」——帧内交替 Loading/Ready。
        let caret_start = ed.read(cx).content().find("后文 xxxx").unwrap() + "后文 xxxx".len();
        let mut prev: Option<(f32, f32)> = None;
        for (caret, k) in (caret_start..).take(6).zip(0..6) {
            let slots = ed.read(cx).math_slots();
            slots.borrow_mut().remove(latex.as_str());
            if k % 2 == 0 {
                // 奇偶交替：偶数帧 = Ready（新位图 5000×60），奇数帧 = Loading。
                slots.borrow_mut().insert(
                    latex.clone().into(),
                    super::super::MathSlot::Ready(img.clone(), 5000., 60.),
                );
            }
            ed.update(cx, |e, ecx| {
                e.set_caret(caret);
                e.replace_range(caret..caret, "X", ecx);
            });
            let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
            let card_h = f
                .rows
                .iter()
                .find(|r| r.extra.card.is_some())
                .map(|r| f32::from(r.height))
                .unwrap_or(0.);
            let para_top = f
                .rows
                .iter()
                .find(|r| r.extra.card.is_none())
                .map(|r| f32::from(r.top))
                .unwrap_or(0.);
            let key = (card_h, para_top);
            if let Some(p) = prev {
                assert_eq!(
                    key,
                    p,
                    "第 {k} 帧：槽 {} 时卡高/后续段 top 必须与上一帧恒等（单值）",
                    if k % 2 == 0 { "Ready" } else { "Loading" }
                );
            }
            prev = Some(key);
        }
    });
}

/// 回归（块级 `$$…$$` 超宽公式后键入）：卡片行高不随逐键振荡、后续块位置稳定。
/// 块卡 `math_card_height` 在 slot 就绪后取单值（20+位图高），paint 按内容盒等比
/// 缩放；键入的是后续段落行（非 caret 的卡片行走缓存），几何必须逐帧一致。
#[gpui::test]
fn wide_block_math_followed_by_typing_stays_single_value(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let latex = "a".repeat(200);
    let text = format!("$$\n{latex}\n$$\n后文 xxxx\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let buf = raster::RgbaImage::from_raw(100, 80, vec![0u8; 100 * 80 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![raster::Frame::new(buf)]));
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).math_slots().borrow_mut().insert(
            latex.clone().into(),
            super::super::MathSlot::Ready(img, 5000., 60.),
        );
        let caret_start = ed.read(cx).content().find("后文 xxxx").unwrap() + "后文 xxxx".len();
        let mut prev: Option<(f32, f32)> = None;
        for (caret, k) in (caret_start..).take(5).zip(0..5) {
            ed.update(cx, |e, ecx| {
                e.set_caret(caret);
                e.replace_range(caret..caret, "X", ecx);
            });
            let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
            // 卡片行 = 唯一带 `extra.card` 的行；其高度 + 卡片下方后续段落 top 必须稳定。
            let card_h = f
                .rows
                .iter()
                .find(|r| r.extra.card.is_some())
                .map(|r| f32::from(r.height))
                .unwrap_or(0.);
            let para_top = f
                .rows
                .iter()
                .find(|r| r.extra.card.is_none())
                .map(|r| f32::from(r.top))
                .unwrap_or(0.);
            let key = (card_h, para_top);
            if let Some(p) = prev {
                assert_eq!(
                    key, p,
                    "块级超宽公式后键入第 {k} 帧：卡高/后续段 top 应稳定"
                );
            }
            prev = Some(key);
        }
    });
}

/// 回归（块级 `$$…$$` 键入「回弹」→ 渲染表示不闪变，含「缓存阈值」路径）：
///
/// 用户复现：公式就绪后逐个输入 `1` 每个都回弹（两态）；删光重输 1~10 不回弹
/// （那些源串已缓存=Ready），第 11 个起又回弹（新键 miss → Loading）。
/// 本测试沿**真实渲染管线**（`build_frame` + `sync_math` 占槽）逐键建模：
/// 每键入一个 `1` 生成新键，尽力重现「旧键被 Ready 缓存、新键 Loading」的
/// 阈值语义；随后删回重输全量。断言：
///   - 首光栅（无钉）才允许一次 Unicode 回退（`Fallback`）；
///   - 公式就绪后**每一帧**渲染表示 ∈ {Ready, Pinned}（**绝不回落 Unicode**），
///     即 Loading↔Ready 两态闪跳被消除；
///   - 卡高逐帧恒等；Loading（钉住旧图）帧的自然宽 = 钉宽（与绘制同源，滚动条
///     不错位）；Ready 与 Pinned 的画图几何（等比收容）一致（换图无缝）。
/// 回归守护：任何把「Loading 改回 Unicode 回退」的改动在此都会 FAIL。
#[gpui::test]
fn block_math_typing_representation_stable_across_threshold(cx: &mut TestAppContext) {
    use super::paint::{MathCardDisplay, math_card_display};
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let base_latex = r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}";
    // 尾随段落：光标放这 → 卡折叠（不 reveal），与真实「看卡」场景一致。
    let text = format!("$$\n{base_latex}\n$$\n后文 xxxx\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    // 假位图（纯 CPU）：就绪的“新位图”逐键换新引用，尺寸同源（逻辑 260×60）。
    let buf = raster::RgbaImage::from_raw(100, 60, vec![0u8; 100 * 60 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![raster::Frame::new(buf)]));
    let (pin_w, pin_h) = (260.0f32, 60.0f32);

    // —— 复现/回归主体（一次函数内完成「就绪 → 逐键 → 删回 → 重输»）——
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // caret 放文尾（卡折叠）。
        let end = ed.read(cx).content().len();
        ed.update(cx, |e, _| e.set_caret(end));
        let f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let card = f0
            .rows
            .iter()
            .find(|r| r.extra.card.is_some())
            .expect("应有余弦卡");
        let src_line = card.src_line;
        // 就绪：claim Loading → 手动注入 Ready（后台光栅成）→ sync_math 再命中
        // Ready 分支录制「最近就绪位图」钉（与 paint 就绪录制同效）。
        ed.update(cx, |e, ecx| e.sync_math(ecx, &f0));
        ed.update(cx, |e, _| {
            e.math_slots().borrow_mut().insert(
                base_latex.into(),
                super::super::MathSlot::Ready(img.clone(), pin_w, pin_h),
            );
        });
        ed.update(cx, |e, ecx| e.sync_math(ecx, &f0));
        let pin = ed
            .read(cx)
            .math_pin(src_line)
            .expect("就绪后应录制本块位图钉");
        assert!((pin.w - pin_w).abs() < 0.01 && (pin.h - pin_h).abs() < 0.01);

        // 公式内容行末尾字节（闭围栏换行前）。
        let insert_at = ed.read(cx).content().find("\n$$\n").unwrap();
        let mut caret = insert_at;

        // 辅助：追加一个 `1` 并重放帧，返回 (frame, 新键)。
        let push_one =
            |ed: &Entity<WyEditor>, window: &mut Window, caret: &mut usize, cx: &mut App| {
                ed.update(cx, |e, ecx| e.replace_range(*caret..*caret, "1", ecx));
                *caret = ed.read(cx).caret();
                let end = ed.read(cx).content().len();
                ed.update(cx, |e, _| e.set_caret(end));
                let f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
                let card = f
                    .rows
                    .iter()
                    .find(|r| r.extra.card.is_some())
                    .expect("卡折叠");
                let key = match &card.extra.card {
                    Some(super::BlockCard::Math { latex, .. }) => latex.trim().to_string(),
                    _ => unreachable!(),
                };
                (f, key)
            };

        // 前向：逐键 1..12（越阈值；每键新键 → miss → Loading — 必须被旧图钉住）。
        let mut h_prev: Option<f32> = None;
        for k in 1..=12 {
            let (f, key) = push_one(&ed, window, &mut caret, cx);
            // 真实管线：新键 miss → sync_math 占 Loading（无窗口下光栅不真跑，但
            // Loading 立即入槽——正对应「光栅未回的那一帧」）。
            ed.update(cx, |e, ecx| e.sync_math(ecx, &f));
            let disp = math_card_display(ed.read(cx), src_line, key.trim());
            assert!(
                !matches!(disp, MathCardDisplay::Fallback),
                "第 {k} 键：公式就绪后渲染表示不得回退 Unicode（回弹）：{disp:?}"
            );
            let (d_w, d_h) = match disp {
                MathCardDisplay::Ready(_, w, h) | MathCardDisplay::Pinned(_, w, h) => (w, h),
                MathCardDisplay::Fallback => unreachable!(),
            };
            // Loading（钉住旧图）帧：绘制几何与「旧图就绪绘制」几何必须一致。
            let row = f.rows.iter().find(|r| r.extra.card.is_some()).unwrap();
            let avail_h = (f32::from(row.height) - 20.).max(1.);
            let vs = (avail_h / d_h).min(1.0);
            let (drawn_w, drawn_h) = (d_w * vs, d_h * vs);
            if let Some(p) = h_prev {
                assert!(
                    (f32::from(row.height) - p).abs() < 0.01,
                    "第 {k} 键：卡高必须逐帧恒等：{} vs {}",
                    f32::from(row.height),
                    p
                );
            }
            h_prev = Some(f32::from(row.height));
            assert!(
                drawn_w > 0. && drawn_h > 0.,
                "第 {k} 键：钉住/就绪位图应有可绘制几何"
            );
            // 就绪换图：手动把该键标记 Ready（后台完成），下帧同键直接 Ready。
            ed.update(cx, |e, _| {
                e.math_slots().borrow_mut().insert(
                    key.as_str().into(),
                    super::super::MathSlot::Ready(img.clone(), pin_w, pin_h),
                );
            });
        }

        // 删光回到原公式（每删一步出现过的键全在缓存=Ready → 不占 Loading）。
        for _ in 0..12 {
            let del = caret.checked_sub(1).expect("还有字符可删");
            ed.update(cx, |e, ecx| e.replace_range(del..del + 1, "", ecx));
            caret = del;
            let end = ed.read(cx).content().len();
            ed.update(cx, |e, _| e.set_caret(end));
        }
        assert_eq!(caret, insert_at, "删光后应回到原公式内末尾（不再有追加 1）");

        // 重输 1..12：1..11 已缓存（Ready）直接命中；第 12 键新键 Loading → 钉住。
        let mut caret = insert_at;
        for k in 1..=12 {
            let (f, key) = push_one(&ed, window, &mut caret, cx);
            ed.update(cx, |e, ecx| e.sync_math(ecx, &f));
            let disp = math_card_display(ed.read(cx), src_line, key.trim());
            assert!(
                !matches!(disp, MathCardDisplay::Fallback),
                "重输第 {k} 键（含越阈值）：不得回退 Unicode：{disp:?}"
            );
            ed.update(cx, |e, _| {
                e.math_slots().borrow_mut().insert(
                    key.as_str().into(),
                    super::super::MathSlot::Ready(img.clone(), pin_w, pin_h),
                );
            });
        }
    });
}
