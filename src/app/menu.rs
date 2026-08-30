//! menu.

use super::*;
use gpui_component::button::{Button, ButtonVariants as _};

impl EditorApp {
    pub(super) fn menu_button_left(id: MenuId) -> Pixels {
        let idx = match id {
            MenuId::File => 0,
            MenuId::Edit => 1,
            MenuId::View => 2,
            MenuId::Help => 3,
        };
        px(4.) + (MENU_ITEM_W + px(4.)) * idx as f32
    }

    pub(super) fn render_menu_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.open_menu;
        let make_button = |id: MenuId, label: &'static str, cx: &mut Context<Self>| {
            let is_open = open == Some(id);
            Button::new(SharedString::from(format!("menu-btn-{id:?}")))
                .label(label)
                .ghost()
                .w(MENU_ITEM_W)
                .h(MENU_BAR_H)
                .flex()
                .items_center()
                .px_2()
                .rounded_md()
                .bg(if is_open {
                    rgb(0xe5e7eb)
                } else {
                    rgba(0x00000000)
                })
                .text_size(px(13.))
                .font_weight(if is_open {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(rgb(0x111827))
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                    this.toggle_menu(id, cx);
                    cx.stop_propagation();
                }))
        };
        let file_btn = make_button(MenuId::File, "文件", cx);
        let edit_btn = make_button(MenuId::Edit, "编辑", cx);
        let view_btn = make_button(MenuId::View, "查看", cx);
        let help_btn = make_button(MenuId::Help, "帮助", cx);

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
            .child(file_btn)
            .child(edit_btn)
            .child(view_btn)
            .child(help_btn)
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn menu_definitions(
        &self,
    ) -> Vec<(MenuId, &'static str, Vec<(&'static str, &'static str)>)> {
        vec![
            (
                MenuId::File,
                "文件",
                vec![
                    ("新建", "Ctrl+N"),
                    ("打开文件…", "Ctrl+O"),
                    ("保存", "Ctrl+S"),
                    ("另存为…", ""),
                    ("—", ""),
                    ("退出", "Ctrl+Q"),
                ],
            ),
            (
                MenuId::Edit,
                "编辑",
                vec![
                    ("撤销", "Ctrl+Z"),
                    ("重做", "Ctrl+Y"),
                    ("—", ""),
                    ("剪切", "Ctrl+X"),
                    ("复制", "Ctrl+C"),
                    ("粘贴", "Ctrl+V"),
                    ("—", ""),
                    ("全选", "Ctrl+A"),
                ],
            ),
            (
                MenuId::View,
                "查看",
                vec![
                    ("预览", "Ctrl+P"),
                    ("—", ""),
                    ("增大字号", "Ctrl+="),
                    ("减小字号", "Ctrl+-"),
                    ("重置字号", "Ctrl+0"),
                ],
            ),
            (MenuId::Help, "帮助", vec![("关于", "")]),
        ]
    }

    pub(super) fn render_menu_dropdown(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(menu) = self.open_menu else {
            return div().into_any_element();
        };
        let left = Self::menu_button_left(menu);
        let hover = self.menu_hover;
        let weak = cx.weak_entity();
        let items = self
            .menu_definitions()
            .into_iter()
            .find(|(id, _, _)| *id == menu)
            .map(|(_, _, items)| items)
            .unwrap_or_default();

        let mut panel_children: Vec<gpui::AnyElement> = Vec::new();
        for (idx, (label, shortcut)) in items.iter().enumerate() {
            // 分隔线
            if *label == "—" {
                panel_children.push(div().my_1().h(px(1.)).bg(rgb(0xe5e7eb)).into_any_element());
                continue;
            }
            let is_hover = hover == Some((menu, idx));
            let weak = weak.clone();
            panel_children.push(
                div()
                    .id(SharedString::from(format!("menu-item-{menu:?}-{idx}")))
                    .h(px(30.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .bg(if is_hover {
                        rgb(0xeef2ff)
                    } else {
                        rgba(0x00000000)
                    })
                    .rounded_md()
                    .text_size(px(13.))
                    .text_color(rgb(0x111827))
                    .cursor(CursorStyle::PointingHand)
                    .child(*label)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0x9ca3af))
                            .child(shortcut.to_string()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                            this.dispatch_menu_action(menu, idx, window, cx);
                            this.close_menus(cx);
                        }),
                    )
                    .on_hover({
                        let weak = weak.clone();
                        move |hovered: &bool, _w, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                if *hovered {
                                    this.menu_hover = Some((menu, idx));
                                } else if this.menu_hover == Some((menu, idx)) {
                                    this.menu_hover = None;
                                }
                                cx.notify();
                            });
                        }
                    })
                    .into_any_element(),
            );
        }

        div()
            .id("menu-overlay")
            .absolute()
            .inset_0()
            // 空白处点击：让事件冒泡到根视图关闭菜单，同时挡住下层编辑器
            .on_mouse_down(MouseButton::Left, |_ev, _w, _cx| {})
            .child(
                div()
                    .absolute()
                    .left(left)
                    .top(MENU_BAR_H + px(2.))
                    .w(MENU_PANEL_W)
                    .bg(white())
                    .border_1()
                    .border_color(rgb(0xe5e7eb))
                    .rounded_md()
                    .shadow_lg()
                    .py_1()
                    .px_1()
                    .flex()
                    .flex_col()
                    .children(panel_children),
            )
            .into_any_element()
    }

    pub(super) fn dispatch_menu_action(
        &mut self,
        menu: MenuId,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let defs = self.menu_definitions();
        let Some(items) = defs
            .iter()
            .find(|(id, _, _)| *id == menu)
            .map(|(_, _, i)| i)
        else {
            return;
        };
        if items.get(idx).is_none() {
            return;
        }
        match menu {
            MenuId::File => match idx {
                0 => self.new_tab(window, cx),
                1 => self.open_dialog(window, cx),
                2 => self.save(cx),
                3 => self.save_as(cx),
                5 => cx.quit(),
                _ => {}
            },
            MenuId::Edit => match idx {
                0 => window.dispatch_action(Box::new(gpui_component::input::Undo), cx),
                1 => window.dispatch_action(Box::new(gpui_component::input::Redo), cx),
                3 => window.dispatch_action(Box::new(gpui_component::input::Cut), cx),
                4 => window.dispatch_action(Box::new(gpui_component::input::Copy), cx),
                5 => self.paste_normalized(window, cx),
                7 => window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx),
                _ => {}
            },
            MenuId::View => match idx {
                0 => {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        tab.preview_on = !tab.preview_on;
                        self.preview_scroll = gpui::ScrollHandle::new();
                    }
                    cx.notify();
                }
                2 => self.change_font_size(1.0, cx),
                3 => self.change_font_size(-1.0, cx),
                4 => {
                    self.font_size = DEFAULT_FONT_SIZE;
                    cx.notify();
                }
                _ => {}
            },
            MenuId::Help => {
                self.status = SharedString::from(
                    "GPUI 编辑器 v0.1.0 — 基于 Rust + GPUI 0.2.2 + gpui-component",
                );
                self.status_ok = true;
                cx.notify();
            }
        }
    }
}
