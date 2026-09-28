//! menu：顶部菜单栏 + 下拉菜单（gpui_component DropdownMenu / PopupMenu）。

use super::*;
use crate::actions::{Copy, Cut, Redo, SelectAll, Undo};
use crate::ui::button::{Button, ButtonVariants as _};
use crate::ui::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};

impl EditorApp {
    /// 构造一个带右侧快捷键提示的菜单项；点击后通过弱引用更新本应用状态。
    fn menu_action_item<F>(
        label: &'static str,
        shortcut: &'static str,
        weak: gpui::WeakEntity<Self>,
        f: F,
    ) -> PopupMenuItem
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        PopupMenuItem::element(move |_, _| {
            let mut content = div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(label);
            if !shortcut.is_empty() {
                content = content.child(div().text_color(rgb(0x9ca3af)).child(shortcut));
            }
            content
        })
        .on_click(move |_, window, cx| {
            let _ = weak.update(cx, |this, cx| f(this, window, cx));
        })
    }

    /// "文件"菜单。
    fn file_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item("新建", "Ctrl+N", weak.clone(), |this, window, cx| {
            this.new_tab(window, cx)
        }))
        .item(Self::menu_action_item(
            "打开文件…",
            "Ctrl+O",
            weak.clone(),
            |this, window, cx| this.open_dialog(window, cx),
        ))
        .item(Self::menu_action_item(
            "保存",
            "Ctrl+S",
            weak.clone(),
            |this, window, cx| this.save(window, cx),
        ))
        .item(Self::menu_action_item(
            "另存为…",
            "Shift+Ctrl+S",
            weak.clone(),
            |this, window, cx| this.save_as(window, cx),
        ))
        .separator()
        .item(Self::menu_action_item(
            "退出",
            "Ctrl+Q",
            weak,
            |_, _window, cx| cx.quit(),
        ))
    }

    /// "编辑"菜单。
    fn edit_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            "撤销",
            "Ctrl+Z",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Undo), cx);
            },
        ))
        .item(Self::menu_action_item(
            "重做",
            "Ctrl+Y",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Redo), cx);
            },
        ))
        .separator()
        .item(Self::menu_action_item(
            "剪切",
            "Ctrl+X",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Cut), cx);
            },
        ))
        .item(Self::menu_action_item(
            "复制",
            "Ctrl+C",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Copy), cx);
            },
        ))
        .item(Self::menu_action_item(
            "粘贴",
            "Ctrl+V",
            weak.clone(),
            |this, window, cx| this.paste_normalized(window, cx),
        ))
        .separator()
        .item(Self::menu_action_item(
            "全选",
            "Ctrl+A",
            weak,
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(SelectAll), cx);
            },
        ))
    }

    /// "查看"菜单。
    fn view_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            "预览",
            "Ctrl+P",
            weak.clone(),
            |this, window, cx| {
                let idx = this.active_tab;
                this.toggle_preview_for_tab(idx, window, cx);
            },
        ))
        .item(Self::live_test_menu_item(weak.clone()))
        .separator()
        .item(Self::menu_action_item(
            "增大字号",
            "Ctrl+=",
            weak.clone(),
            |this, _window, cx| this.change_font_size(1.0, cx),
        ))
        .item(Self::menu_action_item(
            "减小字号",
            "Ctrl+-",
            weak.clone(),
            |this, _window, cx| this.change_font_size(-1.0, cx),
        ))
        .item(Self::menu_action_item(
            "重置字号",
            "Ctrl+0",
            weak.clone(),
            |this, _window, cx| {
                this.font_size = DEFAULT_FONT_SIZE;
                cx.notify();
            },
        ))
        .separator()
        .item(Self::wrap_menu_item(weak.clone()))
        .separator()
        .item(Self::menu_action_item(
            "黑白切换",
            "",
            weak,
            |this, window, cx| this.toggle_theme(window, cx),
        ))
    }

    /// "帮助"菜单。
    fn help_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            "关于",
            "",
            weak,
            |this, _window, cx| {
                this.status = SharedString::from(
                    "GPUI 编辑器 v0.1.0 — 基于 Rust + GPUI 0.2.2 + gpui-component",
                );
                this.status_ok = true;
                cx.notify();
            },
        ))
    }

    /// "查看"菜单：「预览编辑测试」——本项目 `markdown::wysiwyg::WyEditor`
    /// （以 `Doc` 为数据源、所见即所得编辑的 WYSIWYG；已取代 zorite 移植的 `live`）。
    fn live_test_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| {
                    let i = e.read(cx).active_tab;
                    e.read(cx).tabs.get(i).map(|t| t.live2.is_some()).unwrap_or(false)
                })
                .unwrap_or(false);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(if on {
                    "● 预览编辑测试"
                } else {
                    "○ 预览编辑测试"
                })
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_live_test(window, cx));
        })
    }

    /// "自动换行"菜单项：动态显示当前是否开启（前导 ☑/☐），点击切换。
    fn wrap_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).wrap_enabled)
                .unwrap_or(true);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(if on { "☑ 自动换行" } else { "☐ 自动换行" })
                .child(div().text_color(rgb(0x9ca3af)).child("Ctrl+Alt+W"))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_wrap(window, cx));
        })
    }

    /// 把焦点还给当前激活选项卡的编辑区（编辑动作派发到 Input 上下文所需）。
    fn focus_active_editor(this: &mut Self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = this.active_tab;
        let focus = this.tab_focus_handle(idx, cx);
        window.focus(&focus, cx);
    }

    /// 菜单按钮组（文件/编辑/查看/帮助），并入自绘标题栏使用。
    pub(super) fn render_menu_buttons(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let text_primary = self.text_primary();
        let make = |id: &'static str,
                    label: &'static str,
                    items: fn(PopupMenu, gpui::WeakEntity<Self>) -> PopupMenu,
                    weak: gpui::WeakEntity<Self>| {
            Button::new(id)
                .label(label)
                .ghost()
                .w(px(56.))
                .h(MENU_BAR_H)
                .flex()
                .items_center()
                .px_2()
                .rounded_md()
                .text_size(px(13.))
                .text_color(text_primary)
                .dropdown_menu(move |menu, _, _| items(menu, weak.clone()))
        };

        div()
            .id("menu-bar")
            .flex()
            .items_center()
            .gap_1()
            .child(make("menu-btn-file", "文件", Self::file_items, weak.clone()))
            .child(make("menu-btn-edit", "编辑", Self::edit_items, weak.clone()))
            .child(make("menu-btn-view", "查看", Self::view_items, weak.clone()))
            .child(make("menu-btn-help", "帮助", Self::help_items, weak))
    }
}
