//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器。
//!
//! 布局（自上而下）：
//! - 顶部标题栏：标题 + 文件路径输入框 + "打开" / "保存" 按钮 + "预览"开关
//! - 编辑区：多行文本编辑器（[`input::TextArea`]），可编辑任意文本 / 代码文件
//! - 预览区（仅 Markdown 且开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//!
//! 打开文件：在路径框输入路径后回车 / 点"打开"，或用命令行参数传入路径。
//! 保存文件：`Ctrl+S`（未指定路径时会聚焦路径框提示先填写）。

use std::fs;
use std::path::{Path, PathBuf};

use gpui::{
    div, px, rgb, white, App, AppContext, Context, CursorStyle, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

use super::markdown::{parse_markdown, render_markdown_blocks, Block, MarkdownTheme, TableMode};
use super::scrollbar::{Scrollbar, TableScrollState};
use super::input::{self, TextArea, TextInputEvent};

/// 编辑区背景色（亮色）。
const EDIT_BG: gpui::Hsla = white();
/// 编辑器正文字号。
const EDIT_FONT_SIZE: Pixels = px(14.);

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
    /// 是否显示预览（仅对 Markdown 生效）
    preview_on: bool,
    /// 预览区 Markdown 解析缓存（内容不变时复用，避免每帧重新解析大文档）
    md_cache: Option<(SharedString, Vec<Block>)>,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    focus_handle: FocusHandle,
    /// 订阅（保持有效）
    _subscription: Option<Subscription>,
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
            focus_handle,
            _subscription: None,
        };

        // 路径框回车 → 打开该路径
        let ed = app.path.clone();
        let sub = cx.subscribe(&ed, |this, _p, _: &TextInputEvent, cx| {
            let p = this.path.read(cx).content.to_string();
            if !p.trim().is_empty() {
                this.open_file(&p, cx);
            }
        });
        app._subscription = Some(sub);

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
                self.file_path = Some(p.clone());
                // Markdown 默认开启预览
                if Self::is_markdown_path(&p) {
                    self.preview_on = true;
                }
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

    /// 当前文件是否为 Markdown（按扩展名）。
    fn is_markdown(&self) -> bool {
        self.file_path
            .as_ref()
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false)
    }

    fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
    }

    /// 是否展示预览：开启预览且为 Markdown。
    fn show_preview(&self) -> bool {
        self.preview_on && self.is_markdown()
    }

    // ==================== 渲染 ====================

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = if self.status_ok {
            rgb(0x16a34a)
        } else {
            rgb(0xd97706)
        };

        // 打开按钮
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
                cx.listener(move |this: &mut EditorApp, _ev, _window, cx| {
                    let p = path.read(cx).content.to_string();
                    if !p.trim().is_empty() {
                        this.open_file(&p, cx);
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
                            .text_size(px(17.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(0x1f2328))
                            .child("GPUI 编辑器"),
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
        div()
            .id("editor-pane")
            .flex_1()
            .min_w(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.editor.read(cx).scroll_handle.clone())
            .bg(EDIT_BG)
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .px_3()
            .py_2()
            .font_family("Consolas")
            .text_size(EDIT_FONT_SIZE)
            .line_height(gpui::relative(1.5))
            .child(self.editor.clone())
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
            .on_action(cx.listener(|this: &mut EditorApp, _: &input::Save, window, cx| {
                this.save(window, cx)
            }))
            .child(self.render_header(cx))
            .child(body)
    }
}
