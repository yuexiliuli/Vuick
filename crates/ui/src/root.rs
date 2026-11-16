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
        // 真机诊断（VUICK_TAB_DIAG=1）：Root 的 Tab 抢到事件（=焦点不在编辑区
        // Input / WY wy-editor 上下文）→ focus_next 触发（H1 判别行）。
        base::tab_diag::diag("root-focus_next", || "fired".into());
        window.focus_next(cx);
    }

    fn on_action_tab_prev(&mut self, _: &TabPrev, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 启动首帧墙钟：首次 render 时补记 firstframe 段并打印 [STARTUP] 汇总
        // （VUICK_STARTUP_DIAG=1 门控；未开启单次 var_os 检查即返回）。
        base::startup::summarize();
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

    /// 回归：窗口级 Tab 焦点循环在**非编辑焦点**下仍工作——Root 的 `tab→focus_next`
    /// 只在没有更深（编辑器 `Input`/WYSIWYG `wy-editor`）键上下文抢走 Tab 时触发；
    /// 本测试用两个普通可聚焦元素验证「Tab 仍移动焦点」，不受编辑器 Tab 绑定破坏。
    #[gpui::test]
    fn tab_focus_cycle_still_moves_focus_between_focusables(cx: &mut TestAppContext) {
        use super::Root;
        use gpui::{Keystroke, VisualTestContext};
        cx.update(crate::test_support::bind_local_keymaps);

        struct FocusPair {
            a: gpui::FocusHandle,
            b: gpui::FocusHandle,
        }
        impl Render for FocusPair {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .child(div().id("focus-a").track_focus(&self.a).size_full().child("A"))
                    .child(div().id("focus-b").track_focus(&self.b).size_full().child("B"))
            }
        }

        let (root, cx) = cx.add_window_view(|window, cx| {
            // 作为「普通可聚焦控件」：必须是 tab stop，focus_next 才会走到它。
            let a = cx.focus_handle().tab_stop(true);
            let b = cx.focus_handle().tab_stop(true);
            let view = cx.new(move |_| FocusPair { a, b });
            Root::new(view, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        // 渲染一帧，使 dispatch tree 含两个可聚焦节点。
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.refresh();
        }
        cx.run_until_parked();
        let (ha, hb) = cx.update(|_, cx| {
            let fp = root
                .read(cx)
                .view()
                .clone()
                .downcast::<FocusPair>()
                .expect("Root 内应承载 FocusPair");
            (fp.read(cx).a.clone(), fp.read(cx).b.clone())
        });
        let w = cx.windows().first().copied().expect("窗口句柄");
        // 聚焦 A → Tab → 焦点应移到 B（Root 的 focus_next 仍在非编辑焦点下工作）。
        cx.update(|window, cx| window.focus(&ha, cx));
        cx.dispatch_keystroke(w, Keystroke::parse("tab").unwrap());
        cx.run_until_parked();
        let focused = cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused,
            Some(hb),
            "非编辑焦点下 Tab 应触发 Root 焦点循环，焦点从 A 移到 B"
        );
    }
}
