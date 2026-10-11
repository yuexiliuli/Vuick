#[gpui::test]
fn keys_ctrl_a_c_v_select_copy_paste_round_trip(cx: &mut TestAppContext) {
    // D1/Ctrl 修复：Windows 上 Ctrl 报 `modifiers.control`，而 `cmd-*` 绑定是
    // `modifiers.platform`（= Win 键）→ 旧实现 Ctrl+C/V/A 全部失效。现在与
    // `live.rs` 一样显式补 `ctrl-a/c/v` 绑定，经真实键位派发验证整条链路。
    use gpui::{App, Keystroke};
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("hello world"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    // 安装 WYSIWYG 键位（与 main.rs 的 bind_keys 装配一致）。
    vcx.update(|_w, cx: &mut App| crate::markdown::wysiwyg::bind_keys(cx));
    // 聚焦编辑器 → 键位上下文生效。
    vcx.update(|window, cx| {
        let fh = host.read(cx).ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    let wh = vcx.windows().first().copied().expect("窗口句柄");

    // Ctrl+A 全选。
    vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-a").unwrap());
    vcx.update(|_w, cx| {
        assert_eq!(
            host.read(cx).ed.read(cx).selection(),
            Some(0.."hello world".len()),
            "Ctrl+A 应全选"
        );
    });
    // Ctrl+C 复制 → 剪贴板。
    vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-c").unwrap());
    let clip: Option<String> = vcx.update(|_w, cx: &mut App| {
        cx.read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
    });
    assert_eq!(clip.as_deref(), Some("hello world"), "Ctrl+C 应写入剪贴板");
    // 光标置开头后 Ctrl+V 粘贴（选区替换为插入）。
    vcx.update(|_w, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_caret(0);
        });
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    vcx.update(|_w, cx| {
        assert_eq!(
            host.read(cx).ed.read(cx).text().as_ref(),
            "hello worldhello world",
            "Ctrl+V 应在光标处粘贴剪贴板内容"
        );
    });
}

#[gpui::test]
fn plain_edit_reuses_derived_cache(cx: &mut TestAppContext) {
    // 纯文本键入（非结构、行签名不变、不在数学块内）不得触发派生整篇重建：
    // `derived` Rc 指针不变 + regions 版本不递增——这是「输入不卡死」的基石
    // （老实现每键对全篇 `math_regions`×2 + `alert_*`×2 + 行内逐行扫 ≈1s）。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "$$\n\\int_x dx\n$$\n含有 $e^{i\\pi}$ 的一行\n末尾普通段落 here\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        assert_eq!(d.borrow().math_regions.len(), 1, "应扫到这 1 个 $$ 块");
        (d, ed.read(cx).regions_ver())
    });
    // 在「末尾普通段落 here」行中间键入 'a'（纯文本，无 $ / > / ```）。
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("here").unwrap() + 2;
        ed.update(cx, |e, ecx| e.replace_range(at..at, "a", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(
            std::rc::Rc::ptr_eq(&rc0, &d1),
            "纯文本键入应复用 derived Rc（不整篇重建）"
        );
        assert_eq!(ed.read(cx).regions_ver(), rv0, "regions 版本应不变");
        assert_eq!(d1.borrow().math_regions.len(), 1, "块区域数不变");
        assert_eq!(
            d1.borrow().math_regions[0].source,
            "\\int_x dx",
            "数学块 source 应保持"
        );
    });
}

#[gpui::test]
fn math_interior_edit_patches_block_source(cx: &mut TestAppContext) {
    // 编辑 $$ 块**内部**（非边界行）：不整篇重建，只修补该块 `source`——离开块后
    // 卡片显示更新后的公式（老实现要么整篇重建、要么卡片陈旧显示旧公式）。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "$$\n\\int_a dx\n$$\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        assert_eq!(d.borrow().math_regions[0].source, "\\int_a dx");
        (d, ed.read(cx).regions_ver())
    });
    // 把内部行 `\int_a dx` 的 'a' 改成 'b'。
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("\\int_a").unwrap() + 5; // 落在 'a'
        ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "b", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(std::rc::Rc::ptr_eq(&rc0, &d1), "数学块内部编辑不应整篇重建");
        assert_eq!(ed.read(cx).regions_ver(), rv0, "regions 版本应不变");
        assert_eq!(
            d1.borrow().math_regions[0].source,
            "\\int_b dx",
            "块 source 应被单块修补为新公式"
        );
    });
}

/// M5-B3-1 回归（1a）：在数学区**上界**（开界 `$$` 行）处键入 `$` 结构性字符——
/// 开界行 `$$` → `$$$`（`region_line_sig` 变化）→ 派生整篇重建。断言重建后的
/// `math_regions` 与对新文本从零重扫完全一致（区域/边界错位即此处漏网）。
#[gpui::test]
fn typing_dollar_at_math_upper_bound_keeps_regions_consistent(cx: &mut TestAppContext) {
    use crate::markdown::parse::inline_style;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "intro\n$$\n\\int_a b dx\n$$\ntail\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        // 在开界 `$$` 行首插入 `$` → 该行变 `$$$`（不再等于 `$$`，围栏消失）。
        let at = ed.read(cx).content().find("$$\n\\int").unwrap();
        let v0 = ed.read(cx).regions_ver();
        ed.update(cx, |e, ecx| e.replace_range(at..at, "$", ecx));
        assert_ne!(
            ed.read(cx).regions_ver(),
            v0,
            "开界行签名变化应整篇重建"
        );
        // 与新文本从零重扫对拍（range + source + align 全比）。
        let new_text = ed.read(cx).content();
        let expect = inline_style::math_regions(&new_text);
        let derived = ed.read(cx).derived();
        let d = derived.borrow();
        let got = &d.math_regions;
        assert_eq!(got.len(), expect.len(), "区域数应一致");
        for (g, e) in got.iter().zip(expect.iter()) {
            assert_eq!(g.range, e.range, "区域行区间应一致");
            assert_eq!(g.source, e.source, "区域 source 应一致");
            assert_eq!(g.align, e.align, "区域 align 应一致");
        }
        drop(d);
        let _f1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
    });
}

/// M5-B3-1 回归（1b）：Enter 落在 `$$ … $$` 块**内部**（跨线区域，内容/边界变）→
/// 增量平移失败 → 整篇重建（`regions_ver++`）。断言重建后 derived 与从零重扫一致
/// （块被拆成两行 body、`source` 用 `\n` 重拼）。
#[gpui::test]
fn enter_inside_math_block_rebuilds_and_stays_consistent(cx: &mut TestAppContext) {
    use crate::markdown::parse::inline_style;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "$$\n\\int_a b dx\n$$\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert_eq!(ed.read(cx).derived().borrow().math_regions.len(), 1);
        // 在 body 行中段按 Enter（结构编辑跨区域）。
        let at = ed.read(cx).content().find("\\int_a").unwrap() + 4;
        let v0 = ed.read(cx).regions_ver();
        ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
        assert_eq!(
            ed.read(cx).regions_ver(),
            v0.wrapping_add(1),
            "块内 Enter 应整篇重建"
        );
        let new_text = ed.read(cx).content();
        let expect = inline_style::math_regions(&new_text);
        let derived = ed.read(cx).derived();
        let d = derived.borrow();
        let got = &d.math_regions;
        assert_eq!(got.len(), expect.len(), "区域数应一致");
        for (g, e) in got.iter().zip(expect.iter()) {
            assert_eq!(g.range, e.range, "区域行区间应一致");
            assert_eq!(g.source, e.source, "source 应含拆行后的 `\\n`");
            assert_eq!(g.align, e.align, "区域 align 应一致");
        }
        assert_eq!(got[0].range, 0usize..4, "块仍应在 0..4 行");
    });
}

#[gpui::test]
fn inline_dollar_on_plain_line_outside_regions_reuses_derived(cx: &mut TestAppContext) {
    // M5-B3-1 子项 3（替换旧 `dollar_edit_rebuilds_derived`）：在**普通行**（class 7，
    // 无区域边界）打 `$`（行内公式如 `$x$abc`）——`math_regions` 只认 `$$` 围栏与整行
    // `$$…$$`，行内 `$` 由「可见带懒光栅」claim（`sync_math`），不构成区域 → 派生
    // **不再整篇重建**，`regions_ver` 不变、`derived` Rc 跨键复用。行签名虽变化（普通行
    // 美元有无翻转）但可精确判定区域结构未变（旧实现每 `$` 键整篇重扫，数学密集文档
    // 就是逐键 O(n) spike 的来源）。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        (d, ed.read(cx).regions_ver())
    });
    // 行首写入一个行内公式 `$x$`（不构成任何区域）。
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| e.replace_range(0..0, "$x$", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(
            std::rc::Rc::ptr_eq(&rc0, &d1),
            "普通行美元翻转不构成区域 → 派生应复用 Rc（不整篇重建）"
        );
        assert_eq!(ed.read(cx).regions_ver(), rv0, "regions 版本应不变");
        assert_eq!(d1.borrow().math_regions.len(), 0, "仍无数学区域");
    });
}

#[gpui::test]
fn one_line_math_creation_rebuilds_derived(cx: &mut TestAppContext) {
    // M5-B3-1 子项 3 的「边界」：把普通行改成**整行 `$$…$$`**（class 7 → 3）→ 真的
    // 新建了一个区域 → 必须整篇重建（`regions_ver++`）。保住「只加速普通情形、区域
    // 创建/销毁保全量」的收口。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("x^2\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        assert_eq!(d.borrow().math_regions.len(), 0, "初始无数学区域");
        (d, ed.read(cx).regions_ver())
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| e.replace_range(0..3, "$$x^2$$", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(
            !std::rc::Rc::ptr_eq(&rc0, &d1),
            "整行变成 `$$…$$` → 新建区域 → 派生应整篇重建"
        );
        assert_ne!(ed.read(cx).regions_ver(), rv0, "regions 版本应递增");
        assert_eq!(
            d1.borrow().math_regions.len(),
            1,
            "应扫到新单行数学区域"
        );
        assert_eq!(d1.borrow().math_regions[0].range, 0usize..1);
        assert_eq!(d1.borrow().math_regions[0].source, "x^2");
    });
}

#[gpui::test]
fn inline_dollar_inside_math_block_body_patches_source(cx: &mut TestAppContext) {
    // M5-B3-1 子项 3：`$$…$$` 块 **body 行**内打 `$`（普通行美元翻转 + 恰在该块内）
    // → 不整篇重建，只就地重拼该块 `source`（离开块卡片显示新公式）；`regions_ver`
    // 不变、区域区间不变。
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("$$\nE = mc\n$$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
    });
    let (rc0, rv0, src0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        let src = d.borrow().math_regions[0].source.clone();
        (d, ed.read(cx).regions_ver(), src)
    });
    assert_eq!(src0, "E = mc");
    // 在 body 行 `E = mc` 的 `m` 前打 `$` → 行变 `E = $mc`（普通行美元翻转，块内 body）。
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("E = mc").unwrap() + 4; // 落在 `m` 前
        ed.update(cx, |e, ecx| e.replace_range(at..at, "$", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(
            std::rc::Rc::ptr_eq(&rc0, &d1),
            "块 body 内美元翻转 → 派生应复用 Rc（不整篇重建）"
        );
        assert_eq!(ed.read(cx).regions_ver(), rv0);
        assert_eq!(d1.borrow().math_regions[0].range, 0usize..3, "块区间不变");
        assert_eq!(
            d1.borrow().math_regions[0].source,
            "E = $mc",
            "块 source 应被就地修补为含 $"
        );
    });
}

#[gpui::test]
fn inline_dollar_near_block_fence_keeps_regions_stable(cx: &mut TestAppContext) {
    // M5-B3-1 子项 3 极值：`$$` 块 **body 行同时携带行内 `$`**（普通行，class 7），
    // 且恰在闭界围栏行之上——行内 `$` 增补（`$cost` → `$$cost`，trim 仍是普通行、
    // 非整行 `$$…$$`）不得改变任何区域边界：`derived` Rc 复用、块区间不变、`source`
    // 就地修补（若误判重建/误动边界即此漏网）。
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("$$\nE = mc^2 $cost\n$$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0, src0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        assert_eq!(d.borrow().math_regions.len(), 1, "1 个 $$ 块");
        assert_eq!(d.borrow().math_regions[0].range, 0usize..3);
        let src = d.borrow().math_regions[0].source.clone();
        (d, ed.read(cx).regions_ver(), src)
    });
    assert_eq!(src0, "E = mc^2 $cost");
    // 闭界围栏行（行 2）之上的 body 行（行 1）：`$cost` 前补 `$` → `$` 现值 $。
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("$cost").unwrap();
        ed.update(cx, |e, ecx| e.replace_range(at..at, "$", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(
            std::rc::Rc::ptr_eq(&rc0, &d1),
            "块内 body 行行内 `$` 增补 → 派生应复用 Rc（不整篇重建）"
        );
        assert_eq!(ed.read(cx).regions_ver(), rv0);
        assert_eq!(d1.borrow().math_regions.len(), 1);
        assert_eq!(d1.borrow().math_regions[0].range, 0usize..3, "区域区间不变");
        assert_eq!(
            d1.borrow().math_regions[0].source,
            "E = mc^2 $$cost",
            "块 source 应被就地修补"
        );
    });
}

#[gpui::test]
fn dollar_fence_removal_rebuilds_derived(cx: &mut TestAppContext) {
    // M5-B3-1 子项 3 的「边界」：把 `$$` 围栏行改成 `$$$`（class 2 → 7）→ 围栏消失、
    // 区域结构真变 → 仍整篇重建。保「$$ 围栏出入保全量」，不被普通行快捷分支误伤。
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("$$\nx\n$$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        assert_eq!(d.borrow().math_regions.len(), 1, "初始 1 个 $$ 块");
        (d, ed.read(cx).regions_ver())
    });
    // 开界行 `$$` 首插 `$` → `$$$`（围栏消失）。
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| e.replace_range(0..0, "$", ecx));
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(
            !std::rc::Rc::ptr_eq(&rc0, &d1),
            "围栏行变 `$$$` → 区域结构变 → 派生应整篇重建"
        );
        assert_ne!(ed.read(cx).regions_ver(), rv0, "regions 版本应递增");
    });
}

#[gpui::test]
fn inline_math_interior_edit_reuses_derived(cx: &mut TestAppContext) {
    // 行内公式**内部**键入（不新增/删除 `$`）：派生不用整篇重建；新公式（`$y$`）
    // 由下一帧 shape 带出到 `frame.rows[].extra.inline_maths` → `sync_math`（可见带懒
    // 光栅）按 `inline:y` claim 占槽（不再需要全篇行内清单）。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("text $x$ here\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d = ed.read(cx).derived();
        (d, ed.read(cx).regions_ver())
    });
    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("$x$").unwrap() + 1;
        ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "y", ecx));
    });
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let d1 = ed.read(cx).derived();
        assert!(std::rc::Rc::ptr_eq(&rc0, &d1), "行内公式内键入不应整篇重建");
        assert_eq!(ed.read(cx).regions_ver(), rv0);
        // 光标移出 `$y$`（否则 reveal 显示源码、不 splice）→ 下一帧带内整形行
        // 带出 `$y$`（sync_math 据此 claim `inline:y`）。
        let n = ed.read(cx).content().len();
        ed.update(cx, |e, _| e.set_caret(n));
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let lx: Vec<_> = frame
            .rows
            .iter()
            .flat_map(|r| r.extra.inline_maths.iter().map(|m| m.latex.to_string()))
            .collect();
        assert_eq!(
            lx,
            vec!["y".to_string()],
            "帧行内公式应已更新为 $y$：{lx:?}"
        );
    });
}

#[gpui::test]
fn fold_plan_reused_across_caret_move_and_reveal_applies(cx: &mut TestAppContext) {
    // 折叠计划缓存**不含 caret**：光标在普通行间移动 → `fold_plan` Rc 复用（不再
    // 每键/每次光标动整算折叠增量——旧实现 48ms/次）；但光标进数学块 → reveal 生效
    // （该块还原源码、不装卡片），离开块又折叠回卡片。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "$$\n\\int_a dx\n$$\n普通段落 one\n普通段落 two\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (plan0, len0) = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(0)); // 文档开头（`$$\n…`）
        let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
        let plan = ed
            .read(cx)
            .fold_plan_rc()
            .expect("build_frame 应已建折叠计划");
        (
            plan,
            f.rows.iter().filter(|r| r.extra.card.is_some()).count(),
        )
    });
    assert_eq!(len0, 0, "caret 在数学块内 → 不装卡片（reveal 显示源码）");
    // 移到普通行（caret 离开数学块）→ fold_plan 复用 + 卡片出现。
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("one").unwrap();
        ed.update(cx, |e, _| e.set_caret(at));
        let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
        let plan1 = ed.read(cx).fold_plan_rc().expect("折叠计划应已缓存");
        assert!(
            std::rc::Rc::ptr_eq(&plan0, &plan1),
            "仅移动光标不应重建折叠计划"
        );
        assert_eq!(
            f.rows.iter().filter(|r| r.extra.card.is_some()).count(),
            1,
            "caret 离开数学块 → 折叠为卡片"
        );
    });
}

/// 行整形缓存（滚动复用）：同带重建不重 shape（仅光标行例外）、重叠带只 shape 新进块。
#[gpui::test]
fn shaped_row_cache_reuses_rows_across_band_rebuilds(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..120 {
        text.push_str(&format!("para line {i} some more words here to wrap\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let n = text.len();
        // 光标放文档末端（不在可见带内）→ 带内无「光标行 reveal」干扰。
        ed.update(cx, |e, _| e.set_caret(n));
        let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 800.));
        let r1 = ed.read(cx).take_reshapes();
        assert!(r1 > 0, "首帧应实际整形：{r1}");
        let cached1 = ed.read(cx).shape_rows_len();
        assert!(cached1 > 0, "首帧后缓存应有条目：{cached1}");
        // 同带重建：全部命中缓存 → 不再整形。
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 800.));
        assert_eq!(
            ed.read(cx).take_reshapes(),
            0,
            "同带重建应全部命中行整形缓存"
        );
        // 重叠带（下移 300px）：只 shape 新进块的少量行，远少于首帧。
        let _f3 = build_frame(ed.read(cx), window, px(900.), (300., 1100.));
        let r3 = ed.read(cx).take_reshapes();
        assert!(r3 < r1, "重叠带只应重排新进块（{r3} < 首帧 {r1}）");
    });
}

/// 代码块高亮完成不得让**整带**缓存 miss：旧实现 `code_parse_gen` 恒自增并进全局
/// `shape_key` → 在代码块内每键打字（内容变 → 新内容解析完成 → gen++）时整带所有
/// 块（含无关段落）全部 miss 重排（真实窗口顶部密集带 24-34ms/键 → bf.shape 6-8ms）。
/// 修复后：`on_code_parse_ready` 只**定向作废该代码块的缓存条目**，段落行全部命中，
/// 每键只重排被编辑/就绪的这一个代码块。
#[gpui::test]
fn code_parse_ready_does_not_reshuffle_whole_band(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 顶部密集带：代码块前 6 段 + 一个大代码块（>8 行走后台高亮路径）+ 后 4 段。
    // 段落行在代码块解析就绪时内容未变 → 必须命中缓存、不得被全局键换代而重排。
    let mut text = String::new();
    for i in 0..6 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    text.push_str("```rust\n");
    for i in 0..12 {
        text.push_str(&format!(
            "fn compute_{i}(a: u32, b: u32) -> u32 {{\n    a + b + {i}\n}}\n"
        ));
    }
    text.push_str("```\n");
    for i in 6..10 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    // 追加大量尾部段落：把光标（文末）推到 (0., 1600.) 带外，排除「光标行
    // reveal 必重排」的干扰（与其它缓存复用测试同法）。
    for i in 10..90 {
        text.push_str(&format!(
            "tail para line {i} some more words to wrap here\n"
        ));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(text.len()));
        // 首帧：带覆盖代码块（全物化 → 整带入缓存）；随后归零整形计数。
        let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        let r1 = ed.read(cx).take_reshapes();
        assert!(r1 > 0, "首帧应实际整形：{r1}");
        // 模拟代码块后台高亮就绪（`on_code_parse_ready` 只带该代码块首源行）。
        let code_src_line = 6; // ```rust 围栏所在源行
        assert_eq!(
            ed.read(cx).doc().line_str(code_src_line),
            "```rust",
            "测试前提：第 {code_src_line} 行应为代码围栏"
        );
        ed.update(cx, |e, ecx| e.on_code_parse_ready(&[code_src_line], ecx));
        // 修复后：只有代码块本身被定向作废重排；段落块全部命中缓存。
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        let r2 = ed.read(cx).take_reshapes();
        assert_eq!(
            r2, 1,
            "代码高亮就绪只应重排该代码块（段落行命中缓存），实际 {r2}/首帧 {r1}"
        );
    });
}

/// 内容完全相同的**重复代码块**共享同一解析键：就绪时必须对**每个**副本块定向作废
/// 缓存（`on_code_parse_ready` 收全部副本的 src_line）——否则旧全局键被移除后第二个
/// 副本滚回带内会永久保持纯文本（旁路上色）。
#[gpui::test]
fn code_parse_ready_rehighlights_identical_duplicate_blocks(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 两个内容逐字节相同的 >8 行 rust 代码块（同一 (lang, code) 键 → 后台只解析一次）。
    let mut text = String::new();
    for i in 0..4 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    let mut code_block = String::from("```rust\n");
    for i in 0..12i32 {
        code_block.push_str(&format!("fn dup_{i}(a: u32) -> u32 {{\n    a + {i}\n}}\n"));
    }
    code_block.push_str("```\n");
    let fence_a = text.len();
    text.push_str(&code_block);
    let fence_b = text.len();
    text.push_str(&code_block);
    for i in 4..90 {
        text.push_str(&format!(
            "tail para line {i} some more words to wrap here\n"
        ));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(text.len())); // 文末 → 带外
        let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        ed.read(cx).take_reshapes();
        // 两个围栏首源行（fence_a/B 是字节偏移 → 转为行号）。
        let la = ed.read(cx).row_col(fence_a).0;
        let lb = ed.read(cx).row_col(fence_b).0;
        assert_ne!(la, lb, "测试前提：两个代码块应位于不同源行");
        // 一次解析就绪 → 对两个相同副本块一并定向作废 → 下帧两个块都重排上色。
        ed.update(cx, |e, ecx| e.on_code_parse_ready(&[la, lb], ecx));
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        let r2 = ed.read(cx).take_reshapes();
        assert_eq!(
            r2, 2,
            "两个相同代码块副本都应被重排上色（旧全局键/旧单块作废只会上一个），实际 {r2}"
        );
    });
}

