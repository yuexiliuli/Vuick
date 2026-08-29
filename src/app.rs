//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 工具栏：标题 + 文件路径输入框 + "打开" / "保存" 按钮 + "预览"开关
//! - 编辑区：多行文本编辑器（[`input::TextArea`]），可编辑任意文本 / 代码文件
//! - 预览区（开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//! - 状态栏：文件名 / 脏标记 / 语言徽标 / 行列 / 行数 / 字数 / 字号调节
//!
//! 打开文件：路径框输入回车 / 点"打开"、菜单"文件→打开"、命令行参数、或将文件
//! 拖拽到窗口上（[`gpui::ExternalPaths`] 拖放）。
//! 保存文件：`Ctrl+S`（未指定路径时会聚焦路径框提示先填写）。

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, px, rgb, rgba, white, App, AppContext, Context, CursorStyle, ExternalPaths, FocusHandle,
    Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
};

use super::markdown::{parse_markdown, render_markdown_blocks, Block, MarkdownTheme, TableMode};
use super::scrollbar::{Scrollbar, TableScrollState};
use super::input::{self, TextArea, TextEditEvent, TextInputEvent};

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

pub struct EditorApp {
    /// 主编辑区
    editor: gpui::Entity<TextArea>,
    /// 文件路径输入框
    path: gpui::Entity<TextArea>,
    /// 当前打开的文件路径
    file_path: Option<PathBuf>,
    /// 预览区滚动句柄
    preview_scroll: gpui::ScrollHandle,
    /// 预览表格横向滚动状态
    preview_table: TableScrollState,
    /// 是否显示预览（对任意文本生效，内容按 Markdown 渲染）
    preview_on: bool,
    /// 预览区 Markdown 解析缓存（内容不变时复用，避免每帧重新解析大文档）
    md_cache: Option<(SharedString, Vec<Block>)>,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 内容自上次打开/保存后是否被修改（脏标记）
    dirty: bool,
    /// 编辑器正文字号（px，用户可调）
    font_size: f32,
    /// 当前展开的下拉菜单（None = 无）
    open_menu: Option<MenuId>,
    /// 下拉菜单中当前悬停的条目（用于高亮）
    menu_hover: Option<(MenuId, usize)>,
    focus_handle: FocusHandle,
    /// 订阅（保持有效）
    _subscriptions: Vec<Subscription>,
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

    /// 编辑区的焦点句柄（供入口聚焦）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle.clone()
    }

    /// 构造（测试也可复用）。
    pub fn new(cx: &mut Context<Self>) -> EditorApp {
        let focus_handle = cx.focus_handle();
        let editor_focus = cx.focus_handle();
        let path_focus = cx.focus_handle();

        let editor = cx.new(|_cx| {
            TextArea::new(
                editor_focus,
                SharedString::from("在此输入文本，或用命令行 / 路径框打开文件…"),
                SharedString::from(""),
                true,
            )
        });

        // 路径输入框：单行、回车即打开
        let path = cx.new(|_cx| {
            TextArea::new(
                path_focus,
                SharedString::from("输入文件路径后回车，或点“打开”"),
                SharedString::from(""),
                false,
            )
            .with_submit_on_enter()
        });

        let mut app = EditorApp {
            editor,
            path,
            file_path: None,
            preview_scroll: gpui::ScrollHandle::new(),
            preview_table: TableScrollState::new(),
            preview_on: false,
            md_cache: None,
            status: SharedString::from("就绪"),
            status_ok: true,
            dirty: false,
            font_size: DEFAULT_FONT_SIZE,
            open_menu: None,
            menu_hover: None,
            focus_handle,
            _subscriptions: Vec::new(),
        };

        // 监听编辑器"内容被编辑"：置脏标记（未保存）。在 open/save 时清空。
        let ed = app.editor.clone();
        let dirty_sub = cx.subscribe(&ed, |this, _p, _: &TextEditEvent, cx| {
            if !this.dirty {
                this.dirty = true;
                cx.notify();
            }
        });
        app._subscriptions.push(dirty_sub);

        // 观察编辑器实体：光标移动 / 内容变更都会触发 EditorApp 重渲染，
        // 从而让状态栏(行列)与 Markdown 实时预览始终与编辑区保持同步。
        let ed = app.editor.clone();
        let observe_sub = cx.observe(&ed, |_this, _ed, cx| cx.notify());
        app._subscriptions.push(observe_sub);

        // 路径框回车 → 打开该路径
        let ed = app.path.clone();
        let sub = cx.subscribe(&ed, |this, _p, _: &TextInputEvent, cx| {
            let p = this.path.read(cx).content.to_string();
            if !p.trim().is_empty() {
                this.open_file(&p, cx);
            }
        });
        app._subscriptions.push(sub);

        // 命令行参数：第一个参数作为初始打开文件
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(first) = args.first() {
            if Path::new(first).is_file() {
                app.open_file(first, cx);
            }
        }

        app
    }

    /// 打开文件：读入编辑区，记录路径。
    fn open_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let p = PathBuf::from(path.trim());
        match fs::read_to_string(&p) {
            Ok(text) => {
                let len = text.len();
                self.editor.update(cx, |ed, cx| {
                    ed.content = SharedString::from(text);
                    ed.selected_range = len..len;
                    ed.scroll_handle.set_offset(gpui::point(px(0.), px(0.)));
                    cx.notify();
                });
                // 把打开路径回填到路径框，方便用户看到当前文件并继续编辑
                self.path.update(cx, |p, cx| {
                    let display = path.to_string();
                    let plen = display.len();
                    p.content = SharedString::from(display);
                    p.selected_range = plen..plen;
                    cx.notify();
                });
                self.file_path = Some(p.clone());
                // Markdown 默认开启预览
                if Self::is_markdown_path(&p) {
                    self.preview_on = true;
                }
                self.dirty = false;
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

    /// 保存文件：写回当前路径；未指定路径时提示并聚焦路径框。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.file_path.clone() else {
            self.status = SharedString::from("未指定文件路径，请先在路径框填写");
            self.status_ok = false;
            window.focus(&self.path.read(cx).focus_handle.clone());
            cx.notify();
            return;
        };
        let text = self.editor.read(cx).content.to_string();
        match fs::write(&p, text) {
            Ok(()) => {
                self.dirty = false;
                self.status = SharedString::from(format!("已保存：{}", p.display()));
                self.status_ok = true;
            }
            Err(e) => {
                self.status = SharedString::from(format!("保存失败：{e}"));
                self.status_ok = false;
            }
        }
        cx.notify();
    }

    fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
    }

    /// 是否展示预览：预览开关打开即展示（内容一律按 Markdown 渲染，
    /// 不再要求文件扩展名为 .md，方便在未命名缓冲区里直接写 Markdown）。
    fn show_preview(&self) -> bool {
        self.preview_on
    }

    /// 当前打开文件的显示名（无文件时为"未命名"）。
    fn file_display_name(&self) -> String {
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string())
    }

    /// 按扩展名推断语言标签（仅用于状态栏展示）。
    fn language_label(&self) -> &'static str {
        let Some(p) = self.file_path.as_ref() else {
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

    /// 新建文件：清空编辑区与路径，重置状态。
    fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |ed, cx| {
            ed.content = SharedString::from("");
            ed.selected_range = 0..0;
            ed.scroll_handle.set_offset(gpui::point(px(0.), px(0.)));
            cx.notify();
        });
        self.file_path = None;
        self.dirty = false;
        self.md_cache = None;
        self.preview_on = false;
        self.status = SharedString::from("新建文件");
        self.status_ok = true;
        window.focus(&self.editor.read(cx).focus_handle.clone());
        cx.notify();
    }

    /// 打开文件：聚焦路径输入框（无原生文件对话框时的手动路径入口）。
    fn open_file_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = SharedString::from("在路径框输入文件路径后回车，或将文件拖入窗口");
        self.status_ok = true;
        window.focus(&self.path.read(cx).focus_handle.clone());
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

    /// 菜单项分组：`(标签, 快捷键提示, 点击行为)`。
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
                    div()
                        .my_1()
                        .h(px(1.))
                        .bg(rgb(0xe5e7eb))
                        .into_any_element(),
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
        let Some(&(label, _)) = items.get(idx) else {
            return;
        };
        let _ = label;
        match menu {
            MenuId::File => match idx {
                0 => self.new_file(window, cx),
                1 => self.open_file_prompt(window, cx),
                2 => self.save(window, cx),
                3 => self.save_as(window, cx),
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
                    self.preview_on = !self.preview_on;
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

    /// 另存为：无原生文件对话框，引导用户在路径框输入新路径后保存。
    fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = SharedString::from("请在路径框输入新路径，然后按 Ctrl+S 或点“保存”");
        self.status_ok = false;
        window.focus(&self.path.read(cx).focus_handle.clone());
        cx.notify();
    }

    // ==================== 渲染 ====================

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = if self.status_ok {
            rgb(0x16a34a)
        } else {
            rgb(0xd97706)
        };

        // 打开按钮：路径框有内容则直接打开，否则聚焦路径框引导输入
        let open_btn = div()
            .id("open-btn")
            .px_3()
            .h(px(30.))
            .rounded_md()
            .bg(rgb(0x2563eb))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .text_color(white())
            .cursor(CursorStyle::PointingHand)
            .child("打开")
            .on_mouse_up(MouseButton::Left, {
                let path = self.path.clone();
                cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    let p = path.read(cx).content.to_string();
                    if !p.trim().is_empty() {
                        this.open_file(&p, cx);
                    } else {
                        this.open_file_prompt(window, cx);
                    }
                })
            });

        // 保存按钮
        let save_btn = div()
            .id("save-btn")
            .px_3()
            .h(px(30.))
            .rounded_md()
            .bg(white())
            .border_1()
            .border_color(rgb(0xd1d5db))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .text_color(rgb(0x1f2328))
            .cursor(CursorStyle::PointingHand)
            .child("保存")
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, window, cx| this.save(window, cx)),
            );

        // 预览开关（仅 Markdown 有意义）
        let preview_btn = div()
            .id("preview-btn")
            .px_3()
            .h(px(30.))
            .rounded_md()
            .bg(if self.show_preview() {
                rgb(0x2563eb)
            } else {
                rgb(0xffffff)
            })
            .border_1()
            .border_color(rgb(0xd1d5db))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .text_color(if self.show_preview() {
                rgb(0xffffff)
            } else {
                rgb(0x1f2328)
            })
            .cursor(CursorStyle::PointingHand)
            .child(if self.show_preview() { "预览: 开" } else { "预览: 关" })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    this.preview_on = !this.preview_on;
                    cx.notify();
                }),
            );

        div()
            .h(px(56.))
            .bg(white())
            .border_b_1()
            .border_color(rgb(0xe5e7eb))
            .shadow_sm()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(0x1f2328))
                                    .child("GPUI 编辑器"),
                            )
                            // 当前文件名（含未保存脏标记）
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(0xf3f4f6))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(0x4b5563))
                                            .child(format!(
                                                "{}{}",
                                                if self.dirty { "● " } else { "" },
                                                self.file_display_name()
                                            )),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(30.))
                            .border_1()
                            .border_color(rgb(0xd1d5db))
                            .rounded_md()
                            .bg(white())
                            .px_2()
                            .text_size(px(13.))
                            .cursor(CursorStyle::IBeam)
                            .track_focus(&self.path.read(cx).focus_handle.clone())
                            .child(self.path.clone()),
                    )
                    .child(open_btn)
                    .child(save_btn)
                    .child(preview_btn),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(status_color)
                    .child(self.status.to_string()),
            )
    }

    fn render_editor_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = self.editor.read(cx).scroll_handle.clone();
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
                    .child(self.render_line_gutter(cx))
                    .child(
                        div()
                            .id("editor-text-wrap")
                            .flex_1()
                            .min_w(px(0.))
                            .px_3()
                            .font_family("Consolas")
                            .text_size(px(self.font_size))
                            .line_height(gpui::relative(1.5))
                            .child(self.editor.clone()),
                    ),
            )
    }

    /// 行号槽：普通流布局逐行堆叠行号，随外层滚动容器与正文同步平移。
    ///
    /// 顶部内边距补上 TextArea 在短内容（`min_h(40)` 盒子垂直居中）时的偏移，
    /// 否则行号会比正文"偏上"。
    fn render_line_gutter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ed = self.editor.read(cx);
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

    /// 底部状态栏：文件名/语言/脏标记 + 行列/行数/字数 + 字号调节。
    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ed = self.editor.read(cx);
        let (ln, col) = {
            let (l, c) = ed.line_col(ed.cursor_offset());
            (l + 1, c)
        };
        let line_count = ed.line_ranges().len();
        let char_count = ed.content.chars().count();
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
                            .text_color(if self.dirty {
                                rgb(0xdc2626)
                            } else {
                                rgb(0x9ca3af)
                            })
                            .child(if self.dirty { "●" } else { "" }),
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

    fn render_preview_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 实时预览：内容不变时复用上一次解析出的块列表，避免每帧重复解析大文档
        let md = self.editor.read(cx).content.clone();
        let blocks = match &self.md_cache {
            Some((cached_md, cached_blocks)) if *cached_md == md => cached_blocks.clone(),
            _ => {
                let parsed = parse_markdown(&md);
                let parsed_clone = parsed.clone();
                self.md_cache = Some((md.clone(), parsed_clone));
                parsed
            }
        };
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w(px(0.))
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
                    .min_w(px(0.))
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
            // 从资源管理器拖入文件 → 直接打开
            .can_drop(|drag: &dyn Any, _w, _c| {
                drag.downcast_ref::<Arc<ExternalPaths>>().is_some()
            })
            .on_drop(cx.listener(|this: &mut EditorApp, paths: &Arc<ExternalPaths>, _w, cx| {
                if let Some(p) = paths.paths().first() {
                    this.open_file(&p.to_string_lossy(), cx);
                }
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::Save, window, cx| {
                this.save(window, cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::NewFile, window, cx| {
                this.new_file(window, cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::OpenFile, window, cx| {
                this.open_file_prompt(window, cx)
            }))
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::TogglePreview, _w, cx| {
                this.preview_on = !this.preview_on;
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
            .child(self.render_header(cx))
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
            app.editor.update(cx, |ed, cx| {
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
            view.read(cx).editor.read(cx).last_bounds.map(|b| b.top())
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
            app.editor.update(cx, |ed, cx| {
                ed.content = content.into();
                ed.selected_range = 0..0;
                cx.notify();
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let handle = cx.update(|_w, cx| view.read(cx).editor.read(cx).scroll_handle.clone());
        handle.set_offset(gpui::point(px(0.), px(-120.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (offset_after, text_top, offsets) = cx.update(|_w, cx| {
            let ed = view.read(cx).editor.read(cx);
            (
                ed.scroll_handle.offset(),
                ed.last_bounds.map(|b| b.top()),
                ed.last_visual_offsets.clone(),
            )
        });
        let _ = offset_after;
        let gutter_10 = cx.debug_bounds("gutter-line-10").map(|b| b.top());
        let expected = text_top.unwrap() + offsets[9];
        assert!(
            (gutter_10.unwrap() - expected).abs() < px(1.),
            "滚动后第 10 行行号应对齐正文：gutter={gutter_10:?} expected={expected:?}"
        );
    }

    /// 冒烟测试：完整渲染整棵 UI 树（菜单栏/工具栏/编辑区/状态栏），
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
                app.editor.read(cx).content.to_string(),
                app.show_preview(),
            )
        });
        assert!(content.contains("标题"), "打开文件后编辑区应有内容: {content}");
        assert!(preview, "打开 .md 文件应自动开启预览");
        let _ = std::fs::remove_file(&dir);
    }

    /// 冒烟测试：字号调节 / 预览开关等纯状态操作正常。
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
            app.preview_on = true;
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
