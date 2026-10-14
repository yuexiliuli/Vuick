//! 程序入口（薄二进制壳）：初始化 GPUI、注册应用级快捷键、创建主窗口，
//! 把窗口根视图包上自研 `ui::root` 视图，主视图实体由 `shell`（app）crate 提供。
//!
//! P5 拆分终态：全部功能逻辑已迁出到 workspace 各 crate（config/base/ime/viewer/
//! syntax/ui/editor/markdown/toc/pdf/image/video/epub/shell），本包只剩初始化胶水。
//! 各模块职责见各 crate（如 `gpui-editor-shell` 的 `EditorApp` / `bind_local_keymaps`）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::{px, size, App, AppContext, Bounds, KeyBinding, WindowBounds, WindowOptions};
use gpui_editor_base::actions::*;
use gpui_editor_shell::{bind_local_keymaps, EditorApp};
use gpui_editor_syntax::prewarm;
use gpui_editor_ui::root;
use gpui_editor_ui::theme::{Theme, ThemeMode};
use gpui_editor_config::user::UserPrefs;

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(980.), px(720.)), cx);

        // 应用级快捷键（全局，None 上下文）。
        cx.bind_keys([
                KeyBinding::new("ctrl-n", NewFile, None),
                KeyBinding::new("ctrl-o", OpenFile, None),
                KeyBinding::new("ctrl-s", Save, None),
                KeyBinding::new("shift-ctrl-s", SaveAs, None),
                KeyBinding::new("ctrl-w", CloseTab, None),
                // P2-8 命令面板：Ctrl+P 改开「打开文件快选」；
                // 原「预览切换」由标签栏眼睛按钮 + 查看菜单承担（菜单快捷键提示已去掉）。
                KeyBinding::new("ctrl-p", QuickOpenFile, None),
                KeyBinding::new("ctrl-shift-p", ToggleCommandPalette, None),
                KeyBinding::new("ctrl-alt-w", ToggleWordWrap, None),
                KeyBinding::new("ctrl-=", IncreaseFontSize, None),
                KeyBinding::new("ctrl--", DecreaseFontSize, None),
                KeyBinding::new("ctrl-0", ResetFontSize, None),
                KeyBinding::new("ctrl-shift-t", FollowSystemTheme, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);

            // 种入自研主题（已无 gpui-component 双轨，只写 ui::theme）。
            // 启动跟随：按持久化偏好解析；从未手动选过（默认 follow）→ 由系统深浅色决定。
            // EditorApp::new_impl 会用同一逻辑再落地一次（双保险 + 多窗口一致），
            // 此处在首帧前先落定，避免窗口以错误主题闪现。
            let prefs = UserPrefs::load();
            Theme::set(
                if prefs.theme.resolved_dark(cx.window_appearance()) {
                    ThemeMode::Dark
                } else {
                    ThemeMode::Light
                },
                cx,
            );
            // 本地快捷键全集：编辑区 Input + 窗口根 Root + WYSIWYG wy-editor。
            bind_local_keymaps(cx);
            // 后台预热 syntect 全局语法集与明暗色板（A2）：把「首次代码块同步
            // 反序列化几 MB」的一次性成本移出滚动路径 → 首张图片附近不再卡那一下。
            // LazyLock 并发安全，主线程先命中只会短暂阻塞。
            cx.background_executor()
                .spawn(async move { prewarm() })
                .detach();
            // 覆盖默认粘贴：Windows 剪贴板的 CRLF 必须先转为 LF，
            // 否则回车符会作为普通字符参与代码编辑器的布局。
            // （须在 bind_local_keymaps 之后注册：优先级更高，Ctrl+V 只走归一化粘贴。）
            cx.bind_keys([KeyBinding::new("ctrl-v", PasteNormalized, Some("Input"))]);

            let mut created: Option<gpui::Entity<EditorApp>> = None;
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        // 隐藏原生标题栏（它在深色系统上不受控为黑色），
                        // 改由应用内自绘标题栏（随黑白主题）。详见 app::render_title_bar。
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("GPUI 编辑器".into()),
                            appears_transparent: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| {
                        window.set_window_title("GPUI 编辑器");
                        let view = cx.new(|cx| EditorApp::new(window, cx));
                        created = Some(view.clone());
                        // 窗口第一层是自研根视图（ui::root，替代 gpui_component::Root）
                        cx.new(|cx| root::Root::new(view, window, cx))
                    },
                )
                .unwrap();

            // 打开后把焦点放到编辑区
            window
                .update(cx, |_root, window, cx| {
                    if let Some(view) = &created {
                        let app = view.read(cx);
                        let focus = app.editor_focus_handle(cx);
                        window.focus(&focus, cx);
                    }
                    cx.activate(true);
                })
                .unwrap();

            // Ctrl+Q 退出
            cx.on_action(|_: &Quit, cx| cx.quit());
        });
}
