//! 程序入口：初始化 GPUI 应用、注册键盘快捷键、创建主窗口并显示 [`app::EditorApp`]。
//!
//! 模块结构：
//! - [`app`]：主视图 `EditorApp`（标题栏 + 编辑区 + 可选 Markdown 预览区）
//! - [`markdown`]：Markdown → GPUI 元素渲染
//! - [`input`]：自研文本输入控件（`text_area` 多行 / `text_box` 单行，含回车提交、
//!   撤销重做等），本编辑器复用其多行 [`TextArea`] 作为编辑区
//! - [`scrollbar`]：自绘滚动条（GPUI 0.2.2 仅提供系统 overlay 滚动条，Windows 上等同
//!   不可见，故自绘）

// 发布构建（非 debug）时隐藏控制台窗口，只显示 GUI 窗体
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod markdown;
mod scrollbar;
mod input;

use gpui::{
    App, Application, Bounds, KeyBinding, px, size, WindowBounds, WindowOptions,
};
use input::Quit;

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(980.), px(720.)), cx);

        // 全局快捷键 → 文本输入动作；动作由聚焦的 TextArea 控件处理
        cx.bind_keys([
            KeyBinding::new("backspace", input::Backspace, None),
            KeyBinding::new("delete", input::Delete, None),
            KeyBinding::new("left", input::Left, None),
            KeyBinding::new("right", input::Right, None),
            KeyBinding::new("up", input::Up, None),
            KeyBinding::new("down", input::Down, None),
            KeyBinding::new("shift-left", input::SelectLeft, None),
            KeyBinding::new("shift-right", input::SelectRight, None),
            KeyBinding::new("shift-up", input::SelectUp, None),
            KeyBinding::new("shift-down", input::SelectDown, None),
            KeyBinding::new("ctrl-a", input::SelectAll, None),
            KeyBinding::new("ctrl-v", input::Paste, None),
            KeyBinding::new("ctrl-c", input::Copy, None),
            KeyBinding::new("ctrl-x", input::Cut, None),
            KeyBinding::new("ctrl-z", input::Undo, None),
            KeyBinding::new("ctrl-y", input::Redo, None),
            KeyBinding::new("home", input::Home, None),
            KeyBinding::new("end", input::End, None),
            // 词级导航 / 文档级导航（Ctrl 组；Alt 组兼容 macOS Option 习惯）
            KeyBinding::new("ctrl-left", input::MoveToPreviousWordBoundary, None),
            KeyBinding::new("ctrl-right", input::MoveToNextWordBoundary, None),
            KeyBinding::new("alt-left", input::MoveToPreviousWordBoundary, None),
            KeyBinding::new("alt-right", input::MoveToNextWordBoundary, None),
            KeyBinding::new("ctrl-shift-left", input::SelectToPreviousWordBoundary, None),
            KeyBinding::new("ctrl-shift-right", input::SelectToNextWordBoundary, None),
            KeyBinding::new("alt-shift-left", input::SelectToPreviousWordBoundary, None),
            KeyBinding::new("alt-shift-right", input::SelectToNextWordBoundary, None),
            KeyBinding::new("ctrl-backspace", input::DeleteWordBackward, None),
            KeyBinding::new("ctrl-delete", input::DeleteWordForward, None),
            KeyBinding::new("alt-backspace", input::DeleteWordBackward, None),
            KeyBinding::new("alt-delete", input::DeleteWordForward, None),
            KeyBinding::new("ctrl-home", input::MoveToBeginningOfDocument, None),
            KeyBinding::new("ctrl-end", input::MoveToEndOfDocument, None),
            KeyBinding::new("ctrl-shift-home", input::SelectToBeginningOfDocument, None),
            KeyBinding::new("ctrl-shift-end", input::SelectToEndOfDocument, None),
            KeyBinding::new("ctrl-cmd-space", input::ShowCharacterPalette, None),
            // 编辑区：Enter 换行（TextArea 未开启 submit_on_enter）
            KeyBinding::new("enter", input::Enter, None),
            KeyBinding::new("shift-enter", input::ShiftEnter, None),
            KeyBinding::new("ctrl-enter", input::ShiftEnter, None),
            // 保存文件
            KeyBinding::new("ctrl-s", input::Save, None),
            // 应用级命令（新建 / 打开 / 预览 / 字号）
            KeyBinding::new("ctrl-n", input::NewFile, None),
            KeyBinding::new("ctrl-o", input::OpenFile, None),
            KeyBinding::new("ctrl-p", input::TogglePreview, None),
            KeyBinding::new("ctrl-=", input::IncreaseFontSize, None),
            KeyBinding::new("ctrl--", input::DecreaseFontSize, None),
            KeyBinding::new("ctrl-0", input::ResetFontSize, None),
        ]);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    window.set_window_title("GPUI 编辑器");
                    app::EditorApp::build(cx)
                },
            )
            .unwrap();

        // 打开后把焦点放到编辑区
        window.update(cx, |view, window, cx| {
            window.focus(&view.editor_focus_handle(cx));
            cx.activate(true);
        })
        .unwrap();

        // Ctrl+Q 退出
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
    });
}
