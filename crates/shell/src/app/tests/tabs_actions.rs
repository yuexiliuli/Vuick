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
        let editor0 = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
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
                a.tabs.len(),
                a.active_tab,
                a.tabs[a.active_tab].dirty,
                a.tabs[0].editor.read(cx).value(),
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
            "gpui-editor-app-img-{}",
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
            cx.update(|_w, cx| app.read(cx).media_lock_menu_pos.is_some()),
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
                a.media_lock_menu_pos.is_none(),
                tab_image(&a.tabs[a.active_tab])
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
            (a.active_media_locked(cx), a.media_lock_menu_pos.is_none())
        });
        assert!(unlocked.1, "再次点菜单后仍应关闭");
        assert!(!unlocked.0, "再次点「锁定」应解锁回归");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 视频右键「锁定」：右键在光标处弹菜单 → 点「锁定」→ 进入纯视频（外壳隐藏、视频
    /// 充满窗口）；再右键 → 点「锁定」→ 解锁回归。解码失败（假字节）不影响锁定状态机。
    /// ⚠️ `#[ignore]`（P0-A 挂起处置）：本测试创建真 `VideoViewer`（假 mp4 字节），FFmpeg
    /// `avformat_open_input` 探测损坏文件会**永久卡死**、测试进程无法结束（实测单跑 60s 只
    /// 打印 `moov atom not found` 后挂起，与 `media_loads_share_common_fields` 同病）。恢复
    /// 条件：video 打开损坏文件有超时/看门狗、或本测试改用可解析视频夹具后摘除 `#[ignore]`
    /// （断言保留，`-- --ignored` 可手动驱动）。
    #[cfg(feature = "video")]
    #[ignore]
    #[gpui::test]
    async fn video_right_click_lock_menu_toggles(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-app-vid-{}",
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
            cx.update(|_w, cx| app.read(cx).media_lock_menu_pos.is_some()),
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
                a.media_lock_menu_pos.is_none(),
                tab_video(&a.tabs[a.active_tab])
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
            (a.active_media_locked(cx), a.media_lock_menu_pos.is_none())
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
            app.read(cx).tabs[app.read(cx).active_tab].editor.clone()
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
            app.read(cx).tabs[app.read(cx).active_tab].editor.clone()
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
            let tab = &a.tabs[a.active_tab];
            (
                editor.read(cx).value().to_string(),
                tab.content.to_string(),
                tab.dirty,
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
            app.read(cx).tabs[app.read(cx).active_tab]
                .editor
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
        let dir = std::env::temp_dir().join("gpui-editor-test.md");
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
                a.tabs[a.active_tab].editor.read(cx).value().to_string(),
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
            app.tabs[app.active_tab].preview_on = true;
            cx.notify();
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (font, preview) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.font_size, a.show_preview())
        });
        assert_eq!(font, DEFAULT_FONT_SIZE + 2.0);
        assert!(
            preview,
            "预览开关打开后 show_preview 应为 true（不依赖 .md 扩展名）"
        );
    }

    /// 状态栏右侧按钮的可见性跟随文档类型/预览状态（覆盖 `render_status_bar` 拆出的
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
                a.tabs[a.active_tab].cursor_row,
                a.tabs[a.active_tab].cursor_col,
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
                a.tabs[a.active_tab].cursor_row,
                a.tabs[a.active_tab].cursor_col,
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
            (a.tabs[a.active_tab].cursor_row, a.tabs[a.active_tab].cursor_col)
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
            let ed = a.tabs[a.active_tab].editor.read(cx);
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
            (a.tabs[a.active_tab].cursor_row, a.tabs[a.active_tab].cursor_col)
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
            (a.tabs[a.active_tab].cursor_row, a.tabs[a.active_tab].cursor_col)
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
            (a.tabs[a.active_tab].cursor_row, a.tabs[a.active_tab].cursor_col)
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
        let dir = std::env::temp_dir().join("gpui-editor-eye-test.md");
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
                app.toggle_preview_for_tab(app.active_tab, window, cx)
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
        let dir2 = std::env::temp_dir().join("gpui-editor-eye-test.txt");
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
        let tabs0 = cx.update(|_w, cx| app.read(cx).tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs1 = cx.update(|_w, cx| app.read(cx).tabs.len());
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
        let tabs0 = cx.update(|_w, cx| app.read(cx).tabs.len());

        // 右键选项卡栏**空白区**（首标签之后的栏内区域，语义 = 栏级菜单；标签上右键已
        // 专属「选项卡管理菜单」，见 tabs_context.rs）→ 弹「新建文件/打开文件」菜单
        let bar = cx
            .debug_bounds("tab-bar")
            .expect("选项卡栏 tab-bar 应渲染");
        let right_click_at = gpui::Point::new(bar.origin.x + px(400.), bar.origin.y + px(16.));
        cx.simulate_mouse_down(right_click_at, MouseButton::Right, Default::default());
        settle_frames(cx, 3);
        assert!(
            cx.update(|_w, cx| app.read(cx).tab_bar_menu_pos.is_some()),
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
            (a.tabs.len(), a.tab_bar_menu_pos.is_none())
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
            cx.update(|_w, cx| app.read(cx).tab_bar_menu_pos.is_some()),
            "再次右键应重新弹出菜单"
        );
        let item_open2 = cx
            .debug_bounds("tab-bar-menu-item-open")
            .expect("「打开文件」项应再次渲染");
        cx.simulate_click(item_open2.center(), Default::default());
        settle_frames(cx, 3);
        let (calls, menu_closed2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.open_dialog_calls, a.tab_bar_menu_pos.is_none())
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
            cx.update(|_w, cx| app.read(cx).tab_bar_menu_pos.is_some()),
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

