//! 程序入口：初始化 GPUI 应用、注册键盘快捷键、创建主窗口并显示 [`app::ChatApp`]。
//!
//! 模块结构：
//! - [`app`]：主视图 `ChatApp`（标题栏 + 消息区 + 输入栏）
//! - [`backend`]：Dify 后端客户端（上传文档 / 流式问答）
//! - [`markdown`]：Markdown → GPUI 元素渲染
//! - [`text_input`]：自研多行文本输入控件（含回车提交、Shift+Enter 换行、撤销重做等）

// 发布构建（非 debug）时隐藏控制台窗口，只显示 GUI 窗体
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod markdown;
mod scrollbar;
mod text_input;

use gpui::{
    App, Application, Bounds, Focusable, KeyBinding, px, size, WindowBounds, WindowOptions,
};
use text_input::Quit;

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(880.), px(700.)), cx);

        // 全局快捷键 → 文本输入动作；动作由聚焦的 TextInput 控件处理
        cx.bind_keys([
            KeyBinding::new("backspace", text_input::Backspace, None),
            KeyBinding::new("delete", text_input::Delete, None),
            KeyBinding::new("left", text_input::Left, None),
            KeyBinding::new("right", text_input::Right, None),
            KeyBinding::new("up", text_input::Up, None),
            KeyBinding::new("down", text_input::Down, None),
            KeyBinding::new("shift-left", text_input::SelectLeft, None),
            KeyBinding::new("shift-right", text_input::SelectRight, None),
            KeyBinding::new("shift-up", text_input::SelectUp, None),
            KeyBinding::new("shift-down", text_input::SelectDown, None),
            KeyBinding::new("ctrl-a", text_input::SelectAll, None),
            KeyBinding::new("ctrl-v", text_input::Paste, None),
            KeyBinding::new("ctrl-c", text_input::Copy, None),
            KeyBinding::new("ctrl-x", text_input::Cut, None),
            KeyBinding::new("ctrl-z", text_input::Undo, None),
            KeyBinding::new("ctrl-y", text_input::Redo, None),
            KeyBinding::new("home", text_input::Home, None),
            KeyBinding::new("end", text_input::End, None),
            KeyBinding::new("ctrl-cmd-space", text_input::ShowCharacterPalette, None),
            // 聊天输入框：Enter 提交，Shift+Enter / Ctrl+Enter 换行
            KeyBinding::new("enter", text_input::Enter, None),
            KeyBinding::new("shift-enter", text_input::ShiftEnter, None),
            KeyBinding::new("ctrl-enter", text_input::ShiftEnter, None),
        ]);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    window.set_window_title("AI 问答助手");
                    app::ChatApp::build(cx)
                },
            )
            .unwrap();

        // 打开后把焦点放到问题输入框
        window.update(cx, |view, window, cx| {
            window.focus(&view.input.focus_handle(cx));
            cx.activate(true);
        })
        .unwrap();

        // Ctrl+Q 退出
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
    });
}
