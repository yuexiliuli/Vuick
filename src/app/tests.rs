//! 单元测试（自 mod.rs 拆出，仅测试编译）。

    use super::*;
    use gpui::{Entity, EntityInputHandler, TestAppContext, VisualTestContext};

    /// 测试前必须初始化 gpui-component（主题、键盘映射等全局状态）。
    fn init_test(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
    }

    /// 创建一个以 `gpui_component::Root` 为窗口根视图的编辑应用，
    /// 返回视图实体与该窗口的 `VisualTestContext`（后续用其驱动渲染/事件）。
    fn add_app(cx: &mut TestAppContext) -> (Entity<EditorApp>, &mut VisualTestContext) {
        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<EditorApp>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            *holder.borrow_mut() = Some(app.clone());
            gpui_component::Root::new(app, window, cx)
        });
        let _ = root;
        let app = holder.borrow().clone().unwrap();
        (app, cx)
    }

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
        cx.update(|window, cx| {
            editor0.update(cx, |state, cx| state.set_value("hello", window, cx));
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
        cx.update(|window, cx| {
            editor.update(cx, |state, cx| state.set_value("hello", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 粘贴：写入剪贴板后 Ctrl+V 应把内容插入（这是历史上出现“整行消失”的路径）
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("world".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_paste = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_paste, "worldhello",
            "粘贴应在光标处插入（set_value 后光标回到开头），实际={after_paste:?}"
        );

        // 撤销：应撤销粘贴 → 回到插入前（set_value 不进撤销栈，撤销对象是粘贴）
        cx.dispatch_action(gpui_component::input::Undo);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_undo = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_undo, "hello",
            "撤销粘贴后应回到插入前，实际={after_undo:?}"
        );

        // 重做：应恢复粘贴
        cx.dispatch_action(gpui_component::input::Redo);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_redo = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_redo, "worldhello",
            "重做后内容应恢复，实际={after_redo:?}"
        );

        // 全选：Ctrl+A 应选中全部内容
        cx.dispatch_action(gpui_component::input::SelectAll);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sel = cx.update(|window, cx| {
            editor.update(cx, |state, cx| state.selected_text_range(false, window, cx))
        });
        let sel = sel.expect("全选后应有选区");
        assert_eq!(sel.range.start, 0, "全选应从文档开头开始");
        assert!(
            sel.range.end > 0,
            "全选应覆盖全部内容，实际={:?}",
            sel.range
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
            cx.dispatch_action(gpui_component::input::Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 内容与镜像缓存一致、不丢失不错乱
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
        assert_eq!(cached, value, "镜像缓存应等于编辑区内容");
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
            cx.dispatch_action(gpui_component::input::Paste);
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
        editor: Entity<EditorState>,
    }

    impl Render for BareEditor {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(Editor::new(&self.editor).h(gpui::relative(1.)))
        }
    }

    /// 隔离测试：窗口里只有代码编辑器（无任何外围布局干扰）。
    /// 连续粘贴长文本与带换行文本，验证内容层不丢字、不凭空造换行——
    /// 即“整行消失”从内容层看与外围布局无关。
    #[gpui::test]
    async fn minimal_editor_paste_integrity(cx: &mut TestAppContext) {
        init_test(cx);

        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<EditorState>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .tab_size(TabSize {
                        tab_size: 4,
                        hard_tabs: false,
                    })
                    .soft_wrap(false)
            });
            *holder.borrow_mut() = Some(editor.clone());
            let view = cx.new(|_cx| BareEditor { editor });
            gpui_component::Root::new(view, window, cx)
        });
        let _ = root;
        let editor = holder.borrow().clone().unwrap();

        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = editor.read(cx).focus_handle(cx);
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
            cx.dispatch_action(gpui_component::input::Paste);
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
            cx.dispatch_action(gpui_component::input::Paste);
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
            (a.tabs[a.active_tab].content.to_string(), a.show_preview())
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
            gpui_component::Root::new(app, window, cx)
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

    /// 选项 app 里打开超宽 .md 时，预览横向滚动容器应真实溢出
    /// （md-list-wrap 宽度 > md-preview / md-harea 视口宽度）。
    #[gpui::test]
    async fn md_preview_hscroll_in_app(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let dir = std::env::temp_dir().join("gpui-editor-hscroll-test.md");
        std::fs::write(&dir, "# 标题\n\n".to_string() + &"x".repeat(300) + "\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let (preview, harea, wrap) = (
            cx.debug_bounds("md-preview"),
            cx.debug_bounds("md-harea"),
            cx.debug_bounds("md-list-wrap"),
        );
        let preview = preview.expect("md-preview 应渲染");
        let harea = harea.expect("md-harea 应渲染");
        let wrap = wrap.expect("md-list-wrap 应渲染");
        eprintln!(
            "[hscroll] preview={preview:?} harea={harea:?} wrap={wrap:?}"
        );
        assert!(
            preview.size.width > px(0.),
            "预览视口应有宽度，实际={:?}",
            preview.size.width
        );
        assert!(
            wrap.size.width > preview.size.width,
            "宽包装层应宽于预览视口（wrap={:?} > preview={:?}，harea={:?}），
             否则横向不会溢出、滚动条也不出现",
            wrap.size.width,
            preview.size.width,
            harea.size.width
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 选项卡过多时，选项卡栏应可横向滚动（出现横向滚动条），最新选项卡不再被遮挡。
    #[gpui::test]
    async fn tab_bar_scrolls_when_tabs_overflow(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(400.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 连续新建大量选项卡，总宽超过一屏
        for _ in 0..30 {
            cx.update(|window, cx| app.update(cx, |app, cx| app.new_tab(window, cx)));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let max_w = cx.update(|_w, cx| app.read(cx).tab_bar_hscroll.handle.max_offset().x);
        assert!(
            max_w > px(0.),
            "选项卡过多时选项卡栏应可横向滚动：max_offset={max_w:?}"
        );
    }

    /// 外部文件拖放（OS FileDrop）：拖动文件到窗口并松手应打开文件，
    /// 多个文件逐个开选项卡，且激活最后拖入的文件。
    #[gpui::test]
    async fn external_file_drop_opens_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let f1 = std::env::temp_dir().join("gpui-drop-a.txt");
        let f2 = std::env::temp_dir().join("gpui-drop-b.txt");
        std::fs::write(&f1, "AAA").unwrap();
        std::fs::write(&f2, "BBB").unwrap();
        let pos = gpui::Point::new(px(480.), px(700.));

        // 模拟资源管理器拖放文件的完整事件序列
        cx.simulate_event(gpui::FileDropEvent::Entered {
            position: pos,
            paths: gpui::ExternalPaths(vec![f1.clone(), f2.clone()].into_iter().collect()),
        });
        cx.simulate_event(gpui::FileDropEvent::Pending { position: pos });
        cx.simulate_event(gpui::FileDropEvent::Submit { position: pos });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let (tabs, active_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.tabs.len(), a.tabs[a.active_tab].content.to_string())
        });
        assert_eq!(tabs, 3, "拖入 2 个文件后应比初始多 2 个选项卡，实际={tabs}");
        assert_eq!(active_content, "BBB", "激活选项卡应载入最后拖入的文件内容");
        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
    }

    /// 无法读取内容的文件（非 UTF-8）：状态置为失败，并推送一条可关闭的提示通知。
    #[gpui::test]
    async fn unreadable_file_shows_notification(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-unreadable-test.bin");
        std::fs::write(&p, [0xFF, 0xFE, 0x00, 0xD8, 0x80, 0x00]).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (status_ok, tabs_before) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.status_ok, a.tabs.len())
        });
        assert!(!status_ok, "打开非 UTF-8 文件应置状态为失败");
        let count = cx.update(|window, cx| {
            use gpui_component::WindowExt as _;
            window.notifications(cx).len()
        });
        assert_eq!(count, 1, "打开失败应推送一条通知");
        let tabs_after = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(
            tabs_after, tabs_before,
            "打开失败不应新增空白标签页"
        );
        // 5 秒后应自动消失（推进模拟时钟 6 秒）
        cx.executor().advance_clock(std::time::Duration::from_secs(6));
        for _ in 0..10 {
            cx.run_until_parked();
        }
        let count2 = cx.update(|window, cx| {
            use gpui_component::WindowExt as _;
            window.notifications(cx).len()
        });
        assert_eq!(count2, 0, "6 秒后打开失败通知应已自动消失");
        let _ = std::fs::remove_file(&p);
    }

    /// 大文档 Markdown 预览冒烟测试：上千行也应能渲染（走虚拟化列表），不 panic。
    #[gpui::test]
    async fn large_markdown_preview_renders(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-large-md.md");
        let mut doc = String::new();
        for i in 1..=800 {
            doc.push_str(&format!("## 标题 {i}\n"));
        }
        std::fs::write(&p, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
        }
        let (preview_on, content_lines, tabs) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].preview_on,
                a.tabs[a.active_tab].content.lines().count(),
                a.tabs.len(),
            )
        });
        assert!(preview_on, "md 文件打开应自动开启预览");
        assert_eq!(content_lines, 800, "内容应完整载入 800 行");
        assert_eq!(tabs, 2, "打开成功应新增一个标签页（初始 1 + 1）");
        // 切换预览数次后再渲染多帧，确认虚拟化列表路径稳定
        for _ in 0..2 {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.toggle_preview_for_tab(app.active_tab, window, cx));
            });
            for _ in 0..5 {
                cx.run_until_parked();
            }
        }
        let still_preview = cx.update(|_w, cx| app.read(cx).show_preview());
        assert!(still_preview);
        let _ = std::fs::remove_file(&p);
    }

    /// Markdown 预览同步带 200ms 去抖：编辑后不立即整篇重解析，
    /// 停顿足够时间后才落盘到 md_view。
    #[gpui::test]
    async fn preview_sync_is_debounced(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-debounce.md");
        std::fs::write(&p, "# 初始\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 聚焦编辑区并做一次真实编辑（粘贴触发 InputEvent::Change）
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("已编辑".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let pending = cx.update(|_w, cx| app.read(cx).preview_pending);
        assert!(pending, "编辑后应立即进入去抖挂起状态（不逐键同步）");
        // 推进 250ms，去抖计时到点后应执行一次同步并复位
        cx.executor().advance_clock(std::time::Duration::from_millis(250));
        for _ in 0..10 {
            cx.run_until_parked();
        }
        let pending_after = cx.update(|_w, cx| app.read(cx).preview_pending);
        assert!(!pending_after, "去抖时间到点后应复位挂起状态");
        let _ = std::fs::remove_file(&p);
    }
    /// 打开 .epub：应解析正文（含书名/章节），自动进入预览模式并填充内容。
    #[gpui::test]
    async fn opens_epub_and_shows_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/xy.epub");
        assert!(epub.exists(), "测试 EPUB 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(epub.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (preview_on, content, tabs, status_ok, fullscreen, wrap) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].preview_on,
                a.tabs[a.active_tab].content.to_string(),
                a.tabs.len(),
                a.status_ok,
                a.tabs[a.active_tab].preview_fullscreen,
                a.tabs[a.active_tab].md_preview.read(cx).is_wrap(),
            )
        });
        assert!(status_ok, "打开 EPUB 应成功");
        assert!(preview_on, "打开 EPUB 应自动进入预览");
        assert_eq!(tabs, 2, "应新增一个标签页");
        assert!(content.contains("共产党宣言"), "正文应包含书名");
        assert!(content.len() > 1000, "正文应有实际内容（>1000 字符），实际={}", content.len());
        assert!(
            fullscreen,
            "打开 EPUB 应自动进入全屏占满阅读模式"
        );
        assert!(wrap, "打开 EPUB 应默认开启文本自动换行");

        // 回归：EPUB 正文只在预览区、编辑区为空，点击预览/Ctrl+P（toggle_preview_for_tab）
        // 不能翻转进空白编辑区或把预览源覆盖成空串导致内容消失。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_preview_for_tab(app.active_tab, window, cx);
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (still_on, still_fullscreen, still_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.preview_on,
                t.preview_fullscreen,
                t.content.contains("共产党宣言"),
            )
        });
        assert!(
            still_on,
            "EPUB 预览锁定：toggle 后 preview_on 应保持 true"
        );
        assert!(
            still_fullscreen,
            "EPUB 预览锁定：toggle 后应保持全屏，不倒向空白编辑区"
        );
        assert!(
            still_content,
            "toggle 后正文（tab.content）应仍在，未被空编辑区覆盖"
        );
    }

    /// EPUB 阅读体验：状态栏全屏/还原按钮可切换占满与分屏，换行开关可切换自动换行。
    #[gpui::test]
    async fn toggles_epub_fullscreen_and_wrap(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/xy.epub");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(epub.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        // 初始：全屏 + 换行
        let (fs0, wrap0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (t.preview_fullscreen, t.md_preview.read(cx).is_wrap())
        });
        assert!(fs0, "初始应是全屏");
        assert!(wrap0, "初始应换行");

        // 切换全屏 → 分屏
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_preview_fullscreen(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (fs1, wrap1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            // 退出全屏：预览恢复全局偏好（默认 true）
            (t.preview_fullscreen, t.md_preview.read(cx).is_wrap())
        });
        assert!(!fs1, "切换后应退出全屏");
        assert!(wrap1, "退出全屏后预览应保持换行（全局默认换行）");

        // 切换换行 → 不换行
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let wrap2 = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.read(cx).is_wrap()
        });
        assert!(!wrap2, "切换后应关闭换行");

        // 再切回换行
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let wrap3 = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.read(cx).is_wrap()
        });
        assert!(wrap3, "再切换后应恢复换行");
    }

