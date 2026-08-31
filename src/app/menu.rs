//! menu：顶部菜单栏 + 下拉菜单（gpui_component DropdownMenu / PopupMenu）。

use super::*;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};

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
                window.dispatch_action(Box::new(gpui_component::input::Undo), cx);
            },
        ))
        .item(Self::menu_action_item(
            "重做",
            "Ctrl+Y",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(gpui_component::input::Redo), cx);
            },
        ))
        .separator()
        .item(Self::menu_action_item(
            "剪切",
            "Ctrl+X",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(gpui_component::input::Cut), cx);
            },
        ))
        .item(Self::menu_action_item(
            "复制",
            "Ctrl+C",
            weak.clone(),
            |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(gpui_component::input::Copy), cx);
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
                window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
            },
        ))
    }

    /// "查看"菜单。
    fn view_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            "预览",
            "Ctrl+P",
            weak.clone(),
            |this, _window, cx| {
                if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                    tab.preview_on = !tab.preview_on;
                    this.preview_scroll = gpui::ScrollHandle::new();
                }
                cx.notify();
            },
        ))
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
            weak,
            |this, _window, cx| {
                this.font_size = DEFAULT_FONT_SIZE;
                cx.notify();
            },
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

    /// 把焦点还给当前激活选项卡的编辑区（编辑动作派发到 Input 上下文所需）。
    fn focus_active_editor(this: &mut Self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = this.tabs.get(this.active_tab) {
            let focus = tab.editor.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
    }

    pub(super) fn render_menu_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
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
                .text_color(rgb(0x111827))
                .dropdown_menu(move |menu, _, _| items(menu, weak.clone()))
        };

        div()
            .id("menu-bar")
            .h(MENU_BAR_H)
            .w_full()
            .px_1()
            .flex()
            .items_center()
            .gap_1()
            .bg(rgb(0xf3f4f6))
            .border_b_1()
            .border_color(rgb(0xe5e7eb))
            .child(make("menu-btn-file", "文件", Self::file_items, weak.clone()))
            .child(make("menu-btn-edit", "编辑", Self::edit_items, weak.clone()))
            .child(make("menu-btn-view", "查看", Self::view_items, weak.clone()))
            .child(make("menu-btn-help", "帮助", Self::help_items, weak))
    }
}
