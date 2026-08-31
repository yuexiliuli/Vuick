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

mod file_ops;
mod menu;
mod panes;
mod status_bar;
mod tabs;

#[cfg(test)]
mod tests;

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
use gpui_component::{Icon, IconName};

use crate::actions::*;
use crate::scrollbar::TableScrollState;

/// 编辑器默认正文字号（px）。
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// 字号调节范围（px）。
const FONT_MIN: f32 = 10.0;
const FONT_MAX: f32 = 28.0;
/// 菜单栏高度。
const MENU_BAR_H: Pixels = px(26.);

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
    /// gpui-component 的 Markdown 解析、排版和选择状态。
    md_view: gpui::Entity<gpui_component::text::TextViewState>,
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
    /// 选项卡栏横向滚动状态（标签过多时横向滚动 + 滚动条）
    tab_bar_hscroll: TableScrollState,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 编辑器正文字号（px，用户可调）
    font_size: f32,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅
    dark_mode: bool,
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
            tab_bar_hscroll: TableScrollState::new(),
            status: SharedString::from("就绪"),
            status_ok: true,
            font_size: DEFAULT_FONT_SIZE,
            dark_mode: false,
            focus_handle,
        };
        app.make_tab(window, cx);

        // 命令行参数：第一个参数作为初始打开文件（复用首个空选项卡，不额外新建）
        let mut args = std::env::args_os();
        args.next(); // 跳过程序名
        if let Some(first) = args.next() {
            let p = PathBuf::from(&first);
            if p.is_file() {
                app.open_file_in_tab(0, &p.to_string_lossy(), window, cx);
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

    /// 选项卡默认文件名（用于保存对话框）。
    fn tab_file_name(&self, idx: usize) -> String {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名.txt".into())
    }

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
        self.font_size = (self.font_size + delta).clamp(FONT_MIN, FONT_MAX);
        cx.notify();
    }

    /// 窗口整体背景（编辑区以外的画布）。
    fn window_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111827).into()
        } else {
            rgb(0xf3f4f6).into()
        }
    }

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    fn bar_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x1f2937).into()
        } else {
            rgb(0xf3f4f6).into()
        }
    }

    /// 工具条分隔边框。
    fn bar_border(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 编辑器/预览面板背景。
    fn panel_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111827).into()
        } else {
            white()
        }
    }

    /// 主要文字颜色。
    fn text_primary(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xe5e7eb).into()
        } else {
            rgb(0x111827).into()
        }
    }

    /// 次要/弱化文字颜色。
    fn text_minor(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x9ca3af).into()
        } else {
            rgb(0x6b7280).into()
        }
    }

    /// 胶囊/小按钮等"芯片"背景。
    fn chip_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 芯片上的文字颜色。
    fn chip_text(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xe5e7eb).into()
        } else {
            rgb(0x374151).into()
        }
    }

    /// 激活选项卡背景。
    fn tab_active_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            white()
        }
    }

    /// 激活选项卡边框。
    fn tab_active_border(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x4b5563).into()
        } else {
            rgb(0xd1d5db).into()
        }
    }

    /// 状态栏/其他整体面板背景。
    fn surface_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111b2a).into()
        } else {
            white()
        }
    }

    /// 深色模式下把编辑区与行号栏(gutter)背景统一为 0x111827
    /// 消除默认深色主题下二者不慎一致的底色。浅色模式保持默认。
    fn apply_dark_editor_colors(cx: &mut App) {
        let theme = gpui_component::Theme::global_mut(cx);
        let highlight = Arc::make_mut(&mut theme.highlight_theme);
        highlight.style.editor_background = Some(rgb(0x111827).into());
        highlight.style.editor_gutter_background = Some(rgb(0x111827).into());
    }

    /// 切换黑白（深色/浅色）主题：驱动 gpui_component 组件主题与自绘色板。
    /// 自绘标题栏也走同一套色板，因此整窗（含标题栏）随之切换。
    fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        gpui_component::Theme::change(
            if self.dark_mode {
                gpui_component::ThemeMode::Dark
            } else {
                gpui_component::ThemeMode::Light
            },
            None,
            cx,
        );
        if self.dark_mode {
            Self::apply_dark_editor_colors(cx);
        }
        cx.notify();
    }
    /// 自绘标题栏：无边框窗口下代替原生标题栏，颜色跟随黑白主题。
    /// 左侧为 文件/编辑/查看/帮助 菜单，中间空位为窗口拖拽区，
    /// 右侧为最小化 / 最大化 / 关闭按钮。
    ///
    /// 拖拽与窗口控制全部交给平台原生处理：各元素标上
    /// [`gpui::WindowControlArea`]，GPUI 在 WM_NCHITTEST 里返回对应
    /// HTCAPTION / HTMINMAXBUTTON，由系统完成移动（贴边吸附）、
    /// 最小化、最大化（含双击标题栏切换）与关闭。
    pub(super) fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (bar_bg, bar_border, text_minor) =
            (self.bar_bg(), self.bar_border(), self.text_minor());
        let icon_style = |icon: IconName| {
            Icon::new(icon).size(px(14.)).text_color(text_minor)
        };
        let maximize_icon = if window.is_maximized() {
            IconName::WindowRestore
        } else {
            IconName::WindowMaximize
        };

        let minimize = div()
            .id("title-minimize")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(IconName::WindowMinimize))
            .window_control_area(gpui::WindowControlArea::Min);

        let maximize = div()
            .id("title-maximize")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(maximize_icon))
            .window_control_area(gpui::WindowControlArea::Max);

        let close = div()
            .id("title-close")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(IconName::WindowClose))
            .window_control_area(gpui::WindowControlArea::Close);

        div()
            .id("title-bar")
            .h(px(30.))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(bar_bg)
            .border_b_1()
            .border_color(bar_border)
            .child(self.render_menu_buttons(cx))
            .child(
                div()
                    .id("title-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .children([
                minimize.into_any_element(),
                maximize.into_any_element(),
                close.into_any_element(),
            ])
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
            .bg(self.window_bg())
            .track_focus(&self.focus_handle(cx))
            // 从资源管理器拖入文件到非编辑区 → 直接打开（编辑区由 Input 的父面板处理）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &Arc<ExternalPaths>, window, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.open_file(&p.to_string_lossy(), window, cx);
                    }
                },
            ))
            .on_action(cx.listener(|this: &mut EditorApp, _: &Save, window, cx| {
                this.save(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &PasteNormalized, window, cx| {
                    this.paste_normalized(window, cx)
                }),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &SaveAs, window, cx| {
                this.save_as(window, cx)
            }))
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
            .child(self.render_title_bar(window, cx))
            .child(self.render_tab_bar(cx))
            .child(body)
            .child(self.render_status_bar(cx))
    }
}
