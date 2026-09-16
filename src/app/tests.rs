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
            editor.update(cx, |ed, _| ed.set_text("hello"));
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
            cx.dispatch_action(gpui_component::input::Paste);
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
        editor: Entity<crate::editor::CodeEditor>,
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

        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<crate::editor::CodeEditor>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| crate::editor::CodeEditor::new(cx));
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

    /// 选项卡栏最右侧的新建(+)按钮：应渲染出（非零尺寸）按钮 + 手绘「＋」图标，且可点击新建。
    #[gpui::test]
    async fn tab_add_button_renders_plus_icon(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs0 = cx.update(|_w, cx| app.read(cx).tabs.len());
        // 按钮 + 图标都有非零尺寸
        let btn = cx
            .debug_bounds("tab-add")
            .expect("新建按钮 tab-add 应存在");
        let icon = cx
            .debug_bounds("tab-add-icon")
            .expect("手绘 ＋ 图标 tab-add-icon 应存在");
        assert!(
            btn.size.width > px(0.) && btn.size.height > px(0.),
            "新建按钮应有实际尺寸，实际={btn:?}"
        );
        assert!(
            icon.size.width > px(0.) && icon.size.height > px(0.),
            "＋ 图标应有实际尺寸，实际={icon:?}"
        );
        // 点击 → 新建选项卡
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs1 = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(tabs1, tabs0 + 1, "点击/调用新建后应增加 1 个选项卡");
    }

    /// 侧边栏：默认仅收窄为活动栏（目录面板收起，占 46px 不挤正文）；
    /// 整体收起后移除；标题栏右上角有展开/收起开关（title-sidebar-toggle）。
    #[gpui::test]
    async fn sidebar_collapses_and_expands(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 默认：侧边栏在（活动栏），宽 ≈ SIDEBAR_ACTIVITY_W，目录面板未自动展开
        let bar = cx
            .debug_bounds("sidebar")
            .expect("侧边栏 sidebar 应默认渲染");
        assert!(
            (bar.size.width.as_f32() - crate::config::layout::SIDEBAR_ACTIVITY_W).abs() < 2.0,
            "默认目录面板收起，侧边栏应只剩活动栏宽，实际={bar:?}"
        );
        assert!(
            cx.debug_bounds("title-sidebar-toggle").is_some(),
            "标题栏应有侧边栏开关 title-sidebar-toggle"
        );

        // 整体收起：sidebar 与内容区包裹层一起移除
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar").is_none(),
            "收起后侧边栏不应再渲染"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "收起后内容区包裹层不应再渲染"
        );

        // 再次展开：侧边栏恢复
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar").is_some(),
            "再次展开后侧边栏应恢复"
        );
    }

    /// 侧边栏「目录 / 搜索」竖列切换栏可调用出对应界面（已移除文件视图）。
    #[gpui::test]
    async fn sidebar_views_switch(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 目录面板默认收起（活动栏常驻）；先点「目录」打开目录视图
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 竖列切换栏有「目录/搜索」两个按钮（无「文件」）
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "默认应为目录视图"
        );
        assert!(
            cx.debug_bounds("sidebar-activity-bar").is_some(),
            "应有竖列切换栏 sidebar-activity-bar"
        );
        assert!(
            cx.debug_bounds("sidebar-btn-toc").is_some(),
            "应有目录按钮"
        );
        assert!(
            cx.debug_bounds("sidebar-btn-search").is_some(),
            "应有搜索按钮"
        );
        assert!(
            cx.debug_bounds("sidebar-btn-files").is_none(),
            "已去掉文件按钮"
        );
        // 目录按钮与搜索按钮上下竖列（搜索按钮位于目录按钮下方）
        let toc = cx.debug_bounds("sidebar-btn-toc").unwrap();
        let search = cx.debug_bounds("sidebar-btn-search").unwrap();
        assert!(
            search.origin.y > toc.origin.y,
            "搜索按钮应在目录按钮之下（竖列），toc={toc:?} search={search:?}"
        );

        // 切到搜索视图
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx)
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search").is_some(),
            "搜索视图应渲染 sidebar-search"
        );
        assert!(
            cx.debug_bounds("sidebar-search-input").is_some(),
            "搜索视图应有输入框 sidebar-search-input"
        );
        assert!(
            cx.debug_bounds("sidebar-toc").is_none(),
            "切换到搜索后不应再渲染目录视图"
        );
    }

    /// 回归：侧边栏「目录」视图的条目应带 PDF 书签目录同款样式——
    /// 有子级的条目渲染折叠箭头，点击折叠后隐藏其后代标题，再点恢复。
    #[gpui::test]
    async fn sidebar_toc_pdf_style_collapsible(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join("gpui-editor-sidebar-toc.md");
        std::fs::write(
            &dir,
            "# 第一章 这是一个特别长的标题用于验证目录自适应宽度\n正文\n## 第一节\n小节\n# 第二章\n结尾\n",
        )
        .unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 目录面板默认收起（不挤正文）；打开目录视图后再验证自适应宽度/条目/折叠
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 目录视图宽度自适应：长标题应把侧边栏撑到超过固定默认宽（240）
        let bar = cx
            .debug_bounds("sidebar")
            .expect("侧边栏 sidebar 应存在");
        assert!(
            bar.size.width.as_f32() > 240.0,
            "目录视图应随内容自适应加宽，实际={bar:?}"
        );
        assert!(
            cx.debug_bounds("sidebar-resize").is_some(),
            "侧边栏右缘应有拖拽手柄 sidebar-resize"
        );
        // 侧边栏目录应带「目录」面板头（无关闭按钮，开关走活动栏「目录」按钮）
        assert!(
            cx.debug_bounds("sidebar-toc-header").is_some(),
            "目录视图应有面板头 sidebar-toc-header（含「目录」标题）"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-close").is_none(),
            "目录视图不应再有独立的关闭按钮（由「目录」按钮再点一次关闭）"
        );

        // 目录条目 + 折叠箭头渲染
        assert!(
            cx.debug_bounds("sidebar-toc-md-0").is_some()
                && cx.debug_bounds("sidebar-toc-md-1").is_some()
                && cx.debug_bounds("sidebar-toc-md-2").is_some(),
            "侧边栏目录应有标题条目 sidebar-toc-md-*"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-caret-0").is_some(),
            "h1（第一章）有子级 h2，应渲染折叠箭头 sidebar-toc-caret-0"
        );

        // 折叠 h1 → h2 后代条目隐藏
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.toggle_sidebar_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc-md-1").is_none(),
            "折叠父项后其后代 h2 条目应隐藏"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-md-2").is_some(),
            "折叠不影响其它顶级条目"
        );

        // 再展开 → h2 恢复
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.toggle_sidebar_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc-md-1").is_some(),
            "再次展开父项后 h2 条目应恢复"
        );

        // 右缘拖拽调宽：start → update → end，宽度按位移更新，且关闭自适应
        let w0 = cx.update(|_w, cx| app.read(cx).sidebar_width);
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.start_sidebar_resize(300.0, cx);
                app.update_sidebar_resize(320.0, cx);
                app.end_sidebar_resize(cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (w1, auto) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar_width, a.sidebar_width_auto)
        });
        assert!(
            (w1 - w0 - 20.0).abs() < 1.0,
            "拖拽后侧边栏宽应按位移更新，w0={w0:.1} w1={w1:.1}"
        );
        assert!(!auto, "手动拖拽后应关闭宽度自适应");

        // 「目录」按钮开关逻辑：再点一次只收起目录面板，侧边栏（活动栏）保留为窄条
        assert!(
            cx.update(|_w, cx| app.read(cx).sidebar_open),
            "打开文件后侧边栏应处于展开"
        );
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (open, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar_open, a.sidebar_panel_open)
        });
        assert!(open && !panel, "再点「目录」应收起目录面板、侧边栏仍展开");
        assert!(
            cx.debug_bounds("sidebar-toc").is_none(),
            "再次点「目录」后目录内容应关闭"
        );
        // 侧边栏仍可见，但收窄为活动栏（46px），不留空白宽区
        let slim = cx
            .debug_bounds("sidebar")
            .expect("目录面板收起后侧边栏（活动栏）应仍在");
        assert!(
            (slim.size.width.as_f32() - crate::config::layout::SIDEBAR_ACTIVITY_W).abs() < 2.0,
            "面板收起后侧边栏应只剩活动栏宽，实际={slim:?}"
        );
        assert!(
            cx.debug_bounds("sidebar-activity-bar").is_some(),
            "活动栏在面板收起后应仍可见"
        );
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "再次点「目录」应重新展开目录面板"
        );

        // 回归：再次拖窄目录后，条目不得溢出侧边栏右缘（否则其 hover/点击会延伸到编辑器区）
        let wnow = cx.update(|_w, cx| app.read(cx).sidebar_width);
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.start_sidebar_resize(wnow + 50.0, cx);
                app.update_sidebar_resize(wnow - 60.0, cx);
                app.end_sidebar_resize(cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sb = cx
            .debug_bounds("sidebar")
            .expect("拖窄后侧边栏应仍存在");
        let row = cx
            .debug_bounds("sidebar-toc-md-0")
            .expect("目录条目应仍存在");
        let sb_right = sb.origin.x.as_f32() + sb.size.width.as_f32();
        let row_right = row.origin.x.as_f32() + row.size.width.as_f32();
        assert!(
            row_right <= sb_right + 1.0,
            "拖窄后目录条目应随面板裁剪、不溢出右缘：row_right={row_right:.0} sb_right={sb_right:.0}"
        );

        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：侧边栏搜索的「行内容」只搜当前活动文档——在 A 文档激活时，
    /// 输入只出现在 B 文档里的关键字，不应冒出 B 的内容行（旧逻辑扫所有打开
    /// 选项卡的 content，在 PDF 预览下会误显示 EPUB 正文）。
    #[gpui::test]
    async fn sidebar_search_scoped_to_active_doc(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let a = std::env::temp_dir().join("gpui-sz-a.md");
        let b = std::env::temp_dir().join("gpui-sz-b.md");
        std::fs::write(&a, "# 甲\n阿尔法内容仅此一处\n").unwrap();
        std::fs::write(&b, "# 乙\nshanghai 明珠塔夜景\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(a.to_str().unwrap(), window, cx));
            app.update(cx, |app, cx| app.open_file(b.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 第 0 个选项卡是初始空「未命名」，A=1、B=2。激活 A，切到搜索视图，
        // 键入只存在于 B 的关键字
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(1, window, cx));
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx);
            });
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| s.set_value("shanghai", window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        // 活动文档 A 不含 "shanghai" → 不应出现任何行结果（B 的行不得冒出来）
        assert!(
            cx.debug_bounds("sidebar-search-line-2-1").is_none()
                && cx.debug_bounds("sidebar-search-line-1-0").is_none()
                && cx.debug_bounds("sidebar-search-line-0-0").is_none(),
            "激活 A 时搜索 B 的关键字不应返回内容行"
        );
        // 却不影响文件名匹配（文件名含关键字的文件仍可选中）；两文件名均不含关键字，跳过

        // 切到 B 再搜同一关键字 → 应出现 B 的内容行
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(2, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-search-line-2-1").is_some(),
            "激活 B 后搜索同一关键字应出现 B 的内容行"
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 验证侧边栏搜索的「扫描结果缓存」真实生效（优化回归，防负优化）：
    /// - 查询词/活动文档未变时，反复渲染帧**不重扫**（`search_scan_count` 不增）；
    /// - 查询词变化才重扫（计数 +1）；
    /// - 内容命中（当前活动文档行）能出结果行。
    #[gpui::test]
    async fn sidebar_search_scan_is_cached_across_renders(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-sz-cache.md");
        std::fs::write(&f, "# 甲\n阿尔法 shanghai 内容\n真义行 beta\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let ti = cx.update(|_w, cx| app.read(cx).active_tab);

        // 打开搜索面板（查询为空）：空查询不做扫描
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let before = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(before, 0, "查询为空时不应扫描");

        // 第一次键入「shanghai」→ 扫描一次，命中第 1 行
        cx.update(|window, cx| {
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| s.set_value("shanghai", window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let after = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(after, 1, "首次搜索应精确扫描 1 次");
        let hit_row1 = Box::leak(format!("sidebar-search-line-{ti}-1").into_boxed_str());
        assert!(
            cx.debug_bounds(hit_row1).is_some(),
            "应命中第 1 行（shanghai），ti={ti}"
        );

        // 同一查询反复渲染（模拟逐帧渲染）→ 缓存命中，不重扫
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let steady = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(steady, after, "查询未变时重复渲染不应重扫");

        // 换查询「beta」→ 再扫描一次，命中第 2 行
        cx.update(|window, cx| {
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| s.set_value("beta", window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let changed = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(changed, steady + 1, "查询变化应再扫描一次");
        let hit_row2 = Box::leak(format!("sidebar-search-line-{ti}-2").into_boxed_str());
        assert!(
            cx.debug_bounds(hit_row2).is_some(),
            "换查询 beta 应命中第 2 行"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 基准：侧边栏搜索「旧每帧全扫 vs 新缓存 + 无分配内核」的代价对比。
    /// 旧逻辑：渲染帧每帧 `to_string()` 物化全文 + 每行 `to_lowercase()` 分配 + `contains`；
    /// 新逻辑：查询变化才扫一次（无分配 [`find_ci`]），其余渲染帧走 O(1) 缓存键比较。
    /// 打印、不构成断言；`--ignored` 运行（匹配项目既有 bench 惯例）。
    #[test]
    #[ignore]
    fn bench_sidebar_search_old_vs_new() {
        use crate::app::sidebar::find_ci;
        use std::time::Instant;
        let block = "# 章节标题\n\n普通段落内容 alpha beta 关键字 shanghai 你好\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut doc = String::with_capacity(block.len() * 12000);
        for _ in 0..12000 {
            doc.push_str(block);
        }
        let q = "shanghai";
        let lines: Vec<&str> = doc.lines().collect();
        let n = lines.len();

        // 旧内核：每行 to_lowercase() 分配后 contains（模拟每个渲染帧全量重扫）
        let t0 = Instant::now();
        let mut old_hits = 0usize;
        for _ in 0..100 {
            old_hits = lines.iter().filter(|l| l.to_lowercase().contains(q)).count();
        }
        let old = t0.elapsed();

        // 新内核：无分配 find_ci（同样每帧扫，展示内核本身提速）
        let t1 = Instant::now();
        let mut new_hits = 0usize;
        for _ in 0..100 {
            new_hits = lines.iter().filter(|l| find_ci(l, q).is_some()).count();
        }
        let new_kernel = t1.elapsed();
        assert_eq!(old_hits, new_hits, "新旧命中数必须一致");

        // 新路径整体：100 帧 = 1 次真实扫描 + 99 次 O(1) 缓存键比较
        let t2 = Instant::now();
        let mut key = String::new();
        for i in 0..100 {
            if key != q {
                key = q.to_string();
            }
            let _ = i;
        }
        let cached_path = t2.elapsed();

        println!(
            "侧边栏搜索（{n} 行）：100 帧旧内核={}ms, 新内核={}ms（内核提升 {:.1}x）, 新缓存路径(1 扫+99 次键比较)={}ms",
            old.as_millis(),
            new_kernel.as_millis(),
            old.as_secs_f64() / new_kernel.as_secs_f64().max(1e-9),
            cached_path.as_millis(),
        );
    }

    #[gpui::test]
    async fn md_preview_hscroll_in_app(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let long_line = "f".repeat(500);
        let dir = std::env::temp_dir().join("gpui-editor-hscroll-test.md");
        std::fs::write(&dir, format!("```rs\n{long_line}\n```\n")).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let preview = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染");
        assert!(
            preview.size.width > px(0.),
            "预览视口应有宽度，实际={:?}",
            preview.size.width
        );
        // 超宽代码带应宽于预览视口（可横向滚动），滚动条叠层非零渲染
        let band = cx
            .debug_bounds("md-code-band")
            .expect("代码背景带 md-code-band 应存在");
        assert!(
            band.size.width > preview.size.width,
            "代码带宽应宽于预览视口（可横向滚动），band={band:?} preview={preview:?}"
        );
        let sbar = cx
            .debug_bounds("md-code-scrollbar")
            .expect("代码块横向滚动条 md-code-scrollbar 应存在");
        assert!(
            sbar.size.width > px(0.) && sbar.size.height > px(0.),
            "横向滚动条不应塌缩成 0，实际={sbar:?}"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// Markdown 编辑区与预览区默认/始终自动换行，不提供切换 → 状态栏隐藏「换行/不换行」按钮；
    /// 普通文本文件仍保留该切换按钮。
    #[gpui::test]
    async fn wrap_button_hidden_for_markdown(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // md 文件：应隐藏换行按钮
        let md = std::env::temp_dir().join("gpui-editor-wrap-test.md");
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-wrap-btn").is_none(),
            "Markdown 文件预览时状态栏不应出现换行开关"
        );
        let wraps = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.read(cx).is_wrap()
        });
        assert!(wraps, "刚打开 Markdown 预览应默认自动换行");
        let _ = std::fs::remove_file(&md);

        // 普通文本文件：应保留换行按钮
        let txt = std::env::temp_dir().join("gpui-editor-wrap-test.txt");
        std::fs::write(&txt, "hello world\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-wrap-btn").is_some(),
            "普通文本文件状态栏应保留换行开关"
        );
        let _ = std::fs::remove_file(&txt);
    }

    /// 回归：「查看」→「自动换行」只作用编辑区，不改变 Markdown 预览的换行状态。
    /// 旧逻辑 toggle_wrap 以 `preview_on || wrap_enabled` 算 editor wrap——Markdown
    /// 打开即 preview_on=true → 编辑区恒为换行、"开关只动预览"；现在去掉预览联动。
    #[gpui::test]
    async fn wrap_toggle_affects_md_editor_only(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join("gpui-editor-wrap-toggle.md");
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 初始：预览自动换行；全局编辑区换行偏好默认关闭（仅 md 编辑区默认换行）
        let (w0, pref0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_preview.read(cx).is_wrap(),
                a.wrap_enabled,
            )
        });
        assert!(w0, "initial 预览应自动换行，w0={w0}");
        assert!(!pref0, "全局编辑区换行偏好应默认关闭（仅 md 编辑区换行），pref0={pref0}");

        // 切换换行：只作用编辑区 → 预览保持换行、全局偏好开启
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (w1, pref1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_preview.read(cx).is_wrap(),
                a.wrap_enabled,
            )
        });
        assert!(
            w1,
            "切换「自动换行」不应改变 Markdown 预览（预览始终自动换行），实际 w1={w1}"
        );
        assert!(pref1, "切换一次后全局编辑区换行偏好应开启");

        // 再切回：预览仍保持换行、偏好恢复关闭
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (w2, pref2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_preview.read(cx).is_wrap(),
                a.wrap_enabled,
            )
        });
        assert!(w2, "预览应保持换行，w2={w2}");
        assert!(!pref2, "再切换后全局编辑区换行偏好应恢复关闭");
        let _ = std::fs::remove_file(&md);
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
        // 滚动条默认自动隐藏——先“滚动”点亮它，再断言浮层存在。
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| app.poke_tab_hscroll(cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 回归：横向滚动条浮层应在选项卡栏内（底部贴边、有可见高度），
        // 而不是被推到栏外裁掉（选项卡栏去上下内边距后曾把浮层挤出）。
        let overlay = cx
            .debug_bounds("tab-bar-hscroll-overlay")
            .expect("超宽时应有横向滚动条浮层 tab-bar-hscroll-overlay");
        assert!(
            overlay.size.height > px(0.),
            "滚动条浮层应有可见高度：overlay={overlay:?}"
        );
    }

    /// 选项卡栏横向滚动条自动隐藏：默认隐藏 → 滚动(滚轮)时出现 → 停止滚动
    /// 超时后自动隐藏；拖拽滑块期间保持可见。
    #[gpui::test]
    async fn tab_bar_hscroll_auto_hides(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(400.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 大量选项卡 → 超宽可横向滚动
        for _ in 0..30 {
            cx.update(|window, cx| app.update(cx, |app, cx| app.new_tab(window, cx)));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // ① 默认隐藏：即使超宽，可见标记为假且浮层不渲染
        let hidden = cx.update(|_w, cx| *app.read(cx).tab_bar_hscroll.visible.borrow());
        assert!(!hidden, "滚动条初始应隐藏");
        assert!(
            cx.debug_bounds("tab-bar-hscroll-overlay").is_none(),
            "隐藏时浮层不应渲染"
        );

        // ② 向选项卡滚动区派发一次横向滚轮 → 点亮并渲染浮层
        let tabs_area = cx
            .debug_bounds("tab-bar-tabs")
            .expect("应有选项卡滚动区 tab-bar-tabs");
        let wheel_pos = gpui::Point::new(
            px(tabs_area.origin.x.as_f32() + tabs_area.size.width.as_f32() / 2.0),
            px(tabs_area.origin.y.as_f32() + 8.0),
        );
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: wheel_pos,
            delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(-40.), px(0.))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let shown = cx.update(|_w, cx| *app.read(cx).tab_bar_hscroll.visible.borrow());
        assert!(shown, "滚轮滚动后滚动条应可见");
        assert!(
            cx.debug_bounds("tab-bar-hscroll-overlay").is_some(),
            "滚动后浮层应渲染"
        );

        // ③ 停止滚动超过隐藏时长 → 自动隐藏
        cx.executor().advance_clock(std::time::Duration::from_millis(2000));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let hidden_again = cx.update(|_w, cx| *app.read(cx).tab_bar_hscroll.visible.borrow());
        assert!(!hidden_again, "停止滚动超过时长后应自动隐藏");
        assert!(
            cx.debug_bounds("tab-bar-hscroll-overlay").is_none(),
            "隐藏后浮层应消失"
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
            (
                a.tabs.len(),
                a.tabs[a.active_tab].editor.read(cx).value().to_string(),
            )
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
                a.tabs[a.active_tab].editor.read(cx).value().to_string().lines().count(),
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

    /// Markdown 预览同步（无去抖，合成一轮立即同步）：编辑触发后同步完成并复位挂起，
    /// 内容**真实落盘**到预览（后台 `scan_blocks` 路径，UI 线程只做快照 + 装配）。
    #[gpui::test]
    async fn preview_syncs_content_after_edit(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-bgpreview.md");
        std::fs::write(&p, "# 初始标题\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let preview = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.clone()
        });
        let t0 = cx.update(|_w, cx| preview.read(cx).rendered_text());
        assert!(t0.contains("初始标题"), "打开时预览应含初始标题，实际 {t0:?}");

        // 真实编辑：粘贴触发 InputEvent::Change → 立即进入合成一轮同步
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("新增段落".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        // 同步在后台扫描、尽可能多地驱动到完成
        for _ in 0..30 {
            cx.run_until_parked();
        }
        let t1 = cx.update(|_w, cx| preview.read(cx).rendered_text());
        assert!(
            t1.contains("新增段落"),
            "编辑后预览应同步到新内容（后台扫描路径），实际 {t1:?}"
        );
        let pending = cx.update(|_w, cx| app.read(cx).preview_pending);
        assert!(!pending, "同步完成后应复位挂起状态");
        let _ = std::fs::remove_file(&p);
    }

    /// #4 回归：快速连续两次编辑（第一轮后台扫描可能被第二轮抢占/丢弃），最终预览
    /// 必须收敛到**最新**内容，旧轮结果不得覆盖新编辑（`preview_seq` 代次守卫）。
    #[gpui::test]
    async fn preview_sync_rapid_edits_converges_without_stale_override(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-bgpreview2.md");
        std::fs::write(&p, "A\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let preview = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.clone()
        });
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        // 第一次编辑 → 250ms（第一轮计时到点，开始后台扫描）
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("B".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        cx.executor().advance_clock(std::time::Duration::from_millis(250));
        for _ in 0..10 {
            cx.run_until_parked();
        }
        // 第二次编辑 → 再 250ms（第二轮接管），最终预览应含 B、C 且不为旧内容覆盖
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("C".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        for _ in 0..2 {
            cx.run_until_parked();
        }
        cx.executor().advance_clock(std::time::Duration::from_millis(250));
        for _ in 0..30 {
            cx.run_until_parked();
        }
        let t = cx.update(|_w, cx| preview.read(cx).rendered_text());
        assert!(
            t.contains("C") && t.contains("B"),
            "最终预览应含两次编辑内容（收敛到最新），实际 {t:?}"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// #4 回归：编辑同步的行/字数改用 Rope 的 O(1) 计数（每键不再物化全文），
    /// 数值必须与旧 `str::lines()/chars()` 语义一致——在真实编辑（粘贴触发
    /// `InputEvent::Change`）后与 `editor.value()` 的 str 推导值逐项比对。
    #[gpui::test]
    async fn editing_updates_line_char_counts_via_rope(cx: &mut TestAppContext) {
        fn assert_counts(cx: &mut VisualTestContext, app: &gpui::Entity<EditorApp>, what: &str) {
            let (lc, cc, expected_lc, expected_cc) = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                let val = t.editor.read(cx).value();
                (
                    t.line_count,
                    t.char_count,
                    val.lines().count().max(1),
                    val.chars().count(),
                )
            });
            assert_eq!(lc, expected_lc, "{what}：行数应与 str::lines() 一致");
            assert_eq!(cc, expected_cc, "{what}：字数应与 str::chars() 一致");
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-counts.txt");
        std::fs::write(&p, "第一行\n第二行\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "打开文件后");
        // 真实编辑：粘贴触发 InputEvent::Change → 走新的 Rope O(1) 计数
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("第三行".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "粘贴编辑后");
        // 再粘贴带换行的内容，行数应增加且仍与 str 语义一致
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("A\nB".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "粘贴多行后");
        let _ = std::fs::remove_file(&p);
    }

    /// 基准：编辑大文档时「每键同步行/字数」的代价——旧逻辑每键物化全文 +
    /// `lines()/chars()` 全量扫描（16 万行约产生 3 次 O(n) + 一次 6MB 副本，编辑卡顿主因），
    /// 新逻辑走 Rope 的 O(1) 计数零物化。打印、不构成断言；`--ignored` 运行。
    #[test]
    #[ignore]
    fn bench_edit_sync_counts_old_materialize_vs_rope() {
        use gpui_component::input::{Rope, RopeExt};
        use std::time::Instant;
        let line = "普通段落内容 alpha beta 关键字 shanghai 你好\n";
        let mut doc = String::with_capacity(line.len() * 160_000);
        for _ in 0..160_000 {
            doc.push_str(line);
        }
        let rope = Rope::from(doc.as_str());
        let runs = 60usize;
        // 旧：value() 物化（6MB 副本）+ lines().count() + chars().count()
        let t0 = Instant::now();
        let mut sink = 0usize;
        for _ in 0..runs {
            let v: gpui::SharedString = doc.clone().into();
            let lc = v.lines().count().max(1);
            let cc = v.chars().count();
            sink = sink.wrapping_add(lc + cc);
        }
        let old = t0.elapsed();
        // 新：Rope 的 O(1) 计数（len_chars / lines_len / byte），零物化
        let t1 = Instant::now();
        let mut sink2 = 0usize;
        for _ in 0..runs {
            let lines = rope.lines_len();
            let lc = if rope.len() > 0 && rope.byte(rope.len() - 1) == b'\n' {
                lines - 1
            } else {
                lines
            };
            let cc = rope.len_chars();
            sink2 = sink2.wrapping_add(lc.max(1) + cc);
        }
        let new_ = t1.elapsed();
        assert_eq!(sink, sink2, "新旧计数的结果必须一致");
        println!(
            "编辑每键行/字计数（{} 行 × {runs} 次）：旧(物化 6MB+lines+chars)={}ms, 新(Rope O(1))={}ms（提升 {:.1}x）",
            160_000,
            old.as_millis(),
            new_.as_millis(),
            old.as_secs_f64() / new_.as_secs_f64().max(1e-9),
        );
    }

    /// 基准：大文档预览同步里 `scan_blocks` 的代价（#4 把它从 UI 线程挪到后台）。
    /// 打印、不构成断言；`--ignored` 运行。
    #[test]
    #[ignore]
    fn bench_preview_sync_scan_blocks_off_ui_thread() {
        use std::time::Instant;
        let block = "# 章节标题\n\n正文段落 alpha beta 关键字 shanghai\n\n- 列表项\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut doc = String::with_capacity(block.len() * 12000);
        for _ in 0..12000 {
            doc.push_str(block);
        }
        // 旧路径 UI 线程同步成本 = value().to_string() + scan_blocks（都占 UI）
        let t0 = Instant::now();
        let idx = crate::markdown::scan_blocks(&doc);
        let scan = t0.elapsed();
        // 新路径 UI 线程只做快照（value() 的 SharedString 物化），scan_blocks 在后台
        let t1 = Instant::now();
        let _ = std::hint::black_box(gpui::SharedString::from(doc.clone()));
        let snap = t1.elapsed();
        println!(
            "大文档预览同步（{} 行 / {} MB，{} 块）：scan_blocks={}ms（已转后台不占 UI），UI 快照仅 {}ms",
            doc.lines().count(),
            doc.len() / 1024 / 1024,
            idx.blocks.len(),
            scan.as_millis(),
            snap.as_millis(),
        );
    }

    /// 回归：首次在新选项卡编辑内容后打开预览，预览应默认换行
    /// （不出现单行横向滚动），与「md 文件打开即换行」一致。
    #[gpui::test]
    async fn preview_defaults_to_wrap_after_edit_then_toggle(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 初始空选项卡（未命名）里输入一段超长内容
        let long_text = "这是一个比较长的段落".repeat(60);
        cx.update(|window, cx| {
            let idx = app.read(cx).active_tab;
            let ed = app.read(cx).tabs[idx].editor.clone();
            ed.update(cx, |ed, _| ed.set_text(&long_text));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 打开预览
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_preview_for_tab(app.active_tab, window, cx)
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let wrap = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.read(cx).is_wrap()
        });
        assert!(wrap, "编辑内容后首次打开预览应默认为换行模式");
    }

    /// 语法高亮回归：打开 rust 文件，解析在后台异步进行（不卡 UI / 不 panic），
    /// 推进时钟超过防抖后解析落地并触发重渲染，编辑器区正常、内容完整载入。
    #[gpui::test]
    async fn opens_rust_file_highlight_async_no_hang(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join("gpui-syntax-async.rs");
        let mut src = String::from(
            "fn main() {\n    // 注释\n    let s = \"hello\";\n    println!(\"{} 世界\", s);\n}\n",
        );
        src.push_str(&"// filler line\n".repeat(600));
        std::fs::write(&dir, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        // 推进超过解析防抖（150ms），后台解析落地 → 触发重渲染
        cx.executor().advance_clock(std::time::Duration::from_millis(400));
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("editor-area").is_some(),
            "打开 rust 后编辑区应正常渲染"
        );
        assert_eq!(
            cx.update(|_w, cx| {
                app.read(cx).tabs[app.read(cx).active_tab]
                    .editor
                    .read(cx)
                    .value()
                    .to_string()
            }),
            src,
            "rust 文件内容应完整载入"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：>1MB 文本文件走「后台打开」流水线——`open_file` 立即返回（编辑器仍为空、
    /// 不在 UI 线程同步读+统计），随后后台线程装配出完整内容（字节级一致）。同时
    /// 防「打开耗时随文件体积线性冻住 UI」这一性质被后续改动破坏。
    #[gpui::test]
    async fn opens_large_text_file_asynchronously(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-large-open-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let line = "x".repeat(80);
        let content = (0..20_000)
            .map(|i| format!("{line} {i:07}\n"))
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();
        let tabs_before = cx.update(|_, cx| app.read(cx).tabs.len());

        // 打开应**立即返回**且编辑器仍为空（内容由后台线程装配，不阻塞 UI）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let empty_after_open = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].editor.read(cx).value().is_empty()
        });
        assert!(empty_after_open, "大文件应异步打开：open_file 返回时编辑器应为空");

        // 驱动后台任务 + 装配，直到内容字节级一致。
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let tab = &a.tabs[a.active_tab];
                !tab.editor_pending && tab.editor.read(cx).value() == content
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大文件后台打开超时：{} 帧未装配完成", frames);
            }
        }
        assert_eq!(
            cx.update(|_, cx| app.read(cx).tabs.len()),
            tabs_before + 1,
            "大文件打开应新增一个选项卡"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：>128KB 的 Markdown 走「两段式打开」**完整流程**——快段（defer）即时挂载
    /// 编辑器内容，慢段后台并行 行宽统计 + scan_blocks 后补齐滚动条统计与预览索引。
    /// 断言两段各执行 1 次（open_fast_count/open_finish_count）、编辑器内容字节级一致、
    /// 预览块索引就绪、打开状态成功。
    #[gpui::test]
    async fn opens_large_markdown_full_flow(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-large-md-flow-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let content = (0..3000)
            .map(|i| {
                format!(
                    "# 大标题 {i}\n\n这是第 {i} 段正文，包含 *强调*、`行内代码` 与 [链接](https://x.y)。\n\n"
                )
            })
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                let tab = &a.tabs[i];
                !tab.editor_pending
                    && tab.editor.read(cx).value() == content
                    && tab.md_preview.read(cx).memory_estimate().0 > 0
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大 md 完整流程超时：{} 帧", frames);
            }
        }
        let (fast_count, finish_count, preview_blocks, status_ok) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (
                a.open_fast_count,
                a.open_finish_count,
                a.tabs[i].md_preview.read(cx).memory_estimate().0,
                a.status_ok,
            )
        });
        assert_eq!(fast_count, 1, "两段式快段应执行 1 次");
        assert_eq!(finish_count, 1, "两段式慢段应执行 1 次");
        assert!(preview_blocks > 0, "预览块索引应就绪");
        assert!(status_ok, "打开状态应成功");
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：大文档打开把「ListState 全量装配（O(N)）」从打开瞬间移出——直挂总绳的
    /// mount 只建「视口小列表」（同一同步栈内即可断言 item_count 远小于全文行数，
    /// 不依赖帧时序），全文 N 条统一高度由 render 排定的 defer 补建（量程恢复健康）。
    /// 防止退化成 mount 同步 O(N) 建全文（把 ~85ms 开销挪回打开瞬间关键路径）。
    #[gpui::test]
    async fn open_large_doc_mounts_list_two_phase(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let line = "x".repeat(60);
        let content = (0..12_000)
            .map(|i| format!("{line} {i:010}\n"))
            .collect::<String>();
        use gpui_component::input::RopeExt as _;
        let rope = gpui_component::input::Rope::from(content.as_str());
        let line_count = rope.lines_len();
        assert!(
            line_count > crate::config::limits::EDITOR_LIST_DEFER_MIN_LINES as usize,
            "测试应超过两段式阈值，实际 {line_count} 行"
        );

        let ed_ent = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        // 同步栈内断言两段式：mount 返回时只建了视口小列表（若非两段式，这里
        // item_count 直接等于全文行数）。
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, _| {
                ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                let ic = ed.test_list_state().item_count();
                assert!(
                    ic < line_count && ic <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
                    "mount 同步栈内应为视口小列表：item_count={ic}（全文 {line_count} 行，\
                     两段式应只建 ≤{}）",
                    crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
                );
            });
        });
        // 驱动帧：render 排 defer → 补建全文 → 量程健康。
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            let done = cx.update(|_w, cx| ed_ent.read(cx).test_list_state().item_count() == line_count);
            if done {
                break;
            }
            frames += 1;
            if frames > 120 {
                panic!("defer 补建全文超时：{} 帧后 item_count 仍 < line_count", frames);
            }
        }
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let (mo, vh, lh) = cx.update(|_w, cx| {
            let ed = ed_ent.read(cx);
            (
                ed.list_max_offset(),
                ed.list_viewport_height(),
                ed.test_metrics().1,
            )
        });
        let expected = (line_count.saturating_sub(1)) as f32 * lh - vh;
        assert!(
            mo > expected * 0.9,
            "defer 补建后量程应健康：max_off={mo:.0}，预期≈{expected:.0}（行 {line_count}/高 {lh:.0}px）"
        );
    }

    /// 基准（ignored，release）：隔离测量大列表「统一高度重铺」的原始开销——打开大
    /// 文档时 defer 补建全文用的就是它。定位「两段式第二帧」的量级，以及验证
    /// `reset_with_uniform_height` 单趟构建相对旧「reset + with_uniform_item_height」
    /// 两趟的速度差异。运行：
    /// `cargo test --release -- --ignored bench_reset_uniform_height --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_reset_uniform_height(cx: &mut TestAppContext) {
        init_test(cx);
        for n in [16_000usize, 169_721] {
            let ls = gpui::ListState::new(1, gpui::ListAlignment::Top, gpui::px(16.));
            let mut best_new = u128::MAX;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                ls.reset_with_uniform_height(n, gpui::px(21.5));
                best_new = best_new.min(t.elapsed().as_micros());
            }
            let mut best_two = u128::MAX;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                ls.reset(n);
                let _ = ls.clone().with_uniform_item_height(gpui::px(21.5));
                best_two = best_two.min(t.elapsed().as_micros());
            }
            println!(
                "【reset_with_uniform_height】 n={n}: 单趟 new={}µs, 旧两趟(reset+uniform)={}µs",
                best_new, best_two
            );
            let _ = cx;
        }
    }

    /// 回归：两段式打开大 md 后，**纵向滚动条量程必须健康**（拖到底可到全文末尾）。
    /// 根因曾为：gpui 在布局宽度变化时清空 ListState 全部 size_hint；编辑器统一高度
    /// 提示虽在 render 里重挂，但被同帧 prepaint 再次抹掉、且没有请求后续帧 →
    /// `max_offset_for_scrollbar()` 坍缩到 ~0，拖到底也滚不到底。修复：重挂时
    /// `cx.notify()` 多画一帧，下一帧宽度对齐后提示才真正生效（与 Markdown 预览同手法）。
    #[gpui::test]
    async fn opens_large_md_keeps_vertical_scrollbar_healthy(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-scroll-healthy-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut content = format!("{}\n", "x".repeat(8000));
        content.push_str(
            &(0..5000)
                .map(|i| {
                    format!("# 大标题 {i}\n\n这是第 {i} 段正文，包含 *强调* 与 `code`。\n\n")
                })
                .collect::<String>(),
        );
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值"
        );
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        // 等待完整流程（快段 + 慢段 finish），并通过 refresh 驱动「重挂统一高度提示后
        // 的多画一帧」（gpui 测试里 notify 不会自动重画，需 refresh；真实应用由帧循环）。
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            let done = cx.update(|_, cx| app.read(cx).open_finish_count >= 1);
            if done && frames > 6 {
                break;
            }
            frames += 1;
            if frames > 120 {
                break;
            }
        }
        let (lines, lh) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let ed = a.tabs[i].editor.read(cx);
            (ed.line_count(), ed.test_metrics().1)
        });
        // 编辑器「再次渲染后量程自愈」：idle 时 summary 可能被最后一次换宽清掉（gpui
        // 只在实际布局时清），但只要再渲染一次（用户移动鼠标/滚轮即如此），
        // `ensure_uniform_hint` 应重挂恢复。手动 notify 驱动一次渲染来验证。
        let ed_ent = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.clone()
        });
        cx.update(|_, cx| {
            ed_ent.update(cx, |_, cx| cx.notify());
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let (mo, vh) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let ed = a.tabs[i].editor.read(cx);
            (ed.list_max_offset(), ed.list_viewport_height())
        });
        let expected = (lines.saturating_sub(1)) as f32 * lh - vh;
        assert!(
            mo > expected * 0.9,
            "编辑器量程在再次渲染时应自愈（max_off={mo:.0}，预期≈{expected:.0}）"
        );
        // 编辑器「拖到底」：模拟 VerticalScrollHandle.start_drag 的重铺+冻结+拖底，
        // 断言拖到底后最后内容可见。
        let els = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.read(cx).test_list_state()
        });
        els.reset_with_uniform_height(lines, gpui::px(lh));
        els.scrollbar_drag_started();
        els.set_offset_from_scrollbar(gpui::Point::new(gpui::px(0.), gpui::px(-10_000_000.)));
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let eix = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .editor
                .read(cx)
                .test_list_state()
                .logical_scroll_top()
                .item_ix
        });
        assert!(
            lines.saturating_sub(eix) <= 50,
            "编辑器拖到底应到达最后内容（滚动顶行 item_ix={eix}/{lines}）"
        );
        // 预览侧量程同样须**精确**（≈ 全篇块高之和），不能因抽样/坍缩缩到视口高
        // ——否则预览滚动条拖不到底部。回归：精确平均行高替代 256 块抽样。
        let (pv_total, blocks, est, applied) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let pv = a.tabs[i].md_preview.read(cx);
            (
                pv.vertical_total_px(),
                pv.memory_estimate().0,
                pv.test_est_hint(),
                pv.test_hint_applied(),
            )
        });
        assert!(
            pv_total > blocks as f32 * 15.0,
            "预览纵向量程应精确（total={pv_total:.0}px / {blocks} 块，est_hint={est:.0}，applied={applied:?}，坍缩态≈{vh:.0}px）"
        );
        // 确定性「拖到底」回归：模拟 PreviewVerticalScrollHandle.start_drag 的重铺+冻结+
        // 拖底（用户实测：内置滚动条在量程坍缩/未自愈时开始拖，冻结旧值 → 拖不到真正的
        // 底部，只能滚轮兜底）。断言拖到底后逻辑项到最后一两块。
        let drag_top = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].md_preview.read(cx).test_list_state()
        });
        drag_top.reset_with_uniform_height(blocks, gpui::px(est));
        drag_top.scrollbar_drag_started();
        drag_top.set_offset_from_scrollbar(gpui::Point::new(gpui::px(0.), gpui::px(-10_000_000.)));
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let top_ix = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .md_preview
                .read(cx)
                .test_list_state()
                .logical_scroll_top()
                .item_ix
        });
        assert!(
            blocks.saturating_sub(top_ix) <= 50,
            "预览拖到底应到达最后内容（滚动顶块 item_ix={top_ix}，共 {blocks}；应距末尾在一个视口内）"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 诊断（临时，手工跑）：真机大文档上对比编辑器/预览双层量程 + 模拟拖到底。
    /// 依赖 `target/大文档测试.md` 存在；正常跑套件时跳过。#[ignore] 手动：
    /// `cargo test -- --ignored diag_drag_bottom_real_file --nocapture`
    #[gpui::test]
    #[ignore]
    async fn diag_drag_bottom_real_file(cx: &mut TestAppContext) {
        init_test(cx);
        std::env::set_var("MD_OPEN_DEBUG", "1");
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"target/大文档测试.md", window, cx);
            });
        });
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let (n_lines, ed_max, ed_vh, lh, pw_total, pv_blocks, pv_est) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let ed = a.tabs[i].editor.read(cx);
            let pv = a.tabs[i].md_preview.read(cx);
            (
                ed.line_count(),
                ed.list_max_offset(),
                ed.list_viewport_height(),
                ed.test_metrics().1,
                pv.vertical_total_px(),
                pv.memory_estimate().0,
                pv.test_est_hint(),
            )
        });
        println!(
            "DIAG 编辑器: lines={n_lines} 摘要总高={:.1} 句柄高(lines×lh)={:.1} 行高={lh:.1}",
            ed_max + ed_vh,
            n_lines as f32 * lh
        );
        let (ed_vw, ed_cw, ed_lastw) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let ed = a.tabs[i].editor.read(cx);
            (
                ed.test_list_state().viewport_bounds().size.width.as_f32(),
                ed.test_content_w_px(),
                ed.test_last_list_width(),
            )
        });
        println!(
            "DIAG 编辑器宽: viewport_w={ed_vw:.1} content_w={ed_cw:.1} last_list_width={ed_lastw:.1}"
        );
        println!(
            "DIAG 预览: blocks={pv_blocks} est={pv_est:.1} 摘要total={pw_total:.1} 句柄高(blocks×est)={:.1}",
            pv_blocks as f32 * pv_est
        );

        // 编辑器模拟拖到底（重置→冻结→拖超远）
        let ls = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.read(cx).test_list_state()
        });
        ls.reset_with_uniform_height(n_lines, gpui::px(lh));
        ls.scrollbar_drag_started();
        ls.set_offset_from_scrollbar(gpui::Point::new(gpui::px(0.), gpui::px(-10_000_000.)));
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let ed_ix = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .editor
                .read(cx)
                .test_list_state()
                .logical_scroll_top()
                .item_ix
        });
        println!("DIAG 编辑器拖到底: 滚动顶行 item_ix={ed_ix} / {n_lines}（距末尾 {}）", n_lines.saturating_sub(ed_ix));

        // 预览模拟拖到底
        let pls = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].md_preview.read(cx).test_list_state()
        });
        pls.reset_with_uniform_height(pv_blocks, gpui::px(pv_est.max(0.0)));
        pls.scrollbar_drag_started();
        pls.set_offset_from_scrollbar(gpui::Point::new(gpui::px(0.), gpui::px(-10_000_000.)));
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let pv_ix = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .md_preview
                .read(cx)
                .test_list_state()
                .logical_scroll_top()
                .item_ix
        });
        println!(
            "DIAG 预览拖到底: 滚动顶块 item_ix={pv_ix} / {pv_blocks}（距末尾 {}）",
            pv_blocks.saturating_sub(pv_ix)
        );
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

    /// 打开 .pdf：PdfViewer 自包含后台解析（进 UI 不阻塞），
    /// 加载完成后全屏只读预览，解析为页级位图而非 Markdown 文本。
    #[gpui::test]
    async fn opens_pdf_and_shows_preview(cx: &mut TestAppContext) {
        let _pdf_guard = crate::pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/N107内外饰配置表NEW.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(pdf.to_str().unwrap(), window, cx)
            });
        });
        // 解析在后台执行器做，泵驱动到 PdfViewer 加载完成
        for _ in 0..200 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                app.read(cx).tabs[app.read(cx).active_tab]
                    .pdf_preview
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        let (page_count, tabs, status, status_ok, fullscreen, readonly, no_md_content) =
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                (
                    t.pdf_preview
                        .as_ref()
                        .map(|pv| pv.read(cx).page_count())
                        .unwrap_or(0),
                    a.tabs.len(),
                    a.status.to_string(),
                    a.status_ok,
                    t.preview_fullscreen,
                    t.readonly_preview,
                    t.content.is_empty(),
                )
            });
        assert!(status_ok, "打开 PDF 应成功，状态={status:?}");
        assert!(page_count >= 1, "PdfViewer 应载入至少 1 页，实际={page_count}");
        assert_eq!(tabs, 2, "应新增一个标签页");
        assert!(fullscreen, "打开 PDF 应自动进入全屏占满阅读模式");
        assert!(readonly, "打开 PDF 应锁定只读预览");
        assert!(no_md_content, "PDF 不应进入 Markdown 文本（content 应为空）");

        // PDF 阅读器自带全屏与分页，状态栏不应出现「全屏/还原」「换行」按钮
        let ws = cx.debug_bounds("status-wrap-btn").is_none();
        let fs = cx.debug_bounds("status-fullscreen-btn").is_none();
        assert!(ws, "PDF 预览时状态栏不应有换行开关");
        assert!(fs, "PDF 预览时状态栏不应有全屏/还原按钮");

        // 全屏预览下编辑区不进入布局（非“被遮挡”，而是根本不渲染）
        assert!(
            cx.debug_bounds("editor-area").is_none(),
            "PDF 全屏预览时不应渲染编辑区"
        );
        assert!(
            cx.debug_bounds("fullscreen-preview").is_some(),
            "PDF 应以 fullscreen-preview 呈现"
        );
    }

    /// 回归：打开 259 页的长 PDF（曾因全量栅格化耗时 ~270s 表现为“无法加载”）。
    /// 现在应为页级虚拟化——结构解析秒级完成，进 UI 后按需渲染。
    #[gpui::test]
    async fn opens_long_pdf_virtualized(cx: &mut TestAppContext) {
        let _pdf_guard = crate::pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/17. 道路机动车辆产品准入审查要求1.0.pdf.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(pdf.to_str().unwrap(), window, cx)
            });
        });
        // 泵驱动到 PdfViewer 加载完成
        for _ in 0..400 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                app.read(cx).tabs[app.read(cx).active_tab]
                    .pdf_preview
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        let (page_count, status_ok) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.pdf_preview
                    .as_ref()
                    .map(|pv| pv.read(cx).page_count())
                    .unwrap_or(0),
                a.status_ok,
            )
        });
        let elapsed = t0.elapsed();
        assert!(status_ok, "打开长 PDF 应成功");
        assert!(
            page_count == 259,
            "259 页文件应完整列出 259 页，实际={page_count}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "长 PDF 结构解析应秒级完成（曾需 ~270s 全量栅格化），实际 {elapsed:?}"
        );
    }

    /// EPUB 阅读体验：状态栏全屏/还原按钮可切换占满与分屏；
    /// 换行开关只切换编辑区偏好，不改变预览换行（预览独立保持换行）。
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

        // EPUB 是只读预览文档：状态栏不应出现「全屏/还原」「换行」按钮
        assert!(
            cx.debug_bounds("status-wrap-btn").is_none(),
            "EPUB 预览时状态栏不应有换行开关"
        );
        assert!(
            cx.debug_bounds("status-fullscreen-btn").is_none(),
            "EPUB 预览时状态栏不应有全屏/还原按钮"
        );

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
            (t.preview_fullscreen, t.md_preview.read(cx).is_wrap())
        });
        assert!(!fs1, "切换后应退出全屏");
        assert!(wrap1, "退出全屏后预览应保持换行（预览始终自动换行）");

        // 切换换行：只作用编辑区偏好，不改变预览换行（预览独立保持换行）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (wrap2, pref2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_preview.read(cx).is_wrap(),
                a.wrap_enabled,
            )
        });
        assert!(wrap2, "切换换行开关不应影响 EPUB 预览（预览独立保持换行）");
        assert!(pref2, "切换一次后全局编辑区换行偏好应开启");

        // 再切回
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (wrap3, pref3) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_preview.read(cx).is_wrap(),
                a.wrap_enabled,
            )
        });
        assert!(wrap3, "预览应保持换行");
        assert!(!pref3, "再切换后全局编辑区换行偏好应恢复关闭");
    }

    /// 基准：打开 16 万行大文档的逐阶段耗时（读文件 / 编辑器装配 / 预览扫描 / 首帧渲染）。
    /// 用与拖入完全相同的 `open_file` 同步路径，定位"拖入大文档卡顿几秒"到底卡在哪。
    /// 运行：`cargo test --release --bench --ignored bench_open_large_md --nocapture`（见下）
    #[gpui::test]
    #[ignore]
    async fn bench_open_large_md(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        std::env::set_var("MD_OPEN_DEBUG", "1");
        // md：大文档异步打开。open_file 应立即返回（拖入不卡 UI），
        // 内容稍后在后台装配出来。
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"target/大文档测试.md", window, cx);
            });
        });
        let sync_ms = t0.elapsed().as_millis();
        println!("【md 异步打开：open_file 同步返回】(拖入不卡 UI) {} ms", sync_ms);
        // 两段式打开：D=编辑区**内容**即时就绪（快段，≈几十 ms），A=预览就绪（慢段
        // 后台并行统计+scan_blocks 完成后）。B 语义沿用「编辑区挂载完成」。
        // 用「行数 + !editor_pending」作廉价就绪判据（大文档必然远超；不用每帧 value()
        // 物化 6MB，那会污染墙钟）。
        let mut d_ready = false;
        let mut a_ready = false;
        let mut d_ready_ms = 0usize;
        let mut a_ready_ms = 0usize;
        let mut b_ready_ms = 0usize;
        let mut frames = 0;
        loop {
            frames += 1;
            cx.run_until_parked();
            let (preview_blocks, editor_pending, editor_lines) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                (
                    a.tabs[i].md_preview.read(cx).memory_estimate().0,
                    a.tabs[i].editor_pending,
                    a.tabs[i].editor.read(cx).line_count(),
                )
            });
            if !d_ready && !editor_pending && editor_lines > 1000 {
                d_ready = true;
                d_ready_ms = t0.elapsed().as_millis() as usize;
            }
            if !a_ready && preview_blocks > 0 {
                a_ready = true;
                a_ready_ms = t0.elapsed().as_millis() as usize;
            }
            if d_ready && a_ready {
                b_ready_ms = t0.elapsed().as_millis() as usize;
                break;
            }
            if frames > 200 {
                break;
            }
        }
        println!("【D. 编辑区内容即时就绪】(两段式快段) {d_ready_ms} ms");
        println!("【A. 预览内容就绪】(慢段后台完成) {a_ready_ms} ms");
        println!("【B. 两者就绪】(总墙钟) {}", if b_ready_ms > 0 { format!("{b_ready_ms} ms") } else { "超时".to_string() });
        // 首帧真实渲染/布局（编辑器 DisplayMap + 预览 ListState + 高亮窗口）
        let tb = std::time::Instant::now();
        for i in 0..3 {
            let tf = std::time::Instant::now();
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            println!("【C. 挂载后第 {} 次 refresh】(真实布局) {} ms", i + 1, tf.elapsed().as_millis());
        }
        println!("【C. 前 3 次 refresh 合计】 {} ms", tb.elapsed().as_millis());

        // 对照：同内容的 .txt（非 md → 无预览/无 scan_blocks；大文件同样走后台流水线）
        let t_txt = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"target/大文档测试.txt", window, cx);
            });
        });
        println!("【txt 打开：open_file 返回】(对照) {} ms", t_txt.elapsed().as_millis());
    }

    /// 基准：用编辑器本体追加同一段文本 1000 次所需耗时。
    /// 在编辑器里打开目标文件（与真实使用完全相同的 `open_file` 路径），
    /// 再通过 gpui-component 编辑器的 `insert` 接口连续插入同一段样本文本 1000 次，
    /// 统计总耗时。运行：
    /// `cargo test --release -- --ignored --nocapture bench_insert_1000_times`
    #[gpui::test]
    #[ignore]
    async fn bench_insert_1000_times(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 每次插入的“同样文本”（多行样本，贴近真实编辑内容）
        const SAMPLE: &str = "### 性能基准测试\n性能测试文本 0123456789 abcdefghijklmnopqrstuvwxyz 中文内容测试。\n```text\ncode block line\n```\n";
        const N: usize = 1000;

        // 目标文件可由环境变量 BENCH_INSERT_FILES 覆盖（分号分隔），默认三份样本
        let files: Vec<String> = std::env::var("BENCH_INSERT_FILES")
            .map(|v| {
                v.split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_else(|_| {
                vec![
                    r"target/大文档测试.md".to_string(),
                    r"target/SKILL.md".to_string(),
                    r"target/SKILL.txt".to_string(),
                ]
            });

        for path in &files {
            let file_name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string());
            if !std::path::Path::new(path).is_file() {
                println!("跳过（文件不存在）：{file_name}");
                continue;
            }

            // 1) 打开文件（md 大文档走异步打开，小文档同步打开）
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.open_file(path, window, cx));
            });
            // 2) 等待编辑器挂载完成（editor_pending 复位 = 编辑区已升起）
            let t_open = std::time::Instant::now();
            let mut frames = 0;
            loop {
                cx.run_until_parked();
                let pending = cx.update(|_, cx| {
                    let a = app.read(cx);
                    a.tabs.last().map(|t| t.editor_pending).unwrap_or(true)
                });
                if !pending {
                    break;
                }
                frames += 1;
                if frames > 400 {
                    panic!("打开超时：{file_name}");
                }
            }
            let open_ms = t_open.elapsed();

            // 3) 聚焦当前编辑区
            let (editor, focus) = cx.update(|_, cx| {
                let a = app.read(cx);
                let idx = a.active_tab;
                (
                    a.tabs[idx].editor.clone(),
                    a.tabs[idx].editor.read(cx).focus_handle(),
                )
            });
            cx.update(|window, cx| window.focus(&focus, cx));

            // 4) 连续插入同样文本 N 次并计时（编辑器操作本体，不含渲染帧）
            let before = cx.update(|_, cx| editor.read(cx).value().len());
            let t_insert = std::time::Instant::now();
            for _ in 0..N {
                cx.update(|window, cx| {
                    editor.update(cx, |ed, cx| ed.insert_text(SAMPLE, cx));
                });
            }
            let dt = t_insert.elapsed();
            let after = cx.update(|_, cx| editor.read(cx).value().len());
            let grew = after.saturating_sub(before);
            let expect = N * SAMPLE.len();
            assert_eq!(
                grew, expect,
                "插入 {N} 次后应恰好新增 {expect} B，实际 {grew} B"
            );
            println!(
                "【{file_name}】打开(异步装配) {} ms | {} 次 insert 耗时 {:.1} ms（单次平均 {:.2} ms），长度 {} -> {}（+{} B）",
                open_ms.as_millis(),
                N,
                dt.as_secs_f64() * 1000.0,
                dt.as_secs_f64() * 1000.0 / N as f64,
                before,
                after,
                grew,
            );
        }
    }

    /// 基准（debug 体感）：大文档开着 Markdown 预览时，一次按键的 UI 线程成本拆解
    /// （insert 编辑 vs 帧渲染/布局），并对比「预览开/关」以定位预览在每帧里的占比。
    #[gpui::test]
    #[ignore]
    async fn bench_keystroke_frame_cost(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(r"target/大文档测试.md", window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let pending =
                cx.update(|_, cx| app.read(cx).tabs.last().map(|t| t.editor_pending).unwrap_or(true));
            if !pending {
                break;
            }
            frames += 1;
            if frames > 400 {
                panic!("打开超时");
            }
        }
        let editor = cx.update(|_, cx| app.read(cx).tabs.last().unwrap().editor.clone());
        let sample = "x";
        // 预热：预览窗口物化 + 行高提示收敛
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }

        // 预览开：insert / 帧渲染 各自耗时
        {
            let mut insert_tot = 0.0f64;
            let mut paint_tot = 0.0f64;
            let rep = 5;
            for i in 0..rep {
                let t0 = std::time::Instant::now();
                cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
                let t1 = std::time::Instant::now();
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                cx.run_until_parked();
                let t2 = std::time::Instant::now();
                let ins = t1.duration_since(t0).as_secs_f64() * 1000.0;
                let paint = t2.duration_since(t1).as_secs_f64() * 1000.0;
                insert_tot += ins;
                paint_tot += paint;
                println!("  [预览开] iter {i}: insert={ins:.2} ms, frame={paint:.2} ms");
            }
            println!(
                "【预览开】insert={:.2} ms + 帧渲染={:.2} ms = {:.2} ms/键",
                insert_tot / rep as f64,
                paint_tot / rep as f64,
                (insert_tot + paint_tot) / rep as f64,
            );
        }
        // 关预览后再测一次
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.toggle_preview_for_tab(a.active_tab, window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        {
            let mut insert_tot = 0.0f64;
            let mut paint_tot = 0.0f64;
            let rep = 5;
            for _ in 0..rep {
                let t0 = std::time::Instant::now();
                cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
                let t1 = std::time::Instant::now();
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                cx.run_until_parked();
                let t2 = std::time::Instant::now();
                insert_tot += t1.duration_since(t0).as_secs_f64() * 1000.0;
                paint_tot += t2.duration_since(t1).as_secs_f64() * 1000.0;
            }
            println!(
                "【预览关】insert={:.2} ms + 帧渲染={:.2} ms = {:.2} ms/键",
                insert_tot / rep as f64,
                paint_tot / rep as f64,
                (insert_tot + paint_tot) / rep as f64,
            );
        }
    }

    /// 基准（debug，KILO_PROF=1 时为默认关闭内部拆解）：模拟真实编辑大文件的
    /// 「连续输入 → 停顿（触发预览增量同步）」节奏，逐键测插入与渲染停驻耗时，
    /// 并用 [`crate::prof`] 输出各热点（tab_change / hl_update / hl_styles / md_render /
    /// md_ensure_window）的累计拆解。
    #[gpui::test]
    #[ignore]
    async fn bench_realistic_large_file_editing(cx: &mut TestAppContext) {
        use std::time::{Duration, Instant};
        crate::prof::enable();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1400.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(r"target/大文档测试.md", window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let pending = cx
                .update(|_, cx| app.read(cx).tabs.last().map(|t| t.editor_pending).unwrap_or(true));
            if !pending {
                break;
            }
            frames += 1;
            if frames > 600 {
                panic!("打开超时");
            }
        }
        let editor = cx.update(|_, cx| app.read(cx).tabs.last().unwrap().editor.clone());
        // 预热：预览窗口物化 + 行高提示收敛
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        crate::prof::dump("开篇预热后");
        let sample = "性能耗时代码行 performance line 1234567890 更多中文输入继续。\n";
        // —— A. 光标在文档开头连续输入 ~8 键（每键都渲染一帧）——
        let mut ins: Vec<f64> = Vec::new();
        let mut park: Vec<f64> = Vec::new();
        for _ in 0..8 {
            let t = Instant::now();
            cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
            let t1 = Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = Instant::now();
            ins.push(t1.duration_since(t).as_secs_f64() * 1000.0);
            park.push(t2.duration_since(t1).as_secs_f64() * 1000.0);
        }
        crate::prof::dump("A. 开头连续输入8键");
        println!(
            "A. 开头连续输入：insert 均值 {:.2} ms，渲染停驻均值 {:.2} ms",
            ins.iter().sum::<f64>() / ins.len() as f64,
            park.iter().sum::<f64>() / park.len() as f64,
        );
        // 停顿 → 触发预览增量同步（真实应用里用户打字停顿处）
        cx.executor().advance_clock(Duration::from_millis(300));
        let t = Instant::now();
        for _ in 0..80 {
            cx.run_until_parked();
        }
        println!("A. 停顿+预览同步 停驻 {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        crate::prof::dump("A. 停顿同步后");

        // —— B. 光标移到文档中部继续输入 ——
        let total = cx.update(|_, cx| editor.read(cx).value().len());
        let mid = (total / 2).min(total);
        cx.update(|_, cx| {
            editor.update(cx, |e, cx| e.set_selection_range(mid..mid, cx));
        });
        for _ in 0..8 {
            let t = Instant::now();
            cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
            let t1 = Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = Instant::now();
            ins.push(t1.duration_since(t).as_secs_f64() * 1000.0);
            park.push(t2.duration_since(t1).as_secs_f64() * 1000.0);
        }
        crate::prof::dump("B. 中部连续输入8键");
        println!(
            "B. 中部连续输入：insert 均值 {:.2} ms，渲染停驻均值 {:.2} ms",
            ins.iter().rev().take(8).sum::<f64>() / 8.0,
            park.iter().rev().take(8).sum::<f64>() / 8.0,
        );
        cx.executor().advance_clock(Duration::from_millis(300));
        let t = Instant::now();
        for _ in 0..80 {
            cx.run_until_parked();
        }
        println!("B. 停顿+预览同步 停驻 {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        crate::prof::dump("B. 停顿同步后（完成）");

        // —— C. 探针：裸 ropey Rope.insert vs 编辑器 insert，分离引擎/回调 ——
        {
            use gpui_component::input::Rope;
            let text = cx.update(|_, cx| editor.read(cx).value().to_string());
            let mid = text.len() / 2;
            let mut rope = Rope::from(text.as_str());
            let t0 = Instant::now();
            for _ in 0..50 {
                rope.insert(mid.min(rope.len()), "x");
            }
            let rope_ms = t0.elapsed().as_secs_f64() * 1000.0 / 50.0;
            let t1 = Instant::now();
            for _ in 0..50 {
                cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text("x", cx)));
            }
            let ins_ms = t1.elapsed().as_secs_f64() * 1000.0 / 50.0;
            println!(
                "C. 探针：裸 ropey Rope(6MB).insert 中段 = {rope_ms:.2} ms/次；编辑器 insert（含事件/订阅） = {ins_ms:.2} ms/次"
            );
            crate::prof::dump("C. 探针后");
        }
    }

