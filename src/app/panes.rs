//! panes.

use super::*;
use gpui_component::text::TextView;

impl EditorApp {
    pub(super) fn render_editor_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.tabs[self.active_tab].editor.clone();
        let font_size = px(self.font_size);
        div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .relative()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(self.panel_bg())
            .border_1()
            .border_color(self.bar_border())
            .rounded_md()
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

    pub(super) fn render_preview_pane(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab = &self.tabs[self.active_tab];
        div()
            .flex()
            .flex_row()
            .flex_1()
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
                    .overflow_y_scroll()
                    .track_scroll(&self.preview_scroll)
                    .bg(self.panel_bg())
                    .px_4()
                    .py_3()
                    .text_size(px(14.))
                    .child(
                        TextView::new(&tab.md_view)
                            .selectable(true)
                            .selection_format(gpui_component::text::SelectionFormat::Source),
                    ),
            )
    }
}
