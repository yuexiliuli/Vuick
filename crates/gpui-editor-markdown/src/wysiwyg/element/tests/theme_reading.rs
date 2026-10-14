#[gpui::test]
fn theme_mirrors_style_params_single_source(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# t\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx| {
        let ed = host.read(cx).ed.clone();
        let theme = ed.read(cx).theme.clone();
        // WYSIWYG 主题必须逐项抄自预览单一排版源 StyleParams（改样式只动一处）。
        let p = crate::markdown::view::style::StyleParams::from_theme_with_font(
            cx,
            Some(f32::from(theme.font_size)),
        );
        assert_eq!(theme.text, p.fg, "正文色应抄自 StyleParams");
        assert_eq!(theme.code_bg, p.code_bg, "代码块背景应抄自 StyleParams");
        assert_eq!(
            theme.code_inline_fg, p.code_inline_fg,
            "行内代码文字色应抄自 StyleParams"
        );
        assert_eq!(
            theme.code_inline_bg, p.code_inline_bg,
            "行内代码 pill 底色应抄自 StyleParams"
        );
        assert_eq!(theme.divider, p.divider, "分隔线应抄自 StyleParams");
        assert_eq!(
            theme.table_head_bg, p.table_head_bg,
            "表头底色应抄自 StyleParams"
        );
        assert_eq!(theme.para_line(), p.para_line, "正文行高应抄自 StyleParams");
        assert_eq!(theme.code_size(), p.code_size, "代码字号应抄自 StyleParams");
        assert_eq!(theme.code_line(), p.code_line, "代码行高应抄自 StyleParams");
        for lv in 1..=6 {
            let (fs, lh) = p.heading_metrics(lv);
            assert_eq!(theme.heading_fs(lv), fs, "h{lv} 字号应抄自 StyleParams");
            assert_eq!(theme.heading_line(lv), lh, "h{lv} 行高应抄自 StyleParams");
        }
    });
}

/// 居中阅读栏（WYSIWYG md 预览本体）：实测渲染后——帧宽被钳到 `min(72ch, 元素可用宽)`、
/// 居中移位 = 差值一半，且**窗口坐标命中**（叠回元素原点 + 列移位）与**元素局部命中**
/// 反查到同一源偏移（拖拽/点击/光标对齐不因居中而错位）。
#[gpui::test]
fn reading_column_clamps_frame_and_centers_hits(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("# 标题\n一段正文文本，用于验证居中阅读栏。\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (fw, ew, shift) = vcx.update(|_window, cx| {
        let ed = host.read(cx).ed.clone();
        let e = ed.read(cx);
        let ew = e
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        let fw = e
            .frame
            .borrow()
            .as_ref()
            .map(|f| f32::from(f.width))
            .unwrap_or(0.);
        (fw, ew, e.reading_col_shift(ew))
    });
    let body = vcx.update(|_window, cx| f32::from(host.read(cx).ed.read(cx).theme.font_size));
    let col = crate::config::markdown::reading_col_px(body);
    assert!(
        (fw - col.min(ew)).abs() < 1.0,
        "帧宽应钳到 min(72ch={col:.1}, 可用={ew:.1})，实际 {fw:.1}"
    );
    assert!(
        (shift - (ew - fw) / 2.).abs() < 0.1,
        "居中移位应为差值一半：{:.1}/2，实际 {shift:.1}",
        ew - fw
    );
    // 有实际居中（宽窗）时：窗口坐标 = 元素原点 + 列移位 + 列内局部坐标，应反查到
    // 与**直接局部命中**完全相同的源偏移。
    if ew > fw + 1. {
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            let e = ed.read(cx);
            let frame = e.frame.borrow().clone().expect("真实渲染应已建帧");
            let local = point(px(f32::from(frame.padding) + 10.), px(2.));
            let win = point(local.x + px(shift), local.y);
            let off_win = super::source_offset_at(e, &frame, win);
            let off_local = super::source_offset_at_local(e, &frame, local);
            assert_eq!(
                off_win, off_local,
                "窗口坐标叠加居中移位后应命中与局部坐标同一源偏移（off={off_local}）"
            );
        });
    }
}

/// WYSIWYG md 预览随窗口窄化：窗口缩窄 → 元素可用宽收窄 → 帧宽 = min(72ch, 可用宽)
/// 同步收窄（一直折行，永不横向超界）；窄于 72ch 后退化为占满可用宽（居中移位=0）。
#[gpui::test]
fn reading_column_narrows_with_window(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "# 标题\n\n一段很长的正文内容用于验证窗口缩窄后随窗口宽度重排。".to_string();
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let body = vcx.update(|_window, cx| f32::from(host.read(cx).ed.read(cx).theme.font_size));
    let col = crate::config::markdown::reading_col_px(body);
    let read = |vcx: &mut VisualTestContext| {
        vcx.update(|_window, cx| {
            let e = host.read(cx).ed.read(cx);
            let ew = e
                .last_bounds()
                .map(|b| f32::from(b.size.width))
                .unwrap_or(0.);
            let fw = e
                .frame
                .borrow()
                .as_ref()
                .map(|f| f32::from(f.width))
                .unwrap_or(0.);
            (ew, fw, e.reading_col_shift(ew))
        })
    };
    let (ew0, fw0, _) = read(vcx);
    assert!(ew0 > 1.0, "宽窗元素应有布局宽（ew0={ew0:.0}）");
    assert!(
        (fw0 - col.min(ew0)).abs() < 1.0,
        "宽窗帧宽应 = min(72ch={col:.0}, 可用={ew0:.0})，实际 {fw0:.0}"
    );
    // 缩到 500px（< 72ch≈806px）→ 列宽应随窗口收窄并退化为可用宽。
    vcx.simulate_resize(size(px(500.), px(800.)));
    for _ in 0..6 {
        vcx.run_until_parked();
        let _ = vcx.cx.refresh();
    }
    let (ew1, fw1, shift1) = read(vcx);
    assert!(
        ew1 < ew0 - 1.0,
        "窗口缩窄后元素可用宽应收窄：{ew0:.0} -> {ew1:.0}"
    );
    let target = col.min(ew1);
    assert!(
        (fw1 - target).abs() < 1.0,
        "窄窗帧宽应 = min(72ch={col:.0}, 可用={ew1:.0})，实际 {fw1:.0}"
    );
    assert!(
        fw1 < fw0 - 1.0,
        "列宽应随窗口收窄重排：{fw0:.0} -> {fw1:.0}"
    );
    assert!(
        shift1.abs() < 0.5,
        "窄于 72ch 后无居中空余（移位应为 0），实际 {shift1:.0}"
    );
}

#[gpui::test]
fn image_load_updates_size_and_rebuilds_frame(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "![图](x.png)\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let img_row = frame
            .rows
            .iter()
            .find(|r| r.image.is_some())
            .expect("应有图片块");
        let img = img_row.image.as_ref().unwrap();
        // 未加载：块高 = 占位图高 + 题注间隙 + 题注行高。
        assert!(
            (f32::from(img_row.height)
                - (crate::config::markdown::IMAGE_ROW_DEFAULT_H
                    + crate::config::markdown::IMAGE_CAPTION_GAP_PX
                    + f32::from(ed.read(cx).theme.para_line())))
            .abs()
                < 0.5,
            "未加载块高应=占位图高+间隙+题注行高，实际 {:?}",
            img_row.height
        );
        // 模拟加载完成：设定源像素尺寸（200×100 = 宽高比 2:1）→ 重建帧 →
        // 显示宽取内容宽、高按宽高比。
        ed.update(cx, |e, _| {
            e.set_image_size(img.src.clone(), Some((200., 100.)));
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let img_row = frame
            .rows
            .iter()
            .find(|r| r.image.is_some())
            .expect("应有图片块");
        let img = img_row.image.as_ref().unwrap();
        assert!(
            (img.h - img.w / 2.).abs() < 1.,
            "图高应保持宽高比 2:1，w={} h={}",
            img.w,
            img.h
        );
        assert!(
            (f32::from(img_row.height)
                - (img.h
                    + crate::config::markdown::IMAGE_CAPTION_GAP_PX
                    + f32::from(ed.read(cx).theme.para_line())))
            .abs()
                < 0.5,
            "块高应=图高+间隙+题注行高，实际 {} 图高 {}",
            f32::from(img_row.height),
            img.h
        );
    });
}

#[gpui::test]
fn image_alt_editing_roundtrip(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "![图](x.png)\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            let ls = e.line_start(0);
            // 光标置于 alt 起点（`![` 之后，ls+2）→ 输入应写进 alt、图片行保持。
            e.set_caret(ls + 2);
            e.replace_range(e.caret()..e.caret(), "X", cx);
            assert!(
                e.text().as_ref().contains("![X图](x.png)"),
                "alt 前置输入生效：{}",
                e.text().as_ref()
            );
        });
    });
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 重开帧：该行仍是图片块、题注文本 = 新 alt。
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("编辑后仍应解析成图片块");
        assert!(row.editable, "图片题注仍可编辑");
        assert_eq!(
            row.wrapped.first().map(|w| w.text.as_ref()),
            Some("X图"),
            "题注应显示新 alt"
        );
    });
}

#[gpui::test]
fn image_alt_selection_and_replace_roundtrip(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "![hello](x.png)\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            // `hello` 位于源 [2,7)：选 "ell"(3..6) 应映射正确、替换回源。
            e.begin_selection_at(3);
            e.extend_selection_to(6);
            assert_eq!(e.selection(), Some(3..6), "alt 选区应落在源 [3,6)");
            assert_eq!(e.selected_text(), "ell", "选中的源文本应为 alt 子串");
            e.replace_range(e.selection().unwrap(), "o", cx);
            assert!(
                e.text().as_ref().contains("![hoo](x.png)"),
                "alt 选区替换生效：{}",
                e.text().as_ref()
            );
        });
    });
}

#[gpui::test]
fn image_alt_backspace_delete_guards(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "![hello](x.png)\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            let ls = e.line_start(0);
            // 退格在 alt 起点（ls+2）→ 守卫（不吃 `![` 的 `[`）。
            e.set_caret(ls + 2);
            e.backspace(cx);
            assert!(
                e.text().as_ref().contains("![hello](x.png)"),
                "alt 起点退格应被守卫：{}",
                e.text().as_ref()
            );
            // 删除在 alt 尾（ls+7 = `]` 前）→ 守卫（不吃 `](` 的 `]`）。
            e.set_caret(ls + 7);
            e.delete(cx);
            assert!(
                e.text().as_ref().contains("![hello](x.png)"),
                "alt 尾删除应被守卫：{}",
                e.text().as_ref()
            );
            // alt 内正常键入仍有效（`hello` 首字符前插 Y）。
            e.set_caret(ls + 2);
            e.replace_range(e.caret()..e.caret(), "Y", cx);
            assert!(
                e.text().as_ref().contains("![Yhello](x.png)"),
                "alt 内输入生效：{}",
                e.text().as_ref()
            );
        });
    });
}

#[gpui::test]
fn image_alt_caption_click_and_caret_stay_aligned(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "![hello](x.png)\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("应有图片块");
        let pad = f32::from(frame.padding);
        let top = f32::from(row.top);
        // 题注（可编辑 alt 行）在图片盒之下：y = 图高 + 间隙，行内 x 走居中 x_off。
        let img = row.image.as_ref().unwrap();
        let caption_y = top + img.h + crate::config::markdown::IMAGE_CAPTION_GAP_PX + 2.;
        let x = pad + f32::from(row.x_off) + 1.;
        let off = source_offset_at_local(ed.read(cx), &frame, point(px(x), px(caption_y)));
        assert_eq!(off, 2, "点击题注起始应命中 alt 起点（源 2），实际 {off}");
        // 光标置于 alt 起点 → 光标条应落在题注行内（y ≥ 图高，x 对题注居中起点）。
        ed.update(cx, |e, _| e.set_caret(2));
        let (on, caretx, carety) = caret_geometry(ed.read(cx), &frame);
        assert!(on, "题注应可放光标");
        let img = row.image.as_ref().unwrap();
        let caption_top = top + img.h + crate::config::markdown::IMAGE_CAPTION_GAP_PX;
        assert!(
            f32::from(carety) >= caption_top - 0.5
                && f32::from(carety) <= caption_top + f32::from(frame.line_h) + 0.5,
            "光标 y {} 应在题注行带 [{caption_top}, {}]",
            f32::from(carety),
            caption_top + f32::from(frame.line_h)
        );
        assert!(
            (f32::from(caretx) - x).abs() < 12.,
            "光标 x {} 应贴合题注点击 x {}",
            f32::from(caretx),
            x
        );
    });
}

#[gpui::test]
fn ime_composition_replaces_previous_marked_text(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    // 回归「输入“你好”→ “nnini'hni'hao你好”」：Windows 组合更新不携带区间，
    // 必须替换上一次 marked，而不是一前一后堆叠。
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret("你好".len());
            e.replace_and_mark_utf16(None, "n", None, cx);
            assert_eq!(e.text().as_ref(), "你好n", "组合首键插在光标后");
            e.replace_and_mark_utf16(None, "ni", None, cx);
            assert_eq!(
                e.text().as_ref(),
                "你好ni",
                "组合更新应替换旧 marked，不得残留 n"
            );
            e.replace_and_mark_utf16(None, "nih", None, cx);
            assert_eq!(e.text().as_ref(), "你好nih", "再次更新仍替换");
            // 提交：`replace_text_utf16(None)` 落在 marked 上并清掉 marked。
            e.replace_text_utf16(None, "你好", cx);
            assert_eq!(
                e.text().as_ref(),
                "你好你好",
                "提交文本替换组合占位，不重复"
            );
            // 新组合不得再把“已提交中文”当旧 marked 替换。
            e.replace_and_mark_utf16(None, "n", None, cx);
            e.replace_and_mark_utf16(None, "ni", None, cx);
            assert_eq!(e.text().as_ref(), "你好你好ni", "新组合不重复旧提交");
        });
    });
}

#[gpui::test]
fn ime_new_selected_range_is_relative_to_marked(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret(0);
            // AppKit setMarkedText 的 selectedRange 是**相对组合文本**的偏移，
            // 不是文档绝对偏移——光标应落在偏移 1（“好”前），而非文档开头。
            e.replace_and_mark_utf16(None, "你好", Some(1..2), cx);
            assert_eq!(e.text().as_ref(), "你好abc", "无选区时组合插入光标处");
            assert_eq!(
                e.caret(),
                3,
                "光标 = 组合起点 0 + 相对偏移 1 的字节（“好”前），而非文档绝对 1"
            );
        });
    });
}

#[gpui::test]
fn ime_composition_real_path_updates_commits_cjk_and_punctuation(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler;
    // 回归（并入 ime:: 后）：`EntityInputHandler` 直通共享状态机的真实回路——
    // 组合开始→更新（替换旧 marked）→提交，覆盖 CJK（你）与中文标点（，），
    // 提交后的新组合不得把已提交文本当旧 marked 覆盖。
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret("你好".len());
            // 组合第 1 字：n → ni → 你（每次更新替换旧的 marked）
            e.replace_and_mark_text_in_range(None, "n", None, window, cx);
            e.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            e.replace_and_mark_text_in_range(None, "你", None, window, cx);
            assert_eq!(e.text().as_ref(), "你好你", "组合首字插在光标后");
            // 提交第 1 字（结束组合，marked 清空）
            e.replace_text_in_range(None, "你", window, cx);
            assert_eq!(e.text().as_ref(), "你好你");
            // 中文标点组合并提交
            e.replace_and_mark_text_in_range(None, "，", None, window, cx);
            assert_eq!(e.text().as_ref(), "你好你，", "标点组合插在提交文本后");
            e.replace_text_in_range(None, "，", window, cx);
            assert_eq!(e.text().as_ref(), "你好你，", "提交后文本不变");
            assert_eq!(e.caret(), "你好你，".len(), "光标在提交文本末尾");
            // 新组合（含第二轮更新）不得复用旧 marked
            e.replace_and_mark_text_in_range(None, "a", None, window, cx);
            e.replace_and_mark_text_in_range(None, "ab", None, window, cx);
            assert_eq!(e.text().as_ref(), "你好你，ab", "新组合不重复旧提交");
            e.replace_text_in_range(None, "!", window, cx);
            assert_eq!(e.text().as_ref(), "你好你，!", "最终文本");
        });
    });
}

#[gpui::test]
fn ime_composition_cancel_removes_marked_and_stays_clean(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler;
    // 回归（并入 ime:: 后）：组合取消（空替换）删除组合占位、光标回组合起点、
    // marked 清空——取消后开新组合不得把已清除的组合当旧 marked 复用。
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret("你好".len());
            e.replace_and_mark_text_in_range(None, "nih", None, window, cx);
            assert_eq!(e.text().as_ref(), "你好nih");
            // 取消：空替换把组合占位移除，光标回组合起点
            e.replace_and_mark_text_in_range(None, "", None, window, cx);
            assert_eq!(e.text().as_ref(), "你好", "取消后组合文本被移除");
            assert_eq!(e.caret(), "你好".len(), "光标回到组合起点");
            // 取消后新组合 + 提交：只追加一次（旧 marked 已清除）
            e.replace_and_mark_text_in_range(None, "你", None, window, cx);
            e.replace_text_in_range(None, "你", window, cx);
            assert_eq!(e.text().as_ref(), "你好你", "取消后新组合不残留");
        });
    });
}

#[gpui::test]
fn ime_replace_with_explicit_utf16_range_lands_on_doc_bytes(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler;
    // 回归（并入 ime:: 后）：显式 UTF-16 区间（macOS/部分平台）走行粒度索引换算，
    // 落到正确的源字节并清 marked。
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好世界"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            // [1,2) UTF-16 = "好"整字（字节 3..6），替换后文本与光标都应落对。
            e.replace_text_in_range(Some(1..2), "更", window, cx);
            assert_eq!(e.text().as_ref(), "你更世界", "显式 UTF-16 区间替换整字");
            assert_eq!(e.caret(), "你更".len(), "光标落新文本末尾");
        });
    });
}


