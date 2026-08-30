//! 最小化编辑器复现样例：窗口中**只有** gpui-component 的代码编辑器，
//! 没有任何菜单栏 / 选项卡 / 状态栏 / 预览等外围代码，用于隔离验证
//! “在一行反复粘贴长文本后整行消失”是否仍可复现。
//!
//! 运行：
//! ```text
//! cargo run --example minimal_editor
//! ```
//!
//! 复现步骤：窗口打开后编辑区已预先填充 3 段 `use std::sync::Arc;`（历史上
//! 第 4 次粘贴开始触发“整行消失”），按 Ctrl+V 连续粘贴并观察该行是否完整显示。
//! Ctrl+Q 退出。
//!
//! 说明：crates.io 的 gpui-component 0.5.1 还没有文档中的 `Editor`/`EditorState`
//! （那是 git main 0.5.2 的新组件名）；0.5.1 的等价实现就是
//! `InputState::code_editor(language)` + `Input::new(&state)`——
//! 同样的 ropey 文本存储 + 行号 + 语法高亮 + 软换行引擎。这里按“最基本状态”
//! 配置（不设语法高亮语言、保留默认软换行），排除外围代码的相互影响。

use gpui::{
    actions, div, px, size, App, AppContext, Application, Bounds, Focusable, KeyBinding, Window,
    WindowBounds, WindowOptions,
};
use gpui::prelude::*;
use gpui_component::input::{Input, InputState};
use gpui_component::{Root, Theme, ThemeMode};
use gpui_component_assets::Assets;

actions!(minimal_editor, [Quit]);

/// 窗口内唯一的视图：一个撑满窗口的代码编辑器。
struct MinimalEditor {
    editor: gpui::Entity<InputState>,
}

impl MinimalEditor {
    fn new(window: &mut Window, cx: &mut gpui::Context<Self>) -> Self {
        // 预先放入历史上出问题的触发文本
        let seed = "use std::sync::Arc;".repeat(3);
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                // 最基本状态：仅开启代码编辑器模式与自带软换行，不加行号等修饰
                .code_editor("text")
                .default_value(seed)
        });
        Self { editor }
    }
}

impl gpui::Render for MinimalEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        div()
            .size_full()
            .flex()
            .child(Input::new(&self.editor).h_full())
    }
}

fn main() {
    Application::new()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            // 应用级快捷键必须在 gpui_component::init 之前注册（init 会重置键位映射）
            cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
            // 初始化 gpui-component（主题、Input 键绑定等）
            gpui_component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);

            let bounds = Bounds::centered(None, size(px(900.), px(600.)), cx);
            let mut created: Option<gpui::Entity<MinimalEditor>> = None;
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |window, cx| {
                        window.set_window_title("Minimal gpui-component Editor");
                        let view = cx.new(|cx| MinimalEditor::new(window, cx));
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
                        window.focus(&view.read(cx).editor.read(cx).focus_handle(cx));
                    }
                    cx.activate(true);
                })
                .unwrap();

            // Ctrl+Q 退出
            cx.on_action(|_: &Quit, cx| cx.quit());
        });
}