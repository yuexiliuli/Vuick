//! tabs.

use super::*;
use crate::config::timing::TAB_HSCROLL_HIDE_MS;
use crate::editor::EditorEvent;
use crate::markdown::{PreviewSrc, scan_blocks_incremental_rope, scan_blocks_rope};
use crate::scrollbar::horizontal_scrollbar;

impl EditorApp {
    pub(super) fn make_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> usize {
        // 自研编辑器组件：创建即空文档；语法高亮语言在打开文件时 set_language。
        let editor = cx.new(|cx| crate::editor::CodeEditor::new(cx));

        // 编辑内容被修改 → 置脏 + 同步内容/行列缓存 + 预览增量入口。
        let ed = editor.clone();
        let edit_sub = cx.subscribe(&ed, |this, emitter, ev: &EditorEvent, cx| {
            if let Some(tab) = this.tabs.iter_mut().find(|t| t.editor == emitter) {
                if ev == &EditorEvent::Edited {
                    let _t_change = crate::prof::begin();
                    // 内容被编辑 → 侧边栏搜索缓存失效 + 版本号推进
                    this.search_scan = None;
                    tab.edit_version = tab.edit_version.wrapping_add(1);
                    // 编辑侧通道 → dirty 区间（bounding box，当前文本字节坐标）：
                    // CodeEditor 记录最近一次编辑区间（字节），每轮 Change 1:1 取走。
                    if let Some((s, oe, ne)) = tab.editor.update(cx, |ed, _| ed.take_last_edit()) {
                        let delta = ne as i64 - oe as i64;
                        tab.preview_dirty = Some(match tab.preview_dirty {
                            None => (s, ne),
                            Some((lo, hi)) => {
                                // 把上一帧的 dirty 平移到本帧（编辑点之前不变、之后随 δ 平移）
                                // 再并入 [s, ne)（新增变化区间），取 bounding box。
                                let hi_shifted = if hi > oe {
                                    hi as i64 + delta
                                } else if hi >= s {
                                    ne as i64
                                } else {
                                    hi as i64
                                };
                                (lo.min(s), hi_shifted.max(ne as i64).max(0) as usize)
                            }
                        });
                    }
                    // 行/字/光标走 CodeEditor 的 O(1) 计数，不再每键物化全文。
                    // 行数用显示语义（`str::lines()` 口径，与文件打开/状态栏一致），
                    // 不用含幻影尾行的编辑器模型 `line_count()`。
                    let (line_count, char_count, cursor_row, cursor_col) = {
                        let st = tab.editor.read(cx);
                        let cursor = st.cursor_position();
                        (st.display_line_count(), st.char_count(), cursor.0, cursor.1)
                    };
                    // 仅在开启预览时才同步 Markdown 预览，且**合成一轮**：
                    // 一次同步任务里循环「快照 → 后台扫描 → 装配 → 检查版本是否有新编辑」，
                    // 有新编辑就续跑，直到追上最新内容才停——打字期间**不会叠起多轮后台扫描**
                    // （`preview_syncing` 闸门），从「最后一次按键」起该轮扫描结束即收敛。
                    // 大文档用**增量扫描**（[`scan_blocks_incremental_rope`]，复用上一版索引只重扫
                    // 变化区，16 万行从几百 ms 降到几十 ms）在后台线程做，UI 线程只留快照/装配。
                    if tab.preview_on && !this.preview_pending && !this.preview_syncing {
                        this.preview_pending = true;
                        this.preview_syncing = true;
                        this.preview_seq = this.preview_seq.wrapping_add(1);
                        let seq = this.preview_seq;
                        cx.spawn(async move |this, cx| {
                            loop {
                                // 1) UI 线程：快照当前正文 + 取上一版索引（增量基底）+ 版本号
                                //    + 取走 dirty（自上次快照以来累积的变化区间；取走即清零）。
                                //    快照与 dirty 同帧取：dirty 覆盖的正是 快照 vs 上一版 的差异。
                                let Some((snapshot, panel_bg, base_dir, prev, version, dirty)) =
                                    this
                                        .update(cx, |this, cx| {
                                            let panel_bg = this.panel_bg();
                                            let base_dir = this
                                                .tabs
                                                .get(this.active_tab)
                                                .and_then(|t| t.file_path.as_ref())
                                                .and_then(|p| p.parent())
                                                .map(|d| d.to_path_buf());
                                            let snapshot = this
                                                .tabs
                                                .get(this.active_tab)
                                                .filter(|t| t.preview_on)
                                                .map(|t| t.editor.read(cx).text().clone());
                                            let prev = this
                                                .tabs
                                                .get(this.active_tab)
                                                .and_then(|t| t.preview_prev.clone());
                                            let version = this
                                                .tabs
                                                .get(this.active_tab)
                                                .map(|t| t.edit_version)
                                                .unwrap_or(0);
                                            let dirty = this
                                                .tabs
                                                .get_mut(this.active_tab)
                                                .and_then(|t| t.preview_dirty.take());
                                            snapshot.map(|snap| {
                                                (snap, panel_bg, base_dir, prev, version, dirty)
                                            })
                                        })
                                        .ok()
                                        .flatten()
                                else {
                                    break;
                                };
                                // 2) 后台线程：增量（有上一版）或全量扫描。
                                //    有 dirty → 增量扫描跳过 O(n) 快照 diff（O(k)）；
                                //    无 dirty（首轮 / 高亮器未装）→ 走快照 diff 兜底。
                                //    快照 = 编辑器 Rope 的 O(1) COW 克隆（不物化 6.4MB 全文），
                                //    后台扫描只按 dirty 区从 rope 切片（见
                                //    scan_blocks_incremental_rope），UI 线程不再背全文。
                                let index = match &prev {
                                    Some((p_src, p_idx)) => {
                                        let p_src = p_src.clone();
                                        // Rc 非 Send：物化一份 BlockIndex 给后台线程。
                                        // O(块数) memcpy（几 ms）仍远小于全量扫描（几百 ms）。
                                        let p_idx = (**p_idx).clone();
                                        let snap = snapshot.clone();
                                        let dirty = dirty.map(|(a, b)| a..b);
                                        cx.background_executor()
                                            .spawn(async move {
                                                scan_blocks_incremental_rope(
                                                    &p_src,
                                                    &p_idx,
                                                    &snap,
                                                    dirty,
                                                )
                                            })
                                            .await
                                    }
                                    None => {
                                        let snap = snapshot.clone();
                                        cx.background_executor()
                                            .spawn(async move { scan_blocks_rope(&snap) })
                                            .await
                                    }
                                };
                                // 3) UI 线程：装配最新代，记录增量基底；有新版本文 继续循环、
                                //    反之结束（合成一轮完毕）。
                                // 索引只持一份 Rc：视图与 `preview_prev` 共享同一块（O(1) 克隆），
                                // 不再每轮 `Rc::new(index.clone())` 整份拷贝（156K 块 ≈ 3.7MB）。
                                let index = std::rc::Rc::new(index);
                                let keep = this
                                    .update(cx, |this, cx| {
                                        if this.preview_seq != seq {
                                            return false;
                                        }
                                        let mut keep = false;
                                        if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                                            if tab.preview_on {
                                                tab.md_preview.update(cx, |view, cx| {
                                                    view.set_background(panel_bg);
                                                    view.set_base_dir(base_dir);
                                                    view.set_source_with_index(
                                                        PreviewSrc::Rope(snapshot.clone()),
                                                        index.clone(),
                                                        cx,
                                                    );
                                                });
                                                cx.notify();
                                            }
                                            tab.preview_prev = Some((snapshot.clone(), index.clone()));
                                            keep = tab.preview_on && tab.edit_version != version;
                                        }
                                        keep
                                    })
                                    .unwrap_or(false);
                                if !keep {
                                    break;
                                }
                            }
                            // 4) 释放闸门（本轮追上最新内容后）
                            let _ = this.update(cx, |this, _| {
                                this.preview_pending = false;
                                this.preview_syncing = false;
                            });
                        })
                        .detach();
                    }
                    // 只对 readonly_preview（EPUB/PDF，编辑区为空）镜像全文到 content；
                    // 普通文本/Markdown 以编辑器 Rope 为准，不做整份副本（省内存）。
                    if tab.readonly_preview {
                        let content = tab.editor.read(cx).text().to_string();
                        tab.content = SharedString::from(content);
                    }
                    tab.line_count = line_count;
                    tab.char_count = char_count;
                    tab.cursor_row = cursor_row;
                    tab.cursor_col = cursor_col;
                    if !tab.dirty {
                        tab.dirty = true;
                    }
                    cx.notify();
                    crate::prof::end("tab_change", _t_change);
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
            pdf_preview: None,
            preview_on: false,
            preview_fullscreen: false,
            readonly_preview: false,
            editor_pending: false,
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
        let focus = self.tabs[idx].editor.read(cx).focus_handle();
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
            let focus = self.tabs[idx].editor.read(cx).focus_handle();
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
        self.search_scan = None; // 选项卡集合变化 → 搜索缓存失效
        if self.tabs.len() == 1 {
            let tab = &mut self.tabs[0];
            tab.editor.update(cx, |ed, _cx| {
                ed.set_text("");
            });
            tab.md_preview.update(cx, |view, cx| view.set_source("", cx));
            if let Some(pdf) = tab.pdf_preview.take() {
                let _ = pdf.update(cx, |view, cx| view.release(window, cx));
            }
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
            let _ = pdf.update(cx, |view, cx| view.release(window, cx));
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

    pub(super) fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;
        let (text_primary, text_minor, tab_active_bg, tab_active_border, chip_bg, chip_text, bar_bg, panel_bg) = (
            self.text_primary(),
            self.text_minor(),
            self.tab_active_bg(),
            self.tab_active_border(),
            self.chip_bg(),
            self.chip_text(),
            self.bar_bg(),
            self.panel_bg(),
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

            // 激活选项卡顶部 2px 蓝色强调线（色 = scrollbar::RESIZE_ACCENT）。
            // 用绝对定位细条绘制：本版 gpui 的 border_color 是整圈一个色，
            // 没有“只染某一边”的 API，无法用 border 实现顶蓝其余透明。
            let accent = is_active.then(|| {
                div()
                    .id(SharedString::from(format!("tab-accent-{idx}")))
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .right(px(0.))
                    .h(px(1.))
                    .bg(rgb(crate::config::colors::RESIZE_ACCENT))
                    .into_any_element()
            });

            let mut el = div()
                .id(SharedString::from(format!("tab-{idx}")))
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink(1.)
                .relative()
                .bg(if is_active {
                    panel_bg
                } else {
                    rgba(0x00000000).into()
                })
                .cursor(CursorStyle::PointingHand)
                .text_size(px(12.))
                .text_color(text_primary)
                .child(if dirty { "● " } else { "" })
                .child(name)
                .child(close);
            if let Some(accent) = accent {
                el = el.child(accent);
            }
            tabs.push(
                el.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                        this.activate_tab(idx, window, cx);
                    }),
                )
                .into_any_element(),
            );
        }

        // 新建(+)：手绘「＋」形图标（横/竖两条圆角细条居中），替代文本 "+"，
        // 避免不同字体下粗细/偏移不一显得歪斜；带 hover/按下交互反馈。
        let add = div()
            .id("tab-add")
            .debug_selector(|| "tab-add".into())
            .w(px(24.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .bg(chip_bg)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(tab_active_bg))
            .active(|style| style.bg(tab_active_border))
            .on_click(
                cx.listener(|this: &mut EditorApp, _ev, window, cx| this.new_tab(window, cx)),
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
                            .bg(chip_text),
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
                            .bg(chip_text),
                    ),
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

        // 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现；
        // 半透明、贴着标签底部、浮在上方（不额外占高度）。
        let mut hscroll_overlay: Vec<gpui::AnyElement> = Vec::new();
        let hscroll_dragging = self.tab_bar_hscroll.drag.borrow().is_some();
        if self.tab_bar_hscroll.handle.max_offset().x > px(0.)
            && (*self.tab_bar_hscroll.visible.borrow() || hscroll_dragging)
        {
            hscroll_overlay.push(
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
            );
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
            .bg(bar_bg)
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
            .child(add)
            .children(right)
    }
}
