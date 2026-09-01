//! tabs.

use super::*;
use crate::app::file_ops::PREVIEW_DEBOUNCE_MS;
use crate::scrollbar::horizontal_scrollbar;
use gpui_component::button::ButtonVariants as _;

impl EditorApp {
    pub(super) fn make_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
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
                if let InputEvent::Change = ev {
                    let (value, cursor_position) = {
                        let state = tab.editor.read(cx);
                        (state.value(), state.cursor_position())
                    };
                    // 仅在开启预览时才同步 Markdown 预览，且做 200ms 去抖：
                    // 打字时不逐键整篇重解析+重建虚拟化列表，停止后片刻再同步。
                    if tab.preview_on && !this.preview_pending {
                        this.preview_pending = true;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(PREVIEW_DEBOUNCE_MS))
                                .await;
                            let _ = this.update(cx, |this, cx| {
                                this.preview_pending = false;
                                let panel_bg = this.panel_bg();
                                if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                                    if tab.preview_on {
                                        let value = tab.editor.read(cx).value();
                                        tab.md_preview.update(cx, |view, cx| {
                                            view.set_background(panel_bg);
                                            view.set_source(value.as_ref(), cx);
                                        });
                                    }
                                }
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                    tab.content = value.clone();
                    tab.line_count = value.lines().count().max(1);
                    tab.char_count = value.chars().count();
                    tab.cursor_row = cursor_position.line as usize;
                    tab.cursor_col = cursor_position.character as usize;
                    if !tab.dirty {
                        tab.dirty = true;
                    }
                    cx.notify();
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
            md_preview: cx.new(|cx| crate::markdown::MarkdownPreview::new(cx)),
            preview_on: false,
            _edit_sub: edit_sub,
        });
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        idx
    }

    pub(super) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.make_tab(window, cx);
        let focus = self.tabs[idx].editor.read(cx).focus_handle(cx);
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
        let focus = self.tabs[idx].editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
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
                                    let text =
                                        this.tabs[idx].editor.read(cx).value().to_string();
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
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        let file_name = self.tab_file_name(idx);
        let text = tab.editor.read(cx).value().to_string();
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
        if self.tabs.len() == 1 {
            let tab = &mut self.tabs[0];
            tab.editor.update(cx, |ed, cx| {
                ed.set_value("", window, cx);
            });
            tab.md_preview.update(cx, |view, cx| view.set_source("", cx));
            tab.file_path = None;
            tab.dirty = false;
            tab.content = SharedString::default();
            tab.line_count = 1;
            tab.char_count = 0;
            tab.cursor_row = 0;
            tab.cursor_col = 0;
            tab.preview_on = false;
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

    pub(super) fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;
        let (text_primary, text_minor, tab_active_bg, tab_active_border, chip_bg, chip_text, bar_bg, bar_border) = (
            self.text_primary(),
            self.text_minor(),
            self.tab_active_bg(),
            self.tab_active_border(),
            self.chip_bg(),
            self.chip_text(),
            self.bar_bg(),
            self.bar_border(),
        );
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
                .text_color(text_minor)
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
                        tab_active_bg
                    } else {
                        rgba(0x00000000).into()
                    })
                    .border_1()
                    .border_color(if is_active {
                        tab_active_border
                    } else {
                        rgba(0x00000000).into()
                    })
                    .cursor(CursorStyle::PointingHand)
                    .text_size(px(12.))
                    .text_color(text_primary)
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

        let add = gpui_component::button::Button::new("tab-add")
            .label("+")
            .ghost()
            .w(px(24.))
            .h(px(24.))
            .rounded_md()
            .bg(chip_bg)
            .text_size(px(15.))
            .text_color(chip_text)
            .on_click(
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
            .bg(bar_bg)
            .border_b_1()
            .border_color(bar_border)
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
}
