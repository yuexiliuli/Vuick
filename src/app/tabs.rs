//! tabs.

use super::*;
use crate::config::timing::TAB_HSCROLL_HIDE_MS;
use crate::scrollbar::horizontal_scrollbar;
use crate::ui::motion::hover_spring;

/// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
struct TabBarColors {
    text_primary: gpui::Hsla,
    text_minor: gpui::Hsla,
    tab_active_bg: gpui::Hsla,
    tab_active_border: gpui::Hsla,
    hover_fill: gpui::Hsla,
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
            live2_dirty: false,
            _live2_sub: None,
            live_vscroll: gpui::ScrollHandle::new(),
            md_wy: None,
            md_wy_vscroll: gpui::ScrollHandle::new(),
            md_wy_pending: None,
            md_wy_install_seq: 0,
            md_wy_early: None,
            md_wy_pending_full: None,
            epub_images: Default::default(),
            epub_notes: Default::default(),
            edit_version: 0,
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
        // 释放被移除选项卡持有的重资源（PDF 纹理 / 图片解码），实体随后随之释放。
        if let Some(pdf) = self.tabs[idx].pdf_preview.take() {
            pdf.update(cx, |view, cx| view.release(window, cx));
        }
        if let Some(img) = self.tabs[idx].image_viewer.take() {
            img.update(cx, |view, cx| view.release(window, cx));
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            // 关闭最后一个选项卡 → 零选项卡，回到欢迎页：不退出、不新建空选项卡、
            // 不 panic。此时没有编辑区可聚焦，焦点给应用根（窗口级快捷键照样可用）。
            self.active_tab = 0;
            let root = self.focus_handle(cx);
            window.focus(&root, cx);
            self.status = SharedString::from("已关闭");
            self.status_ok = true;
            cx.notify();
            return;
        }
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
    fn tab_bar_colors(&self, _cx: &mut Context<Self>) -> TabBarColors {
        let c = self.chrome();
        TabBarColors {
            text_primary: self.text_primary(),
            text_minor: self.text_minor(),
            tab_active_bg: self.tab_active_bg(),
            tab_active_border: self.tab_active_border(),
            hover_fill: c.hover_fill.into(),
        }
    }

    /// 单个选项卡元素：名称/脏标记/关闭钮/激活顶线 + 拖拽幽灵 + 点击激活。
    fn tab_bar_item(
        tab: &EditorTab,
        idx: usize,
        is_active: bool,
        colors: &TabBarColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = tab
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string());
        let dirty = tab.dirty;

        // 元素级 hover 状态：跨帧记住「该 tab 是否被悬停」，关闭 × 因此在
        // 激活项恒定可见、非激活项悬停显现（VS Code 观感）。
        let view_id = window.current_view();
        let tab_hover = window.use_keyed_state(("tab", idx), cx, |_, _| false);
        let hovered = *tab_hover.read(cx);

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
            .hover(|style| style.bg(colors.hover_fill))
            .child("×")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    this.close_tab(idx, window, cx);
                    cx.stop_propagation();
                }),
            )
            // P2-10：仅视觉的 × 悬停显现淡入（激活项恒定、非激活项随行 hover）。
            .with_spring(
                format!("tab-close-op-{idx}"),
                hover_spring(if is_active || hovered { 1.0 } else { 0.0 }),
                move |this, v| this.opacity(v),
            );

        let mut el = div()
            .id(SharedString::from(format!("tab-{idx}")))
            .debug_selector(move || format!("tab-{idx}"))
            .h(px(26.))
            .my(px(3.))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink(1.)
            .relative()
            .cursor(CursorStyle::PointingHand)
            .text_size(px(12.))
            .text_color(colors.text_primary)
            .child(if dirty { "● " } else { "" })
            .child(SharedString::from(name.clone()))
            .child(close);
        // 激活项 = VS Code 卡片式：26px 圆角白卡浮在灰带上（浅色下）；
        // 非激活项透明底 + hover 填充（背景由底部弹簧统一淡入，P2-10）。
        el = if is_active {
            el.rounded_md()
                .border_1()
                .border_color(colors.tab_active_border)
        } else {
            el
        };
        // hover 状态跨帧记录，驱动关闭 × 悬停显现与行背景淡入。
        el = el.on_hover({
            let st = tab_hover.clone();
            move |hovered, _window, cx| {
                st.update(cx, |h, _| *h = *hovered);
                cx.notify(view_id);
            }
        });
        // 多窗口拖拽：按住选项卡拖动 → 跟出“胶囊”幽灵；
        // 在窗口外松开由 EditorApp 根视图决定「拆独立窗口 / 还原回主窗口」。
        let drag_data = TabDragData {
            idx,
            name: SharedString::from(name),
        };
        let ghost_name = drag_data.name.clone();
        // P2-10：背景弹簧——激活=白卡轻淡入、非激活=hover 填充淡入（同一曲线）；
        // 激活底色 / hover 底色自带 alpha，`opacity` 乘性，v=1 即原色，语义不变。
        let card = if is_active {
            colors.tab_active_bg
        } else {
            colors.hover_fill
        };
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
        .with_spring(
            format!("tab-bg-{idx}"),
            hover_spring(if is_active || hovered { 1.0 } else { 0.0 }),
            move |this, v| this.bg(card.opacity(v)),
        )
        .into_any_element()
    }

    /// 整条选项卡栏最右侧的眼睛图标：仅当"当前选中的文件"是 Markdown（.md / .markdown）
    /// 时才显示，点击切换当前选项卡的 Markdown 预览（与「查看 → 预览」一致）。
    /// 用纯 div 画出（椭圆外框 + 中心瞳孔），避免依赖 emoji 字体；蓝色=已开，灰色=已关。
    fn tab_eye_button(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
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
        let eye_color: gpui::Hsla = if on {
            self.chrome().pill_fg.into()
        } else {
            self.chrome().text_tertiary.into()
        };
        let hover_fill: gpui::Hsla = self.chrome().hover_fill.into();
        // P2-10：hover 状态跨帧记录 → 背景淡入。
        let hover = window.use_keyed_state("tab-eye-hover", cx, |_, _| false);
        let hovered = *hover.read(cx);
        let view_id = window.current_view();
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
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
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
                .with_spring(
                    "tab-eye-bg",
                    hover_spring(if hovered { 1.0 } else { 0.0 }),
                    move |this, v| this.bg(hover_fill.opacity(v)),
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

    /// 选项卡栏：横跨右列顶部的选项卡行，左侧滚动区 + （按需）眼睛按钮；
    /// 右键整条栏弹「新建文件 / 打开文件」上下文菜单（取代已移除的「+」按钮）。
    /// 底边绘一条右列宽发丝线（`bar_border`），分隔下方编辑区正文。
    /// 零选项卡（`tabs.is_empty()`，即欢迎页呈现时）提前返回空元素——选项卡栏
    /// 在 flex 列中占零高度，欢迎页从标题栏下直接铺到状态栏。
    pub(super) fn render_tab_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.tabs.is_empty() {
            return div().into_any_element();
        }
        let active = self.active_tab;
        let colors = self.tab_bar_colors(cx);
        let mut tabs: Vec<gpui::AnyElement> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            tabs.push(Self::tab_bar_item(tab, i, i == active, &colors, window, cx));
        }

        // 选项卡栏右侧：固定放（按需）眼睛图标；标签过多时由左侧滚动区横向滚动
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(eye) = self.tab_eye_button(window, cx) {
            right.push(eye);
        }

        // 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现。
        let mut hscroll_overlay: Vec<gpui::AnyElement> = Vec::new();
        if let Some(o) = self.tab_bar_hscroll_overlay() {
            hscroll_overlay.push(o);
        }

        div()
            .id("tab-bar")
            .debug_selector(|| "tab-bar".into())
            .relative()
            .w_full()
            // 左侧不留内边距：第一个选项卡紧贴窗口左边缘；右侧保留 px_2 给眼睛按钮
            .pl_0()
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(self.mica_bar_bg())
            // 右键整条栏（含点在各选项卡上，事件冒泡到这）→ 「新建文件 / 打开文件」上下文菜单
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::on_tab_bar_right_click),
            )
            // 选项卡栏↔编辑区的发丝线由下方编辑区顶边绘制（见 render_body_card，
            // 此处不画底边框，避免被后绘制的编辑区面板盖住）。
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
                            .items_center()
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
            .children(right)
            .into_any_element()
    }

    /// 选项卡栏右键 → 在光标处弹「新建文件 / 打开文件」上下文菜单（取代已移除的「+」
    /// 按钮）。关闭（选择/Esc/点击外）经 `DismissEvent` 订阅清理；先收掉上一份菜单
    /// （若有）再在光标处新建（旧订阅带 emitter 身份校验，不会误清新菜单）。
    fn on_tab_bar_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 事件继续冒泡到树根会额外触发图片/视频右键「锁定」菜单（on_media_right_click），
        // 此处拦截，保证右键选项卡栏只弹本菜单。
        cx.stop_propagation();
        self.tab_bar_menu_entity = None;
        self.tab_bar_menu_pos = None;
        self.tab_bar_menu_sub = None;
        let weak = cx.weak_entity();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            menu.item({
                let weak = weak.clone();
                PopupMenuItem::element(move |_, _| {
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .child("新建文件")
                        .debug_selector(|| "tab-bar-menu-item-new".into())
                })
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| this.new_tab(window, cx));
                })
            })
            .item({
                let weak = weak.clone();
                PopupMenuItem::element(move |_, _| {
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .child("打开文件")
                        .debug_selector(|| "tab-bar-menu-item-open".into())
                })
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| this.open_dialog(window, cx));
                })
            })
        });
        // 聚焦并记住开菜单前的焦点（关闭后归还）
        let previous_focus = window.focused(cx);
        let focus_handle = menu.read(cx).focus_handle(cx);
        menu.update(cx, |m, _| m.set_previous_focus(previous_focus));
        window.focus(&focus_handle, cx);
        // 关闭（选择/Esc/点击外）→ 清理；emitter 身份校验避免旧菜单的关闭误清新菜单
        let menu_entity = menu.clone();
        let sub = cx.subscribe(&menu_entity, move |this, emitter, _: &DismissEvent, cx| {
            if this.tab_bar_menu_entity.as_ref() == Some(&emitter) {
                this.tab_bar_menu_entity = None;
                this.tab_bar_menu_pos = None;
            }
            cx.notify();
        });
        self.tab_bar_menu_pos = Some(ev.position);
        self.tab_bar_menu_entity = Some(menu);
        self.tab_bar_menu_sub = Some(sub);
        cx.notify();
    }

    /// 选项卡栏右键菜单 Overlay：按右键时记录的**窗口坐标**定位
    /// （`AnchoredPositionMode::Window` + `position(pos)`——`position()` 才是 gpui 语义里
    /// 「窗口坐标」入参；旧的 `offset(pos)` 缺 `position()` 时会被当作「相对锚定元素自身
    /// 静态布局位置的偏移」，而本菜单包在 `#tab-bar-menu` 外壳 flex 流内、静态位在窗口
    /// 底部 → 菜单被推出窗口再被 `snap_to_window_with_margin` 钳回底部（沉底 bug）。
    /// 媒体右键菜单因直接作为外壳 flex 直子、静态位恰为 (0,0) 才表现出正常而已，同隐患。
    pub(super) fn render_tab_bar_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(pos) = self.tab_bar_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.tab_bar_menu_entity.clone() else {
            return div().into_any_element();
        };
        div()
            .id("tab-bar-menu")
            .debug_selector(|| "tab-bar-menu".into())
            .child(
                deferred(
                    anchored()
                        .position_mode(gpui::AnchoredPositionMode::Window)
                        .anchor(Anchor::TopLeft)
                        .position(pos)
                        .snap_to_window_with_margin(Edges::all(px(8.)))
                        .child(menu),
                )
                .with_priority(100),
            )
            .into_any_element()
    }
}
