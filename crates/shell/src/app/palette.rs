//! 命令面板宿主接线（P2-8）：打开/关闭 + 命令/文件条目构建。
//!
//! 单一来源说明：命令面板的命令 label / 快捷键与顶部菜单（`menu.rs`）共享公共命令表
//! （`crate::commands`）为唯一真值——两处从同一常量取 label + 快捷键展示串（个别命令
//! 的菜单/面板 label 历史不同，如「新建」 vs 「新建文件」，同在表内一行）。菜单还承载
//! disabled / 分隔线 / 动态勾选等渲染细节，仍保留各自结构（见 `commands.rs` 模块注释）；
//! 分类（文件/编辑/查看/设置）只被面板使用，随面板内联。

use super::*;
use crate::actions::{Copy, Cut, Redo, SelectAll, Undo};
use crate::commands::{
    COPY, CUT, DECREASE_FONT, FOLLOW_THEME, INCREASE_FONT, NEW_FILE, OPEN_FILE, OPEN_FOLDER,
    PASTE, PREVIEW, QUIT, REDO, RESET_FONT, SAVE, SAVE_AS, SELECT_ALL, THEME, TOGGLE_SIDEBAR,
    TOGGLE_WRAP, UNDO,
};
use ui::palette::{CommandPalette, PaletteItem, PaletteMode};

impl EditorApp {
    /// 命令模式（Ctrl+Shift+P）：打开命令面板；已打开则关闭（toggle）。
    pub(super) fn toggle_command_palette(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let items = self.command_palette_items(cx);
        self.open_palette(PaletteMode::Commands, items, window, cx);
    }

    /// 文件模式（Ctrl+P）：打开文件快选（已打开文件 + 「打开文件…」系统对话框入口）。
    pub(super) fn quick_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let items = self.file_palette_items(cx);
        self.open_palette(PaletteMode::Files, items, window, cx);
    }

    /// 创建面板实体 + 聚焦查询框 + 订阅关闭（关闭即清引用；与新开面板同生命周期）。
    fn open_palette(
        &mut self,
        mode: PaletteMode,
        items: Vec<PaletteItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_focus = window.focused(cx);
        let palette = cx.new(|cx| CommandPalette::new(window, cx, mode, items, previous_focus));
        let sub = cx.subscribe(&palette, |this, _e, _: &DismissEvent, cx| {
            // 面板自关（选择/Esc/点击外）→ 清空持有引用，实体随之释放。
            this.palette = None;
            this._palette_sub = None;
            cx.notify();
        });
        // 打开时聚焦顶部查询输入框（键盘直接流入面板，不再冒泡给编辑器）。
        let input_handle = palette.read(cx).input_handle(cx);
        window.focus(&input_handle, cx);
        self.palette = Some(palette);
        self._palette_sub = Some(sub);
    }

    /// 关闭当前面板（toggle 复用：让面板走 Esc 同款路径归还焦点）。
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.palette.clone() {
            p.update(cx, |pal, cx| pal.dismiss(window, cx));
        }
    }

    /// 命令列表：与顶部菜单共享公共命令表（`crate::commands`）的 label/快捷键单一真值。
    fn command_palette_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let weak = cx.weak_entity();
        macro_rules! command {
            ($category:expr, $cmd:expr, $handler:expr) => {{
                let w = weak.clone();
                let handler: fn(&mut EditorApp, &mut Window, &mut Context<EditorApp>) = $handler;
                PaletteItem::new(
                    $cmd.label,
                    $category,
                    $cmd.shortcut,
                    move |window, cx| {
                        let _ = w.update(cx, |this, cx| handler(this, window, cx));
                    },
                )
            }};
        }
        vec![
            // 文件
            command!("文件", NEW_FILE, |this, window, cx| {
                this.new_tab(window, cx)
            }),
            command!("文件", OPEN_FILE, |this, window, cx| {
                this.open_dialog(window, cx)
            }),
            command!("文件", OPEN_FOLDER, |this, window, cx| {
                this.open_folder_dialog(window, cx)
            }),
            command!("文件", SAVE, |this, window, cx| {
                this.save(window, cx)
            }),
            command!("文件", SAVE_AS, |this, window, cx| {
                this.save_as(window, cx)
            }),
            command!("文件", QUIT, |_this, _window, cx| cx.quit()),
            // 编辑
            command!("编辑", UNDO, |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Undo), cx);
            }),
            command!("编辑", REDO, |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Redo), cx);
            }),
            command!("编辑", CUT, |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Cut), cx);
            }),
            command!("编辑", COPY, |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(Copy), cx);
            }),
            command!("编辑", PASTE, |this, window, cx| {
                this.paste_normalized(window, cx)
            }),
            command!("编辑", SELECT_ALL, |this, window, cx| {
                Self::focus_active_editor(this, window, cx);
                window.dispatch_action(Box::new(SelectAll), cx);
            }),
            // 查看
            command!("查看", PREVIEW, |this, window, cx| {
                let idx = this.active_tab;
                this.toggle_preview_for_tab(idx, window, cx);
            }),
            command!("查看", TOGGLE_SIDEBAR, |this, _window, cx| {
                this.toggle_sidebar(cx)
            }),
            command!("查看", INCREASE_FONT, |this, _window, cx| {
                this.change_font_size(1.0, cx)
            }),
            command!("查看", DECREASE_FONT, |this, _window, cx| {
                this.change_font_size(-1.0, cx)
            }),
            command!("查看", RESET_FONT, |this, _window, cx| {
                this.reset_font_size(cx)
            }),
            command!("查看", TOGGLE_WRAP, |this, window, cx| {
                this.toggle_wrap(window, cx)
            }),
            command!("设置", THEME, |this, window, cx| {
                this.toggle_theme(window, cx)
            }),
            command!("设置", FOLLOW_THEME, |this, _window, cx| {
                this.follow_system_theme(cx)
            }),
        ]
    }

    /// 文件模式条目：已打开选项卡（名称 + 路径提示），末尾追加「打开文件…」入口。
    ///
    /// 复用侧边栏搜索的「文件名匹配 + 点击激活选项卡」语义（`sidebar_search_file_rows`），
    /// 但只列**已打开文件**，不做全文检索（那是侧栏搜索的职责）；过滤走面板过滤。
    fn file_palette_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let weak = cx.weak_entity();
        let mut items: Vec<PaletteItem> = Vec::with_capacity(self.tabs.len() + 1);
        for (idx, tab) in self.tabs.iter().enumerate() {
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let detail = tab
                .file_path
                .as_ref()
                .map(|p| {
                    p.parent()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| p.display().to_string())
                })
                .unwrap_or_else(|| "未保存的新文件".to_string());
            let w = weak.clone();
            items.push(PaletteItem::new(name, detail, "", move |window, cx| {
                let _ = w.update(cx, |this, cx| this.activate_tab(idx, window, cx));
            }));
        }
        let w = weak.clone();
        items.push(PaletteItem::new("打开文件…", "系统文件对话框", "Ctrl+O", move |window, cx| {
            let _ = w.update(cx, |this, cx| this.open_dialog(window, cx));
        }));
        items
    }
}
