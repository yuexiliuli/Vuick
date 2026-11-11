//! 集成测试共享夹具（`crates/shell/tests/` 各目标顶部 `mod common;` 引入）。
//!
//! 与 `src/app/tests/setup.rs` **同源**（lib 单测仍 include 自家 setup.rs；integration
//! 目标下 shell lib 以非 test 编译，**所有** crate 内私有项不可见，故此处按 extern 可见
//! 面重建一份：helper 全 `pub`，并在 [`init_test`] 里做 R3 运行时注入）。
//!
//! Cargo 只把 `tests/` 顶层 `.rs` 当作测试目标；`tests/common/mod.rs` 不产生额外 target。
//!
//! `#![allow(unused_imports)]`：下方重导出是「按需共享面」（每个目标只用自己那一小撮，
//! integration 是独立二进制、无下游消费者），未消费的重导出在相应目标里会被
//! `unused_imports` 虚警——测试脚手架按宽面供给，此处统一豁免，不属于「新告警」。
//! `#![allow(dead_code)]`：同理，`add_app_zero_tabs`/`settle_frames`/`tab_pdf` 等夹具
//! 只被各自使用它的目标消费，未消费目标会报 dead_code——共享夹具模块按设计豁免。
#![allow(unused_imports)]
#![allow(dead_code)]

use shell::bind_local_keymaps;

/// 测试助手可见面重导出（保测试体与 lib 单测同款裸名访问，断言逐字不改）。
/// `Focusable`/`WyEditor` 分别供 recent（面板焦点句柄）与 bench（桥模型 WyEditor 副本）
/// 裸名使用；`MouseButton`/`px` 供 recent/bench。
pub use shell::EditorApp;
/// `EditorTab`/`MediaReader`：media/pdf/find/goto 域断言经 `tabs[..].reader` 匹配
/// `MediaReader::Pdf/Image/Video` 变体（`media.rs`/`pdf.rs`/`find_goto.rs`，§8.6）。
/// `EditorAppRegistry`：`media.rs` 经 `cx.global::<EditorAppRegistry>()` 读窗口注册表。
/// `MediaKind`：`pdf.rs` `load_media` 装配断言（`MediaKind::Pdf/Image/Video`）。
/// `SidebarView`：`splitter.rs` 切换侧栏视图路径重写（`shell::app::SidebarView`，
/// 原 `crate::app::sidebar::SidebarView` 在 integration 不可达）。
pub use shell::app::{
    EditorAppRegistry, EditorTab, MediaKind, MediaReader, SidebarView,
};
pub use shell::actions::*;
pub use gpui::{
    AppContext, AnyWindowHandle, Entity, Focusable, MouseButton, Pixels, TestAppContext,
    VisualTestContext, px,
};
pub use markdown_wysiwyg::WyEditor;
pub use std::path::PathBuf;

/// `shell::app::session` 私有模块的测试可见面：`save_session_to` / `restore_session_from`
/// 已由 `shell::app` 重导出（见 app/mod.rs「测试可见重导出」），此处经子模块别名复现
/// 既有 `session::xxx(...)` 调用面（测试体逐字不改）。
pub(crate) mod session {
    pub use shell::app::{restore_session_from, save_session_to};
}

/// `shell::app` 的 PDF 书签盘上读写面：`load` / `normalize_key` 已由 `shell::app` 重导出
/// （2026-11-11 起域逻辑迁 shell_pdf、经 `shell::app` facade 保持既有路径）；此处经子模块
/// 别名复现 `tests/pdf.rs` 的书签盘上读出断言（`crate::app::pdf_bookmarks::` 路径在
/// integration 不可达，改 `pdf_bookmarks::` 面）。
pub(crate) mod pdf_bookmarks {
    pub use shell::app::{load, normalize_key};
}

/// 测试前必须初始化 gpui-component（主题、键盘映射等全局状态）。
///
/// **R3**：integration 目标下 shell lib `cfg!(test)` 恒 false；此处先于 gpui App 启动
/// 设 `GPUI_EDITOR_TEST=1`，`AppHost::is_test_build()`（runtime env 判定）为真 →
/// `persist_session` / `persist_prefs` 恒短路不写盘（不污染真实/隔离目录）。
pub fn init_test(cx: &mut TestAppContext) {
    // 复用生产键位组合（`test_support` 双份已删，见 lib.rs）。
    unsafe { std::env::set_var("GPUI_EDITOR_TEST", "1") };
    cx.update(bind_local_keymaps);
}

/// 测试辅助：取选项卡的 PDF 阅读器实体（`MediaReader` 收敛后经 `MediaReader::Pdf` 取）。
/// （本批拆出的域尚未消费它，pdf/epub/media 域后拆；按共享夹具契约保留。）
pub fn tab_pdf(tab: &EditorTab) -> Option<&gpui::Entity<pdf::PdfViewer>> {
    match &tab.reader {
        MediaReader::Pdf(pv) => Some(pv),
        _ => None,
    }
}

/// 测试辅助：取选项卡的图片查看器实体（经 `MediaReader::Image` 取）。
pub fn tab_image(tab: &EditorTab) -> Option<&gpui::Entity<image_viewer::ImageViewer>> {
    match &tab.reader {
        MediaReader::Image(iv) => Some(iv),
        _ => None,
    }
}

/// 测试辅助：取选项卡的视频查看器实体（经 `MediaReader::Video` 取）。
#[cfg(feature = "video")]
pub fn tab_video(tab: &EditorTab) -> Option<&gpui::Entity<video::VideoViewer>> {
    match &tab.reader {
        MediaReader::Video(vv) => Some(vv),
        _ => None,
    }
}

/// 渲染若干帧（等动画/重绘收敛后再读 debug_bounds）。与 `src/app/tests/sidebar.rs`
/// 的 `settle_frames` 同款（该文件仍在 lib 单测，此处按需补到共享夹具）。
pub fn settle_frames(cx: &mut VisualTestContext, n: usize) {
    for _ in 0..n {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
    }
}

/// 创建一个以 `ui::root::Root` 为窗口根视图的编辑应用，
/// 返回视图实体与该窗口的 `VisualTestContext`（后续用其驱动渲染/事件）。
///
/// 新模型下启动为零选项卡（欢迎页），但绝大多数既有测试假定「初始有一个未命名空
/// 选项卡」可直接 `tabs[0]` 键入——此处显式 `make_tab` 补建这个空选项卡（行为与旧的
/// 启动即建 tab 一致）。真正要测「零 tab 欢迎页」的用 [`add_app_zero_tabs`]。
pub fn add_app(cx: &mut TestAppContext) -> (Entity<EditorApp>, &mut VisualTestContext) {
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
pub fn add_app_zero_tabs(cx: &mut TestAppContext) -> (Entity<EditorApp>, &mut VisualTestContext) {
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
        cx.update(|_w, cx| app.read(cx).model.borrow().tabs.is_empty()),
        "零选项卡助手应保持零 tab"
    );
    (app, cx)
}
