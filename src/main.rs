//! 程序入口：初始化 GPUI + GPUI-Component、注册应用级快捷键、
//! 创建主窗口并把窗口根视图包上 [`gpui_component::Root`]。
//!
//! 模块结构：
//! - [`app`]：主视图 `EditorApp`（自绘标题栏 + 菜单栏 + 选项卡栏 + 编辑区 + 可选 Markdown 预览 + 状态栏）
//! - `gpui_component::text::TextView`：Markdown 解析、排版与选择
//! - [`scrollbar`]：自绘横向滚动条（选项卡栏用）
//! - [`actions`]：应用级动作类型
//!
//! 文本（代码）编辑区使用 **gpui-component 的 `Input`（CodeEditor 模式）**：
//! 底层的 `EditorState` 用 ropey 存储文本、内置语法高亮（tree-sitter）、行号、
//! 缩进参考线、搜索面板与撤销/重做，替换掉原先自研的 `TextArea`（其换行排版
//! 在大量粘贴时曾出现“整行消失”的问题）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod epub;
mod markdown;
mod pdf;
mod scrollbar;
mod toc;

use gpui::{px, size, App, AppContext, Bounds, KeyBinding, WindowBounds, WindowOptions};
use gpui_component::{Root, Theme, ThemeMode};
use gpui_component_assets::Assets;

use crate::actions::*;

fn main() {
    gpui_platform::application()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(980.), px(720.)), cx);

            // gpui-component 初始化会重建按键映射，因此先注册应用动作。
            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewFile, None),
                KeyBinding::new("ctrl-o", OpenFile, None),
                KeyBinding::new("ctrl-s", Save, None),
                KeyBinding::new("shift-ctrl-s", SaveAs, None),
                KeyBinding::new("ctrl-w", CloseTab, None),
                KeyBinding::new("ctrl-p", TogglePreview, None),
                KeyBinding::new("ctrl-alt-w", ToggleWordWrap, None),
                KeyBinding::new("ctrl-=", IncreaseFontSize, None),
                KeyBinding::new("ctrl--", DecreaseFontSize, None),
                KeyBinding::new("ctrl-0", ResetFontSize, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);

            // 初始化 gpui-component（主题、键盘映射、Input 等）。
            // 这里强制浅色主题，与现有界面的亮色调保持一致。
            gpui_component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            // 覆盖组件库默认粘贴：Windows 剪贴板的 CRLF 必须先转为 LF，
            // 否则回车符会作为普通字符参与代码编辑器的布局。
            cx.bind_keys([KeyBinding::new("ctrl-v", PasteNormalized, Some("Input"))]);

            let mut created: Option<gpui::Entity<app::EditorApp>> = None;
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
                        let view = cx.new(|cx| app::EditorApp::new(window, cx));
                        created = Some(view.clone());
                        // 窗口第一层必须是 gpui_component::Root
                        cx.new(|cx| Root::new(view, window, cx))
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
