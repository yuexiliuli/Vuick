//! panes.

use super::*;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

impl EditorApp {
    /// 当前选项卡的预览视图：PDF 选项卡用 `PdfViewer`（整页原样），
    /// 其余用 `MarkdownPreview`。
    fn preview_element(tab: &EditorTab) -> gpui::AnyElement {
        match &tab.pdf_preview {
            Some(pdf) => pdf.clone().into_any_element(),
            None => tab.md_preview.clone().into_any_element(),
        }
    }

    pub(super) fn render_editor_pane(
        &mut self,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // PDF / EPUB 等只读预览文档不生成编辑区（即使处于分屏/编辑-only 布局也直接跳过）。
        let readonly = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.readonly_preview)
            .unwrap_or(false);
        if readonly {
            return div().id("editor-area").size_full();
        }
        // 大文档异步打开：编辑器内容已就绪，但让预览先把内容画出来。
        // 这一帧渲染轻量占位（不含 16 万行编辑器 → 布局轻快），并复位标记请求
        // 下一帧挂载编辑器；编辑器的首帧 DisplayMap 布局（可能冻结一两秒）
        // 因此排在用户看到预览内容之后，而不是挡在最前面。
        if self.tabs[self.active_tab].editor_pending {
            self.tabs[self.active_tab].editor_pending = false;
            cx.notify();
            let panel_bg = self.panel_bg();
            let muted = self.text_minor();
            // 占位与正式编辑区使用**同一套 flex 布局参数**（flex_1/grow/min_w(0)/
            // overflow_hidden），否则 `.size_full()`（w:100%）在分屏 flex 行里会占满
            // 整行，把预览顶出窗口——用户看到"编辑区一打开就很宽、不与预览平分"。
            return div()
                .id("editor-area")
                .debug_selector(|| "editor-area".into())
                .flex_1()
                .flex_grow(grow)
                .min_w(px(0.))
                .min_h(px(0.))
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .bg(panel_bg)
                .child(
                    div()
                        .text_color(muted)
                        .text_size(px(13.))
                        .child("正在载入编辑区…（预览已就绪）"),
                );
        }
        // 所见即所得预览编辑（WYSIWYG）模式：左面板 = 本项目 `WyEditor`
        // （AUTO-GROW，内部无滚动条，宿主负责滚动 + 自绘纵向滚动条）。
        let live_any: Option<gpui::AnyElement> = self.tabs[self.active_tab]
            .live2
            .clone()
            .map(|e| e.into_any_element());
        if let Some(live_any) = live_any {
            let text_color = self.text_primary();
            let fs = self.font_size;
            // 纵向滚动句柄（跨帧稳定）：`#live-scroll` 原生 `overflow_y_scroll` 在此
            // GPUI 不绘制可见滚动条，右侧 `Scrollbar::vertical` 自绘跟随它。
            let vhand = self.tabs[self.active_tab].live_vscroll.clone();
            return div()
                .id("editor-area")
                .debug_selector(|| "editor-area".into())
                .relative()
                .flex_1()
                .flex_grow(grow)
                .min_w(px(0.))
                .min_h(px(0.))
                .bg(self.panel_bg())
                .overflow_hidden()
                .child(
                    div()
                        .id("live-scroll")
                        .debug_selector(|| "live-scroll".into())
                        .relative()
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&vhand)
                        .child(
                            div()
                                .id("live-body")
                                .debug_selector(|| "live-body".into())
                                .w_full()
                                .px(px(12.))
                                .py(px(8.))
                                .text_size(px(fs))
                                .text_color(text_color)
                                .child(live_any),
                        ),
                )
                .child(
                    div()
                        .id("live-vscrollbar-overlay")
                        .debug_selector(|| "live-vscrollbar-overlay".into())
                        .absolute()
                        .inset_0()
                        .child(
                            Scrollbar::vertical(&vhand)
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                )
                .child(Self::zoom_overlay(cx));
        }
        let editor = self.tabs[self.active_tab].editor.clone();
        // 字号覆盖（用户 Ctrl+=/- 调节；None=主题默认）。渲染时由 CodeEditor 自身读取。
        let font_size = self.font_size;
        editor.update(cx, |ed, _| ed.set_font_override(Some(font_size)));
        div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .relative()
            // grow = 编辑区占宽比例（无预览时为 1）
            .flex_1()
            .flex_grow(grow)
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(self.panel_bg())
            // 无边框、无圆角：编辑区占满整块区域，不留白
            .overflow_hidden()
            // 从资源管理器拖入文件 → 打开（编辑器自身不接收拖放）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            .child(editor)
            // 透明覆盖层（画在编辑器之上，命中测试最靠前)：拦截 Ctrl+滚轮缩放。
            .child(Self::zoom_overlay(cx))
    }

    /// 编辑区顶部的透明滚轮缩放覆盖层（Ctrl+滚轮改字号；普通滚轮透传）。
    fn zoom_overlay(cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("editor-zoom-overlay")
            .debug_selector(|| "editor-zoom-overlay".into())
            .absolute()
            .left(px(0.))
            .right(px(0.))
            .top(px(0.))
            .bottom(px(0.))
            .on_scroll_wheel(cx.listener(
                |this: &mut EditorApp,
                 event: &gpui::ScrollWheelEvent,
                 _window,
                 cx| {
                    if !event.modifiers.control {
                        return;
                    }
                    let delta_y = match event.delta {
                        gpui::ScrollDelta::Lines(lines) => lines.y,
                        gpui::ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                    };
                    // 实测本平台：向上滚动 delta.y>0 → 放大；向下滚动 delta.y<0 → 缩小
                    let step = delta_y.clamp(-1.0, 1.0);
                    if step != 0.0 {
                        this.change_font_size(step, cx);
                    }
                    cx.stop_propagation();
                },
            ))
            .into_any_element()
    }

    /// 编辑区 / 预览区之间的分割线（可拖拽调整分屏比例）。
    pub(super) fn render_splitter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = self.bar_border();
        let line_color: gpui::Hsla = if self.splitter_hovered || self.split_drag.is_some() {
            gpui::rgb(crate::config::colors::RESIZE_ACCENT).into()
        } else {
            border
        };
        div()
            .id("splitter")
            .debug_selector(|| "splitter".into())
            .w(px(6.))
            .h_full()
            .flex_none()
            .cursor(CursorStyle::ResizeColumn)
            .bg(gpui::transparent_black())
            .flex()
            .items_stretch()
            .justify_center()
            // 命中测试加宽（左右各 3px），视觉只有中间 2px 细线
            .on_hover(cx.listener(
                |this: &mut EditorApp, hovered: &bool, _window, cx| {
                    this.splitter_hovered = *hovered;
                    cx.notify();
                },
            ))
            .on_mouse_down(MouseButton::Left, cx.listener(
                |this: &mut EditorApp, ev: &gpui::MouseDownEvent, _window, cx| {
                    this.split_drag = Some((ev.position.x.as_f32(), this.split_ratio));
                    cx.notify();
                },
            ))
            .child(div().h_full().w(px(2.)).flex_none().rounded(px(1.)).bg(line_color))
    }

    pub(super) fn render_preview_pane(
        &mut self,
        _window: &mut Window,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            // grow = 预览区占宽比例
            .flex_grow(grow)
            .flex_shrink_1()
            .flex_basis(px(0.))
            .min_w(px(0.))
            // 预览区也可接收文件拖放
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            .child(
                div()
                    .id("preview-pane")
                    .flex_1()
                    .min_w(px(0.))
                    .bg(self.panel_bg())
                    // PDF → PdfViewer（整页原样）；其余 → Markdown 渲染器。
                    // Entity 直接作为子元素渲染（内部 size_full）。
                    .child(Self::preview_element(&self.tabs[self.active_tab])),
            )
    }

    /// 全屏预览（EPUB / PDF 阅读模式）：渲染占满整个内容区，不显示编辑区/分割线。
    pub(super) fn render_fullscreen_preview(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab = &self.tabs[self.active_tab];
        div()
            .id("fullscreen-preview")
            .debug_selector(|| "fullscreen-preview".into())
            .size_full()
            .bg(self.panel_bg())
            .child(Self::preview_element(tab))
    }
}
