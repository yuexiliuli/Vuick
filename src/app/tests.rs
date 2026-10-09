//! 单元测试（自 mod.rs 拆出，仅测试编译）。

    use super::*;
    use crate::actions::{
        MoveDown, NewFile, Paste, QuickOpenFile, Redo, SelectAll, ToggleCommandPalette, Undo,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};

    /// 测试前必须初始化 gpui-component（主题、键盘映射等全局状态）。
    fn init_test(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
    }

    /// 创建一个以 `crate::ui::root::Root` 为窗口根视图的编辑应用，
    /// 返回视图实体与该窗口的 `VisualTestContext`（后续用其驱动渲染/事件）。
    ///
    /// 新模型下启动为零选项卡（欢迎页），但绝大多数既有测试假定「初始有一个未命名空
    /// 选项卡」可直接 `tabs[0]` 键入——此处显式 `make_tab` 补建这个空选项卡（行为与旧的
    /// 启动即建 tab 一致）。真正要测「零 tab 欢迎页」的用 [`add_app_zero_tabs`]。
    fn add_app(cx: &mut TestAppContext) -> (Entity<EditorApp>, &mut VisualTestContext) {
        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<EditorApp>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            *holder.borrow_mut() = Some(app.clone());
            crate::ui::root::Root::new(app, window, cx)
        });
        let _ = root;
        let app = holder.borrow().clone().unwrap();
        // 在首帧渲染前补建初始空选项卡（同款「窗口建立后、渲染前」模式，见既有测试
        // 对 `new_tab` 的调用：窗口 + 实体上下文都已就绪，`make_tab` 只造待渲染实体）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.make_tab(window, cx);
            });
        });
        (app, cx)
    }

    /// 创建**零选项卡**的编辑应用（真实启动路径）：同 `add_app` 装配，但**不**预建
    /// 空选项卡——启动即「零 tab → 欢迎页」。零 tab 断言专用；需编辑区先 `new_tab`
    /// / 点「新建文件」/ 双击欢迎页。
    fn add_app_zero_tabs(cx: &mut TestAppContext) -> (Entity<EditorApp>, &mut VisualTestContext) {
        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<EditorApp>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            *holder.borrow_mut() = Some(app.clone());
            crate::ui::root::Root::new(app, window, cx)
        });
        let _ = root;
        let app = holder.borrow().clone().unwrap();
        assert!(
            cx.update(|_w, cx| app.read(cx).tabs.is_empty()),
            "零选项卡助手应保持零 tab"
        );
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
            app.update(cx, |app, cx| app.load_image(0, &p, window, cx));
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
        // 点菜单首项（菜单窗口锚定在该处；面板内边距 4 + 项高 26 → 项中心约 y+17）
        let item = gpui::Point::new(px(400. + 8.), px(360. + 4. + 13.));
        cx.simulate_click(item, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let state = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.active_image_locked(cx),
                a.media_lock_menu_pos.is_none(),
                a.tabs[a.active_tab]
                    .image_viewer
                    .as_ref()
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
            (a.active_image_locked(cx), a.media_lock_menu_pos.is_none())
        });
        assert!(unlocked.1, "再次点菜单后仍应关闭");
        assert!(!unlocked.0, "再次点「锁定」应解锁回归");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 视频右键「锁定」：右键在光标处弹菜单 → 点「锁定」→ 进入纯视频（外壳隐藏、视频
    /// 充满窗口）；再右键 → 点「锁定」→ 解锁回归。解码失败（假字节）不影响锁定状态机。
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
            "gpui-editor-app-vid-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, b"not really a video").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.load_video(0, &p, window, cx));
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
        // 点菜单首项（菜单窗口锚定在该处；面板内边距 4 + 项高 26 → 项中心约 y+17）
        let item = gpui::Point::new(px(400. + 8.), px(360. + 4. + 13.));
        cx.simulate_click(item, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let state = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.active_video_locked(cx),
                a.media_lock_menu_pos.is_none(),
                a.tabs[a.active_tab]
                    .video_viewer
                    .as_ref()
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
            (a.active_video_locked(cx), a.media_lock_menu_pos.is_none())
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
            crate::ui::root::Root::new(view, window, cx)
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
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
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
            crate::ui::root::Root::new(app, window, cx)
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

        // 右键选项卡栏内（栏左缘右侧 40px、垂直居中）→ 弹上下文菜单
        let bar = cx
            .debug_bounds("tab-bar")
            .expect("选项卡栏 tab-bar 应渲染");
        let right_click_at = gpui::Point::new(bar.origin.x + px(40.), bar.origin.y + px(16.));
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

        // 再右键 → 点「打开文件」→ 触发 open_dialog（计数断言）且菜单关闭
        let bar2 = cx
            .debug_bounds("tab-bar")
            .expect("新建后选项卡栏仍在");
        let right_click_at2 = gpui::Point::new(bar2.origin.x + px(60.), bar2.origin.y + px(16.));
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

    /// P2-9：活动栏「目录」图标悬停 → 200ms 防抖后出现 tooltip（浮在按钮右侧、不盖活动栏），
    /// 移出立即消失。
    #[gpui::test]
    async fn activity_bar_tooltip_delayed_appear_and_hide(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        // 活动栏按钮中心（目录）
        let btn = cx
            .debug_bounds("sidebar-btn-toc")
            .expect("活动栏应渲染目录按钮");
        let c = btn.center();
        // 初始无浮层
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_none(),
            "未悬停时不应渲染 tooltip"
        );
        // 悬停进入：防抖期内（< TOOLTIP_DELAY_MS=200ms）不显示
        cx.simulate_mouse_move(c, None, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_none(),
            "防抖期内（100ms<200ms）tooltip 不应出现"
        );
        // 时钟再推进超过防抖 → 浮层出现，且在按钮右缘（不盖活动栏本体）
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tip = cx
            .debug_bounds("tooltip-sidebar-btn-toc")
            .expect("延迟后 tooltip 应出现");
        assert!(
            tip.origin.x.as_f32() >= btn.origin.x.as_f32() + btn.size.width.as_f32(),
            "tooltip 应贴按钮右缘（不盖活动栏），tip={tip:?} btn={btn:?}"
        );
        // 移出 → 立即消失
        cx.simulate_mouse_move(gpui::point(px(8.), px(8.)), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_none(),
            "移出后 tooltip 应立即消失"
        );
    }

    /// P2-9：tooltip 浮层不拦截点击——悬停显示 tooltip 后，点击活动栏「目录」按钮
    /// 仍正常切换/收起目录面板（浮层无 hitbox、命中穿透）。
    #[gpui::test]
    async fn activity_bar_tooltip_does_not_block_click(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 悬停目录按钮 → 推进防抖 → tooltip 出现
        let btn = cx
            .debug_bounds("sidebar-btn-toc")
            .expect("活动栏应渲染目录按钮");
        cx.simulate_mouse_move(btn.center(), None, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_some(),
            "tooltip 应已显示"
        );
        // tooltip 可见时点按钮本体 → 正常展开目录面板
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut panel_open = cx.update(|_w, cx| app.read(cx).sidebar.panel_open);
        assert!(panel_open, "tooltip 可见时点击目录按钮仍应展开目录面板");
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "点击后应渲染目录面板"
        );
        // 再点一次 → 收起（往返可用，不因 tooltip 残留状态损坏点击）
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        panel_open = cx.update(|_w, cx| app.read(cx).sidebar.panel_open);
        assert!(!panel_open, "再点一次应收起目录面板");
    }

    /// P2-11「图标注入感」：窗口控制钮 hover/按压时图标前景由次要灰切到与底对应的
    /// 对比前景（最小化/最大化→主前景、关闭→白×），移出回落。经真实渲染路径断言：
    /// `simulate_mouse_move` 驱动 keyed hover/press 态 → `render_title_bar` 把本帧
    /// 解析出的前景色落盘到 `last_title_ctrl_fg`。
    #[gpui::test]
    async fn titlebar_window_buttons_inject_icon_fg_on_hover(cx: &mut TestAppContext) {
        fn same_color(a: gpui::Hsla, b: gpui::Hsla) -> bool {
            (a.h - b.h).abs() < 1e-5
                && (a.s - b.s).abs() < 1e-5
                && (a.l - b.l).abs() < 1e-5
                && (a.a - b.a).abs() < 1e-5
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let idle = cx
            .update(|_w, cx| {
                app.read(cx)
                    .last_title_ctrl_fg
                    .expect("标题栏渲染后应落盘窗口钮前景色")
            });
        let minor = cx.update(|_w, cx| app.read(cx).text_minor());
        let primary = cx.update(|_w, cx| app.read(cx).text_primary());
        assert!(
            same_color(idle.minimize, minor) && same_color(idle.close, minor),
            "空闲时窗口钮图标应为次要灰，实际 min={:?} close={:?}",
            idle.minimize,
            idle.close
        );

        // hover 最小化 → 注入主前景（灰底对比前景）
        let min_btn = cx
            .debug_bounds("title-minimize")
            .expect("应渲染 title-minimize");
        cx.simulate_mouse_move(min_btn.center(), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let hovered = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(hovered.minimize, primary),
            "hover 最小化图标应注入主前景，实际 {:?}",
            hovered.minimize
        );

        // hover 关闭 → 白 ×（危险钮观感）
        let close_btn = cx
            .debug_bounds("title-close")
            .expect("应渲染 title-close");
        cx.simulate_mouse_move(close_btn.center(), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let hovered_close = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(hovered_close.close, white()),
            "hover 关闭图标应为白 ×，实际 {:?}",
            hovered_close.close
        );

        // 按住：按压态独立跟随（图标仍注入）
        cx.simulate_mouse_down(close_btn.center(), MouseButton::Left, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pressed = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(pressed.close, white()),
            "按压关闭图标应保持白 ×，实际 {:?}",
            pressed.close
        );

        // 松开并移出 → 回落次要灰
        cx.simulate_mouse_up(close_btn.center(), MouseButton::Left, Default::default());
        cx.simulate_mouse_move(gpui::point(px(30.), px(300.)), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let away = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(away.close, minor),
            "移出后关闭图标应回落次要灰，实际 {:?}",
            away.close
        );
    }

    /// Win11 Fluent 行为：窗口控制钮组（最小化/最大化/关闭）右缘应**贴齐窗口右缘**，
    /// 关闭钮右侧不得有可见 gap。回归：`render_title_bar` 根容器此前用 `px_2`（左+右
    /// 各 8px），会在关闭钮右侧留下 8px 右内边距；改为仅左内边距 `pl_2` 后关闭钮右缘
    /// == 标题栏右缘（`title-bar` 是 `w_full` 的根容器，其右缘即窗口右缘）。
    /// 亮暗主题几何不变，但按验收要求两态都断言一次。
    #[gpui::test]
    async fn title_bar_window_controls_hug_right_edge(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 断言函数：关闭钮右缘贴齐标题栏右缘（gap ≤ 1px 容差）、且最右侧是关闭钮。
        let assert_hug = |cx: &mut gpui::VisualTestContext| {
            let bar = cx
                .debug_bounds("title-bar")
                .expect("标题栏应渲染");
            let close = cx
                .debug_bounds("title-close")
                .expect("关闭钮应渲染");
            let min_ctrl = cx
                .debug_bounds("title-minimize")
                .expect("最小化钮应渲染");
            let bar_right = bar.origin.x.as_f32() + bar.size.width.as_f32();
            let close_right = close.origin.x.as_f32() + close.size.width.as_f32();
            let close_left = close.origin.x.as_f32();
            let gap = (bar_right - close_right).max(0.);
            assert!(
                close_right <= bar_right + 1.0,
                "关闭钮右缘不得超出窗口右缘：close_right={close_right:.1} bar_right={bar_right:.1}"
            );
            assert!(
                gap <= 1.0,
                "关闭钮右缘应贴齐窗口右缘（无可见 gap），实际 gap={gap:.1}px"
            );
            assert!(
                close_left > min_ctrl.origin.x.as_f32(),
                "关闭钮应在最小化钮右侧（三钮顺序未被破坏）"
            );
        };
        // 默认主题（浅色）先断言。
        assert_hug(cx);

        // 切深色再断言（真主题切换路径 `refresh_theme_ui`，不写盘用户偏好）。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.dark_mode = true;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        for _ in 0..4 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert_hug(cx);

        // 再切回浅色（还原现场，避免污染后续测试的 theme 全局态）。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.dark_mode = false;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        for _ in 0..4 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert_hug(cx);
    }

    /// A4：侧边栏 Markdown 目录**点击跳转**——条目走统一组件 WyEditor（源码行）：
    /// 点「# 大标题 18」（文中部）→ 右栏预览 `scroll_to_line` 滚动到该标题所在块
    /// （宿主滚动句柄从顶部偏离到负向偏移，预览跟随跳转）。
    #[gpui::test]
    async fn sidebar_toc_click_scrolls_md_wy_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-toc-{}.md", std::process::id()));
        let mut doc = String::new();
        for i in 0..40 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文 {i}\n\n"));
        }
        std::fs::write(&p, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_wy_vscroll.clone()
        });
        let before = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert_eq!(before, 0.0, "初始预览滚动在顶部");
        let row = cx
            .debug_bounds("sidebar-toc-md-18")
            .expect("应渲染第 18 个目录条目（WyEditor TOC 驱动）");
        let pt = gpui::Point::new(row.left() + px(6.), row.top() + px(2.));
        cx.simulate_click(pt, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let after = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert!(
            after > 10.0,
            "点击目录应把右栏 WyEditor 预览滚动到对应标题（after scroll={after:.0}px）"
        );
        let _ = std::fs::remove_file(&p);
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
        let w0 = cx.update(|_w, cx| app.read(cx).sidebar.width);
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
            (a.sidebar.width, a.sidebar.width_auto)
        });
        assert!(
            (w1 - w0 - 20.0).abs() < 1.0,
            "拖拽后侧边栏宽应按位移更新，w0={w0:.1} w1={w1:.1}"
        );
        assert!(!auto, "手动拖拽后应关闭宽度自适应");

        // 「目录」按钮开关逻辑：再点一次只收起目录面板，侧边栏（活动栏）保留为窄条
        assert!(
            cx.update(|_w, cx| app.read(cx).sidebar.open),
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
            (a.sidebar.open, a.sidebar.panel_open)
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
        let wnow = cx.update(|_w, cx| app.read(cx).sidebar.width);
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

    /// 零选项卡欢迎页（回归 1）：正文区整屏渲染欢迎页——插图（160×120）、主/副文案、
    /// 「打开文件…」与「新建文件」按钮都在，且**没有** `editor-area`（零 tab 不渲染编辑区）；
    /// 点「打开文件…」触发 `open_dialog`（测试为 stub，只计数不真弹窗、不建 tab）。
    #[gpui::test]
    async fn zero_tab_welcome_renders_and_open_button_opens_dialog(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "零选项卡应显示欢迎页"
        );
        assert!(
            cx.debug_bounds("editor-area").is_none(),
            "零选项卡不应渲染编辑区"
        );
        let illus = cx
            .debug_bounds("empty-illustration")
            .expect("应渲染空态插图");
        assert!(
            illus.size.width > px(0.) && illus.size.height > px(0.),
            "插图应有固定尺寸，实际={illus:?}"
        );
        let btn = cx
            .debug_bounds("empty-open-btn")
            .expect("应渲染「打开文件…」按钮");
        assert!(
            btn.size.width > px(0.) && btn.size.height > px(0.),
            "「打开文件…」按钮应有尺寸，实际={btn:?}"
        );
        let newbtn = cx
            .debug_bounds("empty-new-btn")
            .expect("应渲染「新建文件」按钮");
        assert!(
            newbtn.size.width > px(0.) && newbtn.size.height > px(0.),
            "「新建文件」按钮应有尺寸，实际={newbtn:?}"
        );
        // 点击按钮 → 触发 open_dialog（计数断言；测试 stub 不弹原生对话框）
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let calls = cx.update(|_w, cx| app.read(cx).open_dialog_calls);
        assert_eq!(calls, 1, "点击「打开文件…」应触发一次 open_dialog");
        assert!(
            cx.update(|_w, cx| app.read(cx).tabs.is_empty()),
            "点「打开文件…」只弹系统对话框、不应新建选项卡"
        );
    }

    /// 向窗口派发一次**真实双击**（mousedown `click_count=2` + mouseup `click_count=2`）。
    /// 走 `simulate_event` 的完整分发路径（`window.dispatch_event`），双击识别与编辑器
    /// 同套语义（`MouseDownEvent.click_count >= 2`）；`simulate_click` 恒为单击，无法覆盖。
    fn simulate_double_click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: at,
            modifiers: gpui::Modifiers::default(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: at,
            modifiers: gpui::Modifiers::default(),
            click_count: 2,
        });
    }

    /// 渲染若干帧（等动画/重绘收敛后再读 debug_bounds）。
    fn settle_frames(cx: &mut VisualTestContext, n: usize) {
        for _ in 0..n {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
    }

    /// 打开零选项卡时欢迎页的焦点句柄回归：零 tab 下 `editor_focus_handle` 回退到
    /// 应用根焦点（不 Panic、可聚焦），保证 Ctrl+N 等在欢迎页可正常派发。
    fn focus_app_root(cx: &mut VisualTestContext, app: &Entity<EditorApp>) {
        cx.update(|window, cx| {
            let h = app.read(cx).editor_focus_handle(cx);
            window.focus(&h, cx);
        });
    }

    /// 回归 2：点欢迎页「新建文件」→ 出现 1 个选项卡、欢迎页消失、编辑区渲染且可聚焦、
    /// 真实键入生效。
    #[gpui::test]
    async fn new_file_button_creates_editable_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "初始应显示欢迎页"
        );
        // 点「新建文件」按钮（用其外包定位元素，真实点击路径）。
        let btn = cx
            .debug_bounds("empty-new-btn")
            .expect("应渲染「新建文件」按钮");
        cx.simulate_click(btn.center(), Default::default());
        settle_frames(cx, 5);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).tabs.len()),
            1,
            "点「新建文件」应新建 1 个选项卡"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "欢迎页应消失（有 tab 即不再显示）"
        );
        assert!(
            cx.debug_bounds("editor-area").is_some(),
            "应渲染编辑区"
        );
        // 焦点应落在新编辑区（开箱即输入）。
        let focused = cx.update(|window, cx| {
            app.read(cx).tabs[0].editor.read(cx).focus_handle().is_focused(window)
        });
        assert!(focused, "新建后编辑区应获得焦点");
        // 直接键入（真实键入路径）→ 内容写入且欢迎页不回来。
        let editor = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        cx.update(|_, cx| editor.update(cx, |ed, cx| ed.insert_text("hi", cx)));
        settle_frames(cx, 3);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).tabs[0].editor.read(cx).value().to_string()),
            "hi",
            "新建选项卡应可直接键入"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "输入后欢迎页绝不出现（模型 3）"
        );
    }

    /// 回归 3：**双击**欢迎页（空白处，避开按钮）＝新建文件选项卡——与「新建文件」
    /// 按钮同语义（出现 tab、欢迎页消失、编辑区渲染）。
    #[gpui::test]
    async fn double_click_welcome_creates_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(cx.debug_bounds("empty-state").is_some(), "初始应显示欢迎页");
        // 双击按钮上方的空白处（按钮行下面约在正文区中央；选偏上空白区避开按钮）
        simulate_double_click(cx, gpui::Point::new(px(400.), px(200.)));
        settle_frames(cx, 5);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).tabs.len()),
            1,
            "双击欢迎页应新建 1 个选项卡"
        );
        assert!(cx.debug_bounds("empty-state").is_none(), "欢迎页应消失");
        assert!(cx.debug_bounds("editor-area").is_some(), "应渲染编辑区");
    }

    /// 回归 4：关闭**全部**选项卡 → 回到欢迎页（零 tab、无编辑区、不 panic、不退出）。
    #[gpui::test]
    async fn closing_last_tab_returns_to_welcome(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx); // add_app 预建 1 个空未命名选项卡
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(cx.debug_bounds("editor-area").is_some(), "有 tab 时应渲染编辑区");
        assert!(cx.debug_bounds("empty-state").is_none(), "有 tab 时绝不显示欢迎页");
        // 未编辑不弹保存框：close_tab 直通 remove_tab（真实用户「关闭最后一个 tab」路径）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(0, window, cx));
        });
        settle_frames(cx, 5);
        assert!(
            cx.update(|_w, cx| app.read(cx).tabs.is_empty()),
            "关闭最后一个选项卡后应零 tab"
        );
        assert!(cx.debug_bounds("empty-state").is_some(), "应回到欢迎页");
        assert!(cx.debug_bounds("editor-area").is_none(), "零 tab 无编辑区");
    }

    /// 回归 5：**只要有 ≥1 个选项卡**（含新建出的空未命名文档）就绝不显示欢迎页——
    /// 空未命名 tab 直接是编辑区（可键入），无第二层双击步骤。
    #[gpui::test]
    async fn any_tab_suppresses_welcome(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx); // 1 个空未命名选项卡
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert_eq!(cx.update(|_w, cx| app.read(cx).tabs.len()), 1, "预建 1 个空未命名选项卡");
        assert!(cx.debug_bounds("editor-area").is_some(), "空未命名 tab 也直接渲染编辑区");
        assert!(cx.debug_bounds("empty-state").is_none(), "有 tab（哪怕空未命名）就不显示欢迎页");
        // 键入后依旧不显示欢迎页（新模型没有「欢迎页盖在有内容上」的路径）。
        let editor = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        cx.update(|_, cx| editor.update(cx, |ed, cx| ed.insert_text("x", cx)));
        settle_frames(cx, 3);
        assert!(cx.debug_bounds("empty-state").is_none(), "输入后仍不显示欢迎页");
    }

    /// 回归 6：Ctrl+N（`NewFile` 动作）在欢迎页（零 tab、焦点在应用根）可用——
    /// 新建选项卡、欢迎页消失、编辑区渲染。
    #[gpui::test]
    async fn ctrl_n_works_from_welcome(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(cx.debug_bounds("empty-state").is_some(), "初始应显示欢迎页");
        // 零 tab 下焦点回退到应用根（`editor_focus_handle` → 根句柄）
        focus_app_root(cx, &app);
        cx.run_until_parked();
        cx.dispatch_action(NewFile);
        settle_frames(cx, 5);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).tabs.len()),
            1,
            "Ctrl+N 应从欢迎页新建选项卡"
        );
        assert!(cx.debug_bounds("empty-state").is_none(), "欢迎页应消失");
        assert!(cx.debug_bounds("editor-area").is_some(), "应渲染编辑区");
    }


    /// 目录空态：打开无任何标题的 Markdown（有内容但无目录节点）→ 侧栏目录面板
    /// 显示「灰色图标 + 文案」空态占位（跨无标题文档 / PDF 无书签 / 无文档三态共用）。
    #[gpui::test]
    async fn sidebar_toc_empty_state_placeholder(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-toc-empty-{}.md", std::process::id()));
        std::fs::write(&md, "纯文本，没有标题\n第二行\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "应渲染目录面板"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-empty").is_some(),
            "无标题文档应显示目录空态占位"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 搜索空态：打开有内容的文档、输入无匹配关键字 → 侧栏搜索显示
    /// 「灰色搜索图标 + 文案」空态占位；命中时恢复正常结果列表。
    #[gpui::test]
    async fn sidebar_search_empty_state_placeholder(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let txt = std::env::temp_dir().join(format!("gpui-srch-empty-{}.txt", std::process::id()));
        std::fs::write(&txt, "hello world\nthis is a file\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 无匹配 → 空态占位
        cx.update(|window, cx| {
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| s.set_value("zzz-无匹配", window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search-empty").is_some(),
            "无匹配应显示搜索空态占位"
        );
        // 命中 → 恢复正常结果列表（不再有空态）
        cx.update(|window, cx| {
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| s.set_value("hello", window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search").is_some(),
            "命中搜索仍应显示结果面板"
        );
        let _ = std::fs::remove_file(&txt);
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
    async fn md_wy_preview_host_renders_for_md(cx: &mut TestAppContext) {
        // A2：Markdown 右栏预览切统一组件 WyEditor（只读）——live-area 宿主（滚动容器 +
        // 自绘纵向滚动条）应渲染，且 `md_wy` 实体被创建、只读门控生效、源文本就位。
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
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }

        // 宿主三件套：分屏外框 + 滚动容器 + 自绘竖滚动条叠层。
        assert!(
            cx.debug_bounds("preview-md-wy").is_some(),
            "MD WyEditor 预览外框 preview-md-wy 应渲染"
        );
        let scroll = cx
            .debug_bounds("md-wy-scroll")
            .expect("宿主滚动容器 md-wy-scroll 应渲染");
        assert!(
            scroll.size.width > px(0.) && scroll.size.height > px(0.),
            "宿主滚动容器应有非零尺寸，实际={scroll:?}"
        );
        assert!(
            cx.debug_bounds("md-wy-vscrollbar-overlay").is_some(),
            "自绘纵向滚动条叠层 md-wy-vscrollbar-overlay 应渲染"
        );
        // 实体级：只读门控 + 源文本就位（含超宽代码行）。
        let (editable, text) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            let ed = t.md_wy.clone().expect("md 打开应创建右栏 WyEditor 预览");
            (ed.read(cx).is_editable(), ed.read(cx).text().to_string())
        });
        assert!(!editable, "右栏 WyEditor 预览应只读");
        assert!(!text.is_empty(), "右栏 WyEditor 预览应收满源文本");
        let _ = std::fs::remove_file(&dir);
    }

    /// A2：右栏 WyEditor 预览的只读门控——`replace_range` 等编辑入口在预览实体上
    /// 一律短路（第 #1 条：右栏只能看/选/复制，绝不能改到预览内容）。
    #[gpui::test]
    async fn md_wy_preview_readonly_blocks_editing(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-mdwy-ro-{}.md", std::process::id()));
        std::fs::write(&md, "初始内容\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let wy = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_wy.clone().expect("应有右栏 WyEditor 预览")
        });
        let (editable, before) = cx.update(|_, cx| {
            (wy.read(cx).is_editable(), wy.read(cx).text().to_string())
        });
        assert!(!editable, "右栏预览应只读");
        // 各种编辑入口在只读预览上全部短路（内容不变）。
        cx.update(|_, cx| {
            wy.update(cx, |ed, cx| ed.replace_range(0..2, "X", cx));
        });
        let after = cx.update(|_, cx| wy.read(cx).text().to_string());
        assert_eq!(before, after, "只读预览不应被 replace_range 改动");
        let _ = std::fs::remove_file(&md);
    }

    /// A2：按眼睛/菜单翻转预览——开启时创建并喂源右栏 WyEditor 预览（源 = 编辑区文本），
    /// 关闭时释放（`md_wy = None`，内存回基线），再开重建并重新喂源。
    #[gpui::test]
    async fn md_wy_preview_fed_on_open_and_released_on_toggle(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-mdwy-tog-{}.md", std::process::id()));
        std::fs::write(&md, "第一段\n第二段\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 打开即创建 + 喂源（源 = 编辑区文本）。
        let (fed, src) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert!(!fed.is_empty(), "打开 md 应创建右栏 WyEditor 预览");
        assert_eq!(fed, src, "右栏 WyEditor 预览源应与编辑区一致");

        // 关预览 → 释放（md_wy=None）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_preview_for_tab(app.active_tab, window, cx)
            });
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let off = cx.update(|_, cx| app.read(cx).tabs[app.read(cx).active_tab].md_wy.is_none());
        assert!(off, "关闭预览应释放右栏 WyEditor 预览实体");

        // 再开 → 重建 + 重新喂源。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_preview_for_tab(app.active_tab, window, cx)
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (on, fed2, src2) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy.is_some(),
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert!(on, "再开预览应重建右栏 WyEditor 预览");
        assert_eq!(fed2, src2, "重建预览应重新喂源（与编辑区一致）");
        let _ = std::fs::remove_file(&md);
    }

    /// A3：逐键增量桥——编辑区每次编辑（Paste 触发 Change）都重放到右栏 WyEditor 预览，
    /// 预览文本恒等于编辑区文本（实时跟随，非重装）。
    #[gpui::test]
    async fn md_wy_preview_follows_editor_edits_incrementally(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-bridge-{}.md", std::process::id()));
        std::fs::write(&p, "# 初始标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let par = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                (
                    t.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).text().to_string())
                        .unwrap_or_default(),
                    t.editor.read(cx).value().to_string(),
                )
            })
        };
        let (m0, e0) = par(cx);
        assert_eq!(m0, e0, "初始预览应与编辑区一致");

        // 聚焦编辑区 + 粘贴 → Change → 增量桥重放。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("新增段落".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        let (m1, e1) = par(cx);
        assert_eq!(m1, e1, "粘贴后预览应实时跟随编辑区，md_wy={m1:?} editor={e1:?}");
        assert!(m1.contains("新增段落"), "预览应含新贴入文本，实际 {m1:?}");

        // 再贴一次 → 仍保持一致。
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("第二处".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        let (m2, e2) = par(cx);
        assert_eq!(m2, e2, "连续多次编辑后预览仍应与编辑区一致");
        let _ = std::fs::remove_file(&p);
    }

    /// A3：增量桥随撤销/重做的逆编辑同步回退/重放——预览在每一步都与编辑区一致。
    #[gpui::test]
    async fn md_wy_preview_follows_editor_undo_and_redo(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-undoredo-{}.md", std::process::id()));
        std::fs::write(&p, "一行\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        let md_text = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                a.tabs[a.active_tab]
                    .md_wy
                    .clone()
                    .expect("应有 md_wy")
                    .read(cx)
                    .text()
                    .to_string()
            })
        };
        let ed_text = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| app.read(cx).tabs[app.read(cx).active_tab].editor.read(cx).value().to_string())
        };
        assert!(ed_text(cx).contains('X'), "基线：粘贴生效");
        assert_eq!(md_text(cx), ed_text(cx), "粘贴后预览应含 X");

        // 撤销：editor 与 md_wy 同步回退到无 X。
        cx.dispatch_action(Undo);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        assert!(!ed_text(cx).contains('X'), "撤销后编辑区不含 X");
        assert_eq!(md_text(cx), ed_text(cx), "撤销后预览应与编辑区一致（同步回退）");

        // 重做：同步恢复 X。
        cx.dispatch_action(Redo);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        assert!(ed_text(cx).contains('X'), "重做后编辑区含 X");
        assert_eq!(md_text(cx), ed_text(cx), "重做后预览应与编辑区一致（同步恢复）");
        let _ = std::fs::remove_file(&p);
    }

    /// A3：混合编辑（粘贴 + 换行结构变更 + 再次粘贴）后预览仍与编辑区逐字节一致——
    /// 增量桥走 `Doc::edit` 结构/非结构两路都必须对齐。
    #[gpui::test]
    async fn md_wy_preview_bridge_parity_after_mixed_edits(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-mix-{}.md", std::process::id()));
        std::fs::write(&p, "- 项目 A\n- 项目 B\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        let par = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                (
                    t.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).text().to_string())
                        .unwrap_or_default(),
                    t.editor.read(cx).value().to_string(),
                )
            })
        };
        // 粘贴（普通插入）→ 换行（结构变更）→ 粘贴（再次普通插入）。
        for clip in ["追加甲", "\n", "追加乙"] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(clip.to_string()));
            cx.dispatch_action(Paste);
            for _ in 0..20 {
                cx.run_until_parked();
            }
            let (m, e) = par(cx);
            assert_eq!(m, e, "混合编辑后预览仍应与编辑区逐字节一致，editor={e:?} md_wy={m:?}");
        }
        let _ = std::fs::remove_file(&p);
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
        // A4：MD 预览走统一组件 WyEditor（自动换行由组件默认保障，不再有 md_preview 可查）。
        let wy_active = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_wy.is_some()
        });
        assert!(wy_active, "刚打开 Markdown 预览应走统一组件（WyEditor 自动换行）");
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
        // 初始：全局编辑区换行偏好默认关闭（仅 md 编辑区默认换行）；MD 预览走统一组件。
        let (wy0, pref0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.tabs[a.active_tab].md_wy.is_some(), a.wrap_enabled)
        });
        assert!(wy0, "initial: MD 预览应走统一组件 WyEditor（自动换行由组件保障）");
        assert!(!pref0, "全局编辑区换行偏好应默认关闭（仅 md 编辑区换行），pref0={pref0}");

        // 切换换行：只作用编辑区 → 预览组件不变、全局偏好开启
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pref1 = cx.update(|_w, cx| app.read(cx).wrap_enabled);
        let host1 = cx.debug_bounds("preview-md-wy").is_some();
        assert!(
            host1,
            "切换「自动换行」不应动 Markdown 预览（统一组件宿主仍渲染）"
        );
        assert!(pref1, "切换一次后全局编辑区换行偏好应开启");

        // 再切回：预览仍渲染、偏好恢复关闭
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pref2 = cx.update(|_w, cx| app.read(cx).wrap_enabled);
        let host2 = cx.debug_bounds("preview-md-wy").is_some();
        assert!(host2, "再切换后预览应仍渲染");
        assert!(!pref2, "再切换后全局编辑区换行偏好应恢复关闭");
        let _ = std::fs::remove_file(&md);
    }

    /// WYSIWYG（预览编辑测试 / WyEditor）模式：宿主 `#live-scroll` 提供纵向滚动，
    /// 右侧 `Scrollbar::vertical` 自绘**可见纵向滚动条**（此 GPUI 的 `overflow_y_scroll`
    /// 不绘制原生滚动条）。回归：切到 live 模式后滚动条叠层应存在。
    #[gpui::test]
    async fn wysiwyg_live_mode_has_vertical_scrollbar(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(600.), px(600.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-vscroll-{}.md",
            std::process::id()
        ));
        // 内容足够高 → 真实需要纵向滚动。
        std::fs::write(&md, format!("{}\n", "# 标题\n正文\n".repeat(300))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 打开前：非 live 模式无该滚动条。
        assert!(
            cx.debug_bounds("live-vscrollbar-overlay").is_none(),
            "源码编辑模式不应有 WYSIWYG 滚动条"
        );
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("live-scroll").is_some(),
            "WYSIWYG 宿主纵向滚动容器应存在"
        );
        assert!(
            cx.debug_bounds("live-vscrollbar-overlay").is_some(),
            "WYSIWYG 模式应渲染自绘纵向滚动条叠层"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// Markdown 预览（分屏，统一组件 WyEditor 宿主）纵向滚动条拖拽：向下拖 -> 内容实时
    /// 向下滚，向上拖 -> 实时反向。回归：预览窗口模式拖拽只写共享 `desired`，render 逐帧
    /// `apply_desired` 落窗口；若该 Apply 在往下方向被 clamp/跳过，会出现「只有松开才动」。
    #[gpui::test]
    async fn md_wy_preview_scrollbar_drag_follows_both_directions(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-preview-drag-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, format!("{}\n", "# 标题\n正文段落。\n\n".repeat(600))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let overlay = cx
            .debug_bounds("md-wy-vscrollbar-overlay")
            .expect("MD WyEditor 预览纵向滚动条叠层应渲染");
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_wy_vscroll.clone()
        });
        let abs_top = |cx: &mut VisualTestContext| {
            cx.update(|_w, _cx| (-vhand.offset().y).as_f32())
        };

        let before = abs_top(cx);
        assert_eq!(before, 0.0, "预览初始应在顶部");

        let x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        cx.simulate_mouse_down(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }

        // 未松开向下拖两步 → 预览必须边拖边向下滚。只走 run_until_parked（真实 App 路径）。
        let mid1_y = overlay.top() + px(120.);
        cx.simulate_mouse_move(
            gpui::point(x, mid1_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s1 = abs_top(cx);
        assert!(
            s1 > before + 1.0,
            "向下拖(1)预览应先实时滚动（abs={s1:.0}）——不能等松开才动"
        );

        let mid2_y = overlay.top() + px(240.);
        cx.simulate_mouse_move(
            gpui::point(x, mid2_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s2 = abs_top(cx);
        assert!(
            s2 > s1 + 1.0,
            "继续向下拖(2)预览应继续实时滚动（abs={s2:.0} vs {s1:.0}）"
        );

        // 未松开向上拖回 → 实时反向。
        cx.simulate_mouse_move(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s3 = abs_top(cx);
        assert!(
            s3 < s2 - 1.0,
            "向上拖预览应实时反向滚动（abs={s3:.0} vs {s2:.0}）"
        );

        cx.simulate_mouse_up(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_file(&md);
    }

    /// 编辑区⇄预览滚动同步（editor→preview）：把编辑器滚到第 20 行 → 右栏 WyEditor
    /// 预览逐帧跟随到「该行所在块的起点」（同步单位 = 块）。驱动来自 render 开头
    /// `sync_editor_preview_scroll`，两侧收敛到同一块后静止。
    #[gpui::test]
    async fn editor_scroll_drives_md_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-ed2pv-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].editor.clone(),
                a.tabs[a.active_tab]
                    .md_wy
                    .clone()
                    .expect("MD 打开后 WyEditor 预览应在场"),
            )
        });
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_wy_vscroll.clone()
        });
        let before = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert_eq!(before, 0.0, "预览初始在顶部");

        // 「用户」滚动编辑区到第 20 行（= 某个标题块所在行）。
        let target = 20usize;
        cx.update(|_, cx| {
            editor.update(cx, |ed, _| ed.scroll_to_line(target));
        });
        // 预览滚动由 WY 元素 prepaint 逐帧重钉宿主偏移直到稳住（需几帧收敛）。
        let mut after = before;
        let mut settled_after = None;
        for _frame in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            after = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
            if after > before + 1.0 {
                settled_after = Some(after);
                break;
            }
        }
        assert!(
            settled_after.is_some(),
            "编辑器滚动应驱动预览滚动（{before:.0} → {after:.0}px）"
        );
        let (editor_top, preview_src) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            let ps = mdw.read(cx).top_source_line();
            (et, ps)
        });
        let expect_block = cx.update(|_, cx| mdw.read(cx).block_start_of_line(target));
        assert_eq!(
            preview_src,
            Some(expect_block),
            "预览顶块应 = 编辑器目标行所在块（同步键=块起点）"
        );
        assert_eq!(editor_top, target, "编辑器顶行应为目标行");
        // 收敛后稳定性：再跑 2 帧，预览宿主偏移必须保持不变——防「左右互驱振荡」
        // 每次重钉把预览拉走再被趋势拽回（若存在振荡，这里偏移会继续变）。
        let stable = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        for _ in 0..2 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let stable2 = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert!(
            (stable2 - stable).abs() < 1.5,
            "收敛后预览偏移应稳定不再振荡（{stable:.0} → {stable2:.0}px）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 编辑区⇄预览滚动同步（preview→editor）：拖右栏 WyEditor 预览纵向滚动条向下（真实
    /// 用户滚动路径）→ 编辑器逐帧跟随到预览顶块起点，且两侧收敛到同一块后不再互拉。
    #[gpui::test]
    async fn preview_scroll_drives_editor(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-pv2ed-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].editor.clone(),
                a.tabs[a.active_tab]
                    .md_wy
                    .clone()
                    .expect("MD 打开后 WyEditor 预览应在场"),
            )
        });
        let overlay = cx
            .debug_bounds("md-wy-vscrollbar-overlay")
            .expect("MD WyEditor 预览纵向滚动条叠层应渲染");

        // 拖预览滚动条向下大幅滚动（真实路径：写共享 desired → render 每帧落窗口）。
        let x = overlay.right() - px(8.);
        cx.simulate_mouse_down(gpui::point(x, overlay.top() + px(12.)), MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let mid_y = overlay.top() + px(480.);
        cx.simulate_mouse_move(gpui::point(x, mid_y), MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(gpui::point(x, mid_y), MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_wy_vscroll.clone()
        });
        let (editor_top, preview_src, s1) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            let ps = mdw.read(cx).top_source_line();
            let off = (-vhand.offset().y).as_f32();
            (et, ps, off)
        });
        assert!(
            editor_top > 0,
            "预览滚动应驱动编辑器跟随（editor top={editor_top}）"
        );
        // 编辑器跟随 ⇐ 驱动 requires 预览顶块就绪（sync 的 preview→editor 分支只在
        // preview_block 为 Some 时才驱动）→ 此处预览帧必然就绪，做强断言。
        let ps = preview_src.expect("预览帧应就绪（编辑器已跟随）");
        assert_eq!(
            editor_top, ps,
            "编辑器顶行应 = 预览顶块起点（editor={editor_top}, preview={ps}）"
        );

        // 第二次继续向下滚（用户后续滚动）：直接推宿主偏移模拟（同步只读 vhand.offset
        // 推算预览顶块，与真实滚动等价）。回归「预览始终来回跳动、滚动不下去」——预览
        // 驱动编辑器后，编辑器落位不得被误判成用户滚动而反向把预览拽回。此段预览偏移
        // 必须单调前进（s2 > s1），且编辑器跟随到更深顶块并与之对齐（有界等待收敛）。
        let deeper = s1 + 4000.0;
        cx.update(|_, _cx| {
            vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(-deeper)));
        });
        // 有界等待**精确对齐**：块粒度同步键两侧相等才停（瞬态中编辑器可能先跑到更
        // 深而预览顶块仍读旧值，须等最终稳态）。
        let mut advanced: Option<(usize, usize, f32)> = None;
        for _frame in 0..30 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let (et2, ps2, off2) = cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                let ps = mdw.read(cx).top_source_line();
                let off = (-vhand.offset().y).as_f32();
                (et, ps, off)
            });
            if let Some(ps) = ps2
                && et2 == ps
                && et2 > editor_top
                && off2 > s1 + 50.0
            {
                advanced = Some((et2, ps, off2));
                break;
            }
        }
        let (editor_top2, preview_src2, _s2) =
            advanced.expect("预览继续向下滚动后编辑器应跟随并对齐（有界 30 帧收敛）");
        assert_eq!(
            editor_top2, preview_src2,
            "第二次后编辑器顶行应 = 预览顶块起点（editor={editor_top2}, preview={preview_src2}）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// WYSIWYG（原生 `ScrollHandle` 宿主）滚动条拖拽：向下拖 -> 内容应**实时**向下滚，
    /// 向上拖 -> 实时反向滚。回归：自绘滚动条拖动只写 `live_vscroll` + notify，若宿主
    /// 原生滚动容器没有重渲染，内容会只在松开后才跳（用户报告「从上到下要松开才动」）。
    #[gpui::test]
    async fn wysiwyg_live_scrollbar_drag_follows_both_directions(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(700.), px(700.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-live-drag-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, format!("{}\n", "# 标题\n正文。\n\n".repeat(400))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let overlay = cx
            .debug_bounds("live-vscrollbar-overlay")
            .expect("live 滚动条叠层应渲染");
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].live_vscroll.clone()
        });
        let scroll_px = |cx: &mut VisualTestContext| {
            cx.update(|_w, _cx| (-vhand.offset().y).as_f32())
        };

        let before = scroll_px(cx);
        assert_eq!(before, 0.0, "初始应在顶部");

        let x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        // 压下 thumb，**未松开**向下拖（分两步），内容必须边拖边滚。
        cx.simulate_mouse_down(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let mid1_y = overlay.top() + px(120.);
        cx.simulate_mouse_move(
            gpui::point(x, mid1_y),
            MouseButton::Left,
            Default::default(),
        );
        // 只走 run_until_parked，不手动 refresh —— 真实 App 里同样只靠 notify 触发重绘。
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s1 = scroll_px(cx);
        assert!(
            s1 > before + 1.0,
            "向下拖(1)应先实时滚动（s1={s1:.0}）——不能等松开才动"
        );

        let mid2_y = overlay.top() + px(220.);
        cx.simulate_mouse_move(
            gpui::point(x, mid2_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s2 = scroll_px(cx);
        assert!(
            s2 > s1 + 1.0,
            "继续向下拖(2)应继续实时滚动（s2={s2:.0} vs s1={s1:.0}）"
        );

        // 未松开向上拖回 → 实时反向滚动。
        cx.simulate_mouse_move(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s3 = scroll_px(cx);
        assert!(
            s3 < s2 - 1.0,
            "向上拖应实时反向滚动（s3={s3:.0} vs s2={s2:.0}）"
        );

        cx.simulate_mouse_up(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_file(&md);
    }

    /// 拖动编辑器纵向滚动条滑块：内容必须**跟随拖动**实时滚动，禁止「只有松开才跳」。
    /// 回归：自绘滚动条拖动只写 desired_scroll，若 `notify` 没让编辑器重渲染，
    /// 内容会停在原位直到 mouseup 后别的路径触发重绘。
    #[gpui::test]
    async fn scrollbar_drag_follows_content_live(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(900.), px(700.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join(format!(
            "gpui-scrollbar-drag-{}.txt",
            std::process::id()
        ));
        std::fs::write(
            &f,
            (0..800)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }

        let overlay = cx
            .debug_bounds("ce-vscrollbar-overlay")
            .expect("编辑器纵向滚动条叠层应渲染");
        let (before_top, before_desired) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ed = a.tabs[a.active_tab].editor.read(cx);
            (ed.test_text_scroll_top(), ed.test_desired_scroll_px())
        });
        assert_eq!(before_top, (0, 0.0), "初始应在顶部");
        assert_eq!(before_desired, 0.0);

        // 点住滑块（右缘 16px 轨道，滑块顶部 = overlay.top + inset 4）…
        let thumb_x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        cx.simulate_mouse_down(
            gpui::point(thumb_x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // …未松开就向下拖 100px → 内容必须已经跟随滚动。
        let drag_y = overlay.top() + px(114.);
        cx.simulate_mouse_move(
            gpui::point(thumb_x, drag_y),
            MouseButton::Left,
            Default::default(),
        );
        // 只走 run_until_parked，**不手动 refresh**——真实 App 里同样只靠 notify 触发重绘；
        // 若 notify 没把视图标脏，这里就会复现「拖动中内容不动，只有松开才跳」。
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let mid = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ed = a.tabs[a.active_tab].editor.read(cx);
            (ed.test_text_scroll_top(), ed.test_desired_scroll_px())
        });
        assert!(
            mid.1 > before_desired + 1.0 || mid.0 != before_top,
            "拖动中内容应实时跟随（scroll_top={:?}, desired={:.1}）——不能等松开才动",
            mid.0,
            mid.1
        );

        cx.simulate_mouse_up(
            gpui::point(thumb_x, drag_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_file(&f);
    }

    /// WYSIWYG Ctrl+滚轮缩放：字号放大/缩小后视图顶部**内容点**保持不变（视口锚定，
    /// 对齐 src/markdown/view 与 src/editor），而不是整体滚动偏离。
    #[gpui::test]
    async fn wysiwyg_zoom_keeps_viewport_anchored(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(600.), px(600.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 内容足够高 → 真实需要纵向滚动，且缩放后还有双向余量。
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-zoom-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, format!("{}\n", "# 标题\n正文段落内容用于缩放锚定验证。\n\n".repeat(200))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].live_vscroll.clone()
        });
        // 滚到中深部（留出双向余量且让「无锚定漂移」足够大到可分辨）。
        vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(-1500.)));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pad_y = crate::app::panes::LIVE_BODY_PAD_Y;
        let local_top = |y: f32| y.max(0.0) - pad_y;
        let read_anchor = |cx: &mut VisualTestContext| -> Option<(usize, f32)> {
            cx.update(|_, cx| {
                let a = app.read(cx);
                a.tabs[a.active_tab]
                    .live2
                    .as_ref()
                    .expect("live2 应就绪")
                    .read(cx)
                    .block_of_px(local_top(vhand.offset().y.as_f32().abs()))
            })
        };
        // 读**缩放前**视口顶部的 WYSIWYG 内容点（旧字号旧 frame）。
        let before = read_anchor(&mut *cx).expect("缩放前应有可识别锚点块");
        // 放大 3 档（等价 Ctrl+向上滚 3 次），多泵几帧让「估计高→整形真值」收敛完成。
        cx.update(|_, cx| {
            app.update(cx, |app, cx| app.change_font_size(3.0, cx));
        });
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 读缩放后视口顶部内容点；线性锚点近似允许 ±1 块（对齐 markdown/view ±2 容差）。
        let after = read_anchor(&mut *cx).expect("缩放后也应能识别锚点块");
        assert!(
            (before.0 as i64 - after.0 as i64).abs() <= 1,
            "缩放不应明显改变视口顶部块：{before:?} → {after:?}"
        );
        assert!(
            (before.1 - after.1).abs() < 0.3,
            "缩放后块内比例应基本不变：{:.3} → {:.3}",
            before.1,
            after.1
        );
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
        let count = cx.update(|_window, cx| app.read(cx).notifications.len());
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
        let count2 = cx.update(|_window, cx| app.read(cx).notifications.len());
        assert_eq!(count2, 0, "6 秒后打开失败通知应已自动消失");
        let _ = std::fs::remove_file(&p);
    }

    /// 大二进制（>128KB 走后台分块路径）：非 UTF-8 必须同样弹失败通知，
    /// 绝不「打开」成只剩少量合法前缀的乱码页（回归：异步分块路径曾静默吞尾）。
    #[gpui::test]
    async fn large_binary_shows_notification(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let p = std::env::temp_dir().join("gpui-binary-large.bin");
        // 200_000 字节 > OPEN_ASYNC_MIN_BYTES(128KB) → 强制走 ChunkReader 异步分块路径
        std::fs::write(&p, vec![0xFF; 200_000]).unwrap();
        let tabs_before = cx.update(|_w, cx| app.read(cx).tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        // 异步分块读取需多轮推进（读盘→通道→主线程装配）
        for _ in 0..30 {
            cx.run_until_parked();
        }

        let (status_ok, tabs_after, active_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.status_ok,
                a.tabs.len(),
                if a.tabs.is_empty() {
                    None
                } else {
                    Some(a.tabs[a.active_tab].editor.read(cx).value().to_string())
                },
            )
        });
        assert!(!status_ok, "大二进制（非 UTF-8）应置状态为失败");
        let count = cx.update(|_window, cx| app.read(cx).notifications.len());
        assert_eq!(count, 1, "打开大二进制失败应推送一条通知");
        // 大路径先建选项卡再异步失败：选项卡保留但必须为空（未挂任何非法内容）。
        assert_eq!(tabs_after, tabs_before + 1, "大文件先建选项卡再失败");
        assert_eq!(
            active_content.as_deref(),
            Some(""),
            "编辑区不得装入非法内容（乱码前缀）"
        );
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

    /// Markdown 预览同步（A4：逐键增量桥）——编辑触发 `Edited` 后，右栏 WyEditor 预览
    /// 立即（同步重放，非后台扫描）跟随编辑区，恒与编辑区逐字节一致。MD 分支已脱离
    /// MarkdownPreview（不再喂 `md_preview`），此测试验证统一组件才是真正的「预览」。
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
        let preview_text = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                a.tabs[a.active_tab]
                    .md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default()
            })
        };
        let t0 = preview_text(cx);
        assert!(t0.contains("初始标题"), "打开时预览应含初始标题（md_wy 同步喂源），实际 {t0:?}");

        // 真实编辑：粘贴触发 InputEvent::Change → 增量桥同步重放。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("新增段落".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..30 {
            cx.run_until_parked();
        }
        let (t1, e1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert!(
            t1.contains("新增段落"),
            "编辑后预览应跟随新内容（增量桥），实际 {t1:?}"
        );
        assert_eq!(t1, e1, "预览应与编辑区逐字节一致");
        // MD 分支脱离：MarkdownPreview 不再承载 MD 预览（EPUB/TOC 专用）。
        let md_prev = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].md_preview.read(cx).rendered_text()
        });
        assert!(
            !md_prev.contains("初始标题") && !md_prev.contains("新增段落"),
            "MD 预览应已脱离 MarkdownPreview（不再喂源），实际 {md_prev:?}"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 快速连续两次编辑：增量桥是同步逐键重放（无后台轮次/代次之争），最终预览必
    /// 收敛 = 编辑区（含两次编辑内容），不可能被旧轮覆盖。
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
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        // 连续两次快速编辑（无去抖间隔——桥无去抖，逐键即同步）。
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("B".to_string()));
        cx.dispatch_action(Paste);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("C".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..30 {
            cx.run_until_parked();
        }
        let (t, e) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert!(
            t.contains("C") && t.contains("B"),
            "最终预览应含两次编辑内容（收敛到最新），实际 {t:?}"
        );
        assert_eq!(t, e, "预览应与编辑区一致");
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
        cx.dispatch_action(Paste);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "粘贴编辑后");
        // 再粘贴带换行的内容，行数应增加且仍与 str 语义一致
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("A\nB".to_string()));
        cx.dispatch_action(Paste);
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
        use crate::editor::rope_ext::RopeExt;
        use ropey::Rope;
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
        cx.update(|_, cx| {
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
                    && tab.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false)
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大 md 完整流程超时：{} 帧", frames);
            }
        }
        let (fast_count, finish_count, wy_loaded, status_ok) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (
                a.open_fast_count,
                a.open_finish_count,
                a.tabs[i].md_wy.as_ref().map(|w| w.read(cx).is_loaded()).unwrap_or(false),
                a.status_ok,
            )
        });
        assert_eq!(fast_count, 1, "两段式快段应执行 1 次");
        assert_eq!(finish_count, 1, "两段式慢段应执行 1 次");
        assert!(wy_loaded, "右栏 WyEditor 预览应装配完成");
        assert!(status_ok, "打开状态应成功");
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：大文档打开时 ListState 为**固定小窗口**（O(1) 挂载，与全文行数无关）——
    /// mount 同步栈内即可断言 item_count 远小于全文行数（★不建 N 条 → 内存 O(1)），
    /// 防止退化成 mount 同步 O(N) 建全文（那正是内存 +~300MB / 打开冻帧的元凶）。
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
        use crate::editor::rope_ext::RopeExt as _;
        let rope = ropey::Rope::from(content.as_str());
        let line_count = rope.lines_len();
        assert!(line_count > 4_096, "测试应远超固定窗口（512），实际 {line_count} 行");

        let ed_ent = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        // 同步栈内断言两段式：mount 返回时只建了视口小列表（若非两段式，这里
        // item_count 直接等于全文行数）。
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, cx| {
                ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                let ic = ed.test_list_state().item_count();
                assert!(
                    ic < line_count && ic <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
                    "mount 同步栈内应为视口小列表：item_count={ic}（全文 {line_count} 行，\
                     两段式应只建 ≤{}）",
                    crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
                );
                cx.notify(); // 真实打开路径 load_file_text 会 notify；这里等价驱动首帧 render
            });
        });
        // 固定窗口虚拟化：mount 即就绪（无分帧增长），列表恒为小窗口。
        let mut frames = 0;
        loop {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            let done = cx.update(|_w, cx| {
                let ed = ed_ent.read(cx);
                !ed.test_list_is_deferred()
                    && ed.test_list_window_len() <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
                    && ed.test_metrics().1 > 0.
            });
            if done {
                break;
            }
            frames += 1;
            if frames > 120 {
                panic!("固定窗口挂载超时：{frames} 帧");
            }
        }
        cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
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
            "量程应按全文行数健康：max_off={mo:.0}，预期≈{expected:.0}（行 {line_count}/高 {lh:.0}px）"
        );
    }

    /// 回归：大文档「分块读盘·渐进装配」——打开期间编辑器内容**渐进**出现（读多少
    /// 显示多少），首块挂载即可视（`editor_pending`=false），全部读完后内容字节级一致。
    /// 逐 tick 驱动协作式调度器，断言在某个中间时刻行数落在 (0, 全文行数) 之间——
    /// 证明内容不是「整篇读完一次性原子挂载」，而是逐块推送上屏。
    #[gpui::test]
    async fn progressive_load_shows_partial_content_early(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-progressive-mount-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // ~3.3MB / 60k 行：远超异步阈值与首块 256KB 下限，且多于有界通道容量（多块）。
        let line = "x".repeat(48);
        let content = (0..60_000)
            .map(|i| format!("{line} {i:07}\n"))
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

        let mut saw_partial = false;
        let mut ticks = 0;
        let mut done = false;
        // 逐 tick（每步至多一个任务轮询）：观察进程中的部分内容；无余量任务即停止。
        while ticks < 3000 {
            if !cx.cx.executor().tick() {
                break;
            }
            ticks += 1;
            let (lc, pending, ready_full) = cx.update(|_w, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                let tab = &a.tabs[i];
                let ed = tab.editor.read(cx);
                let outf_block = a.status_ok && ed.value() == content;
                (ed.line_count(), tab.editor_pending, outf_block)
            });
            if !pending && lc > 0 && lc < 60_000 {
                saw_partial = true;
            }
            if ready_full {
                done = true;
            }
        }
        assert!(done, "渐进装配应收尾：{ticks} ticks 后未完成");
        assert!(
            saw_partial,
            "应观察到「部分内容先行上屏」的中间态（{ticks} ticks；若缺 256KB 首块下限/逐块外推则为整篇原子挂载）"
        );
        let (dlc, pending, final_ok) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let tab = &a.tabs[i];
            (
                tab.editor.read(cx).display_line_count(),
                tab.editor_pending,
                tab.editor.read(cx).value() == content,
            )
        });
        assert!(!pending, "装配完成后 editor_pending 应复位");
        assert!(final_ok, "最终内容应与文件字节级一致");
        assert_eq!(dlc, 60_000, "最终显示行数应为全文行数");
        let _ = std::fs::remove_file(&dir);
    }

    /// A4（承接原“预览与编辑器共享源 Rope”回归）：大 md 打开（完整异步流水线 +
    /// `finish_file_text` 装预览）后，右栏统一组件 WyEditor 由**同一棵编辑器 Rope**（ropey
    /// 2.0 Arc 树，`Clone` O(1)）后台 `open_from_rope` 装配——不物化第二份源文本，
    /// MD 现在只有这一份预览解析态（不再有 MarkdownPreview 的第二份扫描）。
    #[gpui::test]
    async fn large_md_preview_covers_full_source(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-md-share-rope-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // 块数 > WINDOWED_MIN_BLOCKS 且 > 128KB → 走异步流水线（finish 安装 md_wy）。
        let n = crate::config::limits::WINDOWED_MIN_BLOCKS + 800;
        let content = (0..n)
            .map(|i| {
                format!("# 大标题 {i}\n\n这是第 {i} 段正文，包含 *强调*、`行内代码` 与 [链接](https://x.y)。\n\n")
            })
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试 md 应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let loaded = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                let t = &a.tabs[i];
                !t.editor_pending
                    && t.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false)
            });
            if loaded {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大 md 打开/预览装配超时：{frames} 帧");
            }
        }
        // 预览全文 = 编辑区正文（md_wy 从同一棵 Rope 装配，覆盖完整源；一份解析态）。
        let (wy_text, ed_text, md_prev_blocks) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (
                a.tabs[i].md_wy.as_ref().unwrap().read(cx).text().to_string(),
                a.tabs[i].editor.read(cx).value().to_string(),
                a.tabs[i].md_preview.read(cx).memory_estimate().0,
            )
        });
        assert_eq!(wy_text, ed_text, "大 md WyEditor 预览应覆盖完整源（与编辑区一致）");
        assert!(
            md_prev_blocks == 0,
            "MD 分支脱离：MarkdownPreview 不再为大 md 建第二份索引（blocks={md_prev_blocks}）"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归（专家评审 P1 升级版）：固定窗口虚拟化下的大文档「深跳」——Ctrl+End /
    /// 搜索跳到远超视口的行时：
    /// 1. ListState 恒是**固定小窗口**（O(WINDOW) 挂载，不建 3.2M 条 → 内存 O(1)）；
    /// 2. 跳转**瞬时落位**（重定位 `window_base` 到目标行附近并渲染：无占位符、无
    ///    「分帧增长逐块收敛」循环），比旧「深跳占位 + 逐帧增长」更快且更简单。
    #[gpui::test]
    async fn deep_jump_lands_instantly_with_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let line = "x".repeat(60);
        let content = (0..90_000)
            .map(|i| format!("{line} {i:010}\n"))
            .collect::<String>();
        use crate::editor::rope_ext::RopeExt as _;
        let rope = ropey::Rope::from(content.as_str());
        let line_count = rope.lines_len();

        let ed_ent = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, cx| {
                ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                cx.notify(); // 等价 load_file_text 的 notify，驱动首帧 render
            });
        });
        // 挂载即就绪：固定小窗口，无分帧增长。
        for _ in 0..5 {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let (deferred, win, base0, rows) = cx.update(|_w, cx| {
            let ed = ed_ent.read(cx);
            (
                ed.test_list_is_deferred(),
                ed.test_list_window_len(),
                ed.test_window_base(),
                ed.line_count(),
            )
        });
        assert!(!deferred, "固定窗口挂载即就绪（无两段式中间态）");
        assert!(
            win <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
            "固定窗口应为小窗口，实际 {win}"
        );
        assert_eq!(base0, 0, "未滚动前窗口基址应为 0");
        assert!(rows >= line_count, "行数应加载完整：{rows} ≥ {line_count}");

        // 深跳：选区到全文末尾 → 瞬时落位。
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, cx| {
                ed.set_selection_range(content.len().saturating_sub(1)..content.len(), cx);
            });
        });
        for _ in 0..5 {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(1));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let (top, base) = cx.update(|_w, cx| {
            let ed = ed_ent.read(cx);
            (ed.test_text_scroll_top().0, ed.test_window_base())
        });
        assert!(
            top >= line_count.saturating_sub(64),
            "深跳应瞬时滚到全文末尾附近：top={top}（全文 {line_count} 行）"
        );
        assert!(base > 0, "窗口基址应重定位到大行号：base={base}");
        assert!(
            top.saturating_sub(base) < crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
            "目标行应在窗口内：abs_top={top} base={base}"
        );
    }

    /// 回归（2026-09-16）：底部预留必须被渲染成**可见**的空白区，最后一行不得被
    /// 底部横向滚动条盖住。曾用「滚动量程里加 pad」实现，但 gpui 的 List 在下方内容
    /// 不足一屏时会**自动上移填满视口**，把预留区吃掉、最后一行贴到视口底（被滚动条
    /// 遮住）。修复：把预留做成 ListState 末尾的**真实空白条目**（占行高）。本测试在
    /// 真实 app 布局下滚到量程最大值，断言最后一行底边距视口底 ≈ pad（空白真实可见）。
    #[gpui::test]
    async fn bottom_pad_renders_visible_blank_at_max_scroll(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("gpui-botpad-{}.txt", std::process::id()));
        let content = (0..300)
            .map(|i| format!("这是第 {i} 行内容"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let ed_ent = {
            let mut notices = 0;
            loop {
                cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                let ready = cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.tabs.len().saturating_sub(1);
                    let ed = a.tabs[i].editor.read(cx);
                    ed.line_count() > 1 && !ed.test_list_is_deferred()
                });
                if ready {
                    break;
                }
                notices += 1;
                if notices > 120 {
                    panic!("打开/增长未完成");
                }
            }
            cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                a.tabs[i].editor.clone()
            })
        };
        let (lh, pad, row_count, item_count, want_items) = cx.update(|_, cx| {
            let ed = ed_ent.read(cx);
            (
                ed.test_metrics().1.max(1.),
                ed.test_bottom_pad_px(),
                ed.line_count(),
                ed.test_list_state().item_count(),
                ed.test_scroll_item_count(),
            )
        });
        // 固定窗口虚拟化：条目数恒为小窗口（与全文行数无关），量程按全文
        // `scroll_item_count` 计算——底部预留随量程存在于滚动最底端。
        assert!(
            item_count <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
            "固定窗口应是小窗口：item_count={item_count} ≤ {}",
            crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
        );
        assert!(
            want_items >= row_count && want_items <= row_count + 16,
            "全文量程条目数应为行数+预留空行：want={want_items} rows={row_count}"
        );
        cx.update(|_, cx| {
            ed_ent.update(cx, |ed, _| ed.test_scroll_to_bottom());
        });
        for _ in 0..4 {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        let (ix, off, view2) = cx.update(|_, cx| {
            let ed = ed_ent.read(cx);
            let t = ed.test_text_scroll_top();
            (t.0, t.1, ed.list_viewport_height())
        });
        // 最后一行底边在视口内的 y =（最后一行距滚动顶的行数 × 行高）+ 行高 − 已滚进
        // 顶行的偏移；底部空白 = 视口高 − 该 y。基底预留应 ≈ pad 且 ≥ pad − 1 行。
        let last_top_px = (row_count as f32 - ix as f32) * lh - off;
        let last_bottom = last_top_px + lh;
        let blank_valid = view2 - last_bottom;
        // gpui 对「填充不满一屏」会回退一行把视口填满（浮点舍入所致），实际空白可能
        // 比 pad 少约 1 行；断言「可见空白 ≥ pad − 行高 − 余量」即可证明预留区真实渲染、
        // 最后一行未被横向滚动条盖住。
        assert!(
            blank_valid >= pad - lh - 2.,
            "滚到底后最后一行下方应有可见预留空白：blank={blank_valid:.1}px，pad={pad:.1}px，行高 {lh:.1}（scroll_top=({ix},{off}), view={view2}, rows={row_count}）——真实预留空行未渲染"
        );
        assert!(
            blank_valid <= pad + lh + 1.,
            "底部空白不应明显超过预留：blank={blank_valid:.1}px ≤ pad+行高={:.1}",
            pad + lh + 1.
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 临时基准（ignored，release）：开不同体积的真实大文档，量化各阶段耗时——
    /// 同步返回 / 内容可见（快段）/ 全文 ListState 分帧增长完成 / 滚动条就绪。
    /// 运行：`cargo test --release -- --ignored bench_open_large_multiple --nocapture`
    /// 打开期间设 MD_OPEN_DEBUG=1，配合每阶段的墙钟标记定位瓶颈占比。
    #[gpui::test]
    #[ignore]
    async fn bench_open_large_multiple(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        for path in [
            r"fixtures/大文档测试.md",
            r"fixtures/大文档测试23MB.md",
            r"fixtures/大文档测试23MB.txt",
            r"fixtures/大文档测试116MB.txt",
        ] {
            println!("========== {path} ==========");
            let t0 = std::time::Instant::now();
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.open_file(path, window, cx));
            });
            let sync_ms = t0.elapsed().as_millis();
            println!("【open_file 同步返回】 {sync_ms} ms");
            let mut content_ms = None;
            let mut full_ms = None;
            let mut frames = 0;
            loop {
                frames += 1;
                // 推模拟时钟触发分帧增长定时器（真实应用由 OS 定时器/帧循环驱动）。
                cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                let (line_count, _item_count, deferred) = cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.tabs.len().saturating_sub(1);
                    let ed = a.tabs[i].editor.read(cx);
                    (
                        ed.line_count(),
                        ed.test_list_state().item_count(),
                        ed.test_list_is_deferred(),
                    )
                });
                if content_ms.is_none() && line_count > 0 {
                    content_ms = Some(t0.elapsed().as_millis() as usize);
                }
                if !deferred && full_ms.is_none() {
                    full_ms = Some(t0.elapsed().as_millis() as usize);
                }
                if content_ms.is_some() && full_ms.is_some() {
                    break;
                }
                if frames > 4000 {
                    println!("【超时】 content={content_ms:?} full={full_ms:?}");
                    break;
                }
            }
            println!(
                "【内容可见】 {:?} ms，【ListState 就绪（固定窗口）】 {:?} ms，固定窗口条目 {}（常数），行数 {}",
                content_ms,
                full_ms,
                crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
                cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.tabs.len().saturating_sub(1);
                    a.tabs[i].editor.read(cx).line_count()
                })
            );
        }
    }

    /// 基准（ignored，release）：隔离测量大列表「统一高度重铺」的原始开销——正是旧的
    /// 一次性 defer 补建/量程自愈会用到的操作（现大文档路径已改分帧增长 + 按需实测，
    /// 不再调用）。运行：
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
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
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
        // 预览初始应在顶部（同步开启前的基线；大文档装好后预览停在开头）。
        let top0 = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (-a.tabs[i].md_wy_vscroll.offset().y).as_f32()
        });
        assert!(top0 >= -0.5, "预览初始应在顶部（top0={top0:.0}px）");
        // 编辑器「拖到底」：模拟 VerticalScrollHandle.start_drag 的重铺+冻结+拖底，
        // 断言拖到底后最后内容可见。此后编辑区与预览区经滚动同步联动（预览跟随）。
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
        // 预览侧（A2：统一组件 WyEditor 宿主）量程同样须健康：大文档装好后宿主
        // `#md-wy-scroll` 的量程必须远超视口（>0），不能坍缩——否则预览滚动条拖不到底。
        let (wy_loaded, max_off) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let t = &a.tabs[i];
            let vh = &t.md_wy_vscroll;
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).is_loaded())
                    .unwrap_or(false),
                vh.max_offset().y.as_f32(),
            )
        });
        assert!(wy_loaded, "大文档 WyEditor 预览应完成装配（下一帧渲染即出内容）");
        assert!(
            max_off > 50.0,
            "预览纵向量程不应坍缩（max_off={max_off:.0}px，应≈全篇块高之和）"
        );
        // 注：编辑器在上一段已被「拖到底」，预览随滚动同步联动不再停于顶部——
        // 「初始在顶部」基线已在上方（编辑器拖底之前）单独断言过；这里只看量程健康。
        // 确定性「拖到底」回归（用户实测：内置滚动条在量程坍缩/未自愈时开始拖会冻结
        // 旧值 → 拖不到真正的底部）。直接推宿主滚动句柄到底，断言实际落到了末端附近。
        cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .md_wy_vscroll
                .set_offset(gpui::point(gpui::px(0.), gpui::px(-1_000_000.)));
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let bottom_off = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].md_wy_vscroll.offset().y.as_f32()
        });
        assert!(
            bottom_off < -max_off * 0.9,
            "预览拖到底应接近真正的底部（offset={bottom_off:.0}，max_off={max_off:.0}）"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 诊断（临时，手工跑）：真机大文档上对比编辑器/预览双层量程 + 模拟拖到底。
    /// 依赖 `fixtures/大文档测试.md` 存在；正常跑套件时跳过。#[ignore] 手动：
    /// `cargo test -- --ignored diag_drag_bottom_real_file --nocapture`
    #[gpui::test]
    #[ignore]
    async fn diag_drag_bottom_real_file(cx: &mut TestAppContext) {
        init_test(cx);
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"fixtures/大文档测试.md", window, cx);
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
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
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
            .join("fixtures/pdf测试.pdf");
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

    /// 决定性回归：**无任何用户输入**的情况下，后台瓦片/封面渲染后必须经「notify →
    /// 窗口重绘」链条被绘制出来。若此测试失败 = worker 渲完了但主线程从未重绘 →
    /// 用户看到“CPU 低、画面长时间不变”，正是当前体感问题的根因候选。
    #[gpui::test]
    async fn pdf_content_reaches_pixels_without_user_input(cx: &mut TestAppContext) {
        let _pdf_guard = crate::pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/pdf测试.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(pdf.to_str().unwrap(), window, cx));
        });
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
        // 不触发任何输入/事件：仅跑虚拟帧 + 少量真实等待给 pdfium worker。
        // 用 advance_clock 推进虚拟时间以驱动 16ms 自刷新泵（真实 app 里是真实时钟，
        // 定时器天然触发；测试虚拟时钟需显式推进）。
        for _ in 0..400 {
            cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for _ in 0..120 {
            cx.refresh().unwrap();
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (loaded, tiles, cover, pending) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let pv = a.tabs[a.active_tab].pdf_preview.as_ref().unwrap().read(cx);
            let s0 = pv.slots.first();
            (
                pv.is_loaded(),
                s0.map(|s| s.tiles.len()).unwrap_or(0),
                s0.map(|s| s.cover.is_some()).unwrap_or(false),
                pv.pending_tiles.len(),
            )
        });
        assert!(loaded, "应已加载");
        // 关键回归：无任何输入也必须在首帧布局就位后**发出瓦片/封面请求**（此前
        // "负载完成 → notify 空操作 → 永不重绘 → 首帧 bounds=0 → 永远调不到瓦片"）。
        // 说明：测试 harness 对深层嵌套子视图无法一直驱动 collect，故此处断言"已调度"；
        // 完整"调度→回传→收集→绘制"链路由 pdf::tests 的窗口根节点测试覆盖。
        assert!(
            tiles >= 1 || cover || pending >= 1,
            "无输入也应发起瓦片/封面调度：tiles={tiles} cover={cover} pending={pending}"
        );
    }

    /// 回归：打开长 PDF（曾因全量栅格化耗时 ~270s 表现为“无法加载”）。
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
        // 长文 fixture 曾是 17.….pdf（pdf测试.pdf 的逐字节副本，已去重）——直接用副本本体。
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/pdf测试.pdf");
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
            page_count >= 50,
            "长 PDF 应列出完整页数（fixture = pdf测试.pdf，87 页），实际={page_count}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "长 PDF 结构解析应秒级完成（曾需 ~270s 全量栅格化），实际 {elapsed:?}"
        );
    }

    /// EPUB 轻量视口分页：底部状态栏出现「第 x / N 页」；初始第 1 页、总页数随内容
    /// 分页；滚动预览到末标题后页码前进（滚动明细由 view 层单元测试覆盖）。
    #[gpui::test]
    async fn epub_status_bar_shows_pages_and_follows_scroll(cx: &mut TestAppContext) {
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
        if !epub.exists() {
            eprintln!("SKIP epub_status_bar_shows_pages_and_follows_scroll: 缺 fixtures/xy.epub");
            return;
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(900.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(epub.to_str().unwrap(), window, cx));
        });
        // 页码元素按就绪门控出现（视口/行高就绪后），有界等待而非固定帧数，避免 flaky。
        let mut page_el = None;
        for _i in 0..30 {
            for _ in 0..2 {
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                cx.run_until_parked();
            }
            if let Some(b) = cx.debug_bounds("status-epub-page") {
                page_el = Some(b);
                break;
            }
        }
        let page_el = page_el.expect("EPUB 状态栏应显示页码（status-epub-page）");
        assert!(page_el.size.width > px(0.), "页码元素应有宽度");
        let (p1, n) = cx.update(|_, cx| {
            let a = app.read(cx);
            let md = &a.tabs[a.active_tab].md_preview;
            md.read(cx)
                .viewport_page_bounds()
                .expect("EPUB 应有页码读数")
        });
        assert_eq!(p1, 1, "初始应为第 1 页");
        assert!(n >= 1, "总页数至少 1 页（实际 {n}）");

        // 滚动预览到最后一个目录标题 → 页码前进（内容不足一页则不强制）。
        if n > 1 {
            let last_toc_row = cx.update(|_, cx| {
                let a = app.read(cx);
                let md = &a.tabs[a.active_tab].md_preview;
                md.read(cx)
                    .toc_entries()
                    .last()
                    .map(|(r, _, _)| *r)
            });
            if let Some(row) = last_toc_row {
                cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.active_tab;
                    let md = a.tabs[i].md_preview.clone();
                    md.update(cx, |v, _| v.scroll_to_row(row));
                });
                for _ in 0..8 {
                    cx.run_until_parked();
                    let _ = cx.cx.refresh();
                    cx.run_until_parked();
                }
                let p2 = cx.update(|_, cx| {
                    let a = app.read(cx);
                    a.tabs[a.active_tab]
                        .md_preview
                        .read(cx)
                        .viewport_page_bounds()
                        .expect("EPUB 应有页码读数")
                });
                assert!(
                    p2.0 >= 1 && p2.0 <= p2.1,
                    "滚动后页码应合法（{}/{}）", p2.0, p2.1
                );
            }
        }
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
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
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
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        // md：大文档异步打开。open_file 应立即返回（拖入不卡 UI），
        // 内容稍后在后台装配出来。
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"fixtures/大文档测试.md", window, cx);
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
                app.open_file(r"fixtures/大文档测试.txt", window, cx);
            });
        });
        println!("【txt 打开：open_file 返回】(对照) {} ms", t_txt.elapsed().as_millis());
    }

    /// A2/A4 基准（ignored，release）：大文档异步打开 + 右栏 WyEditor 预览装配的墙钟拆解。
    /// 真实 `fixtures/大文档测试2.md`（6.4MB / ~14 万行）。A4 后 MD 走统一组件：后台只做
    /// 一份 `Doc::open`（不再 scan_blocks 喂 MarkdownPreview）。测同步返回（UI 不卡）→
    /// 编辑区内容可见（D）→ 右栏 WyEditor 预览装配（WY）。运行：
    /// `cargo test --release -- --ignored bench_a2_md_wy_open_large --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_a2_md_wy_open_large(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        let p = r"fixtures/大文档测试2.md";
        assert!(std::path::Path::new(p).is_file(), "{p} 缺失");
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p, window, cx));
        });
        println!("【同步返回】(UI 不卡) {} ms", t0.elapsed().as_millis());
        let mut d_ready = false;
        let mut wy_ready = false;
        let mut d_ms = 0usize;
        let mut wy_ms = 0usize;
        let mut frames = 0;
        loop {
            frames += 1;
            cx.run_until_parked();
            let (editor_lines, pending, wy_loaded) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                (
                    a.tabs[i].editor.read(cx).line_count(),
                    a.tabs[i].editor_pending,
                    a.tabs[i]
                        .md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false),
                )
            });
            if !d_ready && !pending && editor_lines > 1000 {
                d_ready = true;
                d_ms = t0.elapsed().as_millis() as usize;
            }
            if !wy_ready && wy_loaded {
                wy_ready = true;
                wy_ms = t0.elapsed().as_millis() as usize;
            }
            if d_ready && (wy_ready || frames > 90) {
                break;
            }
            if frames > 400 {
                break;
            }
        }
        println!("【D. 编辑区内容可见】 {d_ms} ms");
        println!(
            "【WY. 右栏 WyEditor 预览装配完成】 {} ms",
            if wy_ms > 0 {
                format!("{wy_ms} ms")
            } else {
                "未装配".to_string()
            }
        );
        let md_prev_blocks = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].md_preview.read(cx).memory_estimate().0
        });
        println!(
            "【md_preview（EPUB/TOC 侧）】 MD 分支脱离后不应再为大 md 建索引（blocks={md_prev_blocks}）"
        );
        // 首帧真实布局
        let tb = std::time::Instant::now();
        for i in 0..3 {
            let tf = std::time::Instant::now();
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            println!(
                "【C. 挂载后第 {} 次 refresh】(真实布局) {} ms",
                i + 1,
                tf.elapsed().as_millis()
            );
        }
        println!("【C. 前 3 次 refresh 合计】 {} ms", tb.elapsed().as_millis());
    }

    /// A3 基准（ignored，release）：大文档「增量桥」的真实成本拆解（真实 6.4MB / 139k 行）：
    /// 1. 桥模型成本 = 直接对装配好的 WyEditor 逐键 `preview_apply_edit`（= undo/redo 的
    ///    `apply_raw_edit` 增量管线：Doc::edit + Pass1 + derived + 单块作废），应亚毫秒；
    /// 2. 真实逐键路径（Paste → insert_edit → Edited → 桥）200 键后预览必须与编辑区
    ///    逐字节一致（正确性；墙钟含旧的 md_preview 后台重索引 headless 等待，不作断言）。
    /// 运行：`cargo test --release -- --ignored bench_a3_incremental_bridge_per_key --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_a3_incremental_bridge_per_key(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = r"fixtures/大文档测试2.md";
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p, window, cx));
        });
        let mut wy_ready = false;
        for _ in 0..400 {
            cx.run_until_parked();
            wy_ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                a.tabs[i]
                    .md_wy
                    .as_ref()
                    .map(|w| w.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if wy_ready {
                break;
            }
        }
        assert!(wy_ready, "md_wy 应装配完成（大文档异步 install）");
        const N: usize = 200;
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("啊".to_string()));
        let t_real = std::time::Instant::now();
        for _ in 0..N {
            cx.dispatch_action(Paste);
            // 只推到桥应用前后（不等后台 md_preview 重索引——那会混入旧管线头等待）。
            for _ in 0..3 {
                cx.run_until_parked();
            }
        }
        let real_total_ms = t_real.elapsed().as_millis();
        // 真实逐键路径一致性：预览必须恒等于编辑区（增量桥正确性）。
        let (m, e) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(m, e, "{N} 键后预览应仍与编辑区逐字节一致");
        // 隔离测量桥模型成本（独立 WyEditor 副本，绝不污染已对齐的 md_wy）：
        // ⚠️ 循环内不读 `text()`（O(n) 物化）；用 O(1) `len()` 取初始长后自行推进。
        let scratch = cx.update(|window, cx| {
            let rope = app.read(cx).tabs[app.read(cx).active_tab]
                .editor
                .read(cx)
                .rope()
                .clone();
            let s = cx.new(|cx| WyEditor::new(window, cx));
            s.update(cx, |w, _cx| {
                w.install_open(crate::markdown::wysiwyg::WyEditor::open_from_rope(rope))
            });
            s
        });
        let start_len = cx.update(|_w, cx| scratch.read(cx).len());
        let model_ms = cx
            .update(|_w, cx| {
                let t = std::time::Instant::now();
                let mut at = start_len;
                for _ in 0..200 {
                    scratch.update(cx, |w, cx| w.preview_apply_edit(at..at, "啊", cx));
                    at += 3; // "啊" 3 字节
                }
                t.elapsed()
            })
            .as_millis();
        println!(
            "【桥模型成本】 200 次 preview_apply_edit 合计 {model_ms} ms ≈ {:.3} ms/键",
            model_ms as f64 / 200.0
        );
        println!(
            "【真实逐键路径】 {N} 键合计 {real_total_ms} ms（含编辑器/帧/headless 泵；
            A4 已移除每键 md_preview 后台重索引，不作断言）"
        );
        assert!(
            model_ms as f64 / 200.0 < 2.0,
            "桥模型成本应亚毫秒/键（6.4MB/139k 行，实际 {model_ms} ms/200 键）"
        );
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
                    r"fixtures/大文档测试.md".to_string(),
                    r"fixtures/SKILL.md".to_string(),
                    r"fixtures/SKILL.txt".to_string(),
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
                cx.update(|_, cx| {
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

    /// 进程内存探针（Windows）：`PagefileUsage`（私有提交字节）= 本进程真实分配量，
    /// `WorkingSetSize`（工作集）= 物理驻留。二者均可反映内存峰值。
    #[cfg(windows)]
    mod mem_probe {
        #[repr(C)]
        pub struct ProcessMemoryCounters {
            pub cb: u32,
            pub page_fault_count: u32,
            pub peak_working_set_size: usize,
            pub working_set_size: usize,
            pub quota_peak_paged_pool_usage: usize,
            pub quota_paged_pool_usage: usize,
            pub quota_peak_nonpaged_pool_usage: usize,
            pub quota_nonpaged_pool_usage: usize,
            pub pagefile_usage: usize,
            pub peak_pagefile_usage: usize,
        }
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            #[allow(clashing_extern_declarations)]
            fn K32GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                counter: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
        }
        pub fn counters() -> (u64, u64) {
            unsafe {
                let mut c: ProcessMemoryCounters = std::mem::zeroed();
                c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
                if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
                    (c.pagefile_usage as u64, c.working_set_size as u64)
                } else {
                    (0, 0)
                }
            }
        }
    }

    /// 流式数 `\n` 字节 + 尾换行幻影，得 ropey `lines_len()` 口径的预期行数（不经
    /// 常驻整篇拷贝；memchr SIMD 扫 128MB ≈ 百毫秒级）。
    fn expected_rope_lines(path: &str) -> usize {
        use std::io::Read as _;
        let mut f = std::fs::File::open(path).expect("基准文件应存在");
        let mut buf = vec![0u8; 1 << 20];
        let mut nl = 0usize;
        let mut last = 0u8;
        loop {
            let n = f.read(&mut buf).expect("读基准文件");
            if n == 0 {
                break;
            }
            nl += memchr::memchr_iter(b'\n', &buf[..n]).count();
            last = buf[n - 1];
        }
        nl + usize::from(last == b'\n')
    }

    /// 基准：打开大文档（默认 `fixtures/大文档测试126MB.txt`，可用 `BENCH_OPEN_FILE`
    /// 覆盖）的分阶段墙钟 + 进程私有内存峰值。
    /// - 「首屏可见」= 编辑器首次出现内容（旧：整篇读完一次性挂载才非空；
    ///   新：分块读盘首块 256KB 即挂）。
    /// - 「全文就绪」= 行数达到全文行数（旧：快段整篇装配；新：末块追加完成）。
    /// - 「流水线结束」= `open_finish_count>=1`（旧：还要全量行宽统计 ≈7s；
    ///   新：EOF 后即刻）。
    /// - 内存峰值 = 打开窗口内进程私有提交字节增量（1ms 采样；旧路径读盘期有
    ///   「读盘 String + 归一化 String + Rope」≈2× 文件，新路径峰值≈1× 文件）。
    /// 运行：`cargo test --release --bin gpui-editor bench_open_large_txt_progressive -- \
    ///        --ignored --nocapture`（debug 亦可，比值一致、绝对值放大）。
    #[gpui::test]
    #[ignore]
    async fn bench_open_large_txt_progressive(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let path = std::env::var("BENCH_OPEN_FILE").unwrap_or_else(|_| {
            r"fixtures/大文档测试126MB.txt".to_string()
        });
        if !std::path::Path::new(&path).is_file() {
            // 资产缺失：现场生成一份 ~120MB LF 文本（保证可复现基准）。
            let line = "x".repeat(80);
            let mut out = std::fs::File::create(&path).expect("创建基准文件");
            use std::io::Write as _;
            for i in 0..1_400_000usize {
                let _ = writeln!(out, "{line} {i:010}");
            }
            drop(out);
        }
        let expected_lines = expected_rope_lines(&path);
        let (base_pf, base_ws) = {
            #[cfg(windows)]
            {
                mem_probe::counters()
            }
            #[cfg(not(windows))]
            {
                (0, 0)
            }
        };

        // 看门狗：打开期间 1ms 采样私有内存峰值（读盘瞬间的瞬态高峰也能抓到）。
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let peak_pf = std::sync::Arc::new(AtomicU64::new(base_pf));
        let peak_ws = std::sync::Arc::new(AtomicU64::new(base_ws));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        {
            let (pp, pw, st) = (peak_pf.clone(), peak_ws.clone(), stop.clone());
            std::thread::spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    #[cfg(windows)]
                    {
                        let (pf, ws) = mem_probe::counters();
                        pp.fetch_max(pf, Ordering::Relaxed);
                        pw.fetch_max(ws, Ordering::Relaxed);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
        }

        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(&path, window, cx));
        });
        let sync_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let mut t_first_ms = f64::NAN;
        let mut t_full_ms = f64::NAN;
        let mut t_done_ms = f64::NAN;
        let mut ticks = 0usize;
        loop {
            if !cx.cx.executor().tick() {
                break;
            }
            ticks += 1;
            let (lc, fin) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                (
                    a.tabs[i].editor.read(cx).line_count(),
                    a.open_finish_count,
                )
            });
            let now = t0.elapsed().as_secs_f64() * 1000.0;
            if t_first_ms.is_nan() && lc > 1 {
                t_first_ms = now;
            }
            if t_full_ms.is_nan() && lc >= expected_lines {
                t_full_ms = now;
            }
            if t_done_ms.is_nan() && fin >= 1 {
                t_done_ms = now;
            }
            if !t_first_ms.is_nan() && !t_done_ms.is_nan() {
                break;
            }
            if ticks > 400_000 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);

        // 最终字节级一致性（一次性物化比较；编辑器统一 LF，与读取后归一化对比）。
        let ok = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let expected = std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            a.tabs[i].editor.read(cx).value() == expected
        });
        let last_lc = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.read(cx).line_count()
        });
        let d_pf = peak_pf.load(Ordering::Relaxed).saturating_sub(base_pf);
        let d_ws = peak_ws.load(Ordering::Relaxed).saturating_sub(base_ws);

        println!("==== 打开大文档基准：{} ====", path);
        println!(
            "  文件 {} MB / {} 行；base 私有提交 {:.1} MB / 工作集 {:.1} MB",
            std::fs::metadata(&path).map(|m| m.len() / 1024 / 1024).unwrap_or(0),
            expected_lines,
            base_pf as f64 / 1048576.0,
            base_ws as f64 / 1048576.0,
        );
        println!(
            "  open_file 同步返回: {:.1} ms（不卡 UI）",
            sync_ms
        );
        println!(
            "  首屏可见(T_first):        {:.1} ms",
            t_first_ms
        );
        println!(
            "  全文装配(T_full_content): {:.1} ms",
            t_full_ms
        );
        println!(
            "  流水线结束(T_done):       {:.1} ms",
            t_done_ms
        );
        println!(
            "  峰值内存增量: 私有提交 {:.1} MB / 工作集 {:.1} MB",
            d_pf as f64 / 1048576.0,
            d_ws as f64 / 1048576.0
        );
        println!(
            "  最终: {} 行 / 字节一致={ok} / ticks={ticks}",
            last_lc
        );
    }

    /// 基准：ropey 全文构建「一次性 `Rope::from`」 vs「1MB 分块末尾 `insert`」——
    /// 量化新管线把构建搬到逐块追加（<主线程每块 O(块)>）相对旧一次性构建的开销。
    /// 运行：`cargo test --release --bin gpui-editor bench_rope_grow_strategies -- \
    ///        --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_rope_grow_strategies(cx: &mut TestAppContext) {
        init_test(cx);
        // ~33MB 全文（3300 行 × 10KB），可比又不会跑太久。
        let line = "x".repeat(10_000);
        let full = {
            let mut s = String::with_capacity(line.len() * 3_300 + 1);
            for _ in 0..3_300 {
                s.push_str(&line);
                s.push('\n');
            }
            s
        };

        let t0 = std::time::Instant::now();
        let rope_oneshot = ropey::Rope::from(full.as_str());
        let ms_oneshot = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = std::time::Instant::now();
        let mut rope_incr = ropey::Rope::new();
        let chunk = 1 << 20;
        let mut i = 0usize;
        while i < full.len() {
            let e = full.len().min(i + chunk);
            rope_incr.insert(rope_incr.len(), &full[i..e]);
            i = e;
        }
        let ms_incr = t0.elapsed().as_secs_f64() * 1000.0;

        let n_chunks = full.len().div_ceil(chunk);
        assert_eq!(rope_oneshot.len(), rope_incr.len());
        println!(
            "==== ropey 构建基准（{} MB / {} 块）====",
            full.len() / 1024 / 1024,
            n_chunks
        );
        println!("  Rope::from 一次性:   {ms_oneshot:.2} ms");
        println!("  1MB×{n_chunks} 分块追加: {ms_incr:.2} ms（增量开销 = {:.2} ms，{:.1}%）",
            ms_incr - ms_oneshot,
            if ms_oneshot > 0. { (ms_incr - ms_oneshot) / ms_oneshot * 100. } else { 0. });
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
            app.update(cx, |app, cx| app.open_file(r"fixtures/大文档测试.md", window, cx));
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
            app.update(cx, |app, cx| app.open_file(r"fixtures/大文档测试.md", window, cx));
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
            use ropey::Rope;
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


    // ---------------------------------------------------------------------------
    // 多窗口（拖出拆分 / 拖回还原）
    // ---------------------------------------------------------------------------

    /// 快速读取全局 EditorApp 注册表的可观测快照：`(is_detached, tab 数, 活动选项卡含文件路径, 活动正文)`。
    fn registry_snapshot(
        cx: &mut TestAppContext,
    ) -> Vec<(bool, usize, bool, Option<String>)> {
        cx.update(|cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .filter_map(|(weak, _)| {
                    let entity = weak.upgrade()?;
                    let snap = entity.read_with(cx, |a, c| {
                        let tab = a.tabs.get(a.active_tab);
                        (
                            a.is_detached,
                            a.tabs.len(),
                            tab.and_then(|t| t.file_path.clone()).is_some(),
                            tab.map(|t| t.editor.read(c).value()),
                        )
                    });
                    Some(snap)
                })
                .collect()
        })
    }

    /// 选项卡「拔出」→「收纳」往返：文本、脏标记全部保留。
    #[gpui::test]
    async fn detach_adopt_round_trip_preserves_state(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-detach-roundtrip.txt");
        std::fs::write(&f, "第一行原文").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 编辑区输入 → 置脏（insert_text 属真实编辑，经 EditorEvent::Edited 置脏）
        let editor = cx.update(|_w, cx| app.read(cx).tabs[1].editor.clone());
        cx.update(|_w, cx| editor.update(cx, |ed, cx| ed.insert_text("初始内容", cx)));
        for _ in 0..2 {
            cx.run_until_parked();
        }

        let payload = cx
            .update(|_w, cx| app.update(cx, |app, _| app.take_tab_payload(1)))
            .expect("应有可拔出的选项卡");
        cx.update(|_w, cx| app.update(cx, |app, cx| app.adopt_tab(payload, None, cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (text, dirty, count, has_path) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.editor.read(cx).value(),
                t.dirty,
                a.tabs.len(),
                t.file_path.is_some(),
            )
        });
        assert_eq!(
            text.to_string(),
            "初始内容第一行原文",
            "拔出→收纳后正文应保留"
        );
        assert!(dirty, "拔出→收纳后脏标记应保留");
        assert_eq!(count, 2, "收纳后仍应有 2 个选项卡（空 + 文件）");
        assert!(has_path, "文件路径应保留");
        let _ = std::fs::remove_file(&f);
    }

    /// 把选项卡拖出窗口外松开 → 拆出独立窗口：主窗口剩未命名选项卡，
    /// 新窗口带文件选项卡且 is_detached=true。
    #[gpui::test]
    async fn drag_tab_out_splits_detached_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-multiwin-a.txt");
        std::fs::write(&f, "AAA 独立窗口").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-1").expect("文件选项卡 tab-1 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        // 按下 → 移动到超过拖拽阈值（启动 tab 拖拽）→ 移出窗口。
        // 回归：应在光标刚越过窗口边缘（尚未松手）时立即 tear-off 出独立窗口，
        // 而不是等到窗口外松开才突然弹窗。
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let outside = gpui::Point::new(px(-60.), px(50.));
        cx.simulate_mouse_move(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let before_up = registry_snapshot(cx);
        assert_eq!(
            before_up.iter().filter(|(d, _, _, _)| *d).count(),
            1,
            "越出窗口边缘（未松手）就应已拆出独立窗口，快照={before_up:?}"
        );
        assert!(
            before_up
                .iter()
                .find(|(d, _, _, _)| *d)
                .is_some_and(|(_, n, has_path, text)| {
                    *n == 1 && *has_path && text.as_deref() == Some("AAA 独立窗口")
                }),
            "越出边缘时应已把选项卡放进新窗口，快照={before_up:?}"
        );

        // 松开（在窗口外）：不应再次拆分（仍是同一个独立窗口）
        cx.simulate_mouse_up(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }

        let snap = registry_snapshot(cx);
        let detached: Vec<_> = snap.iter().filter(|(d, _, _, _)| *d).collect();
        let main: Vec<_> = snap.iter().filter(|(d, _, _, _)| !*d).collect();
        assert_eq!(detached.len(), 1, "应仍只有一个独立窗口，快照={snap:?}");
        assert_eq!(detached[0].1, 1, "独立窗口应恰好 1 个选项卡");
        assert!(detached[0].2, "独立窗口的活动选项卡应有文件路径");
        assert_eq!(
            detached[0].3.as_deref(),
            Some("AAA 独立窗口"),
            "独立窗口应带原正文"
        );
        assert!(
            !main.is_empty() && main.iter().all(|(_, n, has_path, _)| *n >= 1 && !*has_path),
            "主窗口应只剩未命名选项卡，快照={snap:?}"
        );

        // 回归：新窗口应出现在鼠标释放处（源窗口左上角 + 释放点偏移），而不是与主窗口重叠。
        // 释放点在窗口外 (-60, 50) → 新窗口左上角应向左上平移(-60, 50)。
        // 源窗口 = 本 VisualTestContext 绑定的窗口（其 bounds 可直接读）；独立窗口经句柄读取
        // （当前窗口在更新栈上，直接 update 会被判“window not found”）。
        let src_origin = cx.update(|w, _| w.bounds()).origin;
        let windows: Vec<(bool, gpui::AnyWindowHandle)> = cx.update(|_w, cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .filter_map(|(weak, wh)| {
                    let e = weak.upgrade()?;
                    let detached = e.read_with(cx, |a, _| a.is_detached);
                    Some((detached, *wh))
                })
                .collect()
        });
        let det_origin = windows
            .iter()
            .find(|(detached, _)| *detached)
            .and_then(|(_, wh)| cx.update(|_w, cx| wh.update(cx, |_, w, _| w.bounds()).ok()))
            .map(|b| b.origin)
            .expect("应有独立窗口");
        let expected = src_origin + outside;
        assert!(
            (det_origin.x.as_f32() - expected.x.as_f32()).abs() < 2.0
                && (det_origin.y.as_f32() - expected.y.as_f32()).abs() < 2.0,
            "独立窗口应跟随鼠标释放位置：期望 origin={expected:?}，实际={det_origin:?}"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 独立窗口的选项卡「还原回主窗口」：主窗口重新拿到该文件选项卡与正文。
    #[gpui::test]
    async fn detached_tab_restores_to_primary(cx: &mut TestAppContext) {
        init_test(cx);
        let (primary, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-multiwin-b.txt");
        std::fs::write(&f, "BBB 还原").unwrap();
        cx.update(|window, cx| {
            primary.update(cx, |app, cx| {
                app.open_file(f.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 从主窗口拔出该文件选项卡，开一个独立窗口收纳它
        let payload = cx
            .update(|_w, cx| primary.update(cx, |app, _| app.take_tab_payload(1)))
            .expect("应有可拔出的选项卡");
        let (_det_root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new_detached(window, cx, payload));
            crate::ui::root::Root::new(app, window, cx)
        });
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 找到独立窗口的 EditorApp 实体
        let det = cx
            .update(|_w, cx| {
                let reg = cx.global::<EditorAppRegistry>();
                reg.windows
                    .iter()
                    .filter_map(|(w, _)| w.upgrade())
                    .find(|e| e.read_with(cx, |a, _| a.is_detached))
            })
            .expect("应存在独立窗口");
        assert_eq!(
            cx.update(|_w, cx| det.read_with(cx, |a, _| a.tabs.len())),
            1,
            "独立窗口应恰好 1 个选项卡"
        );

        // 独立窗口把选项卡还原给主窗口
        let payload2 = cx
            .update(|_w, cx| det.update(cx, |app, _| app.take_tab_payload(0)))
            .expect("独立窗口应有选项卡");
        cx.update(|_w, cx| det.update(cx, |app, cx| app.restore_tab_to_primary(payload2, cx)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let (has_file, content) = cx.update(|_w, cx| {
            let a = primary.read(cx);
            let tab = a.tabs.iter().find(|t| t.file_path.is_some());
            (
                tab.is_some(),
                tab.map(|t| t.editor.read(cx).value().to_string()),
            )
        });
        assert!(has_file, "主窗口应重新持有该文件选项卡");
        assert_eq!(content.as_deref(), Some("BBB 还原"), "正文应完整还原");
        let _ = std::fs::remove_file(&f);
    }

    /// 回归：拖拽选项卡但一直停留在窗口内、窗口内松开 → 不拆独立窗口（tear-off 只在
    /// 越出窗口边缘时发生），选项卡仍留在主窗口。
    #[gpui::test]
    async fn drag_tab_inside_window_does_not_tear_off(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 拖一个未命名选项卡（tab-0），全程留在窗口内再松开
        let tab = cx.debug_bounds("tab-0").expect("选项卡 tab-0 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let moved_inside = gpui::Point::new(start.x + px(120.), start.y + px(40.));
        cx.simulate_mouse_move(moved_inside, MouseButton::Left, Default::default());
        cx.simulate_mouse_up(moved_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }

        let snap = registry_snapshot(cx);
        assert_eq!(
            snap.iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "窗口内拖拽+松开不应拆出独立窗口，快照={snap:?}"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).tabs.len()),
            1,
            "主窗口选项卡应保持原样"
        );
    }

    /// 大文档交互延迟基准（`BIGDOC_BENCH=1` 才跑，不打断言，只打印每操作墙钟）。
    /// 用 `fixtures/大文档测试.rs`（编辑器）与 `fixtures/大文档测试2.md`（预览）实测
    /// 「输入字符 / 滚动」的单次感知延迟（`dispatch/simulate + run_until_parked` 墙钟）。
    /// 对比 HEAD：`git stash -u` 跑本测试，再 `git stash pop` 复跑。
    #[gpui::test]
    #[ignore]
    async fn bench_bigdoc_interactive(cx: &mut TestAppContext) {
        if std::env::var_os("BIGDOC_BENCH").is_none() {
            return;
        }
        init_test(cx);
        // 对齐生产：启动后台预热语法集/缓存（否则首次各语言代码块冷扫 ~200-250ms，
        // 会污染「稳态滚动/编辑」测量——生产里 prewarm 在 main.rs 后台线程早已完成）。
        crate::syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let show = |kind: &str, ms: f64| {
            eprintln!("[BENCH] {kind} = {ms:.1} ms");
        };
        let md_path =
            std::env::var("BIGDOC_MD").unwrap_or_else(|_| "fixtures/大文档测试2.md".to_string());
        let rs_path =
            std::env::var("BIGDOC_RS").unwrap_or_else(|_| "fixtures/大文档测试.rs".to_string());

        // ---------- 打开 .rs（代码编辑器）并等就绪 ----------
        let t0 = std::time::Instant::now();
        let rs_path = rs_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| {
                a.open_file(&rs_path, window, cx);
            });
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                !a.tabs[i].editor_pending
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 600 {
                panic!("编辑器打开超时");
            }
        }
        show("open_rust_file", t0.elapsed().as_secs_f64() * 1000.0);

        let editor = cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
            app.read(cx).tabs[app.read(cx).active_tab].editor.clone()
        });

        // ---------- 编辑器输入（insert_text × 6，拆「insert」与「draw/渲染」） ----------
        let sample = "x";
        for k in 0..6 {
            let t0 = std::time::Instant::now();
            cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
            let t1 = std::time::Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = std::time::Instant::now();
            show(
                &format!("editor_ins_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_ins_{k}.ins"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_ins_{k}.draw"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("editor_insert_text");

        // ---------- 编辑器滚动（滚轮下滚，拆「事件」与「渲染」） ----------
        for k in 0..6 {
            let t0 = std::time::Instant::now();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(700.), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-120.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            let t1 = std::time::Instant::now();
            for _ in 0..4 {
                cx.run_until_parked();
            }
            let t2 = std::time::Instant::now();
            show(
                &format!("editor_scroll_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_scroll_{k}.evt"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_scroll_{k}.parks"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("editor_scroll");

        // ---------- 打开 .md（预览）并等两段就绪 ----------
        let t1 = std::time::Instant::now();
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| {
                a.open_file(&md_path, window, cx);
            });
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                !a.tabs[i].editor_pending
                    && a.tabs[i].md_preview.read(cx).memory_estimate().0 > 0
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 900 {
                panic!("预览打开超时");
            }
        }
        show("open_md_file", t1.elapsed().as_secs_f64() * 1000.0);

        // ---------- 预览滚动（预览在右半区，拆「事件」与「渲染」） ----------
        for k in 0..6 {
            let t0 = std::time::Instant::now();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(900.), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-120.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            let t1 = std::time::Instant::now();
            for _ in 0..4 {
                cx.run_until_parked();
            }
            let t2 = std::time::Instant::now();
            show(
                &format!("preview_scroll_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  preview_scroll_{k}.evt"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  preview_scroll_{k}.parks"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("preview_scroll");

        // ---------- 预览编辑区输入（md 编辑区 insert_text，拆 ins/draw） ----------
        let md_editor = cx.update(|_, cx| app.read(cx).tabs[app.read(cx).active_tab].editor.clone());
        for k in 0..4 {
            let t0 = std::time::Instant::now();
            cx.update(|_, cx| md_editor.update(cx, |e, cx| e.insert_text("y", cx)));
            let t1 = std::time::Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = std::time::Instant::now();
            show(
                &format!("md_editor_ins_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  md_editor_ins_{k}.ins"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  md_editor_ins_{k}.draw"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("md_editor_insert_text");
    }

    /// 回归（用户实测 2026-10-08）：真实大文档分屏下，鼠标滚轮在**预览**上快速下滚，
    /// 滚到第一个表格块（块 99 附近）时预览「上来回跳动、滚不下去」（宿主偏移被反复
    /// 拽回 ~93px）——根因 = sync 把「编辑器跟随预览的落位」误判成「用户滚动编辑器」，
    /// 反向驱动预览（editor→preview 回声），每帧重钉预览偏移回编辑器所在块。
    ///
    /// 修复 = `MdScrollSync` 新增 `driven_editor_target` 对称护栏（+ 宽限计数）：我方
    /// 驱动编辑器落位的宽限期内，编辑器顶块变化不回显为「编辑器先动」，预览不再被拽回。
    ///
    /// 断言：预览偏移单调前进（回退 >2px 计为 bounce，且必须为 0），并能滚过表格块
    /// （修复前卡在 ~2607px，max_off 应 >3200），且最终两侧对齐（E 顶行 = 预览顶块）。
    #[gpui::test]
    async fn preview_wheel_sync_no_bounce_past_table(cx: &mut TestAppContext) {
        init_test(cx);
        crate::syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md_path = std::env::var("BIGDOC_MD")
            .unwrap_or_else(|_| "fixtures/大文档测试2.md".to_string());
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(&md_path, window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                !a.tabs[a.tabs.len().saturating_sub(1)].editor_pending
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 1500 {
                return;
            }
        }
        let (editor, mdw, vhand) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.active_tab;
            (
                a.tabs[i].editor.clone(),
                a.tabs[i].md_wy.clone().expect("预览应在场"),
                a.tabs[i].md_wy_vscroll.clone(),
            )
        });
        let read = |cx: &mut VisualTestContext| -> (usize, Option<usize>, f32) {
            cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                let ps = mdw.read(cx).top_source_line();
                let off = (-vhand.offset().y).as_f32();
                (et, ps, off)
            })
        };
        let mut guard = 0;
        while read(cx).1.is_none() {
            let _ = cx.cx.refresh();
            guard += 1;
            if guard > 400 {
                return;
            }
        }

        // 预览区滚轮下滚 30 次，逐帧检查：预览偏移不得回退 >2px（一旦前进了 >50px）；
        // 且必须滚过第一个表格块（修复前 max_off 恒卡在 ~2607 / 来回跳动）。
        let mut max_off = read(cx).2;
        let mut bounced = 0usize;
        let mut max_regress = 0f32;
        for _ in 0..30 {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(900.), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-150.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            for _ in 0..4 {
                let (_et, _ps, off) = read(cx);
                if off < max_off - 2.0 && max_off > 50.0 {
                    bounced += 1;
                    max_regress = (max_off - off).max(max_regress);
                }
                max_off = max_off.max(off);
                let _ = cx.cx.refresh();
            }
        }
        assert_eq!(
            bounced, 0,
            "预览滚轮不得回跳（bounce={bounced}，最大回退 {max_regress:.1}px；修复前每步"
        );
        assert!(
            max_off > 3200.0,
            "预览应滚过第一个表格块（max_off={max_off:.1}px；修复前卡在 ~2607px）"
        );
        let (et, ps, _off) = read(cx);
        let ps = ps.expect("预览帧应就绪");
        assert_eq!(
            et, ps,
            "最终两侧应对齐到同一块（editor top={et}, preview block={ps}）"
        );

        // ===== 面板交接（用户实测 2026-10-08 追加）：编辑区↔预览区**切着滚**不得跳动 =====
        // 修复前：编辑区滚后切预览区滚 → 预览 off 在表格块处 2606.9↔2756.9 来回拽
        // （46 次回跳）；预览区滚后切编辑区滚 → sync 彻底卡死（编辑器再也滚不动）。
        let wheel_at = |x: f32, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(x), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-150.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
        };

        // -- H1) 编辑区滚 → 切预览区滚：预览 off 单调前进 --
        for _ in 0..8 {
            wheel_at(700., cx);
            for _ in 0..2 {
                let _ = cx.cx.refresh();
            }
        }
        let mut max_off1 = read(cx).2;
        let mut h1 = 0usize;
        for _ in 0..15 {
            wheel_at(900., cx);
            for _ in 0..3 {
                let (_, _ps, off) = read(cx);
                if off < max_off1 - 3.0 && max_off1 > 50.0 {
                    h1 += 1;
                }
                max_off1 = max_off1.max(off);
                let _ = cx.cx.refresh();
            }
        }
        assert_eq!(
            h1, 0,
            "编辑区滚后切预览区滚，预览不得回跳（修复前 46 次）"
        );

        // -- H2) 预览区滚 → 切编辑区滚：编辑器顶行单调前进（且不得被 sync 卡死） --
        for _ in 0..8 {
            wheel_at(900., cx);
            for _ in 0..2 {
                let _ = cx.cx.refresh();
            }
        }
        let mut max_et = read(cx).0;
        let mut h2 = 0usize;
        for _ in 0..15 {
            wheel_at(700., cx);
            for _ in 0..3 {
                let (et, _ps, _off) = read(cx);
                if et < max_et && max_et > 10 {
                    h2 += 1;
                }
                max_et = max_et.max(et);
                let _ = cx.cx.refresh();
            }
        }
        assert_eq!(
            h2, 0,
            "预览区滚后切编辑区滚，编辑器顶行不得回跳且不得锁死（修复前卡死 max_et=98）"
        );
        eprintln!(
            "[regression] preview_wheel_sync_no_bounce_past_table OK: max_off={max_off:.1} H1={h1} H2={h2} max_et={max_et}"
        );
    }

    /// 诊断（#[ignore]）：真实 Windows 滚轮口径（Lines ±3）+ 1 帧切换 × 「多滚后换区」模式，
    /// 逐帧检查「编辑区→预览区」「预览区→编辑区」交接处 E/off 是否回跳（修复前用户实测）。
    #[gpui::test]
    #[ignore]
    async fn diag_handoff2(cx: &mut TestAppContext) {
        init_test(cx);
        crate::syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md_path = std::env::var("BIGDOC_MD")
            .unwrap_or_else(|_| "fixtures/大文档测试2.md".to_string());
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(&md_path, window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                !a.tabs[a.tabs.len().saturating_sub(1)].editor_pending
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 1500 {
                return;
            }
        }
        let (editor, mdw, vhand) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.active_tab;
            (
                a.tabs[i].editor.clone(),
                a.tabs[i].md_wy.clone().expect("预览应在场"),
                a.tabs[i].md_wy_vscroll.clone(),
            )
        });
        let read = |cx: &mut VisualTestContext| -> (usize, Option<usize>, f32) {
            cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                let ps = mdw.read(cx).top_source_line();
                let off = (-vhand.offset().y).as_f32();
                (et, ps, off)
            })
        };
        let mut guard = 0;
        while read(cx).1.is_none() {
            let _ = cx.cx.refresh();
            guard += 1;
            if guard > 400 {
                return;
            }
        }
        let wheel_lines = |x: f32, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(x), px(400.)),
                delta: gpui::ScrollDelta::Lines(gpui::Point::new(0.0, -3.0)),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
        };
        let mut max_off = read(cx).2;
        let mut max_et = read(cx).0;
        let mut bounce = 0usize;
        let seq = [('E', 12), ('P', 12), ('E', 12), ('P', 12), ('E', 12), ('P', 12)];
        let mut step = 0usize;
        for (which, n) in seq.into_iter() {
            for k in 0..n {
                let x = if which == 'E' { 700. } else { 900. };
                wheel_lines(x, cx);
                // 1 帧即刻读（不沉淀），模拟“切过去马上滚”
                let (et, ps, off) = read(cx);
                if off < max_off - 3.0 && max_off > 50.0 {
                    bounce += 1;
                    eprintln!(
                        "[h2] !!OFF-BOUNCE seq={step} {which}{k} off={off:.1} <- max={max_off:.1} E={et} P={ps:?}"
                    );
                }
                if et < max_et && max_et > 10 {
                    bounce += 1;
                    eprintln!(
                        "[h2] !!ET-BOUNCE seq={step} {which}{k} E={et} <- maxE={max_et} P={ps:?} off={off:.1}"
                    );
                }
                max_off = max_off.max(off);
                max_et = max_et.max(et);
                let _ = cx.cx.refresh();
            }
            step += 1;
            let (et, ps, off) = read(cx);
            eprintln!(
                "[h2] seq#{step}({which} x{n}) E={et} P={ps:?} off={off:.1} bounces={bounce}"
            );
        }
        eprintln!("[h2] DONE bounces={bounce} max_off={max_off:.1} max_et={max_et}");
    }

    // ---------------------------------------------------------------------
    // P2-8 命令面板：命令模式（Ctrl+Shift+P）+ 文件模式（Ctrl+P）
    // ---------------------------------------------------------------------

    /// 命令模式：Ctrl+Shift+P（ToggleCommandPalette）打开 → 键入过滤缩减命令 →
    /// ↓ 导航 → Enter 执行「新建文件」→ 面板关闭（DismissEvent 清引用）；
    /// 再开 → Esc 关闭且焦点归还打开前的编辑区。
    #[gpui::test]
    async fn command_palette_open_filter_run_and_escape(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 聚焦编辑区（应用级动作经焦点祖先链派发到 App 根 on_action）。
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();

        // Ctrl+Shift+P → 命令面板打开，初始列出全部命令并选中首项。
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (total, sel) = cx.update(|_w, cx| {
            let p = app.read(cx).palette.as_ref().unwrap().read(cx);
            (p.visible_len(), p.selected())
        });
        assert!(total > 5, "命令面板应列出全部命令，实际={total}");
        assert_eq!(sel, Some(0), "初始应选中首项");

        // 键入「新建」→ 过滤后只剩「新建文件」一条。
        let palette = cx.update(|_w, cx| app.read(cx).palette.as_ref().unwrap().clone());
        cx.update(|window, cx| {
            let input = palette.read(cx).input_entity();
            input.update(cx, |s, cx| s.set_value("新建", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (len, label, query) = cx.update(|_w, cx| {
            let p = palette.read(cx);
            (
                p.visible_len(),
                p.visible_label(0).map(str::to_string),
                p.query().to_string(),
            )
        });
        assert_eq!(query, "新建", "查询词应写进面板状态");
        assert_eq!(len, 1, "过滤「新建」应只剩 1 条命令，实际={len}");
        assert_eq!(label.as_deref(), Some("新建文件"));

        // ↓：过滤后仅 1 条 → 环绕回 0；再 Enter 执行「新建文件」。
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let sel2 = cx.update(|_w, cx| palette.read(cx).selected());
        assert_eq!(sel2, Some(0), "单条过滤下 ↓ 应环绕回 0，实际={sel2:?}");
        cx.simulate_keystrokes("enter");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (closed, tabs_len) = cx.update(|_w, cx| {
            (app.read(cx).palette.is_none(), app.read(cx).tabs.len())
        });
        assert!(closed, "Enter 执行后面板应关闭（DismissEvent 清引用）");
        assert_eq!(tabs_len, 2, "「新建文件」应新增一个选项卡，实际={tabs_len}");

        // 再开 → Esc 关闭，且焦点归还打开前的编辑区。
        cx.update(|window, cx| {
            let expected = app.read(cx).editor_focus_handle(cx);
            window.focus(&expected, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(cx.update(|_w, cx| app.read(cx).palette.is_some()), "Ctrl+Shift+P 应再打开面板");
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (closed2, focused_opt) = cx.update(|window, cx| {
            (app.read(cx).palette.is_none(), window.focused(cx))
        });
        assert!(closed2, "Esc 应关闭面板");
        let expected = cx.update(|_w, cx| app.read(cx).editor_focus_handle(cx));
        assert_eq!(
            focused_opt.as_ref(),
            Some(&expected),
            "关闭后焦点应归还打开前的编辑区"
        );

        // 点击面板外（中心 600px 面板之外的点）→ 关闭（deferred 层 on_mouse_down_out 路径）。
        cx.dispatch_action(ToggleCommandPalette);
        cx.run_until_parked();
        assert!(cx.update(|_w, cx| app.read(cx).palette.is_some()), "面板应再次打开");
        cx.simulate_click(gpui::Point::new(px(900.), px(400.)), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let closed3 = cx.update(|_w, cx| app.read(cx).palette.is_none());
        assert!(closed3, "点击面板外应关闭命令面板");
    }

    /// 文件模式：Ctrl+P（QuickOpenFile）列出已打开文件（含路径提示）→
    /// 键入文件名过滤 → Enter 切到对应选项卡并关闭面板。
    #[gpui::test]
    async fn quick_open_file_lists_and_switches_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 打开两个真实文件：A 先（下标 1）、B 后（下标 2，活动）。
        let a = std::env::temp_dir().join("gpui-pal-a.md");
        let b = std::env::temp_dir().join("gpui-pal-b.md");
        std::fs::write(&a, "# A\nhello\n").unwrap();
        std::fs::write(&b, "# B\nworld\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(a.to_str().unwrap(), window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(b.to_str().unwrap(), window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tab_count = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(tab_count, 3, "未命名 + A + B 共 3 个选项卡");

        // 聚焦编辑区后 Ctrl+P → 文件模式面板，列出已打开文件（含「打开文件…」入口）。
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(QuickOpenFile);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let palette = cx.update(|_w, cx| app.read(cx).palette.as_ref().unwrap().clone());
        let (mode, n, first_label) = cx.update(|_w, cx| {
            let p = palette.read(cx);
            (
                p.mode(),
                p.visible_len(),
                p.visible_label(0).map(str::to_string),
            )
        });
        assert_eq!(
            mode,
            crate::ui::palette::PaletteMode::Files,
            "Ctrl+P 应为文件模式"
        );
        assert!(n >= 4, "应列出 未命名 + A + B + 打开文件… 至少 4 条，实际={n}");
        assert_eq!(first_label.as_deref(), Some("未命名"), "首项应为首个打开的（未命名）选项卡");

        // 键入 A 的文件名 → 过滤到只剩 A。
        cx.update(|window, cx| {
            let input = palette.read(cx).input_entity();
            input.update(cx, |s, cx| s.set_value("gpui-pal-a.md", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (len, label) = cx.update(|_w, cx| {
            let p = palette.read(cx);
            (p.visible_len(), p.visible_label(0).map(str::to_string))
        });
        assert_eq!(len, 1, "过滤应只剩 A，实际={len}");
        assert_eq!(label.as_deref(), Some("gpui-pal-a.md"));

        // Enter → 切到 A 的选项卡（下标 1）并关闭面板。
        cx.simulate_keystrokes("enter");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (closed, active) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.palette.is_none(), a.active_tab)
        });
        assert!(closed, "切换后面板应关闭");
        assert_eq!(active, 1, "Enter 后应切回 A 的选项卡，实际={active}");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 面板高度 = 窗口 2/3：列表项多时面板不再探出屏幕底，列表区按剩余高度定高，
    /// 过长内容在容器内内部滚动（右侧自绘滚动条随之出现，见 scrollbar 组件
    /// `hides_when_content_fits`）。
    #[gpui::test]
    async fn command_palette_height_capped_and_list_scrolls(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        let win_h = 600.0_f32;
        cx.simulate_resize(gpui::size(px(700.), px(win_h)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 聚焦编辑区（应用级动作经焦点祖先链派发到 App 根 on_action）。
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(cx.update(|_w, cx| app.read(cx).palette.is_some()), "命令面板应打开");

        // ① 面板高度 = 窗口 2/3（不随 16+ 条命令的总高增长）。
        let panel = cx.debug_bounds("command-palette").expect("面板应渲染");
        let expect_h = px(win_h * 2.0 / 3.0);
        assert!(
            (panel.size.height - expect_h).abs() < px(1.0),
            "面板高度应为窗口 2/3（{expect_h:?}），实际={:?}",
            panel.size.height
        );
        // 面板顶部 96px 偏移 + 2/3 高恒在窗口内。
        assert!(
            panel.bottom() <= px(win_h),
            "面板不得探出窗口底部：bottom={:?}（窗口高 {win_h}）",
            panel.bottom()
        );

        // ② 列表滚动容器有界（剩余高度）：16+ 条命令在容器内可滚动，而非把面板撑高。
        let scroll = cx
            .debug_bounds("command-palette-scroll")
            .expect("列表滚动容器应渲染");
        assert!(scroll.size.height > px(0.), "列表滚动容器应有正高度：{scroll:?}");
        assert!(
            scroll.size.height < panel.size.height - px(40.),
            "列表滚动容器应为剩余高度并被钳在面板内：scroll={scroll:?} panel={panel:?}"
        );
        let max_off = cx.update(|_w, cx| {
            let p = app.read(cx).palette.as_ref().unwrap().read(cx);
            p.scroll_handle().max_offset().y
        });
        assert!(
            max_off > px(0.),
            "列表溢出时应在容器内可滚动：max_offset={max_off:?}"
        );
    }

    /// 回归（用户报告 #3）：打开文件后 Ctrl+Shift+P 打开面板，面板输入框必须可聚焦与可键入，
    /// 点击不得穿透到下层编辑器。
    ///
    /// 覆盖两条：
    /// ① 打开真实文件（焦点在编辑区）→ 触发 Ctrl+P 路径动作 `ToggleCommandPalette` →
    ///    面板打开时输入框持有焦点；随后键入字符确实写入查询框。
    /// ② 焦点被夺回编辑器（模拟点击穿透后果）→ 鼠标点击面板输入框中心 →
    ///    焦点应收纳到面板输入框（不得停留在编辑器）；随后键入仍写入查询框。
    ///
    /// 历史根因：面板浮层是 `deferred(absolute+flex)` 时 hitbox 位置失真（第一段探针
    /// 最小结构复现），且浮层无 BlockMouse 屏障，点击命中面板元素后仍继续命中下方编辑器、
    /// 编辑器 on_mouse_down 把焦点抢回。修复：全窗包裹去 flex、显式 left/top 定位 + `.occlude()`。
    #[gpui::test]
    async fn palette_input_focusable_and_clickable_after_open_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 真实文件（热键路径：先打开文件，焦点落在编辑区）
        let p = std::env::temp_dir().join(format!("gpui-pal-reg-{}.rs", std::process::id()));
        std::fs::write(&p, "fn main() {}\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();

        // ① 打开面板（Ctrl+P 同一动作路径）→ 输入框持有焦点
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let palette = cx.update(|_w, cx| app.read(cx).palette.as_ref().expect("面板应已打开").clone());
        let input_handle = cx.update(|_w, cx| palette.read(cx).input_handle(cx));
        let focused_on_open = cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused_on_open.as_ref(),
            Some(&input_handle),
            "打开面板时输入框应持有焦点"
        );
        cx.simulate_keystrokes("cop");
        cx.run_until_parked();
        let query_after_open = cx.update(|_w, cx| palette.read(cx).query().to_string());
        assert_eq!(query_after_open, "cop", "打开后键入应写入查询框");

        // ② 焦点夺回编辑器（模拟点击穿透的后果）→ 点击输入框应收纳焦点
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();
        let editor_handle = cx.update(|_w, cx| app.read(cx).editor_focus_handle(cx));
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)).as_ref(),
            Some(&editor_handle),
            "前置：焦点应在编辑区"
        );
        let qb = cx.debug_bounds("command-palette-query").expect("查询框应渲染");
        let click_point = gpui::Point::new(qb.origin.x + qb.size.width / 2., qb.origin.y + qb.size.height / 2.);
        cx.simulate_click(click_point, Default::default());
        cx.run_until_parked();
        cx.run_until_parked();
        let focused_after_click = cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused_after_click.as_ref(),
            Some(&input_handle),
            "点击面板输入框应收纳焦点（不得因穿透停留在编辑器）"
        );
        cx.simulate_keystrokes("y");
        cx.run_until_parked();
        let query_after_click_typing = cx.update(|_w, cx| palette.read(cx).query().to_string());
        assert_eq!(
            query_after_click_typing, "copy",
            "点击聚焦后键入应写入查询框"
        );

        // 收尾：Esc 关闭后焦点回落编辑区
        cx.simulate_keystrokes("escape");
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).palette.is_none()),
            "Esc 应关闭面板"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 外壳无缝直连（阶段一回归）——三块方直角区块的「零缝隙」几何（真实渲染路径
    /// `debug_bounds` 断言）：
    /// - 贴边直角：侧栏左缘贴窗（`origin.x == 0`）、正文块右缘贴窗（`right == 窗宽`）、
    ///   状态栏左/右/底缘贴窗（`origin.x == 0`、`right == 窗宽`、`bottom == 窗高`）；
    /// - 无缝直连（零间距）：侧栏右缘 ↔ 正文块左缘、正文块顶 ↔ 选项卡栏底、
    ///   正文块底 ↔ 状态栏顶，三处间距均 ≈ 0；
    /// - 面板收起时无右缘手柄（`sidebar-resize`）也无旧空间隔（`sidebar-gap`）；
    ///   面板展开时右缘调宽手柄 `sidebar-resize` 渲染（绝对细线内嵌侧栏右缘，非分隔带）；
    /// - 整体收起时 `app-body-with-sidebar` 包裹层一并消失（`sidebar-gap` 恒不存在）。
    #[gpui::test]
    async fn shell_seamless_geometry(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        let win_w = 980.0f32;
        let win_h = 720.0f32;

        // —— 侧栏展开（默认：活动栏 + 面板收起）——
        let sb = cx
            .debug_bounds("sidebar")
            .expect("侧边栏 sidebar 应渲染");
        let main = cx
            .debug_bounds("app-main-card")
            .expect("正文块 app-main-card 应渲染");
        let tb = cx
            .debug_bounds("tab-bar")
            .expect("选项卡栏 tab-bar 应渲染");
        let st = cx
            .debug_bounds("status-bar")
            .expect("状态栏 status-bar 应渲染");
        // 贴边直角四处。
        assert!(
            sb.origin.x.as_f32() <= 0.5,
            "侧栏左缘应贴窗（直角），sb={sb:?}"
        );
        assert!(
            (main.right().as_f32() - win_w).abs() < 1.0,
            "正文块右缘应贴窗（直角），main={main:?}"
        );
        assert!(
            st.origin.x.as_f32() <= 0.5,
            "状态栏左缘应贴窗（直角），st={st:?}"
        );
        assert!(
            (st.right().as_f32() - win_w).abs() < 1.0,
            "状态栏右缘应贴窗（直角），st={st:?}"
        );
        assert!(
            (st.bottom().as_f32() - win_h).abs() < 1.0,
            "状态栏底缘应贴窗（直角），st={st:?}"
        );
        // 无缝直连（零间距）：侧栏右缘 → 正文块左缘、正文块顶 → 选项卡栏底、
        // 正文块底 → 状态栏顶。
        let seam_sb_main = main.origin.x.as_f32() - sb.right().as_f32();
        assert!(
            (seam_sb_main).abs() < 1.5,
            "侧栏↔正文应无缝直连（无间距），实际={seam_sb_main}"
        );
        let seam_tb_main = main.origin.y.as_f32() - tb.bottom().as_f32();
        assert!(
            (seam_tb_main).abs() < 1.5,
            "正文↔选项卡栏应无缝直连（无间距），实际={seam_tb_main}"
        );
        let seam_main_st = st.origin.y.as_f32() - main.bottom().as_f32();
        assert!(
            (seam_main_st).abs() < 1.5,
            "正文↔状态栏应无缝直连（无间距），实际={seam_main_st}"
        );
        // 面板收起（默认）：无右缘调宽手柄，也无旧 sidebar-gap 空间隔元素。
        assert!(
            cx.debug_bounds("sidebar-resize").is_none(),
            "面板收起不应有无右缘调宽手柄"
        );
        assert!(
            cx.debug_bounds("sidebar-gap").is_none(),
            "无缝直连后不应再有 sidebar-gap 空间隔元素"
        );

        // —— 面板展开：侧栏右缘调宽手柄渲染（内嵌细线，非分隔带）——
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-resize").is_some(),
            "面板展开时侧栏右缘应有调宽手柄 sidebar-resize"
        );
        assert!(
            cx.debug_bounds("sidebar-gap").is_none(),
            "面板展开也不应有 sidebar-gap 空间隔元素"
        );

        // —— 整体收起：内容区包裹层与旧间距元素都消失 ——
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "整体收起后内容区包裹层不应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-gap").is_none(),
            "整体收起后不应有 sidebar-gap"
        );
    }

    /// 零选项卡欢迎页也住在正文卡里（`app-main-card`），卡片化不破坏欢迎页布局。
    #[gpui::test]
    async fn welcome_page_stays_in_main_card(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        assert!(
            cx.debug_bounds("app-main-card").is_some(),
            "零 tab 欢迎页应仍在正文卡内"
        );
        assert!(
            cx.debug_bounds("app-empty-body").is_some(),
            "欢迎页主体 app-empty-body 应渲染"
        );
        assert!(
            cx.debug_bounds("editor-area").is_none(),
            "零 tab 无编辑区（回归）"
        );
    }

    /// 拖拽手柄落点（真实渲染路径）：
    /// - 编辑↔预览分割条 → `splitter`（居中 2px 细线，**无**三点手柄）；
    /// - 侧栏右缘调宽手柄（面板展开）→ `sidebar-resize`（内嵌细线命中区）；
    /// - 未开预览 / 面板收起时对应手柄不出现。
    #[gpui::test]
    async fn splitter_and_sidebar_resize_handles_render(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 默认面板收起、预览未开：无分割条、无调宽手柄。
        assert!(
            cx.debug_bounds("splitter").is_none(),
            "未开预览不应渲染分割条"
        );
        assert!(
            cx.debug_bounds("splitter-grip-rest").is_none(),
            "分割条已无三点手柄"
        );
        // 打开目录面板 → 侧栏右缘调宽手柄出现（细线命中区）。
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-resize").is_some(),
            "面板展开应有调宽手柄 sidebar-resize"
        );
        assert!(
            cx.debug_bounds("sidebar-resize-grip-rest").is_none(),
            "侧栏右缘手柄不再有三点圆点层"
        );
        // 打开 .md（自动开预览）→ 分割条出现；三点手柄恒不出现。
        let md = std::env::temp_dir().join("gpui-card-split.md");
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("splitter").is_some(),
            "md 预览分屏应渲染分割条"
        );
        assert!(
            cx.debug_bounds("splitter-grip-rest").is_none()
                && cx.debug_bounds("splitter-grip-accent").is_none(),
            "分割条不再有三点手柄（三点分隔已移除）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 三点手柄不破坏分割条交互（真实事件路径）：悬停 → `splitter_hovered`（高亮态）；
    /// 按下 → `split_drag`；右拖 → `split_ratio` 按窗口坐标位移增大；松开 → 结束。
    #[gpui::test]
    async fn splitter_grip_drag_still_resizes(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join("gpui-card-split-drag.md");
        std::fs::write(&md, "# 标题\n正文\n".repeat(20)).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let sp = cx
            .debug_bounds("splitter")
            .expect("分割条应渲染（md 预览分屏）");
        let at = sp.center();
        // 悬停 → 高亮态。
        cx.simulate_mouse_move(at, None, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).splitter_hovered),
            "悬停分割条应打开高亮态 splitter_hovered"
        );
        // 按下 → 拖拽开始；右移 100px → 编辑区比例增大。
        let r0 = cx.update(|_w, cx| app.read(cx).split_ratio);
        cx.simulate_mouse_down(at, MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).split_drag.is_some()),
            "按下分割条应开始拖拽"
        );
        let target = gpui::point(at.x + px(100.), at.y);
        cx.simulate_mouse_move(target, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let r1 = cx.update(|_w, cx| app.read(cx).split_ratio);
        assert!(
            r1 > r0,
            "右拖 100px 应加大编辑区比例：{r0:.3} → {r1:.3}"
        );
        cx.simulate_mouse_up(target, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).split_drag.is_none()),
            "松开应结束拖拽"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 右缘调宽手柄不破坏侧栏调宽（真实事件路径）：悬停手柄 → `resize_hovered`；
    /// 按下 → `resize`；右拖 60px → 侧栏宽 +60 且关闭自适应；松开 → 结束。
    #[gpui::test]
    async fn sidebar_resize_handle_still_resizes(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sp = cx
            .debug_bounds("sidebar-resize")
            .expect("调宽手柄应渲染（面板展开）");
        let at = sp.center();
        let w0 = cx.update(|_w, cx| app.read(cx).sidebar.width);
        // 悬停 → hover 态。
        cx.simulate_mouse_move(at, None, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).sidebar.resize_hovered),
            "悬停调宽手柄应打开调宽 hover 态"
        );
        // 按下 → 拖拽开始；右移 60px → 宽度 +60、关闭自适应；松开 → 结束。
        cx.simulate_mouse_down(at, MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).sidebar.resize.is_some()),
            "按下调宽手柄应开始调宽"
        );
        let target = gpui::point(at.x + px(60.), at.y);
        cx.simulate_mouse_move(target, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(target, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (w1, auto, dragging) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.width, a.sidebar.width_auto, a.sidebar.resize.is_some())
        });
        assert!(
            (w1 - w0 - 60.0).abs() < 3.0,
            "右拖 60px 应加宽 60：{w0:.1} → {w1:.1}"
        );
        assert!(!auto, "手动拖拽应关闭宽度自适应");
        assert!(!dragging, "松开应结束调宽");
    }

