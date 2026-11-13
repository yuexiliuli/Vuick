fn img_down(x: f32, y: f32) -> MouseDownEvent {
    MouseDownEvent {
        button: MouseButton::Left,
        position: point(px(x), px(y)),
        modifiers: Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    }
}
fn img_move(x: f32, y: f32) -> MouseMoveEvent {
    MouseMoveEvent {
        position: point(px(x), px(y)),
        pressed_button: None,
        modifiers: Modifiers::default(),
    }
}
/// 与 `img_move` 同但标记**左键物理按住**（Windows 后端 `WM_MOUSEMOVE` 携带 MK_LBUTTON
/// 时 `pressed_button = Some(Left)`）——真实拖拽中的 move 都带这个标记。
fn img_move_held(x: f32, y: f32) -> MouseMoveEvent {
    MouseMoveEvent {
        position: point(px(x), px(y)),
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    }
}
fn img_up(x: f32, y: f32) -> gpui::MouseUpEvent {
    gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: point(px(x), px(y)),
        modifiers: Modifiers::default(),
        click_count: 1,
    }
}

/// 图片编辑测试夹具：按**真实窗口内容宽**建帧并注入、`last_bounds` 置为原点。
/// 返回 `(帧, 内容宽)`。注意：元素经 `notify` 触发重绘后会按真实宽重建帧，
/// 因此测试几何一律从返回的帧/宽推导，不硬编码内容宽。
fn image_harness(
    vcx: &mut VisualTestContext,
    ed: &gpui::Entity<WyEditor>,
) -> (std::rc::Rc<super::Frame>, f32) {
    vcx.update(|window, cx| {
        let width = f32::from(window.viewport_size().width).max(1.);
        let frame = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        assert!(
            frame
                .rows
                .iter()
                .any(|r| r.kind == crate::markdown::ir::IrKind::Image),
            "夹具应含独立图片行"
        );
        let bounds_h = f32::from(frame.total).max(600.);
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(width), px(bounds_h)),
            )));
        });
        let rc = std::rc::Rc::new(frame);
        ed.update(cx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(rc.clone()));
        (rc, width)
    })
}

/// 悬浮到图片盒内 → 编辑按钮亮显（hover_image = 该行）；移出 → 清除。
#[gpui::test]
fn image_hover_toggles_edit_button_line(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap();
    assert_eq!(img.src_line, 0);
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    // 图片盒：x0 = padding(12) + (content_w − w)/2。
    let avail = width - 2.0 * f32::from(frame.padding);
    let cx0 = f32::from(frame.padding) + ((avail - w) / 2.).max(0.);
    // 图片中心 → 命中。
    let mut ev = img_move(cx0 + w / 2., top + h / 2.);
    ed.update(vcx, |e, cx| e.on_mouse_move(&ev, cx));
    assert_eq!(
        ed.read_with(vcx, |e, _| e.hover_image()),
        Some(0),
        "悬浮在图片盒内应记录该行"
    );
    // 移到远离图片处 → 清除。
    ev = img_move(width - 5., 0.);
    ed.update(vcx, |e, cx| e.on_mouse_move(&ev, cx));
    assert_eq!(
        ed.read_with(vcx, |e, _| e.hover_image()),
        None,
        "移出图片盒应清除悬浮"
    );
    // 图片盒之外但行内（题注区）→ 不命中图片盒（不亮按钮）。
    ev = img_move(cx0, top + h + 40.);
    ed.update(vcx, |e, cx| e.on_mouse_move(&ev, cx));
    assert_eq!(
        ed.read_with(vcx, |e, _| e.hover_image()),
        None,
        "题注区不应命中图片盒"
    );
}

/// 窗口级 mouse-move（元素级 `on_mouse_move` 被顶层叠层覆盖时的回退路径）：
/// 指针在 `last_bounds` 内 → 更新悬浮；移出元素 → 清除。
#[gpui::test]
fn image_hover_via_window_mouse_move(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    // 指针在元素内（图片中心）→ 悬浮记录该行。
    let ev = img_move(cx0 + w / 2., top + h / 2.);
    ed.update(vcx, |e, cx| e.on_window_mouse_move(&ev, cx));
    assert_eq!(
        ed.read_with(vcx, |e, _| e.hover_image()),
        Some(0),
        "窗口级移动应记录悬浮图片"
    );
    // 移出元素（last_bounds 之外）→ 清除。
    let ev = img_move(width + 200., 50.);
    ed.update(vcx, |e, cx| e.on_window_mouse_move(&ev, cx));
    assert_eq!(
        ed.read_with(vcx, |e, _| e.hover_image()),
        None,
        "指针移出元素应清除悬浮"
    );
}

/// 点击图片右上角编辑按钮 → 打开编辑卡片（锚定该行、输入框预填当前 alt/src）。
#[gpui::test]
fn image_edit_button_click_opens_card(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let avail = width - 2.0 * f32::from(frame.padding);
    let cx0 = f32::from(frame.padding) + ((avail - w) / 2.).max(0.);
    // 先悬浮（亮起按钮），再点按钮中心（右上角内收 margin+size 的方钮）。
    let btn_x = cx0 + w - super::IMAGE_EDIT_BTN_MARGIN - super::IMAGE_EDIT_BTN_SIZE / 2.;
    let btn_y = top + super::IMAGE_EDIT_BTN_MARGIN + super::IMAGE_EDIT_BTN_SIZE / 2.;
    let ev = img_move(cx0 + w / 2., top + h / 2.);
    ed.update(vcx, |e, cx| e.on_mouse_move(&ev, cx));
    assert_eq!(ed.read_with(vcx, |e, _| e.hover_image()), Some(0));
    // 未打开时点按钮 → 打开。
    let down = img_down(btn_x, btn_y);
    let hit = vcx.update(|_, cx| {
        let f = ed.read(cx).rd.borrow().frame.borrow().clone();
        super::image_edit_button_hit_at(
            ed.read(cx),
            f.as_deref().unwrap(),
            point(px(btn_x), px(btn_y)),
        )
    });
    assert_eq!(
        hit,
        Some(0),
        "点按钮位置应命中编辑按钮（btn={btn_x},{btn_y}）"
    );
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.on_mouse_down(&down, window, cx)));
    assert!(
        ed.read_with(vcx, |e, _| e.image_edit_open()),
        "点编辑按钮应打开卡片"
    );
    assert_eq!(
        ed.read_with(vcx, |e, _| e.image_edit_src_line()),
        Some(0),
        "卡片应锚定该图片行"
    );
    // 输入框预填当前 alt / src。
    vcx.update(|_, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("应有链接输入框");
        let text = ed.read(cx).image_edit_text_input().expect("应有文本输入框");
        assert_eq!(link.read(cx).value().to_string(), "img测试.png");
        assert_eq!(text.read(cx).value().to_string(), "独立图片");
    });
    // 再点同一按钮 → 关闭（toggle）。
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.on_mouse_down(&down, window, cx)));
    assert!(
        !ed.read_with(vcx, |e, _| e.image_edit_open()),
        "再点编辑按钮应关闭卡片"
    );
}

/// 保存卡片：把输入框的链接/文本写回 `![alt](src)`，并关闭卡片。
#[gpui::test]
fn image_edit_card_save_rewrites_source(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.open_image_edit(
                crate::markdown::wysiwyg::ImageEditTarget::Block { src_line: 0 },
                window,
                cx,
            )
        });
    });
    assert!(
        ed.read_with(vcx, |e, _| e.image_edit_open()),
        "程序化打开卡片"
    );
    // 填新值：链接 + 文本。
    vcx.update(|window, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框");
        link.update(cx, |s, cx| s.set_value("assets/new图.png", window, cx));
        let text = ed.read(cx).image_edit_text_input().expect("文本输入框");
        text.update(cx, |s, cx| s.set_value("新描述文本", window, cx));
    });
    vcx.update(|_window, cx| {
        ed.update(cx, |e, cx| e.save_image_edit(cx));
    });
    let out = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(
        out, "![新描述文本](assets/new图.png)\n",
        "保存应改写 alt+src"
    );
    assert!(
        !ed.read_with(vcx, |e, _| e.image_edit_open()),
        "保存后卡片应关闭"
    );
}

/// 不填链接（空 src）保存 → 保留原 src，只改文本。
#[gpui::test]
fn image_edit_save_with_empty_link_keeps_src(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.open_image_edit(
                crate::markdown::wysiwyg::ImageEditTarget::Block { src_line: 0 },
                window,
                cx,
            )
        });
    });
    vcx.update(|window, cx| {
        let text = ed.read(cx).image_edit_text_input().expect("文本输入框");
        text.update(cx, |s, cx| s.set_value("只改文本", window, cx));
    });
    vcx.update(|_window, cx| {
        ed.update(cx, |e, cx| e.save_image_edit(cx));
    });
    let out = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(out, "![只改文本](img测试.png)\n", "空链接应保留原 src");
}

/// 渲染层：打开卡片后 draw，浮层卡片（wy-image-edit-card）真实出现。
#[gpui::test]
fn image_edit_card_renders_as_drawable_overlay(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap()
        .clone();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let avail = width - 2.0 * f32::from(frame.padding);
    let cx0 = f32::from(frame.padding) + ((avail - w) / 2.).max(0.);
    // 悬浮 + 点按钮 → 打开卡片。
    vcx.update(|window, cx| {
        let ev = img_move(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, cx| e.on_mouse_move(&ev, cx));
        let btn_x = cx0 + w - super::IMAGE_EDIT_BTN_MARGIN - super::IMAGE_EDIT_BTN_SIZE / 2.;
        let btn_y = top + super::IMAGE_EDIT_BTN_MARGIN + super::IMAGE_EDIT_BTN_SIZE / 2.;
        let down = img_down(btn_x, btn_y);
        ed.update(cx, |e, cx| e.on_mouse_down(&down, window, cx));
        assert!(ed.read(cx).image_edit_open(), "悬浮+点按钮应打开卡片");
        // 绘制一帧 → 浮层卡片出现（deferred anchored 逃逸裁剪、随内容滚动）。
        let _ = window.draw(cx);
    });
    assert!(
        vcx.debug_bounds("wy-image-edit-card").is_some(),
        "编辑卡片浮层应被绘制（debug_bounds 命中）"
    );
}

/// 点图片**本体** → 选中该图（不落光标进题注文本）；点题注 → 清除选中、光标进 alt。
#[gpui::test]
fn image_click_body_selects_image_not_caption(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap()
        .clone();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    // 点图片中心 → 选中（selected_image=该行、无文本选区、光标不进题注）。
    vcx.update(|window, cx| {
        let down = img_down(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(
            ed.read(cx).selected_image(),
            Some(0),
            "点图片本体应选中该图"
        );
        assert_eq!(ed.read(cx).selection(), None, "选中图片不应产生文本选区");
    });
    // 点题注行（图片盒之下）→ 清除选中，且构成对准 alt 起点的源选区。
    vcx.update(|window, cx| {
        let caption_y = top + h + config::markdown::IMAGE_CAPTION_GAP_PX + 2.;
        let down2 = img_down(cx0, caption_y);
        ed.update(cx, |e, c| e.on_mouse_down(&down2, window, c));
        assert_eq!(ed.read(cx).selected_image(), None, "点题注应清除图片选中");
        // 光标/锚点落在题注内即 alt 源字节（不在图片盒里「跳进题注」的对立面——
        // 点题注本来就该能编辑 alt）。
        assert_eq!(ed.read(cx).caret(), 2, "点题注应把光标放进 alt 起点");
    });
    // 再点图片本体 → 恢复选中（指针移开也不受影响：选中态独立于悬浮）。
    vcx.update(|window, cx| {
        let down3 = img_down(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_down(&down3, window, c));
        assert_eq!(ed.read(cx).selected_image(), Some(0), "再点图片应重新选中");
        // 模块级 mouse-move 到别处只清 hover 不清选中 → 按钮仍常显。
        let ev = img_move(width + 200., 50.);
        ed.update(cx, |e, c| e.on_window_mouse_move(&ev, c));
        assert_eq!(ed.read(cx).hover_image(), None, "移出应清悬浮");
        assert_eq!(
            ed.read(cx).selected_image(),
            Some(0),
            "选中不随指针移出消失"
        );
    });
}

/// 选中图片后 Backspace → 删除整张图片行（含换行），记撤销栈，清除选中态。
#[gpui::test]
fn image_selected_backspace_deletes_line(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![甲](a.png)\n正文\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap()
        .clone();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    vcx.update(|window, cx| {
        let down = img_down(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(ed.read(cx).selected_image(), Some(0));
        ed.update(cx, |e, c| e.backspace(c));
        assert_eq!(
            ed.read(cx).content(),
            "正文\n",
            "Backspace 应删除整张图片行"
        );
        assert_eq!(ed.read(cx).selected_image(), None, "删除后应清除选中态");
    });
}

/// 打开后紧跟「鼠标抬起」：`on_mouse_up → end_selection` 会把帧置空——若 render 期
/// 缺帧就跳过卡片（或画到角落），卡片会「闪烁后失踪、鼠标再动才出现」。回归：抬起
/// 后同一帧重绘，卡片仍在且贴在图片上。
#[gpui::test]
fn image_edit_card_survives_mouse_up(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .unwrap()
        .clone();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    let btn_x = cx0 + w - super::IMAGE_EDIT_BTN_MARGIN - super::IMAGE_EDIT_BTN_SIZE / 2.;
    let btn_y = top + super::IMAGE_EDIT_BTN_MARGIN + super::IMAGE_EDIT_BTN_SIZE / 2.;
    // 悬浮 → 按下（打开）→ 抬起（end_selection 清帧）。
    vcx.update(|window, cx| {
        let ev = img_move(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_move(&ev, c));
        let down = img_down(btn_x, btn_y);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(ed.read(cx).image_edit_open(), "按下应打开卡片");
        ed.update(cx, |e, c| e.on_mouse_up(&img_up(btn_x, btn_y), c));
        assert!(ed.read(cx).image_edit_open(), "抬起不应关闭卡片");
        let _ = window.draw(cx);
    });
    let card = vcx
        .debug_bounds("wy-image-edit-card")
        .expect("抬起后卡片仍应被绘制（不得因帧被清而消失/落角）");
    assert!(
        (f32::from(card.origin.y) - top).abs() < 8.,
        "抬起后卡片 y {} 仍应贴图片行屏幕位 {top}",
        f32::from(card.origin.y)
    );
}

/// 选中图片后 Delete → 删除整张图片行（与 Backspace 同一入口）。
#[gpui::test]
fn image_selected_delete_deletes_line(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![甲](a.png)\n正文\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap()
        .clone();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    vcx.update(|window, cx| {
        let down = img_down(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(ed.read(cx).selected_image(), Some(0));
        ed.update(cx, |e, c| e.delete(c));
        assert_eq!(ed.read(cx).content(), "正文\n", "Delete 应删除整张图片行");
        assert_eq!(ed.read(cx).selected_image(), None, "删除后应清除选中态");
    });
}

/// 卡片锚点必须与「临时重建帧」解耦：打开后 `end_selection` 清帧、紧接着
/// `ensure_frame_for_input` 用兜底 800 宽重建的帧（与真实宽不同）**不得**让卡片
/// 横向移动（修复「卡片在某处突然往左闪烁」：render 期现算 frame 会读到宽度错误
/// 的临时帧）。断言：打开即缓存锚点（同代 origin+frame 得出），清帧 + 换 800 宽帧
/// 后锚点不变——render 永远只读缓存锚点。
#[gpui::test]
fn image_edit_card_anchor_ignores_transient_frame_rebuild(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .unwrap()
        .clone();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    let btn_x = cx0 + w - super::IMAGE_EDIT_BTN_MARGIN - super::IMAGE_EDIT_BTN_SIZE / 2.;
    let btn_y = top + super::IMAGE_EDIT_BTN_MARGIN + super::IMAGE_EDIT_BTN_SIZE / 2.;
    // 打开：锚点按「真实宽 W 的帧 + origin(0,0)」缓存。
    vcx.update(|window, cx| {
        let ev = img_move(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_move(&ev, c));
        ed.update(cx, |e, c| {
            e.on_mouse_down(&img_down(btn_x, btn_y), window, c)
        });
    });
    let expected_l = f32::from(frame.padding)
        + (((width - 2.0 * f32::from(frame.padding)) - super::IMAGE_EDIT_CARD_W) / 2.).max(0.);
    let anchor0 = ed
        .read_with(vcx, |e, _| e.image_edit_anchor.get())
        .expect("打开即缓存锚点");
    assert!(
        (f32::from(anchor0.x) - expected_l).abs() < 1. && (f32::from(anchor0.y) - top).abs() < 1.,
        "打开锚点应贴图（x={:.1} 应≈{expected_l:.1}，y={:.1} 应≈{top:.1}）",
        f32::from(anchor0.x),
        f32::from(anchor0.y)
    );
    // 强制用「兜底 800 宽重建帧」覆盖当前帧（模拟 `ensure_frame_for_input` 在
    // `end_selection` 清帧后的行为；不经过 paint，锚点不得跟着变）。
    vcx.update(|window, cx| {
        let f800 = build_frame(ed.read(cx), window, px(800.), (0., 2048.));
        let rc = std::rc::Rc::new(f800);
        ed.update(cx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(rc));
    });
    let anchor1 = ed
        .read_with(vcx, |e, _| e.image_edit_anchor.get())
        .expect("锚点仍在");
    assert!(
        (f32::from(anchor1.x) - f32::from(anchor0.x)).abs() < 1.
            && (f32::from(anchor1.y) - f32::from(anchor0.y)).abs() < 1.,
        "锚点不得随临时 800 宽重建帧改变（防横向左跳）：({:.1},{:.1}) → ({:.1},{:.1})",
        f32::from(anchor0.x),
        f32::from(anchor0.y),
        f32::from(anchor1.x),
        f32::from(anchor1.y)
    );
}

/// 点**卡片外**任意处 → 关闭卡片（对齐 `ui::menu` 的 on_mouse_down_out 语义）。
#[gpui::test]
fn image_edit_card_click_outside_closes(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == crate::markdown::ir::IrKind::Image)
        .unwrap();
    let (top, w, h) = (
        f32::from(img.top),
        img.image.as_ref().unwrap().w,
        img.image.as_ref().unwrap().h,
    );
    let cx0 =
        f32::from(frame.padding) + ((width - 2.0 * f32::from(frame.padding) - w) / 2.).max(0.);
    // 悬浮 + 点按钮打开卡片。
    let btn_x = cx0 + w - super::IMAGE_EDIT_BTN_MARGIN - super::IMAGE_EDIT_BTN_SIZE / 2.;
    let btn_y = top + super::IMAGE_EDIT_BTN_MARGIN + super::IMAGE_EDIT_BTN_SIZE / 2.;
    let down = img_down(btn_x, btn_y);
    vcx.update(|window, cx| {
        let ev = img_move(cx0 + w / 2., top + h / 2.);
        ed.update(cx, |e, c| e.on_mouse_move(&ev, c));
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
    });
    assert!(
        ed.read_with(vcx, |e, _| e.image_edit_open()),
        "点按钮应打开卡片"
    );
    // 点卡片外的空白区（图片右下方）→ 关闭。
    vcx.simulate_click(point(px(50.), px(460.)), Modifiers::default());
    vcx.run_until_parked();
    assert!(
        !ed.read_with(vcx, |e, _| e.image_edit_open()),
        "点卡片外应关闭卡片"
    );
}

/// 图片调整条（右中边缘）：悬浮/点图本体不误触；按条 → 拖拽实时改宽（保持宽高比、
/// 不改源码），极端拖拽钳到 [MIN, MAX]，松开结束。
#[gpui::test]
fn image_resize_handle_drag_resizes_width(cx: &mut TestAppContext) {
    use config::markdown::{IMAGE_MAX_W, IMAGE_MIN_W};
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, _width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .unwrap()
        .clone();
    let bb = super::image_box_local(&frame, &img).expect("图片盒");
    let hb = super::image_resize_handle_local(&bb);
    let (hx, hy) = (
        f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.,
        f32::from(hb.top()) + f32::from(hb.size.height) / 2.,
    );
    let w0 = img.image.as_ref().unwrap().w;
    // 点**图片本体左中部**（远离右边缘）→ 只选中，不误触调整条。
    vcx.update(|window, cx| {
        let down = img_down(f32::from(bb.left()) + 20., f32::from(bb.top()) + 20.);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(ed.read(cx).selected_image(), Some(0));
        assert!(
            ed.read(cx).image_resize_drag_src().is_none(),
            "点图片中部不应开始调整条拖拽"
        );
    });
    // 点右中边缘调整条 → 开始拖拽 + 选中。
    let down = img_down(hx, hy);
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(
            ed.read(cx)
                .image_resize_drag_src()
                .map(|s| s.to_string())
                .as_deref(),
            Some("img测试.png"),
            "按调整条应开始拖拽"
        );
        assert_eq!(ed.read(cx).selected_image(), Some(0), "拖拽应同时选中该图");
    });
    // 向左拖 60px → 宽缩 60（未触钳制）。
    vcx.update(|_, cx| {
        let mv = img_move_held(hx - 60., hy);
        ed.update(cx, |e, c| e.on_mouse_move(&mv, c));
    });
    let w1 = vcx
        .update(|_, cx| ed.read(cx).image_width("img测试.png"))
        .expect("拖拽后应有覆写宽");
    assert!(
        (w1 - (w0 - 60.)).abs() < 0.5,
        "左拖 60px 应从 {w0} 缩到 {}（实际 {w1}）",
        w0 - 60.
    );
    // 拖极右 → 钳到 MAX；拖极左 → 钳到 MIN。
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_mouse_move(&img_move_held(hx + 3000., hy), c));
    });
    assert_eq!(
        vcx.update(|_, cx| ed.read(cx).image_width("img测试.png")),
        Some(IMAGE_MAX_W),
        "拖到极右应钳到最大宽"
    );
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_mouse_move(&img_move_held(hx - 3000., hy), c));
    });
    assert_eq!(
        vcx.update(|_, cx| ed.read(cx).image_width("img测试.png")),
        Some(IMAGE_MIN_W),
        "拖到极左应钳到最小宽"
    );
    // 松开 → 结束拖拽；源码未被改写。
    vcx.update(|_window, cx| {
        ed.update(cx, |e, c| e.on_mouse_up(&img_up(hx, hy), c));
        assert!(
            ed.read(cx).image_resize_drag_src().is_none(),
            "松开应结束调整条拖拽"
        );
    });
    let out = vcx.update(|_, cx| ed.read(cx).content());
    assert_eq!(out, "![独立图片](img测试.png)\n", "拖拽改宽不得改写源码");
}

/// 回归「首拖不跟手」（用户报告：按住调整条第一次拖动图片宽度不变，先拖一下再点图片
/// 外/别处尺寸才突然改变）：
/// 仅断言 `image_width` 覆写表不够——**重建的帧**必须真的按新宽重排图片行。若行整形
/// 缓存不随图片宽换代失效，`build_frame` 会命中旧整形行（旧 `img.w`），图片仍按旧宽
/// 画 → 首拖无效果；之后只有某次把它逐出缓存的时机才突然跳到目标宽。
#[gpui::test]
fn image_resize_first_drag_reflows_rendered_row(cx: &mut TestAppContext) {
    use config::markdown::IMAGE_MIN_W;
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("some leading paragraph.\n\n![alt](img测试.png)\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = image_harness(vcx, &ed);
    // 图片位于 src_line 2（**非**光标所在行）——真实场景图片行绝大多数不在光标行，
    // 整形行会进 `shape_rows` 缓存被复用；若缓存不随宽换代，首拖就会命中旧行。
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .expect("夹具应含独立图片行")
        .clone();
    assert_eq!(img.src_line, 2);
    assert_ne!(
        vcx.update(|_, cx| ed.read(cx).row_col(ed.read(cx).caret()).0),
        2,
        "前置：光标行 ≠ 图片行"
    );
    let bb = super::image_box_local(&frame, &img).expect("图片盒");
    let hb = super::image_resize_handle_local(&bb);
    let (hx, hy) = (
        f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.,
        f32::from(hb.top()) + f32::from(hb.size.height) / 2.,
    );
    let w0 = img.image.as_ref().unwrap().w;
    assert!(
        w0 > IMAGE_MIN_W + 80.,
        "夹具自然宽应能左拖缩窄（w0={w0}）"
    );
    // 首拖：按下调整条 → 向左移 60px。
    // 故意用**不带按钮标志**的 move（`pressed_button: None`）：真实首拖经窗口级兜底路径
    // 时该标志并不可靠（旧 `pressed_button` 门控正是因此把首拖整段吞掉 → 首拖无效；点
    // 别处才一次性应用）。本回归要求首拖自身即生效，不依赖平台按钮标志。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(hx, hy), window, c));
        ed.update(cx, |e, c| e.on_mouse_move(&img_move(hx - 60., hy), c));
    });
    // 覆写表已按新宽写入（既有测试只断言到这里）……
    let overlay = vcx
        .update(|_, cx| ed.read(cx).image_width("img测试.png"))
        .expect("拖拽后应有覆写宽");
    assert!(
        (overlay - (w0 - 60.)).abs() < 0.5,
        "覆写表应记录 {w0} - 60 = {}，实际 {overlay}",
        w0 - 60.
    );
    // ……但**下一帧重建**必须按新宽重新整形图片行：否则命中的是旧整形行（旧 img.w），
    // 渲染宽度不变 =「第一次拖动无效」。
    let f2 = vcx.update(|window, cx| build_frame(ed.read(cx), window, px(width), (0., 999_999.)));
    let img2 = f2
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .expect("重建帧应含图片行");
    let w1 = img2.image.as_ref().unwrap().w;
    assert!(
        (w1 - (w0 - 60.)).abs() < 0.5,
        "重建帧应按新宽 shape：期望 {} 实际 {w1}（命中旧整形缓存 → 首拖不跟手）",
        w0 - 60.
    );
}

/// 回归「首拖不实时重绘」**真失败条件版**（用户报告：按住调整条第一次拖动，事件层
/// 每帧 `apply`+`set_image_width` 都执行、宽度覆写表也更新，但屏幕一直不动；点图片外/
/// 松手后才「跳」到最终宽度；第二次起才实时跟手）。
///
/// **为何必须用固定渲染宽**：真实渲染路径（element measure/prepaint）恒定用居中阅读栏
/// 宽度（`READING_COL_ENABLED`，body 16px → 921.6），每次拖拽重建帧都在同一宽度 →
/// 行整形缓存**字面命中**旧宽整形行（键 = `block_content_id ^ (width_bits ^ zgen)`，
/// **不含 `images_gen`**，且 `block_content_id` 只含源码文本、拖拽不改源码）→ 重建帧
/// 里图片行的 `img.w` 还是旧值 → paint 画旧宽。上面的 `image_resize_first_drag_reflows_
/// rendered_row` 用 `image_harness` 的**视口宽 1920** 建帧，恰与真实阅读栏 921.6 缓存键
/// 不同 → 重建意外 miss 而漏测；本测试用**同宽两帧**（宽 921.6），先断言图片行真在缓存
/// （同宽同带重建必须 0 整形），再拖拽改宽 → 下一帧同宽重建必须按新宽重排。
///
/// 失败条件 = 「拖拽改宽后，同宽重建的图片行 `img.w` 仍为旧值」；
/// 修复 = `set_image_width`/`set_image_size` 里**定向作废**含该图的整形行缓存条目
/// （`invalidate_image_row_cache`，替代 `images_gen` 进全局 shape_key）。
#[gpui::test]
fn image_resize_first_drag_same_width_rebuild_reflows(cx: &mut TestAppContext) {
    use config::markdown::IMAGE_MIN_W;
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("some leading paragraph.\n\n![alt](img测试.png)\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 光标放文档末端（不在可视带内）→ 排除「光标行 reveal 强制重排」对缓存命中判定的干扰
    // （对齐 `shaped_row_cache_reuses_rows_across_band_rebuilds` 同法）。
    vcx.update(|_, cx| {
        let n = ed.read(cx).len();
        ed.update(cx, |e, _| e.set_caret(n));
    });
    // 固定渲染宽 = **真实居中阅读栏宽度**（`reading_col_px(16.0)` = 72×16×0.8 =
    // 921.59997…，必须用同一函数取到同一 f32 位模式——手写 `px(921.6)` 是 0x44666667，
    // 与它 0x44666666 差 1 个 mantissa bit → `width_bits` 不同 → 行整形缓存键不同 →
    // 拖拽后的重建（渲染路径同款宽）会意外 miss 重排，掩盖「命中旧整形行」这一失败
    // 条件）。两帧同宽同带，且与真实渲染路径同一宽度。
    let w = px(config::markdown::reading_col_px(16.0));
    let (w0, hx, hy, band) = vcx.update(|window, cx| {
        let _g0 = build_frame(ed.read(cx), window, w, (0., 999_999.));
        ed.read(cx).take_reshapes(); // 清掉首次建帧的整形计数
        let g1 = build_frame(ed.read(cx), window, w, (0., 999_999.));
        let b_img = ed.read(cx).doc().block_index_for_line(2);
        // 图片行必须先真正进入行整形缓存（否则重建走 miss 重排，掩盖「命中旧整形行」）。
        assert!(
            ed.read(cx).shape_rows_has(b_img),
            "前置：图片行应先进入行整形缓存（block {b_img}）"
        );
        let rc = std::rc::Rc::new(g1);
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(rc.clone());
            // 元素宽 = 帧宽（阅读栏宽）→ 阅读栏移位 = 0，窗口坐标与列内坐标一致
            // （对齐 `image_harness` 的「last_bounds 宽 = 帧宽」做法；否则元素默认宽
            // 1920 会把阅读栏居中右移 ~499px，命中几何与拖拽换算错轨）。
            e.set_last_bounds(Some(gpui::Bounds::new(
                gpui::point(px(0.), px(0.)),
                gpui::size(w, px(600.)),
            )));
        });
        let img = rc
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("夹具应含独立图片行")
            .clone();
        assert_eq!(img.src_line, 2);
        let w0 = img.image.as_ref().unwrap().w;
        assert!(w0 > IMAGE_MIN_W + 80., "夹具自然宽应能左拖缩窄（w0={w0}）");
        let bb = super::image_box_local(&rc, &img).expect("图片盒");
        let hb = super::image_resize_handle_local(&bb);
        let hx = f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.;
        let hy = f32::from(hb.top()) + f32::from(hb.size.height) / 2.;
        (w0, hx, hy, (0., 999_999.))
    });
    // 首拖：按调整条 → 左移 60px（事件层：begin + apply + 覆写表更新，均正常）。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(hx, hy), window, c));
        ed.update(cx, |e, c| e.on_mouse_move(&img_move(hx - 60., hy), c));
    });
    let overlay = vcx
        .update(|_, cx| ed.read(cx).image_width("img测试.png"))
        .expect("拖拽后应有覆写宽");
    assert!(
        (overlay - (w0 - 60.)).abs() < 0.5,
        "覆写表应记录 {w0} - 60 = {}，实际 {overlay}",
        w0 - 60.
    );
    // **失败条件**：下一帧**同宽同带**重建必须按新宽重新整形图片行。若行整形缓存未随
    // 宽换代失效（命中旧整形行），重建帧的 `img.w` 仍是旧值 → 首拖不实时重绘。
    let f2 = vcx.update(|window, cx| build_frame(ed.read(cx), window, w, band));
    let img2 = f2
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .expect("重建帧应含图片行");
    let w1 = img2.image.as_ref().unwrap().w;
    assert!(
        (w1 - (w0 - 60.)).abs() < 0.5,
        "同宽重建应按新宽 shape：期望 {} 实际 {w1}（命中旧整形缓存 → 首拖不跟手）",
        w0 - 60.
    );
}

/// 回归（用户报告「点到图片外 → 尺寸突然改变」，残留状态清除边界①）：
/// 按调整条后若松开点落在元素/叠层外，元素级 `on_mouse_up`（`hitbox.is_hovered` 门控）
/// 未派发 → 状态残留。此刻**任何一次新左键按下**（如点图片外空白处）都必须在处理前把
/// 残留调整条清掉（`on_mouse_down` 顶部 `end_image_resize`），之后移动绝不改宽。
#[gpui::test]
fn image_resize_click_elsewhere_clears_residual(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, _width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .unwrap()
        .clone();
    let bb = super::image_box_local(&frame, &img).expect("图片盒");
    let hb = super::image_resize_handle_local(&bb);
    let (hx, hy) = (
        f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.,
        f32::from(hb.top()) + f32::from(hb.size.height) / 2.,
    );
    // 按调整条 → 开始拖拽。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(hx, hy), window, c));
        assert!(
            ed.read(cx).image_resize_drag_src().is_some(),
            "按调整条应开始拖拽"
        );
    });
    // 【不发 mouse_up】模拟松开点落空（元素级 on_mouse_up 未派发）→ 状态残留。
    // 之后点图片外（左下方空白区，非调整条）→ 残留状态必须先被清除。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(40., 500.), window, c));
        assert!(
            ed.read(cx).image_resize_drag_src().is_none(),
            "点图片外应先清残留的调整条状态"
        );
    });
    // 随后任何移动（元素级 + 窗口级兜底两条路径）都不得改宽。
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_mouse_move(&img_move(hx - 200., hy), c));
        ed.update(cx, |e, c| e.on_window_mouse_move(&img_move(hx - 200., hy), c));
    });
    assert_eq!(
        vcx.update(|_, cx| ed.read(cx).image_width("img测试.png")),
        None,
        "点图片外后移动不得改宽"
    );
}

/// 回归（「松开后移动仍改宽」的残留防护，替换旧的 `pressed_button` 门控断言）：
/// 拖拽改宽只以 `image_resize_drag` 状态为准（按下命中句柄才置位）。真实的松开动作
/// （元素内 `on_mouse_up` / 元素外 `on_mouse_up_out` / 窗口级 `on_window_mouse_up` / 任何
/// 新左键 `on_mouse_down`）都会把状态清掉，之后**无论有无按键**的移动都不得再改宽。
/// 不再依赖 `pressed_button` 标志（Windows 首拖经窗口级兜底路径该标志并不可靠——依赖它
/// 会让首拖整段失效；清态改由上述边界闭环，对齐本仓 scrollbar/video/image 拖拽惯例）。
#[gpui::test]
fn image_resize_release_boundary_stops_moves(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, _width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .unwrap()
        .clone();
    let bb = super::image_box_local(&frame, &img).expect("图片盒");
    let hb = super::image_resize_handle_local(&bb);
    let (hx, hy) = (
        f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.,
        f32::from(hb.top()) + f32::from(hb.size.height) / 2.,
    );
    let w0 = img.image.as_ref().unwrap().w;
    // 按下调整条 → 拖拽开始。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(hx, hy), window, c));
    });
    assert!(
        ed.read_with(vcx, |e, _| e.image_resize_drag_src().is_some()),
        "按调整条应开始拖拽"
    );
    // 按住拖动（窗口级兜底路径）→ 跟手改宽 = w0 − 60（首拖即生效，不等平台按钮标志）。
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_window_mouse_move(&img_move_held(hx - 60., hy), c));
    });
    let w1 = vcx
        .update(|_, cx| ed.read(cx).image_width("img测试.png"))
        .expect("按住移动后应有覆写宽");
    assert!(
        (w1 - (w0 - 60.)).abs() < 0.5,
        "按住左键移动应从 {w0} 缩到 {}（实际 {w1}）",
        w0 - 60.
    );
    // 拖拽结束的**真实边界**：松开点落在元素外（元素级 `on_mouse_up` 不派发）→
    // `on_mouse_up_out` 清状态。
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_window_mouse_up(c));
    });
    assert!(
        ed.read_with(vcx, |e, _| e.image_resize_drag_src().is_none()),
        "松开（窗口级/up_out 边界）应清掉拖拽状态"
    );
    // 之后：无按键纯移动 + 按住移动，两条路径都不得再改宽。
    let w_frozen = vcx
        .update(|_, cx| ed.read(cx).image_width("img测试.png"))
        .expect("松开后覆写宽应保留已拖值");
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_mouse_move(&img_move(hx - 200., hy), c));
        ed.update(cx, |e, c| e.on_window_mouse_move(&img_move(hx - 200., hy), c));
        ed.update(cx, |e, c| e.on_window_mouse_move(&img_move_held(hx - 300., hy), c));
    });
    assert_eq!(
        vcx.update(|_, cx| ed.read(cx).image_width("img测试.png")),
        Some(w_frozen),
        "松开边界后移动（无论有无按键）不得再改宽"
    );
}

/// 回归（残留状态清除边界④）：**窗口级 mouse-up 兜底**——松开点落在元素外/叠层上
/// （元素级 `on_mouse_up` 未派发）时，窗口级 `on_window_mouse_up` 必须把残留调整条状态
/// 清掉；之后即使按住左键移动，也不再改宽。
#[gpui::test]
fn image_resize_window_mouse_up_clears_state(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![独立图片](img测试.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, _width) = image_harness(vcx, &ed);
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .unwrap()
        .clone();
    let bb = super::image_box_local(&frame, &img).expect("图片盒");
    let hb = super::image_resize_handle_local(&bb);
    let (hx, hy) = (
        f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.,
        f32::from(hb.top()) + f32::from(hb.size.height) / 2.,
    );
    // 按调整条 → 拖拽中。此刻模拟「松开点在元素外」：只派发窗口级 MouseUp 兜底。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(hx, hy), window, c));
        assert!(ed.read(cx).image_resize_drag_src().is_some(), "按下应拖拽");
        ed.update(cx, |e, c| e.on_window_mouse_up(c));
        assert!(
            ed.read(cx).image_resize_drag_src().is_none(),
            "窗口级 mouse-up 兜底应清残留调整条状态"
        );
    });
    // 之后按住左键移动 → 状态已清 → 不得改宽。
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.on_window_mouse_move(&img_move_held(hx - 200., hy), c));
    });
    assert_eq!(
        vcx.update(|_, cx| ed.read(cx).image_width("img测试.png")),
        None,
        "mouse_up 兜底后移动不得改宽"
    );
}

/// M3 A8 回归：`invalidate_image_row_cache` 经「图片 `src` → 块下标」**反向索引**
/// （`image_src_blocks`）在**缺帧**（`frame = None`，旧实现此处退化为逐块
/// `parse_image_line` 全量兜底 O(块数)）时仍**精确**作废「引用该 src 的块」：
/// 同 src 多块全作废、**不误伤**其它 src 图片块与无关文本块；连续两次不同 src 失效
/// 各自只命中自己的块（第二次命中索引 O(1) 路径，无需重建）。
#[gpui::test]
fn image_src_blocks_invalidate_only_referencing_rows(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text(
                "para before\n\n![A](shared.png)\n\n![B](shared.png)\n\n![C](other.png)\n",
            )
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 光标放文档末端 → 排除「光标行 reveal 强制重排」对缓存命中判定的干扰。
    vcx.update(|_, cx| {
        let n = ed.read(cx).len();
        ed.update(cx, |e, _| e.set_caret(n));
    });
    // 同带两帧：整篇四块（段落 + 两幅 `shared.png` + 一幅 `other.png`）都排进整形缓存。
    let width = vcx.update(|window, _| f32::from(window.viewport_size().width).max(1.));
    vcx.update(|window, cx| {
        let _g0 = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        ed.read(cx).take_reshapes();
        let _g1 = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
    });
    let para_b = ed.read_with(vcx, |e, _| e.doc().block_index_for_line(0));
    let a_b = ed.read_with(vcx, |e, _| e.doc().block_index_for_line(2));
    let b_b = ed.read_with(vcx, |e, _| e.doc().block_index_for_line(4));
    let c_b = ed.read_with(vcx, |e, _| e.doc().block_index_for_line(6));
    let real_rows = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        f.rows
            .iter()
            .filter(|r| r.kind == IrKind::Image)
            .count()
    });
    assert_eq!(real_rows, 3, "夹具应含 3 幅独立图片行");
    for blk in [para_b, a_b, b_b, c_b] {
        assert!(
            ed.read_with(vcx, |e, _| e.shape_rows_has(blk)),
            "前置：块 {blk} 应先入行整形缓存"
        );
    }
    // 缺帧（旧实现此处必须走全量兜底；新实现走反向索引）→ 仍须精确定位。
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = None);
    });
    // 失效 `shared.png` → 只命中引用它的两块。
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| e.set_image_size("shared.png".into(), Some((200., 100.))));
    });
    assert!(
        !ed.read_with(vcx, |e, _| e.shape_rows_has(a_b)),
        "同 src 块 A 应作废"
    );
    assert!(
        !ed.read_with(vcx, |e, _| e.shape_rows_has(b_b)),
        "同 src 块 B 应作废"
    );
    assert!(
        ed.read_with(vcx, |e, _| e.shape_rows_has(c_b)),
        "其它 src 块应保留"
    );
    assert!(
        ed.read_with(vcx, |e, _| e.shape_rows_has(para_b)),
        "无关文本块应保留"
    );
    // 再失效 `other.png` → 只命中它（第二次失效走索引 O(1) 命中路径，无需重建）。
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| e.set_image_size("other.png".into(), Some((200., 100.))));
    });
    assert!(
        !ed.read_with(vcx, |e, _| e.shape_rows_has(a_b)),
        "shared 块仍保持已作废（二次失效不应补回/误作废其它块）"
    );
    assert!(
        !ed.read_with(vcx, |e, _| e.shape_rows_has(c_b)),
        "other.png 块应作废"
    );
    assert!(
        ed.read_with(vcx, |e, _| e.shape_rows_has(para_b)),
        "无关文本块仍保留"
    );
}

/// M3 A4 回归（稀疏增量核心）：**图片「占位→真实」与「拖拽改宽」都不再进基础布局代
/// `height_epoch`**——`set_image_size`/`set_image_width` 收录 `pending_image_srcs`，
/// `build_frame` 在**基表复用**路径 `patch_pending_image_heights` 只对**含图块**做稀疏
/// Fenwick 点更新 → 免占位→真实 / 每 tick 拖拽的近全量 Pass1 估算重跑（mgen 与 epoch
/// 分工对齐：一处构建 = `height_epoch` 的 pass1、一处失效 = 稀疏覆写，公式同源）。
/// 高度一致性守恒：带内/带外图片块高与帧 `total` 都收敛到「真实尺寸 × 当前宽覆写」的
/// 精确值（量程/首屏不缺内容），内容/换宽才换代（`ensure_block_layout` 重建）。
#[gpui::test]
fn image_height_change_sparse_no_epoch_bump(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("para before.\n\n![alt](img测试.png)\n\nafter para.\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let w = px(config::markdown::reading_col_px(16.0));
    // 占位帧：图片未加载 → 占位高（IMAGE_ROW_DEFAULT_H），并记录基础布局代。
    let (epoch_p, b_img) = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, w, (0., 999_999.));
        let img = f
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("含图")
            .clone();
        assert_eq!(
            img.image.as_ref().unwrap().h,
            config::markdown::IMAGE_ROW_DEFAULT_H,
            "未加载应占位高"
        );
        let b_img = ed.read(cx).doc().block_index_for_line(2);
        (ed.read(cx).block_layout_epoch().expect("基表已建"), b_img)
    });
    // ① 占位→真实：注入真实尺寸（200×100）→ 只收录 src + 稀疏覆写，**不进代**。
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| e.set_image_size("img测试.png".into(), Some((200., 100.))));
    });
    assert_eq!(
        ed.read_with(vcx, |e, _| e.block_layout_epoch()),
        Some(epoch_p),
        "占位→真实不得进基础布局代（旧实现：images_gen 进代 → 全量 Pass1）"
    );
    let (w0, h0, para, total1) = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, w, (0., 999_999.));
        let img = f
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("含图")
            .clone();
        let w0 = img.image.as_ref().unwrap().w;
        let h0 = img.image.as_ref().unwrap().h;
        assert!(
            (h0 - w0 / 2.).abs() < 0.51,
            "200×100 → 图高 = 宽/2：w0={w0} h0={h0}"
        );
        (w0, h0, f32::from(img.lh), f32::from(f.total))
    });
    // 带外块高也打准（量程正确）：窄带帧读共享基表，图片块高 = 真实尺寸 + 题注。
    vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, w, (0., 50.));
        let bh = f.block_height(b_img);
        let expect = h0 + config::markdown::IMAGE_CAPTION_GAP_PX + para;
        assert!(
            (bh - expect).abs() < 0.51,
            "带外图片块高应 = 真实尺寸+题注：{bh} vs {expect}"
        );
        assert!(
            (f32::from(f.total) - total1).abs() < 0.51,
            "窄带帧 total 与全带一致（共享同一基表）"
        );
    });
    // ② 拖拽改宽：只覆写该图块高，**仍不进代**。
    let shrink = 60.;
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| e.set_image_width("img测试.png".into(), w0 - shrink));
    });
    assert_eq!(
        ed.read_with(vcx, |e, _| e.block_layout_epoch()),
        Some(epoch_p),
        "拖拽改宽不得进基础布局代（稀疏覆写）"
    );
    let (h2, total2) = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, w, (0., 999_999.));
        let img = f
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("含图")
            .clone();
        let w2 = img.image.as_ref().unwrap().w;
        let h2 = img.image.as_ref().unwrap().h;
        assert!(
            (w2 - (w0 - shrink)).abs() < 0.51,
            "覆写宽生效：期望 {} 实际 {w2}",
            w0 - shrink
        );
        assert!(
            (h2 - (w2 / 2.)).abs() < 0.51,
            "图高按真实比例：w2={w2} h2={h2}"
        );
        (h2, f32::from(f.total))
    });
    assert!(
        (total2 - (total1 - (h0 - h2))).abs() < 0.51,
        "量程 = 原量程 − 图高差({}−{})：期望 {} 实际 {total2}",
        h0,
        h2,
        total1 - (h0 - h2)
    );
}

/// M3 A4 回归（必读，真实渲染路径）：**拖拽多 tick 收敛到真实尺寸 + 高度/量程精确**。
/// 事件层真实走 `on_mouse_down`（命中调整条）→ 多次 `on_mouse_move`
/// （`apply_image_resize` → `set_image_width`）的**稀疏覆写**；图片用本仓真实 fixture
/// `fixtures/img测试.png`（真实渲染路径读真实尺寸，占位→真实也走稀疏覆写，不进代）。
/// 收尾重建帧断言行宽/行高/块基础高/帧 `total` 都按「当前宽 × 真实宽高比」首帧即收敛
/// （无跳变/回弹），且**全程基础布局代不变**（免每 tick 近全量 Pass1）；无关前导段仍
/// 留在整形缓存（非整带重排）。
#[gpui::test]
fn image_drag_multitick_sparse_converges(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/img测试.png").replace('\\', "/");
    // 缺夹具即 SKIP（同 pdf 惯例：夹具不入库，本机无 PNG 时跳过真尺寸断言）。
    if !std::path::Path::new(&fixture).exists() {
        eprintln!("SKIP: missing fixtures/img测试.png（夹具不入库，本机无真 PNG）");
        return;
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let text = format!("para before.\n\n![alt]({fixture})\n\nafter para.\n");
        WyHost {
            ed: cx.new(|cx| WyEditor::new(window, cx).with_text(&text)),
        }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 光标放文末：排除「光标行 reveal 强制重排」对稀疏整形断言干扰（对齐 A8/A9 惯例）。
    vcx.update(|_, cx| {
        let n = ed.read(cx).len();
        ed.update(cx, |e, _| e.set_caret(n));
    });
    let w = px(config::markdown::reading_col_px(16.0));
    let (epoch0, ratio, w0, h0, para, total0, hx, hy, b_img) = vcx.update(|window, cx| {
        let _g0 = build_frame(ed.read(cx), window, w, (0., 999_999.));
        ed.read(cx).take_reshapes(); // 清首帧整形计数
        let g1 = build_frame(ed.read(cx), window, w, (0., 999_999.));
        let b_img = ed.read(cx).doc().block_index_for_line(2);
        let img = g1
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("含图")
            .clone();
        let w0 = img.image.as_ref().unwrap().w;
        let h0 = img.image.as_ref().unwrap().h;
        assert!(
            w0 > config::markdown::IMAGE_MIN_W + 80.,
            "自然宽应能左拖缩窄（w0={w0}）"
        );
        let ratio = h0 / w0.max(1.);
        let para = f32::from(img.lh);
        let rc = std::rc::Rc::new(g1);
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(rc.clone());
            // 元素宽 = 帧宽（阅读栏宽）→ 阅读栏移位 = 0，窗口坐标与列内坐标一致。
            e.set_last_bounds(Some(gpui::Bounds::new(
                gpui::point(px(0.), px(0.)),
                gpui::size(w, px(600.)),
            )));
        });
        let bb = super::image_box_local(&rc, &img).expect("图片盒");
        let hb = super::image_resize_handle_local(&bb);
        let hx = f32::from(hb.left()) + super::IMAGE_RESIZE_HANDLE_W / 2.;
        let hy = f32::from(hb.top()) + f32::from(hb.size.height) / 2.;
        (
            ed.read(cx).block_layout_epoch().expect("基表已建"),
            ratio,
            w0,
            h0,
            para,
            f32::from(rc.total),
            hx,
            hy,
            b_img,
        )
    });
    // 按调整条一次 → 多 tick 左拖（每 tick 一次 apply_image_resize → set_image_width）。
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| e.on_mouse_down(&img_down(hx, hy), window, c));
    });
    assert!(
        ed.read_with(vcx, |e, _| e.image_resize_drag_src().is_some()),
        "按下调整条应开始拖拽"
    );
    // 真实 fixture 经 paint 异步加载完成 → 尺寸保持真实（load 路径 set_image_size 走
    // 稀疏覆写，不进代）。
    for dx in [20., 40., 60.] {
        vcx.update(|_, cx| {
            ed.update(cx, |e, c| e.on_mouse_move(&img_move_held(hx - dx, hy), c));
        });
        let ov = vcx
            .update(|_, cx| ed.read(cx).image_width(fixture.as_str()))
            .expect("拖拽中应有覆写宽");
        assert!(
            (ov - (w0 - dx)).abs() < 0.5,
            "tick dx={dx}：覆写宽应 {}=实际 {ov}",
            w0 - dx
        );
        // 每 tick 都不得进基础布局代（否则 = 近全量 Pass1 重建）。
        assert_eq!(
            ed.read_with(vcx, |e, _| e.block_layout_epoch()),
            Some(epoch0),
            "拖拽 tick dx={dx} 不得进基础布局代（稀疏覆写）"
        );
    }
    // 收尾重建：行宽/行高/块基础高/帧 total 首帧即收敛；只重排受影响块（稀疏）。
    let resh = vcx.update(|window, cx| {
        ed.read(cx).take_reshapes();
        let f = build_frame(ed.read(cx), window, w, (0., 999_999.));
        let r = ed.read(cx).take_reshapes();
        let img = f
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("含图")
            .clone();
        let w1 = img.image.as_ref().unwrap().w;
        let h1 = img.image.as_ref().unwrap().h;
        assert!(
            (w1 - (w0 - 60.)).abs() < 0.51,
            "行宽收敛到 {}=实际 {w1}",
            w0 - 60.
        );
        assert!(
            (h1 - (w0 - 60.) * ratio).abs() < 0.51,
            "行高按真实比例收敛：期望 {} 实际 {h1}",
            (w0 - 60.) * ratio
        );
        let bh = ed
            .read(cx)
            .block_base_height(b_img)
            .expect("基表应存在");
        let expect = h1 + config::markdown::IMAGE_CAPTION_GAP_PX + para;
        assert!(
            (bh - expect).abs() < 0.51,
            "块基础高 = 图高+题注：期望 {expect} 实际 {bh}"
        );
        assert!(
            (f32::from(f.total) - (total0 - (h0 - h1))).abs() < 0.51,
            "量程 = 原量程 − 图高差：期望 {} 实际 {}",
            total0 - (h0 - h1),
            f32::from(f.total)
        );
        r
    });
    assert!(
        (1..=3).contains(&resh),
        "收尾重建只重排受影响块（图片 + 光标尾行），非整带：resh={resh}"
    );
    // 无关前导段落仍留在整形缓存（未整带清空）。
    let lead_b = ed.read_with(vcx, |e, _| e.doc().block_index_for_line(0));
    assert!(
        ed.read_with(vcx, |e, _| e.shape_rows_has(lead_b)),
        "无关前导段应保留在整形缓存（非整带重排）"
    );
}

/* ============================ 行内图片编辑（M6） ============================ */

/// 行内图测试夹具：用**元素真实渲染同宽**（阅读栏宽 `reading_col_px(16.0)`）建帧并注入、
/// `last_bounds` 置原点（宽=帧宽 → 阅读栏移位恒 0，元素局部==窗口坐标）。返回帧 `Rc`。
/// ⚠️ 图片「选中/删除」等动作会清空帧（对齐 `select_image`/`replace_range` 语义）——
/// 这类动作之后、再次命中前要**重调本夹具**重新注入，才能保证命中几何与当前帧一致。
fn inline_image_harness(
    vcx: &mut VisualTestContext,
    ed: &gpui::Entity<WyEditor>,
) -> std::rc::Rc<super::Frame> {
    vcx.update(|window, cx| {
        let w = px(config::markdown::reading_col_px(16.0));
        let frame = build_frame(ed.read(cx), window, w, (0., 999_999.));
        assert!(
            frame
                .rows
                .iter()
                .any(|r| !r.extra.inline_images.is_empty()),
            "夹具应含行内图片行"
        );
        let bounds_h = f32::from(frame.total).max(600.);
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(w, px(bounds_h)),
            )));
        });
        let rc = std::rc::Rc::new(frame);
        ed.update(cx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(rc.clone()));
        rc
    })
}

/// 第一幅行内图的**元素局部**显示盒（与 paint/命中共用同一几何）。
fn first_inline_local(frame: &std::rc::Rc<super::Frame>) -> super::Bounds<gpui::Pixels> {
    let para = frame
        .rows
        .iter()
        .find(|r| !r.extra.inline_images.is_empty())
        .expect("夹具应含行内图");
    let ii = &para.extra.inline_images[0];
    super::inline_image_box_local(frame, para, ii).expect("行内图显示盒")
}

/// 元素局部矩形 → **当下 live** 的窗口矩形（内容列屏幕原点 + 阅读栏移位）。
/// ⚠️ 必须**与使用它的鼠标事件同一次 `vcx.update` 内**调用——元素 prepaint/重绘会随时
/// 更新 `last_bounds`/帧（测试里真实渲染路径会刷新几何），隔一次 update 就可能是旧坐标。
fn win_of(
    ed: &gpui::Entity<WyEditor>,
    cx: &mut gpui::App,
    local: &super::Bounds<gpui::Pixels>,
) -> super::Bounds<gpui::Pixels> {
    let o = ed.read(cx).content_origin();
    super::Bounds::new(
        point(o.x + local.left(), o.y + local.top()),
        local.size,
    )
}

/// 行内图「本体」点击点：盒左缘内 3px、垂直居中——**避开右上角编辑按钮区**（小行内图
/// 的 16px 按钮几乎盖住整盒，只剩左缘不带按钮），确保走「点图选中」分支而非「开卡」。
fn body_click_pt(win_box: &super::Bounds<gpui::Pixels>) -> (f32, f32) {
    (
        f32::from(win_box.left()) + 3.,
        f32::from(win_box.top()) + f32::from(win_box.size.height) / 2.,
    )
}

/// 点击行内图**本体** → 选中（selected_inline_image = 该图、无文本选区、光标不落在图上）；
/// 再点同一图保持选中；点图上文字外的文本区 → 清除选中并落光标到该文本。
#[gpui::test]
fn inline_image_body_click_selects_without_caret(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::InlineImageRef;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "前文 ![内联蓝块](shot_blue.png) 后文\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    let ii0_off = frame
        .rows
        .iter()
        .find(|r| !r.extra.inline_images.is_empty())
        .unwrap()
        .extra
        .inline_images[0]
        .display_off;
    // 点盒本体（避开右上角编辑按钮，左缘）→ 选中：无文本选区、光标不落图上。
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        let sel: InlineImageRef = ed.read(cx).selected_inline_image().expect("应选中行内图");
        assert_eq!(sel.src_line, 0);
        assert_eq!(sel.display_off, ii0_off);
        assert_eq!(ed.read(cx).selection(), None, "选中图片不应产生文本选区");
    });
    let sel1 = ed
        .read_with(vcx, |e, _| e.selected_inline_image())
        .expect("选中态应保留");
    let (set0, set1) = vcx.update(|_w, cx| {
        let e = ed.read(cx);
        (
            e.inline_image_source_range_of(sel1).expect("应能反解源区间").0,
            e.caret(),
        )
    });
    assert_eq!(sel1.start, set0.start, "反解的源区间起点应即 `!` 位");
    assert!(
        set1 < set0.start || set1 >= set0.end,
        "光标 {} 不得落在图源区间 {:?}（不产生落在图上的闪烁光标）",
        set1,
        set0
    );
    // 选中清帧 → 重注入（命中几何与当前帧一致）→ 再点同一图 → 保持选中（无文本选区）。
    let _ = inline_image_harness(vcx, &ed);
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(ed.read(cx).selected_inline_image(), Some(sel1), "再点应保持选中");
        assert_eq!(ed.read(cx).selection(), None, "再点不应产生文本选区");
    });
    // 点图上文字区（盒右缘之外）→ 清除图片选中，光标落回文本。
    let _ = inline_image_harness(vcx, &ed);
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        ed.update(
            cx,
            |e, c| {
                e.on_mouse_down(
                    &img_down(
                        f32::from(w.right()) + 40.,
                        f32::from(w.top()) + f32::from(w.size.height) / 2.,
                    ),
                    window,
                    c,
                )
            },
        );
        assert_eq!(ed.read(cx).selected_inline_image(), None, "点文本应清除图片选中");
    });
}

/// 悬浮行内图 → 右上角编辑按钮亮显（hover_inline_image = 该图）；点按钮 → 打开编辑卡
///（同块图表单、输入框预填 alt/src）；再点同按钮 → 关闭（toggle）。
#[gpui::test]
fn inline_image_hover_button_opens_card(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("前文 ![内联蓝块](shot_blue.png) 后文\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    let btn_local = super::inline_image_edit_button_local(&bb_local);
    // 悬浮到盒内 → hover 记录该图（live 窗口坐标，与事件同一 update）。
    vcx.update(|_window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let mv = img_move(f32::from(w.left()) + 4., f32::from(w.top()) + 4.);
        ed.update(cx, |e, c| e.on_mouse_move(&mv, c));
    });
    let hov = ed
        .read_with(vcx, |e, _| e.hover_inline_image())
        .expect("悬浮应记录行内图");
    assert_eq!(hov.src_line, 0);
    // 点按钮中心（live 窗口坐标；与块图同几何）→ 打开编辑卡。
    vcx.update(|window, cx| {
        let b = win_of(&ed, cx, &btn_local);
        let down = img_down(
            f32::from(b.left()) + super::INLINE_IMAGE_EDIT_BTN_SIZE / 2.,
            f32::from(b.top()) + super::INLINE_IMAGE_EDIT_BTN_SIZE / 2.,
        );
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(ed.read(cx).image_edit_open(), "点行内图编辑按钮应打开卡片");
        assert_eq!(ed.read(cx).image_edit_src_line(), Some(0));
    });
    // 输入框预填当前 alt / src。
    vcx.update(|_, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框");
        let text = ed.read(cx).image_edit_text_input().expect("文本输入框");
        assert_eq!(link.read(cx).value().to_string(), "shot_blue.png");
        assert_eq!(text.read(cx).value().to_string(), "内联蓝块");
    });
    // 再点同一按钮 → 关闭（toggle）。
    vcx.update(|window, cx| {
        let b = win_of(&ed, cx, &btn_local);
        let down = img_down(
            f32::from(b.left()) + super::INLINE_IMAGE_EDIT_BTN_SIZE / 2.,
            f32::from(b.top()) + super::INLINE_IMAGE_EDIT_BTN_SIZE / 2.,
        );
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(!ed.read(cx).image_edit_open(), "再点按钮应关闭卡片");
    });
}

/// 卡片保存：改写行内 `![…](…)` 段（只改该图，周围文字原位保留），并关闭卡片。
#[gpui::test]
fn inline_image_card_save_rewrites_only_span(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::ImageEditTarget;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("前文 ![内联蓝块](shot_blue.png) 后文\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    // 点图本体（避开编辑按钮）选中 → 拿到 refd → 程序化打开编辑卡（与点按钮同目标）。
    let refd = vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        ed.read(cx).selected_inline_image().expect("点图应选中")
    });
    vcx.update(|window, cx| {
        ed.update(cx, |e, c| {
            e.open_image_edit(ImageEditTarget::Inline(refd), window, c)
        });
    });
    assert!(ed.read_with(vcx, |e, _| e.image_edit_open()));
    // 填新值：链接 + 文本。
    vcx.update(|window, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框");
        link.update(cx, |s, cx| s.set_value("assets/new图.png", window, cx));
        let text = ed.read(cx).image_edit_text_input().expect("文本输入框");
        text.update(cx, |s, cx| s.set_value("新描述", window, cx));
    });
    vcx.update(|_window, cx| {
        ed.update(cx, |e, c| e.save_image_edit(c));
    });
    let out = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(out, "前文 ![新描述](assets/new图.png) 后文\n", "保存应只改行内图段");
    assert!(
        !ed.read_with(vcx, |e, _| e.image_edit_open()),
        "保存后卡片应关闭"
    );
}

/// 选中行内图后 Backspace → 删除整段 `![…](…)`（保留行内其它文本），清选中态。
#[gpui::test]
fn inline_image_selected_backspace_deletes_span(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("前文 ![内联蓝块](shot_blue.png) 后文\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(
            ed.read(cx).selected_inline_image().is_some(),
            "点行内图应选中"
        );
        ed.update(cx, |e, c| e.backspace(c));
        assert_eq!(
            ed.read(cx).content(),
            "前文  后文\n",
            "Backspace 应删除整段行内图（保留前后文字）"
        );
        assert!(ed.read(cx).selected_inline_image().is_none(), "删除后应清选中态");
    });
}

/// 选中行内图后 Delete → 删除整段 `![…](…)`（与 Backspace 同一入口）。
#[gpui::test]
fn inline_image_selected_delete_deletes_span(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("甲 ![内联图块](a.png) 乙\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(ed.read(cx).selected_inline_image().is_some());
        ed.update(cx, |e, c| e.delete(c));
        assert_eq!(ed.read(cx).content(), "甲  乙\n", "Delete 应删除整段行内图");
        assert!(ed.read(cx).selected_inline_image().is_none());
    });
}

/// 只读预览：点行内图本体不选中、不打开卡片（对齐块图 `core.rs` 只读分支）。
#[gpui::test]
fn inline_image_readonly_no_select_no_card(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("前文 ![内联蓝块](shot_blue.png) 后文\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    let btn_local = super::inline_image_edit_button_local(&bb_local);
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| e.set_editable(false));
    });
    // 点图本体 → 不选中、不打开卡片。
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert_eq!(ed.read(cx).selected_inline_image(), None, "只读点图不得选中");
        assert!(!ed.read(cx).image_edit_open(), "只读不得打开编辑卡");
    });
    // 点编辑按钮位置（只读无论如何不响应）→ 不打开卡片。
    vcx.update(|window, cx| {
        let b = win_of(&ed, cx, &btn_local);
        let down = img_down(
            f32::from(b.left()) + super::INLINE_IMAGE_EDIT_BTN_SIZE / 2.,
            f32::from(b.top()) + super::INLINE_IMAGE_EDIT_BTN_SIZE / 2.,
        );
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(!ed.read(cx).image_edit_open(), "只读点按钮不得打开编辑卡");
    });
}

/// 回归（M6 修 bug）：行内图**不在第 0 行**时，`inline_image_source_range_of` 反解的
/// `![…](…)` 源区间必须是**全局**字节偏移（`apply_edit`/`replace_range` 按整篇源操作；
/// 块图 `delete_selected_image` 用 `line_start` 归位）。早先实现把**行内**相对偏移当
/// 全局用——仅第 0 行测试碰巧通过（首行行内偏移 == 全局偏移），非首行 Backspace/保存
/// 会删错字节。真实渲染路径：点图本体选中 → 校验源区间全局偏移 → Backspace 整段删。
#[gpui::test]
fn inline_image_later_line_backspace_deletes_span(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("上\n前文 ![内联蓝块](shot_blue.png) 后文\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    // 点图本体（避开右上角编辑按钮）选中 → 校验源区间为全局字节偏移。
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        let refd = ed.read(cx).selected_inline_image().expect("点图应选中");
        assert_eq!(refd.src_line, 1, "行内图应在第 1 行（非首行回归场景）");
        let global = ed
            .read(cx)
            .content()
            .find("![内联蓝块]")
            .expect("图段应在源中");
        let (span, _) = ed
            .read(cx)
            .inline_image_source_range_of(refd)
            .expect("应能反解源区间");
        assert_eq!(
            span.start, global,
            "源区间起点必须是全局字节偏移（行内相对偏移会被 apply_edit 当全局用而删错字节）"
        );
    });
    // 选中清帧 → 重注入 → 再点图选中 → Backspace 删除整段（保留前后文字）。
    let _ = inline_image_harness(vcx, &ed);
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(ed.read(cx).selected_inline_image().is_some());
        ed.update(cx, |e, c| e.backspace(c));
        assert_eq!(
            ed.read(cx).content(),
            "上\n前文  后文\n",
            "非首行行内图 Backspace 应删除整段（保留前后文字）"
        );
        assert!(ed.read(cx).selected_inline_image().is_none());
    });
}

/// 图片编辑卡：聚焦链接输入框 + 光标摆到中部 → Ctrl+V（WYSIWYG `paste` 路由）应把
/// 剪贴板**插入链接框光标处**（原文本 + 剪贴板内容在光标处插入），且不误粘文档正文；
/// 未打开卡片时 `paste_into_focused_image_edit` 必须返回 false（走文档粘贴路径）。
#[gpui::test]
fn image_edit_link_focus_paste_routes_into_link_input(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("![pic desc](http://ex.com/pic.png)\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 未打开卡片：`paste_into_focused_image_edit` 必须 false（不截胡普通粘贴）。
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            assert!(
                !e.paste_into_focused_image_edit(window, "A", cx),
                "未打开图片卡时不得路由到卡片输入框"
            );
        });
    });
    // 程序化打开编辑卡（焦点应落链接输入框，预填 src）。
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.open_image_edit(
                crate::markdown::wysiwyg::ImageEditTarget::Block { src_line: 0 },
                window,
                cx,
            )
        });
    });
    // 显式聚焦链接框（对齐真实点击后的焦点态）+ 光标摆到 "http://ex." 之后（中部）。
    vcx.update(|window, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框");
        let fh = link.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        link.update(cx, |s, cx| {
            use gpui::EntityInputHandler as _;
            s.set_selected_text_range(10..10, window, cx);
        });
        assert!(
            ed.read(cx).image_edit_input_focused(window, cx),
            "链接框聚焦时 image_edit_input_focused 应为 true"
        );
    });
    // 写剪贴板 → 调 WYSIWYG `paste` → 应路由到链接框光标处插入（中部），正文不动。
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".to_string()));
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| e.paste(window, cx));
    });
    let (link_val, content) = vcx.update(|_w, cx| {
        (
            ed.read(cx)
                .image_edit_link_input()
                .expect("链接输入框")
                .read(cx)
                .value()
                .to_string(),
            ed.read(cx).content(),
        )
    });
    assert_eq!(
        link_val.as_str(),
        "http://ex.XYcom/pic.png",
        "粘贴应落在链接框光标处（原文本 + 剪贴板内容在光标处插入）：{link_val:?}"
    );
    assert_eq!(
        content.as_str(),
        "![pic desc](http://ex.com/pic.png)\n",
        "粘贴不得误粘到文档正文：{content:?}"
    );
}

/// 图片编辑卡：**聚焦文本输入框**（非链接框）→ Ctrl+V 同样路由到文本框光标处，
/// 链接框 / 文档正文保持原样。
#[gpui::test]
fn image_edit_text_focus_paste_routes_into_text_input(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("![pic desc](http://ex.com/pic.png)\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.open_image_edit(
                crate::markdown::wysiwyg::ImageEditTarget::Block { src_line: 0 },
                window,
                cx,
            )
        });
    });
    // 打开后默认聚焦链接框 → 显式切到文本框（真实点击后的焦点态）+ 光标摆到中部
    //（"pic " 之后，ASCII 4 字节 = UTF-16 4 单元）。
    vcx.update(|window, cx| {
        let text = ed.read(cx).image_edit_text_input().expect("文本输入框");
        let fh = text.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        text.update(cx, |s, cx| {
            use gpui::EntityInputHandler as _;
            s.set_selected_text_range(4..4, window, cx);
        });
        assert!(
            ed.read(cx).image_edit_input_focused(window, cx),
            "文本框聚焦时 image_edit_input_focused 应为 true"
        );
    });
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".to_string()));
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| e.paste(window, cx));
    });
    let (text_val, link_val, content) = vcx.update(|_w, cx| {
        (
            ed.read(cx)
                .image_edit_text_input()
                .expect("文本输入框")
                .read(cx)
                .value()
                .to_string(),
            ed.read(cx)
                .image_edit_link_input()
                .expect("链接输入框")
                .read(cx)
                .value()
                .to_string(),
            ed.read(cx).content(),
        )
    });
    assert_eq!(
        text_val.as_str(),
        "pic XYdesc",
        "粘贴应落在文本框光标处（中部插入）：{text_val:?}"
    );
    assert_eq!(
        link_val.as_str(),
        "http://ex.com/pic.png",
        "链接框不得被改动：{link_val:?}"
    );
    assert_eq!(
        content.as_str(),
        "![pic desc](http://ex.com/pic.png)\n",
        "文档正文不得被粘：{content:?}"
    );
}

/// **快照测试（诚实化，非「无泄漏」证明）**：本夹具下 WyEditor 的 caret/选区落在「换行
/// 结构性 no-op」位置，焦点在图片编辑卡链接输入框时按 Enter/Esc，文档不改写。
///
/// ⚠️ 已知既存泄漏：deferred 浮层经 `set_active_node(prepaint_deferred_draws,
/// window.rs:3931)` 挂回 WyEditor dispatch 路径，`wy-editor` CONTEXT 的
/// `on_action(Enter→newline)`（mod.rs:519）在动作层**先于** key_down 派发——一旦 caret
/// 移到可换行位置，图片卡输入框内按 Enter 会让 `WyEditor::newline` 向文档插换行。
/// 本批不做修复（见 `ui::input::InputState::on_key_down` 注释的机制边界），下一批专项修。
/// 本快照只在「单行图片 + 无显示 caret/选区」这一特定状态下成立；断言本身不改写。
#[gpui::test]
fn image_edit_enter_escape_snapshot_noop_caret_no_doc_rewrite(cx: &mut gpui::TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("![pic desc](http://ex.com/pic.png)\n")
        });
        WyHost { ed }
    });
    let vcx: &mut gpui::VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.open_image_edit(
                crate::markdown::wysiwyg::ImageEditTarget::Block { src_line: 0 },
                window,
                cx,
            )
        });
    });
    // 打开后默认聚焦链接框（真实焦点态）。
    vcx.update(|window, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框");
        let fh = link.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        assert!(
            ed.read(cx).image_edit_input_focused(window, cx),
            "链接框聚焦时 image_edit_input_focused 应为 true"
        );
    });
    let wh = vcx.windows().first().copied().expect("窗口句柄");
    let before = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(
        before.as_str(),
        "![pic desc](http://ex.com/pic.png)\n",
        "测试前提：正文未改动"
    );
    // 快照：本状态下按 Enter（caret 处于换行结构性 no-op 位置）不改写文档。
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    let after_enter = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(
        after_enter.as_str(),
        before.as_str(),
        "快照：图片卡链接输入框 Enter 在该 no-op 位置不改写文档：{after_enter:?}"
    );
    // 快照：按 Esc 同样不改写文档（卡片维持打开）。
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("escape").unwrap());
    let after_esc = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(
        after_esc.as_str(),
        before.as_str(),
        "快照：图片卡链接输入框 Esc 不改写文档：{after_esc:?}"
    );
}

/// **真实泄漏回归（Batch 2）**：caret 置于**普通可换行段落**（非图片行 alt 的 no-op 位置）、
/// 焦点在图片编辑卡输入框时按 Enter，不得向文档正文插入换行。
///
/// 机制：deferred 浮层经 `set_active_node(prepaint_deferred_draws, window.rs:3931)` 挂回
/// WyEditor dispatch 路径；gpui 按键派发**先动作层后 key_down**（`dispatch_key_event`：
/// `for binding in match_result.bindings { dispatch_action_on_node }` → 之后才派发
/// `key_down`）。由此 WyEditor 根的 `key_context(CONTEXT)` + `on_action(Enter→newline)`
/// （wysiwyg/mod.rs:561,615 + bind_keys mod.rs:519，生产装配 shell/lib.rs:45）在动作层
/// **先于** 卡片输入框 key_down 层派发——焦点在卡片时按 Enter 即向 caret 位置插换行。
/// 修复：在卡片根 `.on_action(Enter)` 拦截（距离焦点比 WyEditor 根近，动作冒泡在此被
/// 消费 + stop_propagation），WyEditor 根不再收到 Enter action。
#[gpui::test]
fn image_edit_enter_in_card_does_not_insert_newline_into_doc(cx: &mut gpui::TestAppContext) {
    // 演算字节区间（ASCII 单字节）：`para one\n`=[0,8)+1、`![pic](a.png)\n`=13+1、
    // `para two\n`=[23,31)+1；caret=4 落在首段 "para one" 中部（换行允许位）。
    let before = "para one\n![pic](a.png)\npara two\n";
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(before));
        WyHost { ed }
    });
    let vcx: &mut gpui::VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // caret 置于普通段落中段（首行第 5 字节，源字节偏移 4）。
    vcx.update(|_, cx| {
        ed.update(cx, |e, _| e.set_caret(4));
        assert_eq!(ed.read(cx).caret(), 4, "caret 应落到普通段落中段");
    });
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.open_image_edit(
                crate::markdown::wysiwyg::ImageEditTarget::Block { src_line: 1 },
                window,
                cx,
            )
        });
    });
    // 聚焦「链接」输入框（真实焦点态）。
    vcx.update(|window, cx| {
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框");
        let fh = link.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        assert!(
            ed.read(cx).image_edit_input_focused(window, cx),
            "链接框聚焦时 image_edit_input_focused 应为 true"
        );
    });
    let wh = vcx.windows().first().copied().expect("窗口句柄");
    let content_before = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(
        content_before.as_str(),
        before,
        "测试前提：正文未改动"
    );
    // 打开卡片 + 聚焦后重断言 caret 仍在可换行位置（杜绝「被钳到 no-op 位」的假绿疑虑）。
    vcx.update(|_w, cx| {
        assert_eq!(
            ed.read(cx).caret(),
            4,
            "打开图片卡后 caret 应仍在普通段落中段（换行允许位）"
        );
    });
    // 链接输入框持焦按 Enter → 修复后文档不得插换行（caret 在可换行段落）。
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    vcx.run_until_parked();
    let (content_enter, link_enter) = vcx.update(|_w, cx| {
        (
            ed.read(cx).content(),
            ed.read(cx)
                .image_edit_link_input()
                .map(|l| l.read(cx).value().to_string()),
        )
    });
    assert_eq!(
        content_enter.as_str(),
        before,
        "图片卡链接输入框 Enter 不得向正文插换行：{content_enter:?}"
    );
    assert_eq!(
        link_enter.as_deref(),
        Some("a.png"),
        "Enter 不得改动链接输入框内容：{link_enter:?}"
    );
    // 文本框持焦按 Enter → 同样不得插换行。
    vcx.update(|window, cx| {
        let text = ed.read(cx).image_edit_text_input().expect("文本输入框");
        let fh = text.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
    });
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    vcx.run_until_parked();
    let (content_text, text_val) = vcx.update(|_w, cx| {
        (
            ed.read(cx).content(),
            ed.read(cx)
                .image_edit_text_input()
                .map(|t| t.read(cx).value().to_string()),
        )
    });
    assert_eq!(
        content_text.as_str(),
        before,
        "图片卡文本框 Enter 不得向正文插换行：{content_text:?}"
    );
    assert_eq!(
        text_val.as_deref(),
        Some("pic"),
        "Enter 不得改动文本框内容：{text_val:?}"
    );
    // Esc（wy-editor 上下文未绑 escape，走 key_down 层被输入框 Cancel 消费）→ 不改文。
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("escape").unwrap());
    vcx.run_until_parked();
    let content_esc = vcx.update(|_w, cx| ed.read(cx).content());
    assert_eq!(
        content_esc.as_str(),
        before,
        "图片卡输入框 Esc 不得改写文档：{content_esc:?}"
    );
}

