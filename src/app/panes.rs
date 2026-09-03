//! panes.

use super::*;

impl EditorApp {
    pub(super) fn render_editor_pane(
        &mut self,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let editor = self.tabs[self.active_tab].editor.clone();
        let font_size = px(self.font_size);
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
            // 从资源管理器拖入文件 → 打开（Input 本身不接收拖放）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            // 字号直接作用于编辑器自身：`Editor` 渲染时会显式设置 text_size，
            // 会覆盖父级继承值，因此在此设 text_size 才真正生效。
            .child(
                Editor::new(&editor)
                    .h(gpui::relative(1.))
                    .bordered(false)
                    .text_size(font_size),
            )
            // 透明覆盖层（画在编辑器之上，命中测试最靠前）：拦截 Ctrl+滚轮缩放。
            // 编辑器的自带滚动处理在内容溢出时会 stop_propagation()，父级监听收不到，
            // 因此必须用这个更靠前的覆盖层先处理。普通滚轮不拦截，仍传给下层编辑区滚动。
            .child(
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
                    )),
            )
    }

    /// 编辑区 / 预览区之间的分割线（可拖拽调整分屏比例）。
    pub(super) fn render_splitter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = self.bar_border();
        let line_color: gpui::Hsla = if self.splitter_hovered || self.split_drag.is_some() {
            gpui::rgb(0x2563eb).into()
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
        let tab = &self.tabs[self.active_tab];
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
                    // 自研 Markdown 渲染器：行级虚拟化 + 自带置顶滚动条；
                    // Entity 直接作为子元素渲染（内部 size_full）。
                    .child(tab.md_preview.clone()),
            )
    }

    /// 全屏预览（EPUB 阅读模式）：渲染占满整个内容区，不显示编辑区/分割线。
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
            .child(tab.md_preview.clone())
    }
}
