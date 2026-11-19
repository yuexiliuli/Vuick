    /// 选项卡：新建文件产生新选项卡，内容彼此独立，且脏标记正确。
    #[gpui::test]
    async fn tabs_are_independent(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 第 1 个选项卡输入内容
        let editor0 = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[0].payload.editor.clone());
        cx.update(|_, cx| {
            editor0.update(cx, |ed, _| ed.set_text("hello"));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 新建选项卡
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (count, active, tab1_dirty, tab0_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs.len(),
                a.model.borrow().active_tab,
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.dirty,
                a.model.borrow().tabs[0].payload.editor.read(cx).value(),
            )
        });
        assert_eq!(count, 2, "新建后应有 2 个选项卡");
        assert_eq!(active, 1, "新选项卡应被激活");
        assert!(!tab1_dirty, "新选项卡初始未修改");
        assert_eq!(tab0_content.to_string(), "hello", "第 1 个选项卡内容应保留");
    }

    /// 图片右键「锁定」：右键在光标处弹菜单 → 点「锁定」→ 进入干净看图（外壳隐藏）；
    /// 再右键 → 点「锁定」→ 解锁回归。解码失败（假字节）不影响锁定状态机。
    #[gpui::test]
    async fn image_right_click_lock_menu_toggles(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "vuick-app-img-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.jpg");
        std::fs::write(&p, b"not really a jpeg").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.load_media(0, &p, MediaKind::Image, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        // 右键（图片区任一点）→ 菜单在光标处弹出
        let menu_at = gpui::Point::new(px(400.), px(360.));
        cx.simulate_mouse_down(menu_at, MouseButton::Right, Default::default());
        cx.run_until_parked();
        assert!(
            cx.update(|_w, cx| app.read(cx).media_lock_menu.is_open()),
            "图片右键应在光标处弹出锁定菜单"
        );
        // 位置回归：菜单项应出现在右键点附近（非沉底）。若被 clamp 回窗口底部，dy 会接近
        // 整窗高——阈值取窗口高 1/3 区分；debug_selector 见 on_media_right_click（图片/视频共用）。
        let item_bounds = cx
            .debug_bounds("media-lock-menu-item")
            .expect("右键后应渲染「锁定」菜单项");
        let viewport_h = 720.;
        let dy = (item_bounds.origin.y - menu_at.y).as_f32();
        assert!(
            dy < viewport_h * 0.3,
            "图片右键菜单应出现在光标附近（right_click y={}, 菜单项 y={}, dy={}，应当 <{}）",
            menu_at.y.as_f32(),
            item_bounds.origin.y.as_f32(),
            dy,
            viewport_h * 0.3
        );
        let dx = (item_bounds.origin.x - menu_at.x).as_f32();
        assert!(
            dx.abs() < 24.,
            "图片右键菜单应水平跟随光标（right_click x={}, 菜单 x={}, dx={}，应 |dx|<24）",
            menu_at.x.as_f32(),
            item_bounds.origin.x.as_f32(),
            dx
        );
        // 点菜单首项（菜单窗口锚定在该处；面板内边距 4 + 项高 26 → 项中心约 y+17）
        let item = gpui::Point::new(px(400. + 8.), px(360. + 4. + 13.));
        cx.simulate_click(item, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let state = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.active_media_locked(cx),
                !a.media_lock_menu.is_open(),
                tab_image(&a.model.borrow().tabs[a.model.borrow().active_tab])
                    .map(|v| v.read(cx).is_locked())
                    .unwrap_or(false),
            )
        });
        assert!(state.1, "点「锁定」后菜单应关闭");
        assert!(state.0 && state.2, "点「锁定」应进入锁定（外壳隐藏）");
        // 再右键 → 再点「锁定」→ 解锁
        cx.simulate_mouse_down(menu_at, MouseButton::Right, Default::default());
        cx.run_until_parked();
        cx.simulate_click(item, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let unlocked = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.active_media_locked(cx), !a.media_lock_menu.is_open())
        });
        assert!(unlocked.1, "再次点菜单后仍应关闭");
        assert!(!unlocked.0, "再次点「锁定」应解锁回归");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 视频右键「锁定」：右键在光标处弹菜单 → 点「锁定」→ 进入纯视频（外壳隐藏、视频
    /// 充满窗口）；再右键 → 点「锁定」→ 解锁回归。解码失败（假字节）不影响锁定状态机。
    /// 已在失败路径停 pump（P0-A 收口）：`media-ffmpeg` 看门狗让损坏文件快速失败（8s
    /// 截止，实测 4 次 open 均 <10ms 返回 Err），且视频失败分支清 `playing`+`pending_seek`
    /// 使 pump 不再自续 → `run_until_parked` 正常静止、不再挂起（此前 pump 无限自续
    /// 定时器，executor 永不静止）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn video_right_click_lock_menu_toggles(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "vuick-app-vid-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, b"not really a video").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.load_media(0, &p, MediaKind::Video, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        // 右键（视频区任一点）→ 菜单在光标处弹出
        let menu_at = gpui::Point::new(px(400.), px(360.));
        cx.simulate_mouse_down(menu_at, MouseButton::Right, Default::default());
        cx.run_until_parked();
        assert!(
            cx.update(|_w, cx| app.read(cx).media_lock_menu.is_open()),
            "视频右键应在光标处弹出锁定菜单"
        );
        // 位置回归：菜单项应出现在右键点附近（非沉底）。若被 clamp 回窗口底部，dy 会接近
        // 整窗高——阈值取窗口高 1/3 区分；debug_selector 见 on_media_right_click（图片/视频共用）。
        let item_bounds = cx
            .debug_bounds("media-lock-menu-item")
            .expect("右键后应渲染「锁定」菜单项");
        let viewport_h = 720.;
        let dy = (item_bounds.origin.y - menu_at.y).as_f32();
        assert!(
            dy < viewport_h * 0.3,
            "视频右键菜单应出现在光标附近（right_click y={}, 菜单项 y={}, dy={}，应当 <{}）",
            menu_at.y.as_f32(),
            item_bounds.origin.y.as_f32(),
            dy,
            viewport_h * 0.3
        );
        let dx = (item_bounds.origin.x - menu_at.x).as_f32();
        assert!(
            dx.abs() < 24.,
            "视频右键菜单应水平跟随光标（right_click x={}, 菜单 x={}, dx={}，应 |dx|<24）",
            menu_at.x.as_f32(),
            item_bounds.origin.x.as_f32(),
            dx
        );
        // 点菜单首项（菜单窗口锚定在该处；面板内边距 4 + 项高 26 → 项中心约 y+17）
        let item = gpui::Point::new(px(400. + 8.), px(360. + 4. + 13.));
        cx.simulate_click(item, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let state = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.active_media_locked(cx),
                !a.media_lock_menu.is_open(),
                tab_video(&a.model.borrow().tabs[a.model.borrow().active_tab])
                    .map(|v| v.read(cx).is_locked())
                    .unwrap_or(false),
            )
        });
        assert!(state.1, "点「锁定」后菜单应关闭");
        assert!(state.0 && state.2, "点「锁定」应进入纯视频（外壳隐藏）");
        // 再右键 → 再点「锁定」→ 解锁
        cx.simulate_mouse_down(menu_at, MouseButton::Right, Default::default());
        cx.run_until_parked();
        cx.simulate_click(item, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let unlocked = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.active_media_locked(cx), !a.media_lock_menu.is_open())
        });
        assert!(unlocked.1, "再次点菜单后仍应关闭");
        assert!(!unlocked.0, "再次点「锁定」应解锁回归");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 编辑菜单/快捷键的动作：撤销、重做、粘贴、全选应能到达聚焦的编辑区。
    #[gpui::test]
    async fn edit_actions_dispatch(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 聚焦编辑区并输入内容
        let editor = cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone()
        });
        cx.update(|_, cx| {
            editor.update(cx, |ed, _| ed.set_text("hello"));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 粘贴：写入剪贴板后 Ctrl+V 应把内容插入（这是历史上出现“整行消失”的路径）
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("world".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_paste = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_paste, "worldhello",
            "粘贴应在光标处插入（set_value 后光标回到开头），实际={after_paste:?}"
        );

        // 撤销：应撤销粘贴 → 回到插入前（set_value 不进撤销栈，撤销对象是粘贴）
        cx.dispatch_action(Undo);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_undo = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_undo, "hello",
            "撤销粘贴后应回到插入前，实际={after_undo:?}"
        );

        // 重做：应恢复粘贴
        cx.dispatch_action(Redo);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_redo = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_redo, "worldhello",
            "重做后内容应恢复，实际={after_redo:?}"
        );

        // 全选：Ctrl+A 应选中全部内容
        cx.dispatch_action(SelectAll);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sel = cx.update(|_w, cx| editor.read(cx).selection());
        assert_eq!(sel.start(), 0, "全选应从文档开头开始");
        assert!(
            sel.end() > 0,
            "全选应覆盖全部内容，实际={:?}",
            sel.range()
        );
    }

    /// 回归测试：历史上“在一行反复粘贴同一段文本后整行消失”的复现场景。
    /// gpui-component 的 CodeEditor 以 ropey 存储文本、独立换行管线排版，
    /// 反复粘贴大段内容时内容不应丢失、镜像缓存与输入区保持一致。
    #[gpui::test]
    async fn repeated_pastes_keep_content(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let editor = cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone()
        });

        let chunk = "Option<Vec<WrappedLine>>";
        let text = chunk.repeat(300);
        let expected_len = text.len() * 4;

        // 连续粘贴多次（历史上的触发路径）
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        for _ in 0..4 {
            cx.dispatch_action(Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 内容不丢失不错乱；普通选项卡不再整份镜像全文到 content（省一份副本）
        let (value, cached, dirty) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let tab = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                editor.read(cx).value().to_string(),
                tab.payload.content.to_string(),
                tab.payload.dirty,
            )
        });
        assert_eq!(
            value.len(),
            expected_len,
            "反复粘贴后内容不得丢失，实际长度={} 期望={expected_len}",
            value.len()
        );
        assert!(value.contains(chunk), "内容应仍包含原文片段");
        assert!(cached.is_empty(), "普通选项卡不应镜像全文到 content（省内存）");
        assert!(dirty, "粘贴后应置脏");

        // 纯文本剪贴板不会凭空引入换行：仍是单个逻辑行（软换行是排版行为，不改文本）
        assert_eq!(
            value.matches('\n').count(),
            0,
            "无换行的剪贴板反复粘贴不应产生换行，实际={value:?}"
        );

        // 真实场景：从源码/聊天处复制的行通常自带尾随换行，粘贴多次应变成多行且不丢失
        let chunk_line = "use std::sync::Arc;\n";
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(chunk_line.to_string()));
        for _ in 0..4 {
            cx.dispatch_action(Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let value2 = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            value2.matches('\n').count(),
            4,
            "带尾随换行的行重复粘贴 4 次应有 4 个换行，实际={value2:?}"
        );
        assert_eq!(value2.lines().count(), 4, "应形成 4 个逻辑行");
        assert!(
            value2.contains("Arc;\nuse std::sync::Arc;"),
            "多行内容应完整保留，实际={value2:?}"
        );
    }

    /// VS Code 在 Windows 剪贴板中使用 CRLF；应用级粘贴入口必须转为 LF 后再
    /// 交给 gpui-component 的代码编辑器，避免回车符进入代码布局。
    #[gpui::test]
    async fn normalized_paste_removes_windows_carriage_returns(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            "use std::cell::RefCell;\r\nuse std::collections::HashMap;\r\nuse std::ops::Range;"
                .to_owned(),
        ));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.paste_normalized(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let value = cx.update(|_window, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .payload.editor
                .read(cx)
                .value()
                .to_string()
        });
        assert_eq!(
            value,
            "use std::cell::RefCell;\nuse std::collections::HashMap;\nuse std::ops::Range;"
        );
        assert!(!value.contains('\r'), "编辑器内部不能保留 CR 字符");
    }

    /// 仅含编辑器的极简宿主：窗口中只有 gpui-component 的代码编辑器，
    /// 没有菜单栏/选项卡/状态栏/预览等任何外围代码。
    struct BareEditor {
        editor: Entity<editor::CodeEditor>,
    }

    impl Render for BareEditor {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.editor.clone())
        }
    }

    /// 隔离测试：窗口里只有代码编辑器（无任何外围布局干扰）。
    /// 连续粘贴长文本与带换行文本，验证内容层不丢字、不凭空造换行——
    /// 即“整行消失”从内容层看与外围布局无关。
    #[gpui::test]
    async fn minimal_editor_paste_integrity(cx: &mut TestAppContext) {
        init_test(cx);

        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<editor::CodeEditor>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| editor::CodeEditor::new(cx));
            *holder.borrow_mut() = Some(editor.clone());
            let view = cx.new(|_cx| BareEditor { editor });
            ui::root::Root::new(view, window, cx)
        });
        let _ = root;
        let editor = holder.borrow().clone().unwrap();

        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = editor.read(cx).focus_handle();
            window.focus(&focus, cx);
        });
        for _ in 0..2 {
            cx.run_until_parked();
        }

        // 1) 无换行大段文本连续粘贴：内容不丢、不产生换行（仍是单个逻辑行）
        let chunk = "Option<Vec<WrappedLine>>".repeat(200);
        let expected = chunk.len() * 5;
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(chunk));
        for _ in 0..5 {
            cx.dispatch_action(Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let value = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            value.len(),
            expected,
            "长文本反复粘贴不得丢字：实际长度={} 期望={expected}",
            value.len()
        );
        assert_eq!(value.matches('\n').count(), 0, "无换行剪贴板不应产生换行");

        // 2) 带尾随换行的行连续粘贴：应形成多行且内容完整
        let line = "use std::sync::Arc;\n";
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(line.to_string()));
        for _ in 0..4 {
            cx.dispatch_action(Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let value2 = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            value2.matches('\n').count(),
            4,
            "带换行行重复粘贴应有 4 个换行"
        );
        assert_eq!(value2.lines().count(), 4, "应形成 4 个逻辑行");
    }

    /// 冒烟测试：完整渲染整棵 UI 树（菜单/选项卡/编辑区/状态栏），
    /// 展开下拉菜单并打开一个临时文件，确保渲染管线无 panic。
    #[gpui::test]
    async fn renders_and_opens_menu(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 点击"文件"菜单按钮，展开 gpui_component 的 DropdownMenu/PopupMenu 并渲染
        // （菜单已并入顶部 30px 自绘标题栏，首个按钮"文件"中心约为 (36, 15)）
        cx.simulate_click(gpui::Point::new(px(36.), px(15.)), Default::default());
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 打开一个真实文件，内容进入编辑区，再渲染（覆盖 open_file 路径）
        let dir = std::env::temp_dir().join("vuick-test.md");
        std::fs::write(&dir, "# 标题\n\n**加粗** 正文 `code`\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(dir.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (content, preview) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx).value().to_string(),
                a.show_preview(),
            )
        });
        assert!(
            content.contains("标题"),
            "打开文件后编辑区应有内容: {content}"
        );
        assert!(preview, "打开 .md 文件应自动开启预览");
        let _ = std::fs::remove_file(&dir);
    }

    /// 字号调节 / 预览开关等纯状态操作正常。
    #[gpui::test]
    async fn font_and_preview_toggles(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        app.update(cx, |app, cx| {
            app.change_font_size(3.0, cx);
            app.change_font_size(-1.0, cx);
            let active = app.model.borrow().active_tab;
            app.model.borrow_mut().tabs[active].payload.preview_on = true;
            cx.notify();
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (font, preview) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().font_size, a.show_preview())
        });
        assert_eq!(font, DEFAULT_FONT_SIZE + 2.0);
        assert!(
            preview,
            "预览开关打开后 show_preview 应为 true（不依赖 .md 扩展名）"
        );
    }

    /// 状态栏右侧按钮的可见性跟随文档类型/预览状态（覆盖 `render_status_bar` 的
    /// 条件分支）：
    /// - 非只读且非 Markdown（.txt）→ 显示「换行」按钮；预览未开 → 不显示「全屏」。
    /// - Markdown（自动开预览、恒自动换行）→ 显示「全屏」，不显示「换行」。
    /// - 只读预览（EPUB）→ 两个阅读控制按钮都隐藏（自带全屏阅读）。
    #[gpui::test]
    async fn status_bar_buttons_follow_doc_kind_and_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // .txt：非只读、非 Markdown → 换行按钮；预览未开 → 无全屏按钮。
        let txt = std::env::temp_dir().join(format!("gpui-statusbar-{}.txt", std::process::id()));
        std::fs::write(&txt, "hello\nworld\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-wrap-btn").is_some(),
            ".txt 非只读非 Markdown 应显示换行按钮"
        );
        assert!(
            cx.debug_bounds("status-fullscreen-btn").is_none(),
            ".txt 未开预览不应有全屏按钮"
        );
        // .md：自动开预览（非只读）→ 全屏按钮；Markdown 恒自动换行 → 无换行按钮。
        let md = std::env::temp_dir().join(format!("gpui-statusbar-{}.md", std::process::id()));
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-fullscreen-btn").is_some(),
            "Markdown 自动开预览应有全屏按钮"
        );
        assert!(
            cx.debug_bounds("status-wrap-btn").is_none(),
            "Markdown 恒自动换行不显示换行按钮"
        );
        // 只读预览（EPUB）：两个阅读控制按钮都隐藏（自带全屏阅读）。
        let epub =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
        assert!(epub.exists(), "测试 EPUB 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(epub.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-fullscreen-btn").is_none(),
            "只读预览（EPUB）不应显示全屏按钮"
        );
        assert!(
            cx.debug_bounds("status-wrap-btn").is_none(),
            "只读预览（EPUB）不应显示换行按钮"
        );
        let _ = std::fs::remove_file(&txt);
        let _ = std::fs::remove_file(&md);
    }

    /// wave4：PDF 状态栏「页面网格视图」按钮——显隐只随激活选项卡是否为 PDF 阅读器，
    /// 点击经 `PdfViewer::toggle_grid` 在「单列阅读 ↔ 页面网格」间往返，高亮随 grid_mode。
    /// （缺夹具 SKIP；shell 侧只断言按钮显隐 + grid_mode 翻转 + 网格容器渲染，
    /// 网格卡渲染细节已由 wave3 pdf crate 覆盖。）
    #[gpui::test]
    async fn status_bar_pdf_grid_btn_shows_for_pdf_and_toggles(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 初始未命名空选项卡（非 PDF）→ 不显示网格按钮。
        assert!(
            cx.debug_bounds("status-pdf-grid-btn").is_none(),
            "非 PDF 选项卡不应显示页面网格按钮"
        );
        // 打开 PDF → 泵到加载完成 → 按钮出现、初始非网格。
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(pdf.to_str().unwrap(), window, cx));
        });
        for _ in 0..400 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let loaded = cx.update(|_w, cx| {
                tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        let btn_initial = cx.debug_bounds("status-pdf-grid-btn").is_some();
        let grid_initial = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            tab_pdf(t).map(|pv| pv.read(cx).grid_mode()).unwrap_or(true)
        });
        assert!(btn_initial, "PDF 选项卡应显示页面网格按钮");
        assert!(!grid_initial, "新开 PDF 初始应为单列阅读（grid_mode=false）");
        // 顺序断言（用户验收项）：网格按钮应位于「只读」状态项**左侧**（不放最右边）。
        let grid_bounds = cx.debug_bounds("status-pdf-grid-btn").expect("网格按钮应在场");
        let readonly_bounds = cx
            .debug_bounds("status-mode-readonly")
            .expect("PDF 只读预览应显示只读状态项");
        assert!(
            grid_bounds.left() < readonly_bounds.left(),
            "网格按钮应位于只读状态项左侧（got grid.left={:?} readonly.left={:?}）",
            grid_bounds.left(),
            readonly_bounds.left()
        );
        // 两态视觉（关=纯文字无底 / 开=高亮 pill）为 `ui::theme` 语义色差，headless 经
        // debug_bounds 不可断底色，留待真机确认；此处只保证两态共用同一 debug_selector
        //（点击命中依赖它）且 grid_mode 翻转驱动态切换（下两步断言）。
        // 点击按钮 → 进入网格：viewer.grid_mode==true 且网格容器渲染。
        let btn = cx
            .debug_bounds("status-pdf-grid-btn")
            .expect("网格按钮应在场");
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let grid_on = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            tab_pdf(t).map(|pv| pv.read(cx).grid_mode()).unwrap_or(false)
        });
        assert!(grid_on, "点击网格按钮后 grid_mode 应为 true");
        assert!(
            cx.debug_bounds("pdf-grid").is_some(),
            "网格模式应渲染 pdf-grid 内容容器"
        );
        // 再点 → 退出：grid_mode==false 且网格容器消失。
        let btn = cx
            .debug_bounds("status-pdf-grid-btn")
            .expect("第二次点击前网格按钮应在场");
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let grid_off = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            tab_pdf(t).map(|pv| pv.read(cx).grid_mode()).unwrap_or(true)
        });
        assert!(!grid_off, "再点网格按钮后 grid_mode 应为 false");
        assert!(
            cx.debug_bounds("pdf-grid").is_none(),
            "退出网格后 pdf-grid 容器应消失"
        );
        // 切到非 PDF 选项卡（.txt）→ 按钮随 tab 切换隐藏。
        let txt = std::env::temp_dir().join(format!("gpui-pdfgrid-{}.txt", std::process::id()));
        std::fs::write(&txt, "hello\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-pdf-grid-btn").is_none(),
            "切到非 PDF 选项卡后网格按钮应隐藏（tab 切换刷新）"
        );
        let _ = std::fs::remove_file(&txt);
    }

    /// 打开 `target/pdf测试.pdf` 并泵到 `is_loaded()`（夹具缺时由调用方先判
    /// `pdf::test_util::missing_pdf_fixture`）；返回激活选项卡的 PdfViewer 实体
    /// （供 page_mode / zoom / grid_mode 断言）。
    fn open_pdf_loaded(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
    ) -> gpui::Entity<pdf::PdfViewer> {
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(pdf.to_str().unwrap(), window, cx));
        });
        for _ in 0..400 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let loaded = cx.update(|_w, cx| {
                tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            tab_pdf(t).expect("PDF 选项卡应在场").clone()
        })
    }

    /// 状态栏 PDF 页排模式钮（单页/双页/网格三钮互斥选中）：pdf 打开 → 三钮都渲染、
    /// 初始连续（三钮均不选中）；点单页 → page_mode==Single；再点单页 → 弹起回
    /// Continuous；点双页 → Facing；点网格 → grid_mode==true 且 page_mode 保留（网格
    /// 与页排正交）；退网格 → 回阅读、page_mode 原样。选中高亮 = `selected_bg` 主题色
    /// 差（headless 不可断底色，同网格钮先例留真机确认），headless 断言状态翻转即可。
    #[gpui::test]
    async fn status_bar_pdf_single_facing_buttons_toggle_page_mode(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, mut cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 非 PDF 幕：页排钮不渲染。
        assert!(
            cx.debug_bounds("status-pdf-single-btn").is_none()
                && cx.debug_bounds("status-pdf-facing-btn").is_none(),
            "非 PDF 选项卡不应显示页排按钮"
        );
        // 打开 PDF → 三钮渲染、初始连续。
        let pv = open_pdf_loaded(&app, &mut cx);
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-pdf-single-btn").is_some(),
            "PDF 选项卡应显示单页钮"
        );
        assert!(
            cx.debug_bounds("status-pdf-facing-btn").is_some(),
            "PDF 选项卡应显示双页钮"
        );
        assert!(
            cx.debug_bounds("status-pdf-grid-btn").is_some(),
            "PDF 选项卡应显示网格钮"
        );
        assert_eq!(
            cx.update(|_w, cx| pv.read(cx).page_mode()),
            pdf::view::PageMode::Continuous,
            "新开 PDF 默认连续阅读（三钮均不选中）"
        );
        // 点单页 → Single。
        let single = cx.debug_bounds("status-pdf-single-btn").expect("单页钮应在场");
        cx.simulate_click(single.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| pv.read(cx).page_mode()),
            pdf::view::PageMode::Single,
            "点单页后应为单页"
        );
        // 再点单页 → 弹起回 Continuous。
        let single = cx.debug_bounds("status-pdf-single-btn").expect("单页钮应仍在场");
        cx.simulate_click(single.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| pv.read(cx).page_mode()),
            pdf::view::PageMode::Continuous,
            "再点单页应弹起回连续"
        );
        // 点双页 → Facing。
        let facing = cx
            .debug_bounds("status-pdf-facing-btn")
            .expect("双页钮应在场");
        cx.simulate_click(facing.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| pv.read(cx).page_mode()),
            pdf::view::PageMode::Facing,
            "点双页后应为双页对开"
        );
        // 点网格 → grid_mode==true 且 page_mode 保留（网格与页排正交）。
        let grid = cx.debug_bounds("status-pdf-grid-btn").expect("网格钮应在场");
        cx.simulate_click(grid.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let (gm, pm) = cx.update(|_w, cx| (pv.read(cx).grid_mode(), pv.read(cx).page_mode()));
        assert!(gm, "点网格后 grid_mode 应为 true");
        assert_eq!(pm, pdf::view::PageMode::Facing, "进网格不应清 page_mode");
        // 退网格 → 回阅读、page_mode 原样。
        let grid = cx
            .debug_bounds("status-pdf-grid-btn")
            .expect("网格钮应仍在场");
        cx.simulate_click(grid.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let (gm, pm) = cx.update(|_w, cx| (pv.read(cx).grid_mode(), pv.read(cx).page_mode()));
        assert!(!gm, "退网格后 grid_mode 应为 false");
        assert_eq!(pm, pdf::view::PageMode::Facing, "退网格后 page_mode 应保留");
    }

    /// 状态栏 PDF「显示比例」滑条显隐：阅读（单列、非网格）模式可见；进网格 → 隐藏
    /// （网格有独立 grid_zoom，滑条只控单列阅读缩放）；退网格回到阅读 → 复现。
    #[gpui::test]
    async fn status_bar_pdf_zoom_slider_hidden_in_grid_shown_in_reading(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, mut cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pv = open_pdf_loaded(&app, &mut cx);
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-pdf-zoom-slider").is_some(),
            "阅读（单列非网格）模式应显示显示比例滑条"
        );
        // 点网格 → 滑条隐藏。
        let grid = cx.debug_bounds("status-pdf-grid-btn").expect("网格钮应在场");
        cx.simulate_click(grid.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| pv.read(cx).grid_mode()),
            "进网格后 grid_mode 应为 true"
        );
        assert!(
            cx.debug_bounds("status-pdf-zoom-slider").is_none(),
            "网格模式应隐藏显示比例滑条"
        );
        // 退网格 → 滑条复现。
        let grid = cx
            .debug_bounds("status-pdf-grid-btn")
            .expect("网格钮应仍在场");
        cx.simulate_click(grid.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            !cx.update(|_w, cx| pv.read(cx).grid_mode()),
            "退网格后 grid_mode 应为 false"
        );
        assert!(
            cx.debug_bounds("status-pdf-zoom-slider").is_some(),
            "退网格回到阅读应复现显示比例滑条"
        );
    }

    /// 状态栏 PDF「显示比例」滑条交互：命中盒内按下/拖拽按 x 映射分数 → `set_zoom_abs`，
    /// pdf.zoom 实时变化（按下 50% ≈ 2.25、拖到 75% ≈ 3.125），中间区 zoom% 文本同源
    /// （均派生自 `zoom_factor`）；松手保持末值。
    #[gpui::test]
    async fn status_bar_pdf_zoom_slider_drag_updates_zoom(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, mut cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pv = open_pdf_loaded(&app, &mut cx);
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let slider = cx
            .debug_bounds("status-pdf-zoom-slider")
            .expect("阅读模式应有显示比例滑条");
        let w = slider.size.width.as_f32();
        let cy = slider.center().y;
        // 按下 50% → on_change(0.5) → set_zoom_abs(0.5+0.5×3.5=2.25)。
        let p_half = gpui::Point::new(slider.left() + gpui::px(0.5 * w), cy);
        cx.simulate_mouse_move(p_half, None, Default::default());
        cx.simulate_mouse_down(p_half, MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let zoom_half = cx.update(|_w, cx| pv.read(cx).zoom_factor());
        assert!(
            (zoom_half - 2.25).abs() < 0.1,
            "按下 50% 应 set_zoom_abs≈2.25，got {zoom_half}"
        );
        // 拖到 75% → on_change_during_drag(0.75) → set_zoom_abs(0.5+0.75×3.5=3.125)。
        let p_75 = gpui::Point::new(slider.left() + gpui::px(0.75 * w), cy);
        cx.simulate_mouse_move(p_75, Some(MouseButton::Left), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let zoom_75 = cx.update(|_w, cx| pv.read(cx).zoom_factor());
        assert!(
            (zoom_75 - 3.125).abs() < 0.1,
            "拖到 75% 应 set_zoom_abs≈3.125，got {zoom_75}"
        );
        // 中间区 zoom% 文本在场（同源：`{:.0}%` 由 zoom_factor 派生，与滑条双向同步）。
        assert!(
            cx.debug_bounds("status-pdf-page").is_some(),
            "中间区页码/缩放文本应渲染"
        );
        // 松手 → 末值保持（Immediate 模式 up 不再补发，zoom 保留拖拽末值）。
        cx.simulate_mouse_up(p_75, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let zoom_final = cx.update(|_w, cx| pv.read(cx).zoom_factor());
        assert_eq!(zoom_final, zoom_75, "松手后 zoom 应保持拖拽末值");
    }

    /// P2-11 状态栏分区化：三区布局 + 分隔线 + 语言徽标（含徽记色点 pill）。
    /// - 未命名空选项卡：语言徽标（占位）显示、中间区（光标位置）省略、右侧主题/编码状态项恒在。
    /// - `.md`：自动预览 → 右侧出现「预览」状态项；有内容 → 中间区出现 `status-cursor`。
    /// 分区之间细分隔线 `status-bar-divider` 恒存在。
    #[gpui::test]
    async fn status_bar_sections_language_badge_and_cursor(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 未命名空选项卡：无编辑内容 → 中间区（光标位置）省略。
        assert!(
            cx.debug_bounds("status-language-badge").is_some(),
            "语言徽标在未打开文件时也应显示（占位）"
        );
        assert!(
            cx.debug_bounds("status-cursor").is_none(),
            "未命名空文档无编辑内容，中间区应省略（status-cursor）"
        );
        assert!(cx.debug_bounds("status-theme").is_some(), "右侧主题状态项应恒在");
        assert!(
            cx.debug_bounds("status-encoding").is_some(),
            "可编辑文档应显示 UTF-8 编码状态项"
        );
        assert!(
            cx.debug_bounds("status-bar-divider").is_some(),
            "分区之间应有细分隔线（status-bar-divider）"
        );

        // 打开 .md（自动预览、有内容）→ 语言徽标 / 预览 / 光标中间区齐全。
        let md = std::env::temp_dir().join(format!("gpui-statusbar-p2-{}.md", std::process::id()));
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-mode-preview").is_some(),
            "Markdown 自动预览应显示「预览」状态项"
        );
        let cursor_bounds = cx
            .debug_bounds("status-cursor")
            .expect("有内容的编辑区应显示光标位置（Ln, Col）");
        assert!(cursor_bounds.size.width > px(0.), "光标中间区文本应有宽度");
        assert!(
            cx.debug_bounds("status-bar-divider").is_some(),
            "打开 .md 后分隔线仍应存在"
        );
        // 文件名展示迁移（2026-11-10）：状态栏不再显示文件名块（含脏点/路径 tooltip），
        // 语言徽标保留。
        assert!(
            cx.debug_bounds("status-file-name").is_none(),
            "状态栏文件名块（status-file-name）已迁移移除"
        );
        assert!(
            cx.debug_bounds("status-language-badge").is_some(),
            "语言徽标仍应在状态栏左侧显示"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// P2-11 光标位置数据源：编辑区输入推进 `cursor_col`，中间区随激活选项卡数据刷新；
    /// 普通文本不显示「只读」状态项。
    #[gpui::test]
    async fn status_bar_cursor_data_follows_editing(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let txt = std::env::temp_dir().join(format!(
            "gpui-statusbar-cur-{}.txt",
            std::process::id()
        ));
        std::fs::write(&txt, "hello\nworld\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (row0, col0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_row,
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_col,
            )
        });
        assert_eq!((row0, col0), (0, 0), "打开文件后光标应位于 (0,0)");
        assert!(
            cx.debug_bounds("status-cursor").is_some(),
            "有内容的文件应显示光标位置（Ln 1, Col 1）"
        );

        // 聚焦编辑区 → 粘贴一个字符到光标处 → cursor_col 推进为 1，中间区随数据刷新。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("A".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (row1, col1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_row,
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_col,
            )
        });
        assert_eq!((row1, col1), (0, 1), "光标处粘贴后列号推进为 1");
        assert!(
            cx.debug_bounds("status-cursor").is_some(),
            "输入后光标中间区仍存在（行:列随数据源刷新）"
        );
        assert!(
            cx.debug_bounds("status-mode-readonly").is_none(),
            "普通文本不应显示只读状态项"
        );
        let _ = std::fs::remove_file(&txt);
    }

    /// P2-11 Bug 回归：纯光标/选区移动（**不改文本**）后，状态栏中间区 `Ln x, Col y`
    /// 数据源也必须即时刷新。此前只有文本变化（`EditorEvent::Edited`）才走
    /// `on_editor_edited` 更新选项卡缓存；鼠标点击只发编辑器自身 `notify`、不发事件，
    /// 状态栏重渲染时读到的还是旧缓存 → 行/列不变。
    /// 本测试走真实路径：`simulate_mouse_down/up` 派发到编辑区元素 → `on_mouse_down`
    /// 放下光标 → 新事件 `SelectionChanged` → app 订阅 `on_editor_selection_changed`
    /// 刷新 `tabs[active].cursor_row/col`（状态栏 `Ln x, Col y` 由它 1:1 派生）。
    #[gpui::test]
    async fn status_bar_cursor_follows_mouse_click_and_arrow_and_tab_switch(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 多行文档：打开后光标在 (0,0)，随后点中段某行验证即时刷新。
        let text = (0..60).map(|i| format!("行{i:02}")).collect::<Vec<_>>().join("\n");
        let txt = std::env::temp_dir().join(format!(
            "gpui-statusbar-click-{}.txt",
            std::process::id()
        ));
        std::fs::write(&txt, &text).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (row0, col0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_row, a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_col)
        });
        assert_eq!((row0, col0), (0, 0), "打开文件后光标应位于 (0,0)");
        assert!(
            cx.debug_bounds("status-cursor").is_some(),
            "有内容文档应显示光标中间区"
        );

        // 鼠标点击第 20 行（0 基）行首偏 1 字符处 → 光标应落该行，状态栏数据源即刷。
        let target_row = 20;
        let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
        let (cw, lh, gutter) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ed = a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx);
            ed.test_metrics()
        });
        let click = gpui::Point::new(
            gpui::px(ed_b.origin.x.as_f32() + gutter + cw),
            gpui::px(ed_b.origin.y.as_f32() + (target_row as f32 + 0.5) * lh),
        );
        let mods = gpui::Modifiers::default();
        cx.simulate_mouse_move(click, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_down(click, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_up(click, gpui::MouseButton::Left, mods);
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (row1, _col1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_row, a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_col)
        });
        assert_eq!(
            row1, target_row,
            "鼠标点击后光标行应即时跳到第 {target_row} 行（状态栏 Ln 数据源刷新）"
        );
        assert!(
            cx.debug_bounds("status-cursor").is_some(),
            "点击后状态栏光标中间区保持显示"
        );

        // 方向键向下：移动光标（不改文本）后仍即时刷新。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.dispatch_action(MoveDown);
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (row2, _) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_row, a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_col)
        });
        assert_eq!(row2, target_row + 1, "方向键下移后光标行应 +1（行数据刷新）");

        // 切 tab：新打开的文件自动激活，其光标位置 (0,0) 即时落到状态栏数据源。
        let md = std::env::temp_dir().join(format!(
            "gpui-statusbar-switch-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, "# 新文件\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (row3, col3) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_row, a.model.borrow().tabs[a.model.borrow().active_tab].payload.cursor_col)
        });
        assert_eq!((row3, col3), (0, 0), "切到新选项卡后光标数据源应为其 (0,0)");
        assert!(
            cx.debug_bounds("status-cursor").is_some(),
            "切 tab 后状态栏光标中间区仍显示"
        );
        let _ = std::fs::remove_file(&txt);
        let _ = std::fs::remove_file(&md);
    }

    /// 当前选中的文件是 .md 时，选项卡栏最右侧出现眼睛图标，点击切换预览；非 .md 文件不出现。
    #[gpui::test]
    async fn md_tab_bar_shows_eye_and_toggles_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 打开 .md 文件（自动新建选项卡并选中它）：选项卡栏最右侧应显示眼睛，并自动开启预览
        let dir = std::env::temp_dir().join("vuick-eye-test.md");
        std::fs::write(&dir, "# 标题\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(dir.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let eye_before = cx.debug_bounds("tab-bar-eye");
        let preview_before = cx.update(|_w, cx| app.read(cx).show_preview());
        assert!(
            eye_before.is_some(),
            "当前选中 .md 文件时，选项卡栏最右侧应显示眼睛图标"
        );
        assert!(preview_before, "打开 .md 文件应自动开启预览");

        // 点击眼睛：应关闭当前选项卡的预览（当前仍是 .md，眼睛保持显示，仅变关闭）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let active_idx = app.model.borrow().active_tab;
                app.toggle_preview_for_tab(active_idx, window, cx)
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let preview_off = cx.update(|_w, cx| app.read(cx).show_preview());
        let eye_still = cx.debug_bounds("tab-bar-eye").is_some();
        assert!(!preview_off, "点击眼睛后预览应关闭");
        assert!(eye_still, "当前仍是 .md 文件，眼睛图标应保持显示");
        let _ = std::fs::remove_file(&dir);

        // 另起一个窗口：打开非 .md 文件，选项卡栏最右侧不应出现眼睛（该窗口从未渲染过眼睛）
        let (view, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            ui::root::Root::new(app, window, cx)
        });
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir2 = std::env::temp_dir().join("vuick-eye-test.txt");
        std::fs::write(&dir2, "plain").unwrap();
        cx.update(|window, cx| {
            if let Ok(app) = view.read(cx).view().clone().downcast::<EditorApp>() {
                app.update(cx, |app, cx| {
                    app.open_file(dir2.to_str().unwrap(), window, cx);
                });
            }
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let eye_txt = cx.debug_bounds("tab-bar-eye");
        assert!(
            eye_txt.is_none(),
            "当前选中的文件不是 .md 时，选项卡栏不应显示眼睛图标"
        );
        let _ = std::fs::remove_file(&dir2);
    }

    /// 回归：选项卡栏最右侧的「新建(+)」按钮已移除——不再渲染 `tab-add` / `tab-add-icon`；
    /// 新建能力由右键菜单 / 菜单栏 / Ctrl+N 承担（此处直接调用 `new_tab`，与旧测试同意图）。
    #[gpui::test]
    async fn tab_add_button_removed(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tab-add").is_none(),
            "「+」新建按钮 tab-add 应已移除"
        );
        assert!(
            cx.debug_bounds("tab-add-icon").is_none(),
            "手绘 ＋ 图标 tab-add-icon 应已移除"
        );
        // 新建选项卡的能力仍在
        let tabs0 = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs1 = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        assert_eq!(tabs1, tabs0 + 1, "新建应增加 1 个选项卡");
    }

    /// 选项卡栏右键上下文菜单（取代「+」按钮）：右键整条 tab-bar → 弹「新建文件 /
    /// 打开文件」菜单（`tab-bar-menu` + 两个带 debug_selector 的菜单项）；点「新建文件」
    /// → 新建选项卡且菜单关闭；再右键 → 点「打开文件」→ 触发 `open_dialog`
    /// （测试为 stub，只计数不真弹窗）且菜单关闭。
    #[gpui::test]
    async fn tab_bar_right_click_menu_creates_tab_and_opens(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 3);
        let tabs0 = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());

        // 右键选项卡栏**空白区**（首标签之后的栏内区域，语义 = 栏级菜单；标签上右键已
        // 专属「选项卡管理菜单」，见 tabs_context.rs）→ 弹「新建文件/打开文件」菜单
        let bar = cx
            .debug_bounds("tab-bar")
            .expect("选项卡栏 tab-bar 应渲染");
        let right_click_at = gpui::Point::new(bar.origin.x + px(400.), bar.origin.y + px(16.));
        cx.simulate_mouse_down(right_click_at, MouseButton::Right, Default::default());
        settle_frames(cx, 3);
        assert!(
            cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "右键选项卡栏应弹出上下文菜单"
        );
        assert!(
            cx.debug_bounds("tab-bar-menu").is_some(),
            "应渲染选项卡栏右键菜单根 tab-bar-menu"
        );
        let item_new = cx
            .debug_bounds("tab-bar-menu-item-new")
            .expect("菜单应有「新建文件」项");
        let item_open = cx
            .debug_bounds("tab-bar-menu-item-open")
            .expect("菜单应有「打开文件」项");
        assert!(
            item_new.size.width > px(0.) && item_open.size.width > px(0.),
            "菜单项应有实际尺寸"
        );

        // 点「新建文件」→ 新建选项卡 + 菜单关闭
        cx.simulate_click(item_new.center(), Default::default());
        settle_frames(cx, 3);
        let (tabs1, menu_closed) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs.len(), !a.tab_bar_menu.is_open())
        });
        assert_eq!(tabs1, tabs0 + 1, "点「新建文件」应新建 1 个选项卡");
        assert!(menu_closed, "点菜单项后菜单应关闭");
        assert!(
            cx.debug_bounds("tab-bar-menu-item-new").is_none(),
            "关闭后菜单项不应再渲染"
        );

        // 再右键（空白区）→ 点「打开文件」→ 触发 open_dialog（计数断言）且菜单关闭
        let bar2 = cx
            .debug_bounds("tab-bar")
            .expect("新建后选项卡栏仍在");
        let right_click_at2 = gpui::Point::new(bar2.origin.x + px(400.), bar2.origin.y + px(16.));
        cx.simulate_mouse_down(right_click_at2, MouseButton::Right, Default::default());
        settle_frames(cx, 3);
        assert!(
            cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "再次右键应重新弹出菜单"
        );
        let item_open2 = cx
            .debug_bounds("tab-bar-menu-item-open")
            .expect("「打开文件」项应再次渲染");
        cx.simulate_click(item_open2.center(), Default::default());
        settle_frames(cx, 3);
        let (calls, menu_closed2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.open_dialog_calls, !a.tab_bar_menu.is_open())
        });
        assert_eq!(calls, 1, "点「打开文件」应触发一次 open_dialog");
        assert!(menu_closed2, "点「打开文件」后菜单应关闭");
    }

    /// 选项卡栏右键菜单位置回归：菜单（以「新建文件」项 debug_bounds 代表）应出现在
    /// **鼠标附近**（右键点处，几 px 容差），而不是被 `snap_to_window_with_margin` 钳到
    /// 窗口底部。历史缺陷：`AnchoredPositionMode::Window` 下没给 `.position()`，`.offset(pos)`
    /// 被 gpui 当作「相对锚定元素自身静态布局位置的偏移」；而本菜单包在 `#tab-bar-menu`
    /// 外壳 flex 流内、静态位在窗口底部（0,720）→ 锚定点被加成 `(86,48)+(0,720)` → 沉底。
    /// 修复用 gpui 的窗口坐标入参 `.position(pos)`（媒体右键菜单因静态位恰为 (0,0) 才幸免）。
    #[gpui::test]
    async fn tab_bar_right_click_menu_anchored_near_cursor(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 3);

        // 右键选项卡栏**空白区**（首标签右方、栏级菜单）→ 菜单应弹在光标处
        let bar = cx
            .debug_bounds("tab-bar")
            .expect("选项卡栏 tab-bar 应渲染");
        let right_click_at = gpui::Point::new(bar.origin.x + px(400.), bar.origin.y + px(16.));
        cx.simulate_mouse_down(right_click_at, MouseButton::Right, Default::default());
        settle_frames(cx, 3);
        assert!(
            cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "右键选项卡栏应弹出上下文菜单"
        );
        let item_new = cx
            .debug_bounds("tab-bar-menu-item-new")
            .expect("右键后应渲染「新建文件」菜单项");

        // 「新建文件」菜单项即菜单本体（面板内边距 4 的左上角 ≈ 光标，几 px 容差）。
        // 若菜单被钳到窗口底部，此处会差出近一整窗高——断言用 >= 窗口高的 1/3 阈值区分。
        let viewport_h = 720.;
        let dy = (item_new.origin.y - right_click_at.y).as_f32();
        assert!(
            dy < viewport_h * 0.3,
            "选项卡栏右键菜单应出现在光标附近（right_click y={}, 菜单项 y={}, dy={}，应当 <{}）",
            right_click_at.y.as_f32(),
            item_new.origin.y.as_f32(),
            dy,
            viewport_h * 0.3
        );
        // 横向也应跟光标（±24px 容差）
        let dx = (item_new.origin.x - right_click_at.x).as_f32();
        assert!(
            dx.abs() < 24.,
            "选项卡栏右键菜单应水平跟随光标（right_click x={}, 菜单 x={}, dx={}，应 |dx|<24）",
            right_click_at.x.as_f32(),
            item_new.origin.x.as_f32(),
            dx
        );
    }

    /// 视频选项卡激活 → 隐藏底部状态栏 + 左侧侧边栏（渲染级守卫，不写 `sidebar.open`；
    /// 切到非视频 tab 立即恢复）。R1/R2/R3 + 「非活动视频 tab 时安全」触发腿：
    /// - 视频 tab 在列但**非活动** → 两栏照常渲染（触发点=活动 tab 的 reader）；
    /// - 切到视频 tab（活动 = 视频）→ 状态栏不渲染、侧边栏包裹层不渲染、右列/标题栏/
    ///   选项卡栏/正文块仍在；`sidebar.open` 全程不变；
    /// - 切回文本 tab → 状态栏 + 侧边栏恢复。
    /// 视频用假字节（解码失败不影响 reader 变体判定，P0-A 已让失败路径快速静止）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn video_autohide_hides_status_bar_and_sidebar(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 默认侧边栏即展开（`sidebar.open = true`，只留窄活动栏）。确认 `app-body-with-sidebar`
        // 在场作为「非视频态」预置（视频激活时隐藏是**渲染级守卫**而非改写 open，R3 依赖）。
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "预置：默认侧边栏展开，包裹层应渲染"
        );
        assert!(
            cx.debug_bounds("status-bar").is_some(),
            "预置：非视频态状态栏应渲染"
        );

        // tab 0 = 空文本；新建 tab 1 作为「切回」目标（new_tab 建后即激活，活动=1）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 把视频载入 tab 0（**非活动**）→ 两栏应照常渲染
        let dir = std::env::temp_dir().join(format!(
            "vuick-app-vid-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, b"not really a video").unwrap();
        cx.update(|window, cx| {
            app.update(
                cx,
                |app, cx| app.load_media(0, &p, MediaKind::Video, window, cx),
            );
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-bar").is_some(),
            "视频 tab 非活动时状态栏应照常渲染"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "视频 tab 非活动时侧边栏应照常渲染"
        );

        // 切到视频 tab（活动 = 视频）→ R1：状态栏 + 侧边栏不渲染，标题栏/选项卡栏/右列仍在
        let open_before = cx.update(|_w, cx| app.read(cx).sidebar.open);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(0, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-bar").is_none(),
            "R1：视频激活时底部状态栏不应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar").is_none(),
            "R1：视频激活时侧边栏本体不应渲染"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "R1：视频激活时侧边栏包裹层不应渲染"
        );
        assert!(
            cx.debug_bounds("app-right-column").is_some(),
            "R1：右列（选项卡栏+正文）应在"
        );
        assert!(
            cx.debug_bounds("title-bar").is_some(),
            "R1：标题栏应仍在"
        );
        assert!(
            cx.debug_bounds("tab-bar").is_some(),
            "R1：选项卡栏应仍在"
        );
        assert!(
            cx.debug_bounds("app-main-card").is_some(),
            "R1：正文块应在"
        );
        // R3：隐藏动作不改变 sidebar.open
        let open_after = cx.update(|_w, cx| app.read(cx).sidebar.open);
        assert_eq!(
            open_before, open_after,
            "R3：视频激活仅隐藏渲染、不写 sidebar.open"
        );

        // R2：切到文本 tab → 状态栏 + 侧边栏恢复；R3 延续：open 仍保持展开原态
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(1, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-bar").is_some(),
            "R2：切回文本后状态栏应恢复"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "R2：切回文本后侧边栏应恢复"
        );
        let open_restored = cx.update(|_w, cx| app.read(cx).sidebar.open);
        assert_eq!(
            open_before, open_restored,
            "R3：恢复后 sidebar.open 仍保持原开合态"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 视频「打开文件」触发腿 + 关闭视频 tab 后恢复正常：
    /// - 把视频直接载入当前**活动** tab（打开文件路由）→ 状态栏 + 侧边栏立即隐藏；
    /// - 关闭该视频 tab（命名文件、干净 → 直通 `remove_tab`，不弹确认）→ 回到余下文本
    ///   tab，状态栏 + 侧边栏恢复正常。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn video_autohide_open_and_close_toggles(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-bar").is_some(),
            "预置：文本态状态栏应渲染"
        );

        // 新建文本 tab 1（视频关闭后落到它）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 把视频载入当前活动 tab（tab 1）→ 打开文件即生效
        let dir = std::env::temp_dir().join(format!(
            "vuick-app-vid-open-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, b"not really a video").unwrap();
        cx.update(|window, cx| {
            app.update(
                cx,
                |app, cx| app.load_media(1, &p, MediaKind::Video, window, cx),
            );
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-bar").is_none(),
            "打开视频（活动）即隐藏状态栏"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "打开视频（活动）即隐藏侧边栏"
        );

        // 关闭视频 tab（命名文件、干净）→ 落回文本 tab，两栏恢复
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(1, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(
            !cx.has_pending_prompt(),
            "干净命名视频 tab 关闭不应弹确认"
        );
        assert!(
            cx.debug_bounds("status-bar").is_some(),
            "关闭视频 tab 后状态栏应恢复"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "关闭视频 tab 后侧边栏应恢复"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 视频激活时标题栏【打开/隐藏侧边栏】按钮 = 显式打开（覆盖自动隐藏），再点为关闭；
    /// 活动选项卡切换自动重新武装（切走复位 override、切回视频从隐藏开始）。延续
    /// `video_autohide_*` 夹具姿势（假字节视频、reader 变体判定）；复核 `video_hide_override`
    /// 状态机与 `open` 不被持久化改写：
    /// - 视频激活 → 侧边栏自动隐藏（open 保持 true）；
    /// - 视频下点按钮 → 侧边栏出现（open=true、override=true）；再点 → 关闭（open=false、
    ///   override=false）；
    /// - 切走文本 → override 复位、`open` 不变；切回视频 → 自动隐藏重新武装（即使 open=true
    ///   也隐藏）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn video_autohide_toggle_opens_and_rearms(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (open0, over0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.video_hide_override)
        });
        assert!(open0 && !over0, "预置：默认侧边栏展开、无手动覆盖");

        // 新建文本 tab（切回/切走目标），视频载入 tab 0（非活动）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "vuick-app-vid-tg-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, b"not really a video").unwrap();
        cx.update(|window, cx| {
            app.update(
                cx,
                |app, cx| app.load_media(0, &p, MediaKind::Video, window, cx),
            );
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }

        // 切到视频（活动=0）→ 自动隐藏生效（open 保持 true、override false）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(0, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "视频激活：侧边栏自动隐藏"
        );
        let over_enter = cx.update(|_w, cx| app.read(cx).sidebar.video_hide_override);
        assert!(!over_enter, "进视频时 override 应为 false（自动隐藏武装）");

        // 视频下点按钮 → 显式打开：override=true、open=true、侧边栏出现；状态栏仍藏
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (open_on, over_on) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.video_hide_override)
        });
        assert!(open_on && over_on, "视频下点按钮→显式打开：open=true、override=true");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "视频下点按钮后侧边栏应出现"
        );
        assert!(
            cx.debug_bounds("status-bar").is_none(),
            "状态栏仍按视频激活即藏（无按钮、不涉覆盖）"
        );

        // 再点 → 关闭（回到隐藏）：open=false、override=false
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (open_off, over_off) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.video_hide_override)
        });
        assert!(!open_off && !over_off, "再点→关闭：open=false、override=false");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "再点后侧边栏应回到隐藏"
        );

        // 切走文本 → override 复位（open 不变，仍 false 故隐藏）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(1, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (open_text, over_text) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.video_hide_override)
        });
        assert!(!over_text, "切到文本后 override 应复位 false");
        assert_eq!(open_text, open_off, "切走不改 open（保持用户关闭语义）");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "文本下按 open=false 保持隐藏"
        );

        // 回视频（open 仍 false）→ 自动隐藏重新武装（无需点按钮）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(0, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let over_back = cx.update(|_w, cx| app.read(cx).sidebar.video_hide_override);
        assert!(!over_back, "回视频：override 重新武装 false");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "回视频：自动隐藏重新武装"
        );

        // 视频下再开一次侧边栏（open=true、override=true）→ 出现的状态；随后
        // 切走文本 → override 复位、侧边栏按 open=true 恢复；再切回视频 → open=true 也
        // 仍被自动隐藏覆盖（武装生效的关键断言）。
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "视频下再开侧边栏应出现"
        );
        let open_re = cx.update(|_w, cx| app.read(cx).sidebar.open);
        assert!(open_re, "视频下再开：open 回 true");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(1, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let over_final = cx.update(|_w, cx| app.read(cx).sidebar.video_hide_override);
        assert!(!over_final, "切回文本：override 复位");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "文本下按 open=true 恢复侧边栏"
        );
        // 关键：回视频 → 即使 open=true 也自动隐藏（每次视频激活都从隐藏开始）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(0, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (open_back, over_back2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.video_hide_override)
        });
        assert_eq!(open_back, true, "回视频不改 open（仍为用户开的 true）");
        assert!(!over_back2, "回视频：override 重新武装 false");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "回视频：open=true 仍被自动隐藏覆盖（重新武装）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关闭**活动**视频选项卡落回另一视频 tab → 自动隐藏重新武装：即便用户先前已手动
    /// 打开侧边栏（override=true、open=true），关闭活动视频后 override 复位 false，侧边栏
    /// 按「每次视频激活都从隐藏开始」归于隐藏；`open` 不被改写（仅渲染级掉隐藏）。
    /// 该腿走 `remove_tab` glue（域内自选新活动、不经 activate_tab glue），回归保护
    /// `EditorApp::remove_tab` 的重新武装。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn video_autohide_close_active_video_rearms(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "vuick-app-vid-close-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 两个视频 tab（1、2）+ 落点文本 tab 0；当前活动 = tab 2（视频）
        for vid in 1..=2 {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.new_tab(window, cx));
            });
            for _ in 0..3 {
                cx.run_until_parked();
            }
            let p = dir.join(&format!("v{vid}.mp4"));
            std::fs::write(&p, b"not really a video").unwrap();
            cx.update(|window, cx| {
                app.update(
                    cx,
                    |app, cx| app.load_media(vid, &p, MediaKind::Video, window, cx),
                );
            });
            for _ in 0..4 {
                cx.run_until_parked();
            }
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "预置：视频激活自动隐藏"
        );
        // 手动打开侧边栏（override=true、open=true）
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "手动打开后侧边栏应出现"
        );
        // 关闭活动视频 tab 2 → 落回视频 tab 1 → 重新武装：隐藏 + override 复位 + open 不变
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(2, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            !cx.has_pending_prompt(),
            "干净命名视频 tab 关闭不应弹确认"
        );
        let (open_after, over_after) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.video_hide_override)
        });
        assert!(open_after, "关闭活动视频不写 open（仍为用户开的 true）");
        assert!(!over_after, "关闭活动视频 → override 重新武装 false");
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "落回另一视频 tab：侧边栏归于隐藏（重新武装）"
        );
        // 切回文本 tab 0 → 按 open=true 恢复（override 已复位、无视频隐藏）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(0, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_some(),
            "切回文本：按 open=true 恢复侧边栏"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-33 回归：关闭弹「未保存」确认框的异步 await 期间用户拖拽重排选项卡 → 应答后
    /// 必须按「发起时的选项卡身份」关闭正确选项卡，不得按下标误关移位的其它选项卡。
    /// （修复前：`remove_tab(发起时 idx)` 直接把重排后落在该下标的 B 关掉、真正的 A 还开着。）
    #[gpui::test]
    async fn close_tab_prompt_answer_revalidates_identity_after_reorder(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 三个未命名选项卡（未命名关闭恒弹确认框）：A=tabs[0]（目标）、B、C。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let target_id = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[0].payload.editor.entity_id()
        });
        // 对 A 发起关闭 → 弹「未保存的更改」确认框（不立即应答）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(0, window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(cx.has_pending_prompt(), "未命名关闭应弹确认框");
        // await 期间拖拽重排：A 由 0 移到末尾 → [B, C, A]。
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.reorder_tab(0, 3, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 应答「不保存」：应关闭发起时身份对应的 A（现下标 2），而非现 tabs[0]=B。
        cx.simulate_prompt_answer("不保存");
        let mut target_closed = false;
        for _ in 0..120 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            target_closed = cx.update(|_w, cx| {
                !app.read(cx).model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.editor.entity_id() == target_id)
            });
            if target_closed {
                break;
            }
        }
        assert!(target_closed, "应答后应关闭发起时身份对应的选项卡（而非重排后的下标位）");
        let len = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        assert_eq!(len, 2, "3 个未命名选项卡关闭 1 个后应剩 2");
    }

