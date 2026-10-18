
    use super::*;
    use crate::app::file_ops::MediaKind;
    use crate::actions::{
        MoveDown, NewFile, Paste, QuickOpenFile, Redo, SelectAll, ToggleCommandPalette, Undo,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};

    /// 测试前必须初始化 gpui-component（主题、键盘映射等全局状态）。
    fn init_test(cx: &mut TestAppContext) {
        // 复用生产键位组合（`test_support` 双份已删，见 lib.rs）。
        cx.update(crate::bind_local_keymaps);
    }

    /// 测试辅助：取选项卡的 PDF 阅读器实体（`MediaReader` 收敛后经 `MediaReader::Pdf` 取）。
    fn tab_pdf(tab: &EditorTab) -> Option<&gpui::Entity<pdf::PdfViewer>> {
        match &tab.reader {
            MediaReader::Pdf(pv) => Some(pv),
            _ => None,
        }
    }

    /// 测试辅助：取选项卡的图片查看器实体（经 `MediaReader::Image` 取）。
    fn tab_image(tab: &EditorTab) -> Option<&gpui::Entity<image_viewer::ImageViewer>> {
        match &tab.reader {
            MediaReader::Image(iv) => Some(iv),
            _ => None,
        }
    }

    /// 测试辅助：取选项卡的视频查看器实体（经 `MediaReader::Video` 取）。
    #[cfg(feature = "video")]
    fn tab_video(tab: &EditorTab) -> Option<&gpui::Entity<video::VideoViewer>> {
        match &tab.reader {
            MediaReader::Video(vv) => Some(vv),
            _ => None,
        }
    }

    /// 创建一个以 `ui::root::Root` 为窗口根视图的编辑应用，
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
            ui::root::Root::new(app, window, cx)
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
            ui::root::Root::new(app, window, cx)
        });
        let _ = root;
        let app = holder.borrow().clone().unwrap();
        assert!(
            cx.update(|_w, cx| app.read(cx).tabs.is_empty()),
            "零选项卡助手应保持零 tab"
        );
        (app, cx)
    }

