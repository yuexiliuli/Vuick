//! tabs.

use super::*;
use crate::config::timing::TAB_HSCROLL_HIDE_MS;
use crate::scrollbar::horizontal_scrollbar;

/// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
struct TabBarColors {
    text_primary: gpui::Hsla,
    text_minor: gpui::Hsla,
    tab_active_bg: gpui::Hsla,
    tab_active_border: gpui::Hsla,
    chip_bg: gpui::Hsla,
    chip_text: gpui::Hsla,
    bar_bg: gpui::Hsla,
    panel_bg: gpui::Hsla,
    accent: gpui::Hsla,
}

impl EditorApp {
    pub(super) fn make_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> usize {
        // 自研编辑器组件：创建即空文档；语法高亮语言在打开文件时 set_language。
        let editor = cx.new(|cx| crate::editor::CodeEditor::new(cx));
        // 编辑内容被修改 → 置脏 + 同步内容/行列缓存 + 预览增量入口。
        let edit_sub = self.subscribe_editor(&editor, cx);

        self.tabs.push(EditorTab {
            editor,
            file_path: None,
            dirty: false,
            content: SharedString::default(),
            line_count: 1,
            char_count: 0,
            cursor_row: 0,
            cursor_col: 0,
            md_preview: cx.new(|cx| crate::markdown::MarkdownPreview::new(cx)),
            pdf_preview: None,
            image_viewer: None,
            #[cfg(feature = "video")]
            video_viewer: None,
            preview_on: false,
            preview_fullscreen: false,
            readonly_preview: false,
            editor_pending: false,
            live_edit: false,
            live2: None,
            _live2_sub: None,
            live_vscroll: gpui::ScrollHandle::new(),
            md_wy: None,
            md_wy_vscroll: gpui::ScrollHandle::new(),
            preview_prev: None,
            edit_version: 0,
            preview_dirty: None,
            _edit_sub: edit_sub,
        });
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        idx
    }

    pub(super) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.make_tab(window, cx);
        let focus = self.tab_focus_handle(idx, cx);
        window.focus(&focus, cx);
        self.status = SharedString::from("新建文件");
        self.status_ok = true;
        cx.notify();
    }

    pub(super) fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.active_tab = idx;
        // PDF / EPUB 等只读预览文档没有可见编辑区，不把焦点给隐藏的编辑器，
        // 而是给 App 根（让窗口级快捷键仍可用）。
        let readonly = self.tabs[idx].readonly_preview;
        if readonly {
            let root = self.focus_handle(cx);
            window.focus(&root, cx);
        } else {
            let focus = self.tab_focus_handle(idx, cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
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
                let should_close = match idx_ans {
                    0 => {
                        let (saved, need_dialog) = cx
                            .update_window_entity(&entity, |this, _window, cx| match this
                                .tabs
                                .get(idx)
                                .and_then(|t| t.file_path.clone())
                            {
                                Some(p) => {
                                    // 走统一口径：live2 开启时读 WyEditor 实时内容
                                    // （否则关闭保存的是陈旧的源码编辑区，丢整段编辑）。
                                    let text = this.tab_effective_text(idx, cx);
                                    (Some(this.write_to_path(idx, &p, text, cx)), false)
                                }
                                None => (None, true),
                            })
                            .unwrap_or((None, false));
                        if need_dialog {
                            let _ =
                                cx.update_window_entity(&entity, |this, window, cx| {
                                    this.save_unnamed_and_close(idx, window, cx);
                                });
                            false
                        } else {
                            saved.unwrap_or(false)
                        }
                    }
                    1 => true,
                    _ => false,
                };
                if should_close {
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.remove_tab(idx, window, cx);
                    });
                }
            })
            .detach();
    }

    fn save_unnamed_and_close(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let file_name = self.tab_file_name(idx);
        // live2 开启时读 WyEditor 实时内容（防关闭保存丢编辑）。
        let text = self.tab_effective_text(idx, cx);
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("保存文件")
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return;
                };
                let file = file.path().to_path_buf();
                let saved = cx
                    .update_window_entity(&entity, |this, _window, cx| {
                        this.write_to_path(idx, &file, text.clone(), cx)
                    })
                    .unwrap_or(false);
                if saved {
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.remove_tab(idx, window, cx);
                    });
                }
            })
            .detach();
    }

    pub(super) fn remove_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.search_scan = None; // 选项卡集合变化 → 搜索缓存失效
        if self.tabs.len() == 1 {
            let tab = &mut self.tabs[0];
            tab.editor.update(cx, |ed, _cx| {
                ed.set_text("");
            });
            tab.md_preview.update(cx, |view, cx| view.set_source("", cx));
            if let Some(pdf) = tab.pdf_preview.take() {
                pdf.update(cx, |view, cx| view.release(window, cx));
            }
            if let Some(img) = tab.image_viewer.take() {
                img.update(cx, |view, cx| view.release(window, cx));
            }
            // A2：右栏 WyEditor 预览随选项卡复位释放（同 md_preview 释放内存）。
            tab.md_wy = None;
            tab.md_wy_vscroll = gpui::ScrollHandle::new();
            tab.file_path = None;
            tab.dirty = false;
            tab.content = SharedString::default();
            tab.line_count = 1;
            tab.char_count = 0;
            tab.cursor_row = 0;
            tab.cursor_col = 0;
            tab.preview_on = false;
            tab.preview_fullscreen = false;
            tab.readonly_preview = false;
            let focus = tab.editor.read(cx).focus_handle();
            window.focus(&focus, cx);
            self.status = SharedString::from("已关闭");
            self.status_ok = true;
            cx.notify();
            return;
        }
        if let Some(pdf) = self.tabs[idx].pdf_preview.take() {
            pdf.update(cx, |view, cx| view.release(window, cx));
        }
        if let Some(img) = self.tabs[idx].image_viewer.take() {
            img.update(cx, |view, cx| view.release(window, cx));
        }
        self.tabs.remove(idx);
        if idx < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        self.activate_tab(self.active_tab, window, cx);
    }

    /// 标记“选项卡栏最近在滚动”：横向滚动条立即可见，并重启自动隐藏计时
    /// （[`TAB_HSCROLL_HIDE_MS`] 后无新滚动则隐藏）。带代序号防旧定时器误清新滚动。
    pub(super) fn poke_tab_hscroll(&mut self, cx: &mut Context<Self>) {
        *self.tab_bar_hscroll.visible.borrow_mut() = true;
        cx.notify();
        self.tab_hscroll_seq = self.tab_hscroll_seq.wrapping_add(1);
        let seq = self.tab_hscroll_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(TAB_HSCROLL_HIDE_MS))
                .await;
            let _ = this.update(cx, |app, cx| {
                if app.tab_hscroll_seq == seq {
                    *app.tab_bar_hscroll.visible.borrow_mut() = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
    fn tab_bar_colors(&self, cx: &mut Context<Self>) -> TabBarColors {
        TabBarColors {
            text_primary: self.text_primary(),
            text_minor: self.text_minor(),
            tab_active_bg: self.tab_active_bg(),
            tab_active_border: self.tab_active_border(),
            chip_bg: self.chip_bg(),
            chip_text: self.chip_text(),
            bar_bg: self.bar_bg(),
            panel_bg: self.panel_bg(),
            accent: crate::ui::theme::Theme::global(cx).colors.accent.into(),
        }
    }

    /// 单个选项卡元素：名称/脏标记/关闭钮/激活顶线 + 拖拽幽灵 + 点击激活。
    fn tab_bar_item(
        tab: &EditorTab,
        idx: usize,
        is_active: bool,
        colors: &TabBarColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = tab
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string());
        let dirty = tab.dirty;

        let close = div()
            .id(SharedString::from(format!("tab-close-{idx}")))
            .w(px(18.))
            .h(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(13.))
            .text_color(colors.text_minor)
            .cursor(CursorStyle::PointingHand)
            .child("×")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    this.close_tab(idx, window, cx);
                    cx.stop_propagation();
                }),
            );

        // 激活选项卡顶部 2px 蓝色强调线（色 = 语义 `accent`）。
        // 用绝对定位细条绘制：本版 gpui 的 border_color 是整圈一个色，
        // 没有“只染某一边”的 API，无法用 border 实现顶蓝其余透明。
        let accent_line = is_active.then(|| {
            div()
                .id(SharedString::from(format!("tab-accent-{idx}")))
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .right(px(0.))
                .h(px(1.))
                .bg(colors.accent)
                .into_any_element()
        });

        let mut el = div()
            .id(SharedString::from(format!("tab-{idx}")))
            .debug_selector(move || format!("tab-{idx}"))
            .h_full()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink(1.)
            .relative()
            .bg(if is_active {
                colors.panel_bg
            } else {
                rgba(0x00000000).into()
            })
            .cursor(CursorStyle::PointingHand)
            .text_size(px(12.))
            .text_color(colors.text_primary)
            .child(if dirty { "● " } else { "" })
            .child(SharedString::from(name.clone()))
            .child(close);
        if let Some(accent_line) = accent_line {
            el = el.child(accent_line);
        }
        // 多窗口拖拽：按住选项卡拖动 → 跟出“胶囊”幽灵；
        // 在窗口外松开由 EditorApp 根视图决定「拆独立窗口 / 还原回主窗口」。
        let drag_data = TabDragData {
            idx,
            name: SharedString::from(name),
        };
        let ghost_name = drag_data.name.clone();
        el.on_drag(
            drag_data,
            move |_value: &TabDragData, _offset, _window, cx| {
                cx.new(|_| TabDragGhost {
                    name: ghost_name.clone(),
                })
            },
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                this.activate_tab(idx, window, cx);
            }),
        )
        .into_any_element()
    }

    /// 新建(+)：手绘「＋」形图标（横/竖两条圆角细条居中），替代文本 "+"，
    /// 避免不同字体下粗细/偏移不一显得歪斜；带 hover/按下交互反馈。
    fn tab_add_button(colors: &TabBarColors, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("tab-add")
            .debug_selector(|| "tab-add".into())
            .w(px(24.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .bg(colors.chip_bg)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(colors.tab_active_bg))
            .active(|style| style.bg(colors.tab_active_border))
            .on_click(
                cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .child(
                div()
                    .id("tab-add-icon")
                    .debug_selector(|| "tab-add-icon".into())
                    .relative()
                    .w(px(10.))
                    .h(px(10.))
                    // 横条：居中水平线
                    .child(
                        div()
                            .absolute()
                            .top(px(4.))
                            .left(px(0.))
                            .right(px(0.))
                            .h(px(2.))
                            .rounded_full()
                            .bg(colors.chip_text),
                    )
                    // 竖条：居中垂直线（盖在横条之上）
                    .child(
                        div()
                            .absolute()
                            .left(px(4.))
                            .top(px(0.))
                            .bottom(px(0.))
                            .w(px(2.))
                            .rounded_full()
                            .bg(colors.chip_text),
                    ),
            )
            .into_any_element()
    }

    /// 整条选项卡栏最右侧的眼睛图标：仅当"当前选中的文件"是 Markdown（.md / .markdown）
    /// 时才显示，点击切换当前选项卡的 Markdown 预览（与「查看 → 预览」一致）。
    /// 用纯 div 画出（椭圆外框 + 中心瞳孔），避免依赖 emoji 字体；蓝色=已开，灰色=已关。
    fn tab_eye_button(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let active_is_md = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);
        if !active_is_md {
            return None;
        }
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
    }

    /// 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现；
    /// 半透明、贴着标签底部、浮在上方（不额外占高度）。
    fn tab_bar_hscroll_overlay(&self) -> Option<gpui::AnyElement> {
        let hscroll_dragging = self.tab_bar_hscroll.drag.borrow().is_some();
        if self.tab_bar_hscroll.handle.max_offset().x > px(0.)
            && (*self.tab_bar_hscroll.visible.borrow() || hscroll_dragging)
        {
            Some(
                div()
                    .absolute()
                    .debug_selector(|| "tab-bar-hscroll-overlay".into())
                    .left(px(0.))
                    .right(px(0.))
                    .bottom(px(0.))
                    .h(px(3.))
                    .child(horizontal_scrollbar(
                        &self.tab_bar_hscroll,
                        "tab-bar-hscroll",
                    ))
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    pub(super) fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;
        let colors = self.tab_bar_colors(cx);
        let mut tabs: Vec<gpui::AnyElement> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            tabs.push(Self::tab_bar_item(tab, i, i == active, &colors, cx));
        }

        // 选项卡栏右侧：固定放"新建(+)"与（按需）眼睛图标；标签过多时由左侧滚动区横向滚动
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(eye) = self.tab_eye_button(cx) {
            right.push(eye);
        }

        // 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现。
        let mut hscroll_overlay: Vec<gpui::AnyElement> = Vec::new();
        if let Some(o) = self.tab_bar_hscroll_overlay() {
            hscroll_overlay.push(o);
        }

        div()
            .id("tab-bar")
            .relative()
            .w_full()
            // 左侧不留内边距：第一个选项卡紧贴窗口左边缘；右侧保留 px_2 给新建/眼睛按钮
            .pl_0()
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(colors.bar_bg)
            // 选项卡横向滚动区：占满剩余宽度；标签不收缩，过多即横向溢出可滚轮/拖拽滚动。
            // 横向滚动条浮在标签底部（见 hscroll_overlay），不改变整体高度。
            .child(
                div()
                    .id("tab-bar-scroll")
                    .relative()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(32.))
                    .child(
                        div()
                            .id("tab-bar-tabs")
                            .debug_selector(|| "tab-bar-tabs".into())
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_row()
                            .items_stretch()
                            .gap_1()
                            .overflow_x_scroll()
                            .track_scroll(&self.tab_bar_hscroll.handle)
                            // 滚动时点亮横向滚动条（自动隐藏计时由此重启）
                            .on_scroll_wheel(cx.listener(
                                |this: &mut EditorApp, _ev, _window, cx| {
                                    this.poke_tab_hscroll(cx);
                                },
                            ))
                            .children(tabs),
                    )
                    .children(hscroll_overlay),
            )
            .child(Self::tab_add_button(&colors, cx))
            .children(right)
    }
}
