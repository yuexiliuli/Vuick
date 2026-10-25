//! menu：顶部菜单栏 + 下拉菜单（gpui_component DropdownMenu / PopupMenu）。

use super::*;
use crate::actions::{Copy, Cut, Redo, SelectAll, Undo};
use crate::commands::{
    COPY, CUT, DECREASE_FONT, FOLLOW_THEME, INCREASE_FONT, NEW_FILE, OPEN_FILE, OPEN_FOLDER,
    PASTE, PREVIEW, QUIT, REDO, RESET_FONT, SAVE, SAVE_AS, SELECT_ALL, THEME,
    TOGGLE_MUSIC_PLAYER, TOGGLE_WRAP, UNDO,
};
use ui::button::{Button, ButtonVariants as _};
use ui::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};

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
        PopupMenuItem::element(move |_, cx| {
            // 快捷键提示灰字：渲染期取当前明暗模式（外壳色单一来源）。
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            let mut content = div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(label);
            if !shortcut.is_empty() {
                content = content.child(div().text_color(chrome.text_tertiary).child(shortcut));
            }
            content
        })
        .on_click(move |_, window, cx| {
            let _ = weak.update(cx, |this, cx| f(this, window, cx));
        })
    }

    /// "文件"菜单。label / 快捷键来自公共命令表（`crate::commands` 单一真值）。
    fn file_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            NEW_FILE.menu_label,
            NEW_FILE.shortcut,
            weak.clone(),
            |this, window, cx| {
                this.new_tab(window, cx)
            },
        ))
        .item(Self::menu_action_item(
            OPEN_FILE.menu_label,
            OPEN_FILE.shortcut,
            weak.clone(),
            |this, window, cx| this.open_dialog(window, cx),
        ))
        .item(Self::menu_action_item(
            OPEN_FOLDER.menu_label,
            OPEN_FOLDER.shortcut,
            weak.clone(),
            |this, window, cx| this.open_folder_dialog(window, cx),
        ))
        // 「打开最近…」：`.keep_open()` → 点击**不关闭**文件菜单（反馈 1：文件菜单
        // 保持展开），打开居中大面板（`recent_panel::RecentPanel`，Ctrl+P 同款样式）。
        .item({
            let weak2 = weak.clone();
            PopupMenuItem::element(move |_, _| {
                div()
                    .w_full()
                    .debug_selector(|| "recent-menu-open".into())
                    .child("打开最近…")
            })
            .keep_open()
            .on_click(move |_click, window, cx| {
                let _ = weak2.update(cx, |this, cx| this.open_recent_panel(window, cx));
            })
        })
        .item(Self::menu_action_item(
            SAVE.menu_label,
            SAVE.shortcut,
            weak.clone(),
            |this, window, cx| this.save(window, cx),
        ))
        .item(Self::menu_action_item(
            SAVE_AS.menu_label,
            SAVE_AS.shortcut,
            weak.clone(),
            |this, window, cx| this.save_as(window, cx),
        ))
        .separator()
        // 「导出/导入设置…」：配置 + 会话全量打包为单个 .json（系统保存/打开对话框）。
        .item(Self::menu_action_item(
            "导出设置…",
            "",
            weak.clone(),
            |this, window, cx| this.export_settings(window, cx),
        ))
        .item(Self::menu_action_item(
            "导入设置…",
            "",
            weak.clone(),
            |this, window, cx| this.import_settings(window, cx),
        ))
        .separator()
        .item(Self::session_restore_menu_item(weak.clone()))
        .item(Self::close_to_tray_menu_item(weak.clone()))
        .separator()
        .item(Self::menu_action_item(
            QUIT.menu_label,
            QUIT.shortcut,
            weak,
            |_, _window, cx| cx.quit(),
        ))
    }

    /// "编辑"菜单。
    fn edit_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            UNDO.menu_label,
            UNDO.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Undo)),
        ))
        .item(Self::menu_action_item(
            REDO.menu_label,
            REDO.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Redo)),
        ))
        .separator()
        .item(Self::menu_action_item(
            CUT.menu_label,
            CUT.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Cut)),
        ))
        .item(Self::menu_action_item(
            COPY.menu_label,
            COPY.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Copy)),
        ))
        .item(Self::menu_action_item(
            PASTE.menu_label,
            PASTE.shortcut,
            weak.clone(),
            |this, window, cx| this.paste_normalized(window, cx),
        ))
        .separator()
        .item(Self::menu_action_item(
            SELECT_ALL.menu_label,
            SELECT_ALL.shortcut,
            weak,
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(SelectAll)),
        ))
    }

    /// "查看"菜单。
    fn view_items(menu: PopupMenu, weak: gpui::WeakEntity<Self>) -> PopupMenu {
        menu.item(Self::menu_action_item(
            PREVIEW.menu_label,
            // 快捷键提示走命令表（空串 = 无提示）：P2-8 原 Ctrl+P = 预览切换已改开
            // 「打开文件快选」→ 预览切换现由标签栏眼睛按钮承担。
            PREVIEW.shortcut,
            weak.clone(),
            |this, window, cx| {
                let idx = this.active_tab;
                this.toggle_preview_for_tab(idx, window, cx);
            },
        ))
        .item(Self::live_test_menu_item(weak.clone()))
        .separator()
        // P4-1：EPUB 章节翻转（WyEditor 只读宿主 / MarkdownPreview 回退通用）。
        // 快捷键 Ctrl+[/Ctrl+] 仅为**菜单提示**（历史未实际绑定，本仓库不发其他键绑定）。
        .item(Self::menu_action_item(
            "上一章",
            "Ctrl+[",
            weak.clone(),
            |this, _window, cx| {
                this.epub_flip_chapter(-1, cx);
                cx.notify();
            },
        ))
        .item(Self::menu_action_item(
            "下一章",
            "Ctrl+]",
            weak.clone(),
            |this, _window, cx| {
                this.epub_flip_chapter(1, cx);
                cx.notify();
            },
        ))
        .separator()
        .item(Self::menu_action_item(
            INCREASE_FONT.menu_label,
            INCREASE_FONT.shortcut,
            weak.clone(),
            |this, _window, cx| this.change_font_size(1.0, cx),
        ))
        .item(Self::menu_action_item(
            DECREASE_FONT.menu_label,
            DECREASE_FONT.shortcut,
            weak.clone(),
            |this, _window, cx| this.change_font_size(-1.0, cx),
        ))
        .item(Self::menu_action_item(
            RESET_FONT.menu_label,
            RESET_FONT.shortcut,
            weak.clone(),
            |this, _window, cx| this.reset_font_size(cx),
        ))
        .separator()
        .item(Self::wrap_menu_item(weak.clone()))
        .separator()
        .item(Self::menu_action_item(
            THEME.menu_label,
            THEME.shortcut,
            weak.clone(),
            |this, window, cx| this.toggle_theme(window, cx),
        ))
        .item(Self::menu_action_item(
            FOLLOW_THEME.menu_label,
            FOLLOW_THEME.shortcut,
            weak.clone(),
            |this, _window, cx| this.follow_system_theme(cx),
        ))
        // 音乐播放器：整窗音乐模式开关（Ctrl+M / 命令面板 / 菜单共用同一动作）。
        .item(Self::menu_action_item(
            TOGGLE_MUSIC_PLAYER.menu_label,
            TOGGLE_MUSIC_PLAYER.shortcut,
            weak,
            |this, window, cx| this.toggle_music_player(window, cx),
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

    /// "查看"菜单：「预览编辑测试」——本项目 `markdown_wysiwyg::WyEditor`
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
    /// label 与快捷键来自公共命令表（`commands::TOGGLE_WRAP`）。
    fn wrap_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).wrap_enabled)
                .unwrap_or(true);
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            let label = SharedString::from(format!(
                "{} {}",
                if on { "☑" } else { "☐" },
                TOGGLE_WRAP.menu_label,
            ));
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(label)
                .child(div().text_color(chrome.text_tertiary).child(TOGGLE_WRAP.shortcut))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_wrap(window, cx));
        })
    }

    /// "恢复上次会话"菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即持久化
    /// （同 [`Self::wrap_menu_item`] 的 ☑/☐ 动态项模式）。
    fn session_restore_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).session_restore)
                .unwrap_or(true);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(if on { "☑ 恢复上次会话" } else { "☐ 恢复上次会话" })
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_session_restore(window, cx));
        })
    }

    /// 「关闭到系统托盘」菜单标签（动态 ☑/☐ 复选标记，同 `session_restore_menu_item`
    /// 模式；纯函数供菜单渲染与 R4 回归断言共用）。
    pub(super) fn close_to_tray_label(on: bool) -> &'static str {
        if on {
            "☑ 关闭时最小化到系统托盘"
        } else {
            "☐ 关闭时最小化到系统托盘"
        }
    }

    /// "关闭到系统托盘"菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即持久化
    /// （同 [`Self::session_restore_menu_item`] 的 ☑/☐ 动态项模式）。
    fn close_to_tray_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).close_to_tray)
                .unwrap_or(false);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Self::close_to_tray_label(on))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_close_to_tray(window, cx));
        })
    }

    /// 把焦点还给当前激活选项卡的编辑区（编辑动作派发到 Input 上下文所需）。
    pub(super) fn focus_active_editor(this: &mut Self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = this.active_tab;
        let focus = this.tab_focus_handle(idx, cx);
        window.focus(&focus, cx);
    }

    /// 编辑类命令助手：先 `focus_active_editor` 把焦点还给当前激活选项卡编辑区，再向
    /// 窗口派发编辑动作。右键「编辑」菜单（`edit_items`）与命令面板命令表（`palette.rs`）
    /// 共 5×2 个同构闭包（Undo/Redo/Cut/Copy/SelectAll）此前各自手抄，收敛于此。两处
    /// 目标解析一致（都走 `focus_active_editor` 的 active_tab），故直接共用；
    /// 粘贴（Paste）走 `paste_normalized` 不在此列。
    pub(super) fn dispatch_editor_action(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: Box<dyn gpui::Action>,
    ) {
        Self::focus_active_editor(this, window, cx);
        window.dispatch_action(action, cx);
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
                .rounded_sm()
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

    /// 「打开最近」居中大面板入口（文件菜单「打开最近…」点击，反馈 2）：创建
    /// [`recent_panel::RecentPanel`] 实体（Ctrl+P 命令面板同款 Acrylic 居中卡片）、
    /// 聚焦面板、订阅 `DismissEvent`（择项/Esc/点击外 → 清引用释放实体）。
    ///
    /// 反馈 1：源菜单项 `.keep_open()` → 文件菜单**不 dismiss**（保持展开，面板 occlude
    /// 盖在其上不穿透）；面板关闭（含择项打开文件后）也不动文件菜单——它留到自身
    /// 下一次外部点击才收起（无 PopupMenu 句柄，不强行程序关菜单）。
    pub(super) fn open_recent_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 最近列表快照：面板打开期间被别处更新 → 下次打开才反映最新（面板实体不
        // 在渲染路径反读宿主演变状态，避免 re-entrant borrow；记录逻辑仍走 file_ops）。
        let folders = self.recent_folders.clone();
        let files = self.recent_files.clone();
        // 打开前的焦点 = 仍展开的文件菜单句柄；面板关闭（动作未带走焦点）时归还它。
        let previous_focus = window.focused(cx);
        let host = cx.weak_entity();
        let panel = cx.new(|cx| {
            recent_panel::RecentPanel::new(host, folders, files, previous_focus, cx)
        });
        let sub = cx.subscribe(&panel, |this, _e, _: &DismissEvent, cx| {
            this.recent_panel = None;
            this._recent_panel_sub = None;
            cx.notify();
        });
        // 打开即聚焦面板（↑↓/Enter/Esc 直达；源菜单无 dismiss 不再抢焦点）。
        let handle = panel.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.recent_panel = Some(panel);
        self._recent_panel_sub = Some(sub);
        cx.notify();
    }
}
