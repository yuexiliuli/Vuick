//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器（多文件选项卡）。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 选项卡栏：每个文件一个选项卡，可新建（+）、切换、关闭（未保存时弹窗提示保存）
//! - 编辑区：**gpui-component 的 `Input`（CodeEditor 模式）**，内置语法高亮、
//!   行号、缩进参考线、搜索面板、撤销重做与滚动（替换原先自研 `TextArea`）
//! - 预览区（开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//! - 状态栏：文件名 / 脏标记 / 语言徽标 / 行列 / 行数 / 字数 / 字号调节
//!
//! 打开 / 保存 / 另存为均调用**系统原生文件对话框**（`rfd`），无需手动输入路径；
//! 也支持把文件从资源管理器拖拽到窗口/编辑区打开。
//!
//! 文本编辑动作（backspace/ctrl-v/undo…）由 gpui-component 的 `Input` 在 `Input`
//! 键上下文中自行处理，这里只订阅其 `InputEvent::Change` 刷新状态栏与脏标记。

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, px, rgb, rgba, white, App, AppContext, Context, CursorStyle, ExternalPaths, FocusHandle,
    Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, PromptLevel,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, VisualContext, Window,
};
use gpui_component::input::{Editor, EditorState, InputEvent, TabSize};

use crate::actions::*;
use crate::markdown::{parse_markdown, render_markdown_blocks, Block, MarkdownTheme, TableMode};
use crate::scrollbar::{horizontal_scrollbar, Scrollbar, TableScrollState};

/// 编辑区背景色（亮色）。
const EDIT_BG: gpui::Hsla = white();
/// 编辑器默认正文字号（px）。
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// 字号调节范围（px）。
const FONT_MIN: f32 = 10.0;
const FONT_MAX: f32 = 28.0;
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

/// 一个文件选项卡：独立持有 gpui-component 的代码编辑器状态、文件路径与脏标记。
struct EditorTab {
    /// 本选项卡的编辑区（gpui-component `EditorState`）。
    editor: gpui::Entity<EditorState>,
    /// 当前文件路径（None = 未命名）
    file_path: Option<PathBuf>,
    /// 内容自上次打开/保存后是否被修改
    dirty: bool,
    /// 当前内容的镜像缓存（在 `InputEvent::Change` 时同步，避免每帧克隆 Rope）
    content: SharedString,
    /// 行数缓存
    line_count: usize,
    /// 字数缓存
    char_count: usize,
    /// 光标行（0 基）
    cursor_row: usize,
    /// 光标列（0 基）
    cursor_col: usize,
    /// 预览区 Markdown 解析缓存（内容不变时复用）
    md_cache: Option<(SharedString, Vec<Block>)>,
    /// 是否开启 Markdown 预览
    preview_on: bool,
    /// 监听编辑事件 → 置脏 + 刷新状态栏（保持有效）
    _edit_sub: Subscription,
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
    /// 选项卡栏横向滚动状态（标签过多时横向滚动 + 滚动条）
    tab_bar_hscroll: TableScrollState,
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
    /// 组件编辑器内部统一使用 LF，避免 Windows 剪贴板 CRLF 进入文本布局。
    fn normalize_newlines(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// 创建并注册 `EditorApp` 实体（作为窗口的第一层视图，外层再由
    /// `gpui_component::Root` 包裹）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> EditorApp {
        let focus_handle = cx.focus_handle();
        let mut app = EditorApp {
            tabs: Vec::new(),
            active_tab: 0,
            preview_scroll: gpui::ScrollHandle::new(),
            preview_table: TableScrollState::new(),
            tab_bar_hscroll: TableScrollState::new(),
            status: SharedString::from("就绪"),
            status_ok: true,
            font_size: DEFAULT_FONT_SIZE,
            open_menu: None,
            menu_hover: None,
            focus_handle,
        };
        app.make_tab(window, cx);

        // 命令行参数：第一个参数作为初始打开文件
        let mut args = std::env::args_os();
        args.next(); // 跳过程序名
        if let Some(first) = args.next() {
            let p = PathBuf::from(&first);
            if p.is_file() {
                app.open_file(&p.to_string_lossy(), window, cx);
            }
        }
        app
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.editor.read(cx).focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }

    // ==================== 选项卡管理 ====================

    /// 新建一个空白选项卡并设为激活。返回其下标。
    fn make_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .soft_wrap(false)
        });

        // 编辑内容被修改 → 置脏 + 同步内容/行列缓存
        let ed = editor.clone();
        let edit_sub = cx.subscribe(&ed, |this, emitter, ev: &InputEvent, cx| {
            if let Some(tab) = this.tabs.iter_mut().find(|t| t.editor == emitter) {
                match ev {
                    InputEvent::Change => {
                        let state = tab.editor.read(cx);
                        let value = state.value();
                        tab.content = value.clone();
                        tab.line_count = value.lines().count().max(1);
                        tab.char_count = value.chars().count();
                        let pos = state.cursor_position();
                        tab.cursor_row = pos.line as usize;
                        tab.cursor_col = pos.character as usize;
                        if !tab.dirty {
                            tab.dirty = true;
                        }
                        cx.notify();
                    }
                    _ => {}
                }
            }
        });

        self.tabs.push(EditorTab {
            editor,
            file_path: None,
            dirty: false,
            content: SharedString::default(),
            line_count: 1,
            char_count: 0,
            cursor_row: 0,
            cursor_col: 0,
            md_cache: None,
            preview_on: false,
            _edit_sub: edit_sub,
        });
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        self.preview_scroll = gpui::ScrollHandle::new();
        self.preview_table = TableScrollState::new();
        idx
    }

    /// 新建文件（新增选项卡）。
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.make_tab(window, cx);
        let focus = self.tabs[idx].editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
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
        let focus = self.tabs[idx].editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
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
        window
            .spawn(cx, async move |cx| {
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
            })
            .detach();
    }

    /// 真正移除选项卡。最后一个选项卡退化为"重置为空白"。
    fn remove_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            let tab = &mut self.tabs[0];
            tab.editor.update(cx, |ed, cx| {
                ed.set_value("", window, cx);
            });
            tab.file_path = None;
            tab.dirty = false;
            tab.content = SharedString::default();
            tab.line_count = 1;
            tab.char_count = 0;
            tab.cursor_row = 0;
            tab.cursor_col = 0;
            tab.md_cache = None;
            tab.preview_on = false;
            self.preview_scroll = gpui::ScrollHandle::new();
            self.preview_table = TableScrollState::new();
            let focus = tab.editor.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
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
    fn open_file_in_tab(
        &mut self,
        idx: usize,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = PathBuf::from(path.trim());
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        match fs::read_to_string(&p) {
            Ok(text) => {
                let text = Self::normalize_newlines(&text);
                let lang = Self::language_id(&p);
                let lines = text.lines().count().max(1);
                let chars = text.chars().count();
                tab.editor.update(cx, |ed, cx| {
                    ed.set_value(text.clone(), window, cx);
                    ed.set_highlighter(lang, cx);
                });
                tab.file_path = Some(p.clone());
                tab.content = SharedString::from(text);
                tab.line_count = lines;
                tab.char_count = chars;
                tab.cursor_row = 0;
                tab.cursor_col = 0;
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
    fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_file_in_tab(self.active_tab, path, window, cx);
    }

    /// 打开：系统原生"选择文件"对话框。
    fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new().set_title("打开文件").pick_file() else {
            return;
        };
        let idx = self.active_tab;
        self.open_file_in_tab(idx, &path.to_string_lossy(), window, cx);
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
        let text = tab.editor.read(cx).value().to_string();
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
        let text = tab.editor.read(cx).value().to_string();
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

    /// 切换指定选项卡的 Markdown 预览（眼睛图标 / 菜单共用）。
    /// 会先激活该选项卡，再翻转其 `preview_on`，并重置预览滚动状态。
    fn toggle_preview_for_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_tab(idx, window, cx);
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.preview_on = !tab.preview_on;
        }
        self.preview_scroll = gpui::ScrollHandle::new();
        self.preview_table = TableScrollState::new();
        cx.notify();
    }

    fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false)
    }

    /// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
    fn language_id(p: &Path) -> &'static str {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "markdown",
            "rs" => "rust",
            "toml" => "toml",
            "json" => "json",
            "py" => "python",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "html" | "htm" => "html",
            "css" | "scss" => "css",
            "c" | "h" => "c",
            "cpp" | "cc" | "hpp" => "cpp",
            "go" => "go",
            "java" => "java",
            "sh" | "bash" => "bash",
            "yaml" | "yml" => "yaml",
            "sql" => "sql",
            _ => "text",
        }
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
        self.open_menu = if self.open_menu == Some(id) {
            None
        } else {
            Some(id)
        };
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
            (MenuId::Help, "帮助", vec![("关于", "")]),
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

    /// 下拉菜单条目 → 具体操作。
    fn dispatch_menu_action(
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
                self.status = SharedString::from(
                    "GPUI 编辑器 v0.1.0 — 基于 Rust + GPUI 0.2.2 + gpui-component",
                );
                self.status_ok = true;
                cx.notify();
            }
        }
    }

    /// 从系统剪贴板粘贴前规范化 CRLF；通过 `EditorState::replace` 保持选区替换、
    /// 光标移动和组件库自身的撤销历史语义。
    fn paste_normalized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = clipboard.text() else {
            return;
        };
        let text = Self::normalize_newlines(&text);
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        tab.editor.update(cx, |editor, cx| {
            editor.replace(text, window, cx);
        });
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
                    .flex_shrink(1.)
                    .rounded_md()
                    .bg(if is_active {
                        rgb(0xffffff)
                    } else {
                        rgba(0x00000000)
                    })
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

        // 整条选项卡栏最右侧的眼睛图标：仅当"当前选中的文件"是 Markdown（.md / .markdown）
        // 时才显示，点击切换当前选项卡的 Markdown 预览（与「查看 → 预览」一致）。
        // 用纯 div 画出（椭圆外框 + 中心瞳孔），避免依赖 emoji 字体；蓝色=已开，灰色=已关。
        let active_is_md = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);

        let eye = if active_is_md {
            let on = self.show_preview();
            let eye_color = if on { rgb(0x4338ca) } else { rgb(0x9ca3af) };
            Some(
                div()
                    .id("tab-bar-eye")
                    .debug_selector(|| "tab-bar-eye".into())
                    .w(px(22.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor(CursorStyle::PointingHand)
                    .child(
                        div()
                            .w(px(16.))
                            .h(px(10.))
                            .rounded_full()
                            .border_1()
                            .border_color(eye_color)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().w(px(5.)).h(px(5.)).rounded_full().bg(eye_color)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                            let idx = this.active_tab;
                            this.toggle_preview_for_tab(idx, window, cx);
                        }),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

        // 选项卡栏右侧：固定放"新建(+)"与（按需）眼睛图标；标签过多时由左侧滚动区横向滚动
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(eye) = eye {
            right.push(eye);
        }

        // 横向滚动条只在标签超宽时出现：半透明、贴着标签底部、浮在上方（不额外占高度）
        let mut hscroll_overlay: Vec<gpui::AnyElement> = Vec::new();
        if self.tab_bar_hscroll.handle.max_offset().x > px(0.) {
            hscroll_overlay.push(
                div()
                    .absolute()
                    .left(px(0.))
                    .right(px(0.))
                    .bottom(px(0.))
                    .h(px(0.))
                    .child(horizontal_scrollbar(
                        &self.tab_bar_hscroll,
                        "tab-bar-hscroll",
                    ))
                    .into_any_element(),
            );
        }

        div()
            .id("tab-bar")
            .relative()
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_1()
            .bg(rgb(0xf3f4f6))
            .border_b_1()
            .border_color(rgb(0xe5e7eb))
            // 选项卡横向滚动区：占满剩余宽度；标签不收缩，过多即横向溢出可滚轮/拖拽滚动。
            // 横向滚动条浮在标签底部（见 hscroll_overlay），不改变原 34px 高度。
            .child(
                div()
                    .id("tab-bar-scroll")
                    .relative()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(26.))
                    .child(
                        div()
                            .id("tab-bar-tabs")
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .overflow_x_scroll()
                            .track_scroll(&self.tab_bar_hscroll.handle)
                            .children(tabs),
                    )
                    .children(hscroll_overlay),
            )
            .child(add)
            .children(right)
    }

    // ==================== 编辑区 ====================

    /// 编辑区：gpui-component 的 `Input`（CodeEditor 模式）撑满面板，
    /// 自身负责行号槽、语法高亮、垂直滚动与搜索面板。
    fn render_editor_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.tabs[self.active_tab].editor.clone();
        div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .relative()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(EDIT_BG)
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .rounded_md()
            .overflow_hidden()
            // 文字样式会沿元素树继承，`Input` 的代码编辑器随字号重新排版
            .text_size(px(self.font_size))
            // 从资源管理器拖入文件 → 打开（Input 本身不接收拖放）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &Arc<ExternalPaths>, window, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.open_file(&p.to_string_lossy(), window, cx);
                    }
                },
            ))
            .child(Editor::new(&editor).h(gpui::relative(1.)).bordered(false))
    }

    // ==================== 预览区 ====================

    fn render_preview_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 实时预览：内容不变时复用上一次解析出的块列表，避免每帧重复解析大文档
        let tab = &self.tabs[self.active_tab];
        let md = tab.content.clone();
        let blocks = match &tab.md_cache {
            Some((cached_md, cached_blocks)) if *cached_md == md => cached_blocks.clone(),
            _ => {
                let parsed = parse_markdown(&md);
                let parsed_clone = parsed.clone();
                let t = &mut self.tabs[self.active_tab];
                t.md_cache = Some((md, parsed_clone));
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
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &Arc<ExternalPaths>, window, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.open_file(&p.to_string_lossy(), window, cx);
                    }
                },
            ))
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
        let tab = self.tabs.get(self.active_tab);
        let (ln, col) = tab
            .map(|t| (t.cursor_row + 1, t.cursor_col + 1))
            .unwrap_or((1, 1));
        let (line_count, char_count) = tab.map(|t| (t.line_count, t.char_count)).unwrap_or((0, 0));
        let dirty = tab.map(|t| t.dirty).unwrap_or(false);
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
                            .text_color(if dirty { rgb(0xdc2626) } else { rgb(0x9ca3af) })
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
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _e, _w, cx| this.change_font_size(-1.0, cx)),
            );
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
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _e, _w, cx| this.change_font_size(1.0, cx)),
            );
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _w, cx| this.close_menus(cx)),
            )
            // 从资源管理器拖入文件到非编辑区 → 直接打开（编辑区由 Input 的父面板处理）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &Arc<ExternalPaths>, window, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.open_file(&p.to_string_lossy(), window, cx);
                    }
                },
            ))
            .on_action(cx.listener(|this: &mut EditorApp, _: &Save, _w, cx| this.save(cx)))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &PasteNormalized, window, cx| {
                    this.paste_normalized(window, cx)
                }),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &SaveAs, _w, cx| this.save_as(cx)))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &NewFile, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &OpenFile, window, cx| {
                    this.open_dialog(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &CloseTab, window, cx| {
                    this.close_tab(this.active_tab, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &TogglePreview, _w, cx| {
                    if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                        tab.preview_on = !tab.preview_on;
                        this.preview_scroll = gpui::ScrollHandle::new();
                        this.preview_table = TableScrollState::new();
                    }
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &IncreaseFontSize, _w, cx| {
                    this.change_font_size(1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &DecreaseFontSize, _w, cx| {
                    this.change_font_size(-1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ResetFontSize, _w, cx| {
                    this.font_size = DEFAULT_FONT_SIZE;
                    cx.notify();
                }),
            )
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
    use gpui::{Entity, EntityInputHandler, TestAppContext, VisualTestContext};

    /// 测试前必须初始化 gpui-component（主题、键盘映射等全局状态）。
    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_component::init(cx));
    }

    /// 创建一个以 `gpui_component::Root` 为窗口根视图的编辑应用，
    /// 返回视图实体与该窗口的 `VisualTestContext`（后续用其驱动渲染/事件）。
    fn add_app(cx: &mut TestAppContext) -> (Entity<EditorApp>, &mut VisualTestContext) {
        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<EditorApp>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            *holder.borrow_mut() = Some(app.clone());
            gpui_component::Root::new(app, window, cx)
        });
        let _ = root;
        let app = holder.borrow().clone().unwrap();
        (app, cx)
    }

    /// 选项卡：新建文件产生新选项卡，内容彼此独立，且脏标记正确。
    #[gpui::test]
    async fn tabs_are_independent(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 第 1 个选项卡输入内容
        let editor0 = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        cx.update(|window, cx| {
            editor0.update(cx, |state, cx| state.set_value("hello", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 新建选项卡
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.new_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (count, active, tab1_dirty, tab0_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs.len(),
                a.active_tab,
                a.tabs[a.active_tab].dirty,
                a.tabs[0].editor.read(cx).value(),
            )
        });
        assert_eq!(count, 2, "新建后应有 2 个选项卡");
        assert_eq!(active, 1, "新选项卡应被激活");
        assert!(!tab1_dirty, "新选项卡初始未修改");
        assert_eq!(tab0_content.to_string(), "hello", "第 1 个选项卡内容应保留");
    }

    /// 编辑菜单/快捷键的动作：撤销、重做、粘贴、全选应能到达聚焦的编辑区。
    #[gpui::test]
    async fn edit_actions_dispatch(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 聚焦编辑区并输入内容
        let editor = cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
            app.read(cx).tabs[app.read(cx).active_tab].editor.clone()
        });
        cx.update(|window, cx| {
            editor.update(cx, |state, cx| state.set_value("hello", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 粘贴：写入剪贴板后 Ctrl+V 应把内容插入（这是历史上出现“整行消失”的路径）
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("world".to_string()));
        cx.dispatch_action(gpui_component::input::Paste);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_paste = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_paste, "worldhello",
            "粘贴应在光标处插入（set_value 后光标回到开头），实际={after_paste:?}"
        );

        // 撤销：应撤销粘贴 → 回到插入前（set_value 不进撤销栈，撤销对象是粘贴）
        cx.dispatch_action(gpui_component::input::Undo);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_undo = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_undo, "hello",
            "撤销粘贴后应回到插入前，实际={after_undo:?}"
        );

        // 重做：应恢复粘贴
        cx.dispatch_action(gpui_component::input::Redo);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let after_redo = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            after_redo, "worldhello",
            "重做后内容应恢复，实际={after_redo:?}"
        );

        // 全选：Ctrl+A 应选中全部内容
        cx.dispatch_action(gpui_component::input::SelectAll);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sel = cx.update(|window, cx| {
            editor.update(cx, |state, cx| state.selected_text_range(false, window, cx))
        });
        let sel = sel.expect("全选后应有选区");
        assert_eq!(sel.range.start, 0, "全选应从文档开头开始");
        assert!(
            sel.range.end > 0,
            "全选应覆盖全部内容，实际={:?}",
            sel.range
        );
    }

    /// 回归测试：历史上“在一行反复粘贴同一段文本后整行消失”的复现场景。
    /// gpui-component 的 CodeEditor 以 ropey 存储文本、独立换行管线排版，
    /// 反复粘贴大段内容时内容不应丢失、镜像缓存与输入区保持一致。
    #[gpui::test]
    async fn repeated_pastes_keep_content(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let editor = cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
            app.read(cx).tabs[app.read(cx).active_tab].editor.clone()
        });

        let chunk = "Option<Vec<WrappedLine>>";
        let text = chunk.repeat(300);
        let expected_len = text.len() * 4;

        // 连续粘贴多次（历史上的触发路径）
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        for _ in 0..4 {
            cx.dispatch_action(gpui_component::input::Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 内容与镜像缓存一致、不丢失不错乱
        let (value, cached, dirty) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let tab = &a.tabs[a.active_tab];
            (
                editor.read(cx).value().to_string(),
                tab.content.to_string(),
                tab.dirty,
            )
        });
        assert_eq!(
            value.len(),
            expected_len,
            "反复粘贴后内容不得丢失，实际长度={} 期望={expected_len}",
            value.len()
        );
        assert!(value.contains(chunk), "内容应仍包含原文片段");
        assert_eq!(cached, value, "镜像缓存应等于编辑区内容");
        assert!(dirty, "粘贴后应置脏");

        // 纯文本剪贴板不会凭空引入换行：仍是单个逻辑行（软换行是排版行为，不改文本）
        assert_eq!(
            value.matches('\n').count(),
            0,
            "无换行的剪贴板反复粘贴不应产生换行，实际={value:?}"
        );

        // 真实场景：从源码/聊天处复制的行通常自带尾随换行，粘贴多次应变成多行且不丢失
        let chunk_line = "use std::sync::Arc;\n";
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(chunk_line.to_string()));
        for _ in 0..4 {
            cx.dispatch_action(gpui_component::input::Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let value2 = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            value2.matches('\n').count(),
            4,
            "带尾随换行的行重复粘贴 4 次应有 4 个换行，实际={value2:?}"
        );
        assert_eq!(value2.lines().count(), 4, "应形成 4 个逻辑行");
        assert!(
            value2.contains("Arc;\nuse std::sync::Arc;"),
            "多行内容应完整保留，实际={value2:?}"
        );
    }

    /// VS Code 在 Windows 剪贴板中使用 CRLF；应用级粘贴入口必须转为 LF 后再
    /// 交给 gpui-component 的代码编辑器，避免回车符进入代码布局。
    #[gpui::test]
    async fn normalized_paste_removes_windows_carriage_returns(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            "use std::cell::RefCell;\r\nuse std::collections::HashMap;\r\nuse std::ops::Range;"
                .to_owned(),
        ));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.paste_normalized(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let value = cx.update(|_window, cx| {
            app.read(cx).tabs[app.read(cx).active_tab]
                .editor
                .read(cx)
                .value()
                .to_string()
        });
        assert_eq!(
            value,
            "use std::cell::RefCell;\nuse std::collections::HashMap;\nuse std::ops::Range;"
        );
        assert!(!value.contains('\r'), "编辑器内部不能保留 CR 字符");
    }

    /// 仅含编辑器的极简宿主：窗口中只有 gpui-component 的代码编辑器，
    /// 没有菜单栏/选项卡/状态栏/预览等任何外围代码。
    struct BareEditor {
        editor: Entity<EditorState>,
    }

    impl Render for BareEditor {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(Editor::new(&self.editor).h(gpui::relative(1.)))
        }
    }

    /// 隔离测试：窗口里只有代码编辑器（无任何外围布局干扰）。
    /// 连续粘贴长文本与带换行文本，验证内容层不丢字、不凭空造换行——
    /// 即“整行消失”从内容层看与外围布局无关。
    #[gpui::test]
    async fn minimal_editor_paste_integrity(cx: &mut TestAppContext) {
        init_test(cx);

        let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<EditorState>>>> =
            std::rc::Rc::default();
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .tab_size(TabSize {
                        tab_size: 4,
                        hard_tabs: false,
                    })
                    .soft_wrap(false)
            });
            *holder.borrow_mut() = Some(editor.clone());
            let view = cx.new(|_cx| BareEditor { editor });
            gpui_component::Root::new(view, window, cx)
        });
        let _ = root;
        let editor = holder.borrow().clone().unwrap();

        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = editor.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        for _ in 0..2 {
            cx.run_until_parked();
        }

        // 1) 无换行大段文本连续粘贴：内容不丢、不产生换行（仍是单个逻辑行）
        let chunk = "Option<Vec<WrappedLine>>".repeat(200);
        let expected = chunk.len() * 5;
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(chunk));
        for _ in 0..5 {
            cx.dispatch_action(gpui_component::input::Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let value = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            value.len(),
            expected,
            "长文本反复粘贴不得丢字：实际长度={} 期望={expected}",
            value.len()
        );
        assert_eq!(value.matches('\n').count(), 0, "无换行剪贴板不应产生换行");

        // 2) 带尾随换行的行连续粘贴：应形成多行且内容完整
        let line = "use std::sync::Arc;\n";
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(line.to_string()));
        for _ in 0..4 {
            cx.dispatch_action(gpui_component::input::Paste);
            cx.run_until_parked();
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let value2 = cx.update(|_w, cx| editor.read(cx).value().to_string());
        assert_eq!(
            value2.matches('\n').count(),
            4,
            "带换行行重复粘贴应有 4 个换行"
        );
        assert_eq!(value2.lines().count(), 4, "应形成 4 个逻辑行");
    }

    /// 冒烟测试：完整渲染整棵 UI 树（菜单/选项卡/编辑区/状态栏），
    /// 展开下拉菜单并打开一个临时文件，确保渲染管线无 panic。
    #[gpui::test]
    async fn renders_and_opens_menu(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 展开"文件"菜单并渲染（覆盖 render_menu_dropdown 全路径）
        app.update(cx, |app, cx| {
            app.toggle_menu(MenuId::File, cx);
            app.toggle_menu(MenuId::View, cx);
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        app.update(cx, |app, cx| app.close_menus(cx));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 打开一个真实文件，内容进入编辑区，再渲染（覆盖 open_file 路径）
        let dir = std::env::temp_dir().join("gpui-editor-test.md");
        std::fs::write(&dir, "# 标题\n\n**加粗** 正文 `code`\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(dir.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (content, preview) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.tabs[a.active_tab].content.to_string(), a.show_preview())
        });
        assert!(
            content.contains("标题"),
            "打开文件后编辑区应有内容: {content}"
        );
        assert!(preview, "打开 .md 文件应自动开启预览");
        let _ = std::fs::remove_file(&dir);
    }

    /// 字号调节 / 预览开关等纯状态操作正常。
    #[gpui::test]
    async fn font_and_preview_toggles(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        app.update(cx, |app, cx| {
            app.change_font_size(3.0, cx);
            app.change_font_size(-1.0, cx);
            app.tabs[app.active_tab].preview_on = true;
            cx.notify();
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (font, preview) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.font_size, a.show_preview())
        });
        assert_eq!(font, DEFAULT_FONT_SIZE + 2.0);
        assert!(
            preview,
            "预览开关打开后 show_preview 应为 true（不依赖 .md 扩展名）"
        );
    }

    /// 当前选中的文件是 .md 时，选项卡栏最右侧出现眼睛图标，点击切换预览；非 .md 文件不出现。
    #[gpui::test]
    async fn md_tab_bar_shows_eye_and_toggles_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 打开 .md 文件（当前选中的就是它）：选项卡栏最右侧应显示眼睛，并自动开启预览
        let dir = std::env::temp_dir().join("gpui-editor-eye-test.md");
        std::fs::write(&dir, "# 标题\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(dir.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let eye_before = cx.debug_bounds("tab-bar-eye");
        let preview_before = cx.update(|_w, cx| app.read(cx).show_preview());
        assert!(
            eye_before.is_some(),
            "当前选中 .md 文件时，选项卡栏最右侧应显示眼睛图标"
        );
        assert!(preview_before, "打开 .md 文件应自动开启预览");

        // 点击眼睛：应关闭当前选项卡的预览（当前仍是 .md，眼睛保持显示，仅变关闭）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_preview_for_tab(0, window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let preview_off = cx.update(|_w, cx| app.read(cx).show_preview());
        let eye_still = cx.debug_bounds("tab-bar-eye").is_some();
        assert!(!preview_off, "点击眼睛后预览应关闭");
        assert!(eye_still, "当前仍是 .md 文件，眼睛图标应保持显示");
        let _ = std::fs::remove_file(&dir);

        // 另起一个窗口：打开非 .md 文件，选项卡栏最右侧不应出现眼睛（该窗口从未渲染过眼睛）
        let (view, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir2 = std::env::temp_dir().join("gpui-editor-eye-test.txt");
        std::fs::write(&dir2, "plain").unwrap();
        cx.update(|window, cx| {
            if let Ok(app) = view.read(cx).view().clone().downcast::<EditorApp>() {
                let _ = app.update(cx, |app, cx| {
                    app.open_file(dir2.to_str().unwrap(), window, cx);
                });
            }
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let eye_txt = cx.debug_bounds("tab-bar-eye");
        assert!(
            eye_txt.is_none(),
            "当前选中的文件不是 .md 时，选项卡栏不应显示眼睛图标"
        );
        let _ = std::fs::remove_file(&dir2);
    }

    /// 选项卡过多时，选项卡栏应可横向滚动（出现横向滚动条），最新选项卡不再被遮挡。
    #[gpui::test]
    async fn tab_bar_scrolls_when_tabs_overflow(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(400.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 连续新建大量选项卡，总宽超过一屏
        for _ in 0..30 {
            cx.update(|window, cx| app.update(cx, |app, cx| app.new_tab(window, cx)));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let max_w = cx.update(|_w, cx| app.read(cx).tab_bar_hscroll.handle.max_offset().x);
        assert!(
            max_w > px(0.),
            "选项卡过多时选项卡栏应可横向滚动：max_offset={max_w:?}"
        );
    }
}
