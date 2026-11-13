//! 聚焦边框色共享 helper：输入框聚焦 → 主题 `focus_ring`，否则回退调用方给的默认边框色。
//!
//! 收敛 4 处手写的「if 聚焦 { focus_ring } else { fallback }」——侧栏搜索框
//! （`shell_sidebar::search`）、文件树过滤框（`shell_sidebar::files`）、命令面板查询框
//! （`palette`）、新建/重命名输入弹层（`shell_sidebar::prompt`）。聚焦判定统一走
//! [`FocusHandle::is_focused`]：与 `window.focused(cx) == Some(handle)` 语义等价——窗口
//! `focus` 槽位记的是已注册聚焦句柄的 id，两侧在真实调用路径下逐字节一致（prompt
//! 原先即用 `is_focused`，本 helper 让 4 处判定统一到同一姿势）。

use gpui::{App, FocusHandle, Hsla, Window};

use crate::ui::theme::Theme;

/// 聚焦边框色：`handle` 聚焦 → 主题 `focus_ring`（Fluent 聚焦描边），否则 `fallback`。
pub fn focus_border_color(
    window: &Window,
    handle: &FocusHandle,
    fallback: Hsla,
    cx: &App,
) -> Hsla {
    if handle.is_focused(window) {
        Theme::global(cx).colors.focus_ring.into()
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Theme, ThemeMode};
    use gpui::{Context, IntoElement, Render, TestAppContext, VisualTestContext, div, rgb};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 仅持焦点句柄的最小宿主：焦点判定只读 `window.focus`，无需渲染真实可聚焦控件。
    struct Harness {
        _handle: FocusHandle,
    }
    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// 建 Light 主题窗口并暴露焦点句柄（句柄经共享槽位回传）。
    fn harness<'a>(
        cx: &'a mut TestAppContext,
        holder: &Rc<RefCell<Option<FocusHandle>>>,
    ) -> &'a mut VisualTestContext {
        cx.update(|cx| Theme::set(ThemeMode::Light, cx));
        let (_root, wcx) = cx.add_window_view({
            let holder = holder.clone();
            move |_window, cx| {
                let handle = cx.focus_handle();
                *holder.borrow_mut() = Some(handle.clone());
                Harness { _handle: handle }
            }
        });
        let _ = _root;
        wcx
    }

    #[gpui::test]
    fn focused_returns_focus_ring_unfocused_passthrough(cx: &mut TestAppContext) {
        let holder = Rc::new(RefCell::new(None::<FocusHandle>));
        let wcx = harness(cx, &holder);
        let handle = holder.borrow().clone().expect("窗口已建，句柄已登记");
        let fallback: Hsla = rgb(0x112233).into();

        // 未聚焦：应原样透传 fallback（不取 theme 色，防 helper 改动连带消费方）。
        let got = wcx.update(|window, cx| focus_border_color(window, &handle, fallback, cx));
        assert_eq!(got, fallback, "未聚焦应返回传入的 fallback");

        // 聚焦 → 主题 focus_ring（Light = #005FB8，与 Theme::Light 同源钉死）。
        wcx.update(|window, cx| window.focus(&handle, cx));
        let got = wcx.update(|window, cx| focus_border_color(window, &handle, fallback, cx));
        let expected: Hsla = Theme::light().colors.focus_ring.into();
        assert_eq!(got, expected, "聚焦应返回主题 focus_ring（Light）");
        assert_eq!(got, rgb(0x005fb8).into(), "Light focus_ring = #005FB8（钉死）");
        assert_ne!(got, fallback, "聚焦时不应透传 fallback");
    }
}
