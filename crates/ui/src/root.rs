//! 窗口根视图（#9）：替代 `gpui_component::Root` 作为每窗口第一层。
//!
//! 只做「宿主」：把内容视图铺满窗口并垫主题背景/rem。不再管理组件的 Sheet/Dialog/
//! 通知/工具提示叠加层——通知早已由 App 自持（#8），弹层由 `ui::menu` 自持，无需根级
//! 槽位。原 `Root::render` 的 `UiGlobalState::begin_selection_frame` /
//! `TextSelection::activate_scope` / `TextSelectionLayer` 本项目无消费者，故不迁。
//!
//! Tab / Shift+Tab 焦点循环用本地 action 复刻（[`bind_keys`] 入口在 `main.rs`），
//! 与组件库 root keymap 解耦（在 `gpui_component::init` 之后注册、优先级更高，遮蔽
//! 组件绑定）。

use gpui::{
    div, AnyView, App, Context, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _,
    Render, Styled as _, Window, actions,
};

use crate::ui::theme::Theme;

actions!(ui_root, [Tab, TabPrev]);

const CONTEXT: &str = "Root";

/// Tab / Shift+Tab 焦点循环键位（入口：`main.rs`，在 `gpui_component::init` 之后注册，
/// 以遮蔽组件库 root keymap；行为 = 原 `Root::on_action_tab/tab_prev` 的 focus_next/prev）。
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", Tab, Some(CONTEXT)),
        KeyBinding::new("shift-tab", TabPrev, Some(CONTEXT)),
    ])
}

/// 窗口根视图（每窗口第一层）：包裹内容视图并按主题铺背景/rem。
pub struct Root {
    view: AnyView,
}

impl Root {
    /// 窗口根视图：包裹内容视图（`main.rs` / 独立窗口 / 测试统一用）。
    pub fn new(view: impl Into<AnyView>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            view: view.into(),
        }
    }

    /// 根视图内承载的内容视图（如 `app::EditorApp`；测试里下转型取应用实体）。
    /// 跨 crate 测试锚：`shell/src/app/tests/tabs_actions.rs` 经它下转型回 `EditorApp`。
    pub fn view(&self) -> &AnyView {
        &self.view
    }

    fn on_action_tab(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn on_action_tab_prev(&mut self, _: &TabPrev, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        // 与原组件库 Root 一致：以正文缺省字号为 rem 基准（本项目视图全走 px，供保险）。
        window.set_rem_size(theme.typography.default_body_size);
        div()
            .id("ui-root")
            .debug_selector(|| "ui-root".into())
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_tab_prev))
            .relative()
            .size_full()
            // 根层不铺不透明底色，最终底色由 EditorApp 的 Mica 半透明窗口底提供。
            .bg(gpui::transparent_black())
            .text_color(theme.colors.fg)
            .child(self.view.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext, px};

    struct FakeView;

    impl Render for FakeView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// 宿主正确包裹内容视图（`view()` 可下转型回原视图），且根元素撑满窗口。
    #[gpui::test]
    fn root_hosts_view_and_fills_window(cx: &mut TestAppContext) {
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| FakeView);
            Root::new(view, window, cx)
        });
        let hosts = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<FakeView>().is_ok()
        });
        assert!(hosts, "view() 应能下转型回原内容视图");
        cx.simulate_resize(gpui::size(px(400.), px(300.)));
        let _ = cx.refresh();
        let bounds = cx.debug_bounds("ui-root").expect("根元素应渲染");
        assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
    }
}
