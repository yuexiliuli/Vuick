//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器（多文件选项卡）。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 选项卡栏：每个文件一个选项卡，可新建（+）、切换、关闭（未保存时弹窗提示保存）
//! - 编辑区：多行文本编辑器（[`input::TextArea`]），可编辑任意文本 / 代码文件
//! - 预览区（开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//! - 状态栏：文件名 / 脏标记 / 语言徽标 / 行列 / 行数 / 字数 / 字号调节
//!
//! 打开 / 保存 / 另存为均调用**系统原生文件对话框**（`rfd`），无需手动输入路径；
//! 也支持把文件从资源管理器拖拽到窗口/编辑区打开。

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, point, px, rgb, rgba, white, App, AppContext, Context, CursorStyle, ExternalPaths,
    FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    VisualContext, Window,
};

use super::markdown::{parse_markdown, render_markdown_blocks, Block, MarkdownTheme, TableMode};
use super::scrollbar::{Scrollbar, TableScrollState};
use super::input::{self, FileDropped, TextArea, TextEditEvent};

/// 编辑区背景色（亮色）。
const EDIT_BG: gpui::Hsla = white();
/// 编辑器默认正文字号（px）。
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// 字号调节范围（px）。
const FONT_MIN: f32 = 10.0;
const FONT_MAX: f32 = 28.0;
/// 行号槽宽度。
const GUTTER_W: Pixels = px(46.);
/// 菜单栏高度。
const MENU_BAR_H: Pixels = px(26.);
/// 菜单项宽度。
const MENU_ITEM_W: Pixels = px(56.);
/// 下拉菜单面板宽度。
const MENU_PANEL_W: Pixels = px(220.);

/// 顶部菜单标识。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuId {
    File,
    Edit,
    View,
    Help,
}

/// 一个文件选项卡：独立持有编辑区实体、文件路径与脏标记。
struct EditorTab {
    /// 本选项卡的编辑区
    editor: gpui::Entity<TextArea>,
    /// 当前文件路径（None = 未命名）
    file_path: Option<PathBuf>,
    /// 内容自上次打开/保存后是否被修改
    dirty: bool,
    /// 预览区 Markdown 解析缓存（内容不变时复用）
    md_cache: Option<(SharedString, Vec<Block>)>,
    /// 是否开启 Markdown 预览
    preview_on: bool,
    /// 监听编辑事件 → 置脏（保持有效）
    _edit_sub: Subscription,
    /// 监听文件拖放 → 打开文件（保持有效）
    _drop_sub: Subscription,
    /// 观察编辑区 → 状态栏/预览实时刷新（保持有效）
    _observe_sub: Subscription,
}

pub struct EditorApp {
    /// 所有文件选项卡
    tabs: Vec<EditorTab>,
    /// 当前激活的选项卡下标
    active_tab: usize,
    /// 预览区滚动句柄（切换选项卡时重置）
    preview_scroll: gpui::ScrollHandle,
    /// 预览表格横向滚动状态
    preview_table: TableScrollState,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 编辑器正文字号（px，用户可调）
    font_size: f32,
    /// 当前展开的下拉菜单（None = 无）
    open_menu: Option<MenuId>,
    /// 下拉菜单中当前悬停的条目（用于高亮）
    menu_hover: Option<(MenuId, usize)>,
    focus_handle: FocusHandle,
}

impl Focusable for EditorApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EditorApp {
    /// 创建并注册 `EditorApp` 实体（在 App 上下文中使用）。
    pub fn build(cx: &mut App) -> gpui::Entity<EditorApp> {
        cx.new(|cx| Self::new(cx))
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.editor.read(cx).focus_handle.clone())
            .unwrap_or_else(|| self.focus_handle.clone())
    }

    /// 构造（测试也可复用）：初始创建一个空白选项卡。
    pub fn new(cx: &mut Context<Self>) -> EditorApp {
        let focus_handle = cx.focus_handle();
        let mut app = EditorApp {
            tabs: Vec::new(),
            active_tab: 0,
            preview_scroll: gpui::ScrollHandle::new(),
            preview_table: TableScrollState::new(),
            status: SharedString::from("就绪"),
            status_ok: true,
            font_size: DEFAULT_FONT_SIZE,
            open_menu: None,
            menu_hover: None,
            focus_handle,
        };
        app.make_tab(cx);

        // 命令行参数：第一个参数作为初始打开文件
        let mut args = std::env::args_os();
        args.next(); // 跳过程序名
        if let Some(first) = args.next() {
            let p = PathBuf::from(&first);
            if p.is_file() {
                app.open_file(&p.to_string_lossy(), cx);
            }
        }
        app
    }

    // ==================== 选项卡管理 ====================

    /// 新建一个空白选项卡并设为激活。返回其下标。
    fn make_tab(&mut self, cx: &mut Context<Self>) -> usize {
        let focus = cx.focus_handle();
        let editor = cx.new(|_cx| {
            TextArea::new(
                focus,
                SharedString::from("在此输入文本，或用菜单/拖放打开文件…"),
                SharedString::from(""),
                true,
            )
        });

        // 编辑内容被修改 → 置脏
        let ed = editor.clone();
        let edit_sub = cx.subscribe(&ed, |this, emitter, _: &TextEditEvent, cx| {
            if let Some(tab) = this.tabs.iter_mut().find(|t| t.editor == emitter) {
                if !tab.dirty {
                    tab.dirty = true;
                    cx.notify();
                }
            }
        });
        // 文件拖放到编辑区 → 打开该文件
        let drop_sub = cx.subscribe(&ed, |this, emitter, ev: &FileDropped, cx| {
            if let Some(idx) = this.tabs.iter().position(|t| t.editor == emitter) {
                this.open_file_in_tab(idx, &ev.0.to_string_lossy(), cx);
            }
        });
        // 观察编辑区：光标移动/内容变更都刷新应用（状态栏行列、实时预览）
        let observe_sub = cx.observe(&ed, |_this, _ed, cx| cx.notify());

        self.tabs.push(EditorTab {
            editor,
            file_path: None,
            dirty: false,
            md_cache: None,
            preview_on: false,
            _edit_sub: edit_sub,
            _drop_sub: drop_sub,
            _observe_sub: observe_sub,
        });
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        self.preview_scroll = gpui::ScrollHandle::new();
        self.preview_table = TableScrollState::new();
        idx
    }

    /// 新建文件（新增选项卡）。
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.make_tab(cx);
        window.focus(&self.tabs[idx].editor.read(cx).focus_handle.clone());
        self.status = SharedString::from("新建文件");
        self.status_ok = true;
        cx.notify();
    }

    /// 切换到指定选项卡。
    fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.active_tab = idx;
        self.preview_scroll = gpui::ScrollHandle::new();
        self.preview_table = TableScrollState::new();
        self.menu_hover = None;
        window.focus(&self.tabs[idx].editor.read(cx).focus_handle.clone());
        cx.notify();
    }

    /// 关闭选项卡。未保存时先弹原生确认框（保存 / 不保存 / 取消）。
    fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        if !self.tabs[idx].dirty {
            self.remove_tab(idx, window, cx);
            return;
        }
        // 未保存 → 弹原生任务对话框，异步等待用户选择
        let entity = cx.entity();
        let answer = window.prompt(
            PromptLevel::Warning,
            "未保存的更改",
            Some("此文件尚未保存，关闭前要保存吗？"),
            &["保存", "不保存", "cancel"],
            cx,
        );
        window.spawn(cx, async move |cx| {
            // oneshot 通道被取消视为"取消"
            let idx_ans = answer.await.unwrap_or(2);
            let _ = cx.update_window_entity(&entity, |this, window, cx| match idx_ans {
                0 => {
                    if this.save_tab(idx, cx) {
                        this.remove_tab(idx, window, cx);
                    }
                }
                1 => this.remove_tab(idx, window, cx),
                _ => {}
            });
        }).detach();
    }

    /// 真正移除选项卡。最后一个选项卡退化为"重置为空白"。
    fn remove_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            let tab = &mut self.tabs[0];
            tab.editor.update(cx, |ed, cx| {
                ed.content = SharedString::from("");
                ed.selected_range = 0..0;
                ed.scroll_handle.set_offset(point(px(0.), px(0.)));
                cx.notify();
            });
            tab.file_path = None;
            tab.dirty = false;
            tab.md_cache = None;
            tab.preview_on = false;
            self.preview_scroll = gpui::ScrollHandle::new();
            self.preview_table = TableScrollState::new();
            window.focus(&tab.editor.read(cx).focus_handle.clone());
            self.status = SharedString::from("已关闭");
            self.status_ok = true;
            cx.notify();
            return;
        }
        self.tabs.remove(idx);
        if idx < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        self.activate_tab(self.active_tab, window, cx);
    }

    // ==================== 文件操作（系统对话框） ====================

    /// 打开文件到指定选项卡。
    fn open_file_in_tab(&mut self, idx: usize, path: &str, cx: &mut Context<Self>) {
        let p = PathBuf::from(path.trim());
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        match fs::read_to_string(&p) {
            Ok(text) => {
                let len = text.len();
                tab.editor.update(cx, |ed, cx| {
                    ed.content = SharedString::from(text);
                    ed.selected_range = len..len;
                    ed.scroll_handle.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                });
                tab.file_path = Some(p.clone());
                tab.md_cache = None;
                tab.dirty = false;
                if Self::is_markdown_path(&p) {
                    tab.preview_on = true;
                }
                self.preview_scroll = gpui::ScrollHandle::new();
                self.preview_table = TableScrollState::new();
                self.status = SharedString::from(format!("已打开：{}", p.display()));
                self.status_ok = true;
                cx.notify();
            }
            Err(e) => {
                self.status = SharedString::from(format!("打开失败：{e}"));
                self.status_ok = false;
                cx.notify();
            }
        }
    }

    /// 打开文件（到当前激活选项卡）。
    fn open_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.open_file_in_tab(self.active_tab, path, cx);
    }

    /// 打开：系统原生"选择文件"对话框。
    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new().set_title("打开文件").pick_file() else {
            return;
        };
        let idx = self.active_tab;
        self.open_file_in_tab(idx, &path.to_string_lossy(), cx);
    }

    /// 保存指定选项卡。无路径时转入"另存为"。
    /// 返回是否保存成功（用户取消对话框返回 false）。
    fn save_tab(&mut self, idx: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get(idx) else {
            return false;
        };
        let Some(p) = tab.file_path.clone() else {
            return self.save_tab_as(idx, cx);
        };
        let text = tab.editor.read(cx).content.to_string();
        match fs::write(&p, text) {
            Ok(()) => {
                self.tabs[idx].dirty = false;
                self.status = SharedString::from(format!("已保存：{}", p.display()));
                self.status_ok = true;
                cx.notify();
                true
            }
            Err(e) => {
                self.status = SharedString::from(format!("保存失败：{e}"));
                self.status_ok = false;
                cx.notify();
                false
            }
        }
    }

    /// 另存为：系统原生"保存文件"对话框。
    fn save_tab_as(&mut self, idx: usize, cx: &mut Context<Self>) -> bool {
        let Some(file) = rfd::FileDialog::new()
            .set_title("保存文件")
            .set_file_name(&self.tab_file_name(idx))
            .save_file()
        else {
            return false;
        };
        let Some(tab) = self.tabs.get(idx) else {
            return false;
        };
        let text = tab.editor.read(cx).content.to_string();
        match fs::write(&file, text) {
            Ok(()) => {
                self.tabs[idx].file_path = Some(file.clone());
                self.tabs[idx].dirty = false;
                self.status = SharedString::from(format!("已保存：{}", file.display()));
                self.status_ok = true;
                cx.notify();
                true
            }
            Err(e) => {
                self.status = SharedString::from(format!("保存失败：{e}"));
                self.status_ok = false;
                cx.notify();
                false
            }
        }
    }

    /// 保存当前选项卡（Ctrl+S / 菜单）。
    fn save(&mut self, cx: &mut Context<Self>) {
        self.save_tab(self.active_tab, cx);
    }

    /// 另存为当前选项卡。
    fn save_as(&mut self, cx: &mut Context<Self>) {
        self.save_tab_as(self.active_tab, cx);
    }

    /// 选项卡默认文件名（用于保存对话框）。
    fn tab_file_name(&self, idx: usize) -> String {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名.txt".into())
    }

    // ==================== 状态查询 ====================

    /// 当前激活选项卡是否开启预览。
    fn show_preview(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.preview_on)
            .unwrap_or(false)
    }

    fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
    }

    /// 当前文件显示名（无文件时为"未命名"）。
    fn file_display_name(&self) -> String {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string())
    }

    /// 按扩展名推断语言标签（仅用于状态栏展示）。
    fn language_label(&self) -> &'static str {
        let Some(p) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
        else {
            return "纯文本";
        };
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "Markdown",
            "rs" => "Rust",
            "toml" => "TOML",
            "json" => "JSON",
            "py" => "Python",
            "js" | "ts" | "jsx" | "tsx" => "JavaScript",
            "html" | "htm" => "HTML",
            "css" | "scss" => "CSS",
            "c" | "h" => "C",
            "cpp" | "cc" | "hpp" => "C++",
            "go" => "Go",
            "java" => "Java",
            "sh" | "bash" => "Shell",
            "yaml" | "yml" => "YAML",
            "xml" => "XML",
            "sql" => "SQL",
            "txt" => "纯文本",
            _ => "文本",
        }
    }

    /// 调节编辑器字号并刷新预览（范围约束见 FONT_MIN/FONT_MAX）。
    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + delta).max(FONT_MIN).min(FONT_MAX);
        cx.notify();
    }

    /// 关闭所有下拉菜单。
    fn close_menus(&mut self, cx: &mut Context<Self>) {
        if self.open_menu.is_some() {
            self.open_menu = None;
            self.menu_hover = None;
            cx.notify();
        }
    }

    /// 切换指定下拉菜单的展开状态。
    fn toggle_menu(&mut self, id: MenuId, cx: &mut Context<Self>) {
        self.open_menu = if self.open_menu == Some(id) { None } else { Some(id) };
        self.menu_hover = None;
        cx.notify();
    }

    // ==================== 菜单栏 ====================

    /// 菜单栏中各按钮的水平偏移（与布局一致：px_1 内边距 + 等宽按钮 + gap_1）。
    fn menu_button_left(id: MenuId) -> Pixels {
        let idx = match id {
            MenuId::File => 0,
            MenuId::Edit => 1,
            MenuId::View => 2,
            MenuId::Help => 3,
        };
        px(4.) + (MENU_ITEM_W + px(4.)) * idx as f32
    }

    /// 顶部菜单栏：文件 / 编辑 / 查看 / 帮助。
    fn render_menu_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.open_menu;
        let make_button = |id: MenuId, label: &'static str, cx: &mut Context<Self>| {
            let is_open = open == Some(id);
            div()
                .id(SharedString::from(format!("menu-btn-{id:?}")))
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
                .cursor(CursorStyle::PointingHand)
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                        this.toggle_menu(id, cx);
                        cx.stop_propagation();
                    }),
                )
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

    /// 菜单项分组：`(菜单, 标题, 条目列表[(标签, 快捷键)])`。
    fn menu_definitions(&self) -> Vec<(MenuId, &'static str, Vec<(&'static str, &'static str)>)> {
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
            (
                MenuId::Help,
                "帮助",
                vec![("关于", "")],
            ),
        ]
    }

    /// 下拉菜单面板（作为根布局最后一层、绝对定位绘制，覆盖在编辑区之上）。
    fn render_menu_dropdown(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                panel_children.push(
                    div().my_1().h(px(1.)).bg(rgb(0xe5e7eb)).into_any_element(),
                );
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

    /// 下拉菜单条目 → 具体操作。
    fn dispatch_menu_action(
        &mut self,
        menu: MenuId,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let defs = self.menu_definitions();
        let Some(items) = defs.iter().find(|(id, _, _)| *id == menu).map(|(_, _, i)| i) else {
            return;
        };
        if items.get(idx).is_none() {
            return;
        }
        match menu {
            MenuId::File => match idx {
                0 => self.new_tab(window, cx),
                1 => self.open_dialog(cx),
                2 => self.save(cx),
                3 => self.save_as(cx),
                5 => cx.quit(),
                _ => {}
            },
            MenuId::Edit => match idx {
                0 => window.dispatch_action(Box::new(input::Undo), cx),
                1 => window.dispatch_action(Box::new(input::Redo), cx),
                3 => window.dispatch_action(Box::new(input::Cut), cx),
                4 => window.dispatch_action(Box::new(input::Copy), cx),
                5 => window.dispatch_action(Box::new(input::Paste), cx),
                7 => window.dispatch_action(Box::new(input::SelectAll), cx),
                _ => {}
            },
            MenuId::View => match idx {
                0 => {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        tab.preview_on = !tab.preview_on;
                        self.preview_scroll = gpui::ScrollHandle::new();
                        self.preview_table = TableScrollState::new();
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
                self.status = SharedString::from("GPUI 编辑器 v0.1.0 — 基于 Rust + GPUI 0.2.2");
                self.status_ok = true;
                cx.notify();
            }
        }
    }

    // ==================== 选项卡栏 ====================

    /// 选项卡栏：每个选项卡显示文件名 + 脏标记 + 关闭按钮，右侧是"新建"按钮。
    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;
        let mut tabs: Vec<gpui::AnyElement> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == active;
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let dirty = tab.dirty;
            let idx = i;

            let close = div()
                .id(SharedString::from(format!("tab-close-{idx}")))
                .w(px(18.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_size(px(13.))
                .text_color(rgb(0x9ca3af))
                .cursor(CursorStyle::PointingHand)
                .child("×")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                        this.close_tab(idx, window, cx);
                        cx.stop_propagation();
                    }),
                );

            tabs.push(
                div()
                    .id(SharedString::from(format!("tab-{idx}")))
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .bg(if is_active { rgb(0xffffff) } else { rgba(0x00000000) })
                    .border_1()
                    .border_color(if is_active {
                        rgb(0xd1d5db)
                    } else {
                        rgba(0x00000000)
                    })
                    .cursor(CursorStyle::PointingHand)
                    .text_size(px(12.))
                    .text_color(rgb(0x111827))
                    .child(if dirty { "● " } else { "" })
                    .child(name)
                    .child(close)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                            this.activate_tab(idx, window, cx);
                        }),
                    )
                    .into_any_element(),
            );
        }

        let add = div()
            .id("tab-add")
            .w(px(24.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .bg(rgb(0xe5e7eb))
            .text_size(px(15.))
            .text_color(rgb(0x374151))
            .cursor(CursorStyle::PointingHand)
            .child("+")
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, window, cx| this.new_tab(window, cx)),
            );

        div()
            .id("tab-bar")
            .h(px(34.))
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_1()
            .bg(rgb(0xf3f4f6))
            .border_b_1()
            .border_color(rgb(0xe5e7eb))
            .children(tabs)
            .child(add)
    }

    // ==================== 编辑区 / 行号槽 ====================

    fn render_editor_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.tabs[self.active_tab].editor.clone();
        let handle = editor.read(cx).scroll_handle.clone();
        div()
            .id("editor-area")
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_row()
            .bg(EDIT_BG)
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .rounded_md()
            .overflow_hidden()
            .child(
                // 编辑区滚动容器：行号槽 + 编辑器作为普通流子元素，共用同一个句柄
                // 一起平移，行号与正文永远不会被 clamping / scroll-to-active 拆散。
                div()
                    .id("editor-scroll")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .flex()
                    .flex_row()
                    .py_2()
                    .child(self.render_line_gutter(&editor, cx))
                    .child(
                        div()
                            .id("editor-text-wrap")
                            .flex_1()
                            .min_w(px(0.))
                            .px_3()
                            .font_family("Consolas")
                            .text_size(px(self.font_size))
                            .line_height(gpui::relative(1.5))
                            .child(editor),
                    ),
            )
    }

    /// 行号槽：普通流布局逐行堆叠行号，随外层滚动容器与正文同步平移。
    ///
    /// 顶部内边距补上 TextArea 在短内容（`min_h(40)` 盒子垂直居中）时的偏移，
    /// 否则行号会比正文"偏上"。
    fn render_line_gutter(&mut self, editor: &gpui::Entity<TextArea>, cx: &mut Context<Self>) -> impl IntoElement {
        let ed = editor.read(cx);
        let lh = ed.last_line_height;
        let offsets = ed.last_visual_offsets.clone();
        let total = ed.last_content_height;
        let lines = ed.last_layout.as_ref().map(|l| l.len()).unwrap_or(0);

        // TextArea 在内容不足一行（或很短）时会在 `min_h(40)` 盒子里垂直居中
        let center_offset = ((px(40.) - ed.last_content_height) / 2.0).max(px(0.));

        let mut numbers: Vec<gpui::Div> = Vec::new();
        for i in 0..lines {
            let top = offsets.get(i).copied().unwrap_or(px(0.));
            let bottom = offsets.get(i + 1).copied().unwrap_or(total);
            let row_h = (bottom - top).max(lh);
            numbers.push(
                div()
                    .debug_selector(move || format!("gutter-line-{}", i + 1))
                    .h(row_h)
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr_2()
                    .text_size(px(12.))
                    .text_color(rgb(0x9ca3af))
                    .child(format!("{}", i + 1)),
            );
        }

        div()
            .id("editor-gutter")
            .w(GUTTER_W)
            .flex_shrink_0()
            .pt(center_offset)
            .pb_2()
            .border_r_1()
            .border_color(rgb(0xe5e7eb))
            .bg(rgb(0xfafafa))
            .font_family("Consolas")
            .children(numbers)
    }

    // ==================== 预览区 ====================

    fn render_preview_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 实时预览：内容不变时复用上一次解析出的块列表，避免每帧重复解析大文档
        let tab = &self.tabs[self.active_tab];
        let md = tab.editor.read(cx).content.clone();
        let blocks = match &tab.md_cache {
            Some((cached_md, cached_blocks)) if *cached_md == md => cached_blocks.clone(),
            _ => {
                let parsed = parse_markdown(&md);
                let parsed_clone = parsed.clone();
                let t = &mut self.tabs[self.active_tab];
                t.md_cache = Some((md.clone(), parsed_clone));
                parsed
            }
        };
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w(px(0.))
            // 预览区也可接收文件拖放
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(|this: &mut EditorApp, paths: &Arc<ExternalPaths>, _w, cx| {
                if let Some(p) = paths.paths().first() {
                    this.open_file(&p.to_string_lossy(), cx);
                }
            }))
            .child(
                div()
                    .id("preview-pane")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.preview_scroll)
                    .bg(rgb(0xf8f9fa))
                    .px_4()
                    .py_3()
                    .text_size(px(14.))
                    .child(render_markdown_blocks(
                        &blocks,
                        &MarkdownTheme::light(),
                        TableMode::Wrap,
                        Some(&self.preview_table),
                        Some(window),
                        None,
                    )),
            )
            .child(Scrollbar::new(self.preview_scroll.clone()))
    }

    // ==================== 状态栏 ====================

    /// 底部状态栏：文件名/语言/脏标记 + 行列/行数/字数 + 字号调节。
    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ed = self.tabs.get(self.active_tab).map(|t| t.editor.clone());
        let (ln, col) = if let Some(ed) = &ed {
            let e = ed.read(cx);
            let (l, c) = e.line_col(e.cursor_offset());
            (l + 1, c)
        } else {
            (1, 0)
        };
        let (line_count, char_count) = if let Some(ed) = &ed {
            let e = ed.read(cx);
            (e.line_ranges().len(), e.content.chars().count())
        } else {
            (0, 0)
        };
        let dirty = self.tabs.get(self.active_tab).map(|t| t.dirty).unwrap_or(false);
        let language = self.language_label().to_string();

        // 左侧：文件名 + 脏标记 + 语言徽标
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w(px(0.))
            .flex_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if dirty {
                                rgb(0xdc2626)
                            } else {
                                rgb(0x9ca3af)
                            })
                            .child(if dirty { "●" } else { "" }),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0x111827))
                            .child(self.file_display_name()),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(0xeef2ff))
                    .text_size(px(11.))
                    .text_color(rgb(0x4338ca))
                    .child(language),
            );

        // 右侧：统计信息 + 字号控制
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0x374151))
                    .child(format!("Ln {ln}, Col {col}")),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0x6b7280))
                    .child(format!("{line_count} 行")),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0x6b7280))
                    .child(format!("{char_count} 字")),
            )
            .child(self.status_bar_font_controls(cx));

        div()
            .h(px(28.))
            .bg(rgb(0xffffff))
            .border_t_1()
            .border_color(rgb(0xe5e7eb))
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .child(left)
            .child(right)
    }

    /// 状态栏的字号调节按钮（减小 / 显示 / 增大）。
    fn status_bar_font_controls(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let minus = div()
            .id("font-minus")
            .w(px(22.))
            .h(px(22.))
            .rounded_md()
            .bg(rgb(0xf3f4f6))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(rgb(0x374151))
            .cursor(CursorStyle::PointingHand)
            .child("−")
            .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut EditorApp, _e, _w, cx| {
                this.change_font_size(-1.0, cx)
            }));
        let plus = div()
            .id("font-plus")
            .w(px(22.))
            .h(px(22.))
            .rounded_md()
            .bg(rgb(0xf3f4f6))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(rgb(0x374151))
            .cursor(CursorStyle::PointingHand)
            .child("+")
            .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut EditorApp, _e, _w, cx| {
                this.change_font_size(1.0, cx)
            }));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(minus)
            .child(
                div()
                    .w(px(34.))
                    .text_size(px(12.))
                    .text_color(rgb(0x6b7280))
                    .flex()
                    .justify_center()
                    .child(format!("{}px", self.font_size.round() as i32)),
            )
            .child(plus)
    }
}

impl Render for EditorApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.show_preview() {
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.))
                .gap_2()
                .px_2()
                .py_2()
                .child(self.render_editor_pane(cx))
                .child(self.render_preview_pane(window, cx))
        } else {
            div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .px_2()
                .py_2()
                .child(self.render_editor_pane(cx))
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0xf3f4f6))
            .track_focus(&self.focus_handle(cx))
            // 任意空白处点击 → 关闭已展开的下拉菜单
            .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut EditorApp, _ev, _w, cx| {
                this.close_menus(cx)
            }))
            // 从资源管理器拖入文件到非编辑区 → 直接打开（编辑区由 TextArea 自己处理）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(|this: &mut EditorApp, paths: &Arc<ExternalPaths>, _w, cx| {
                if let Some(p) = paths.paths().first() {
                    this.open_file(&p.to_string_lossy(), cx);
                }
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::Save, _w, cx| {
                this.save(cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::NewFile, window, cx| {
                this.new_tab(window, cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::OpenFile, _w, cx| {
                this.open_dialog(cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::TogglePreview, _w, cx| {
                if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                    tab.preview_on = !tab.preview_on;
                    this.preview_scroll = gpui::ScrollHandle::new();
                    this.preview_table = TableScrollState::new();
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::IncreaseFontSize, _w, cx| {
                this.change_font_size(1.0, cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::DecreaseFontSize, _w, cx| {
                this.change_font_size(-1.0, cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::ResetFontSize, _w, cx| {
                this.font_size = DEFAULT_FONT_SIZE;
                cx.notify();
            }))
            .child(self.render_menu_bar(cx))
            .child(self.render_tab_bar(cx))
            .child(body)
            .child(self.render_status_bar(cx))
            // 下拉菜单覆盖层放在最后一层，保证绘制在编辑区之上
            .child(self.render_menu_dropdown(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// 行号槽对齐：单行短内容（TextArea 垂直居中）与多行滚动后，行号都应与正文对齐。
    #[gpui::test]
    async fn gutter_numbers_align_with_text(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, cx| EditorApp::new(cx));
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 单行短内容（触发 TextArea 垂直居中，正是行号"偏上"的历史场景）
        view.update(cx, |app, cx| {
            let ed = app.tabs[app.active_tab].editor.clone();
            ed.update(cx, |ed, cx| {
                ed.content = "hello world".into();
                ed.selected_range = 11..11;
                cx.notify();
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 行号 1 顶部应等于编辑区文本元素顶部（last_bounds 已含居中偏移）
        let gutter_top = cx.debug_bounds("gutter-line-1").map(|b| b.top());
        let text_top = cx.update(|_w, cx| {
            view.read(cx).tabs[view.read(cx).active_tab]
                .editor
                .read(cx)
                .last_bounds
                .map(|b| b.top())
        });
        let (gutter_top, text_top) = (gutter_top.unwrap(), text_top.unwrap());
        assert!(
            (gutter_top - text_top).abs() < px(1.),
            "单行内容行号应对齐正文首行：gutter={gutter_top:?} text={text_top:?}"
        );

        // 多行内容 + 滚动：行号应随正文同步平移
        view.update(cx, |app, cx| {
            let content = (0..30)
                .map(|i| format!("第{i}行内容 xyz"))
                .collect::<Vec<_>>()
                .join("\n");
            let ed = app.tabs[app.active_tab].editor.clone();
            ed.update(cx, |ed, cx| {
                ed.content = content.into();
                ed.selected_range = 0..0;
                cx.notify();
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let handle = cx.update(|_w, cx| {
            view.read(cx).tabs[view.read(cx).active_tab]
                .editor
                .read(cx)
                .scroll_handle
                .clone()
        });
        handle.set_offset(gpui::point(px(0.), px(-120.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let gutter_10 = cx.debug_bounds("gutter-line-10").map(|b| b.top());
        let (text_top, offsets) = cx.update(|_w, cx| {
            let ed = view.read(cx).tabs[view.read(cx).active_tab].editor.read(cx);
            (ed.last_bounds.map(|b| b.top()), ed.last_visual_offsets.clone())
        });
        let expected = text_top.unwrap() + offsets[9];
        assert!(
            (gutter_10.unwrap() - expected).abs() < px(1.),
            "滚动后第 10 行行号应对齐正文：gutter={gutter_10:?} expected={expected:?}"
        );
    }

    /// 选项卡：新建文件产生新选项卡，内容彼此独立，且脏标记正确。
    #[gpui::test]
    async fn tabs_are_independent(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, cx| EditorApp::new(cx));
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 第 1 个选项卡输入内容
        let editor0 = cx.update(|_w, cx| view.read(cx).tabs[0].editor.clone());
        editor0.update(cx, |ed, cx| {
            ed.content = "hello".into();
            ed.selected_range = 5..5;
            cx.notify();
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 新建选项卡
        cx.update(|window, cx| {
            view.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (count, active, tab1_dirty, tab0_content) = cx.update(|_w, cx| {
            let app = view.read(cx);
            (
                app.tabs.len(),
                app.active_tab,
                app.tabs[app.active_tab].dirty,
                app.tabs[0].editor.read(cx).content.to_string(),
            )
        });
        assert_eq!(count, 2, "新建后应有 2 个选项卡");
        assert_eq!(active, 1, "新选项卡应被激活");
        assert!(!tab1_dirty, "新选项卡初始未修改");
        assert_eq!(tab0_content, "hello", "第 1 个选项卡内容应保留");
    }

    /// 冒烟测试：完整渲染整棵 UI 树（菜单/选项卡/编辑区/状态栏），
    /// 展开下拉菜单并打开一个临时文件，确保渲染管线无 panic。
    #[gpui::test]
    async fn renders_and_opens_menu(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, cx| EditorApp::new(cx));
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 展开"文件"菜单并渲染（覆盖 render_menu_dropdown 全路径）
        view.update(cx, |app, cx| {
            app.toggle_menu(MenuId::File, cx);
            app.toggle_menu(MenuId::View, cx);
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        view.update(cx, |app, cx| app.close_menus(cx));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 打开一个真实文件，内容进入编辑区，再渲染（覆盖 open_file 路径）
        let dir = std::env::temp_dir().join("gpui-editor-test.md");
        std::fs::write(&dir, "# 标题\n\n**加粗** 正文 `code`\n").unwrap();
        view.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), cx));
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (content, preview) = cx.update(|_w, cx| {
            let app = view.read(cx);
            (
                app.tabs[app.active_tab].editor.read(cx).content.to_string(),
                app.show_preview(),
            )
        });
        assert!(content.contains("标题"), "打开文件后编辑区应有内容: {content}");
        assert!(preview, "打开 .md 文件应自动开启预览");
        let _ = std::fs::remove_file(&dir);
    }

    /// 字号调节 / 预览开关等纯状态操作正常。
    #[gpui::test]
    async fn font_and_preview_toggles(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, cx| EditorApp::new(cx));
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        view.update(cx, |app, cx| {
            app.change_font_size(3.0, cx);
            app.change_font_size(-1.0, cx);
            app.tabs[app.active_tab].preview_on = true;
            cx.notify();
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (font, preview) = cx.update(|_w, cx| {
            let app = view.read(cx);
            (app.font_size, app.show_preview())
        });
        assert_eq!(font, DEFAULT_FONT_SIZE + 2.0);
        assert!(preview, "预览开关打开后 show_preview 应为 true（不依赖 .md 扩展名）");
    }
}
