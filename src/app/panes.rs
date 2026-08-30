//! panes.

use super::*;
use gpui_component::text::TextView;

impl EditorApp {
    pub(super) fn render_editor_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.tabs[self.active_tab].editor.clone();
        div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .relative()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(EDIT_BG)
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .rounded_md()
            .overflow_hidden()
            // 文字样式会沿元素树继承，`Input` 的代码编辑器随字号重新排版
            .text_size(px(self.font_size))
            // 从资源管理器拖入文件 → 打开（Input 本身不接收拖放）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &Arc<ExternalPaths>, window, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.open_file(&p.to_string_lossy(), window, cx);
                    }
                },
            ))
            .child(Editor::new(&editor).h(gpui::relative(1.)).bordered(false))
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
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<Arc<ExternalPaths>>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &Arc<ExternalPaths>, window, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.open_file(&p.to_string_lossy(), window, cx);
                    }
                },
            ))
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
                    .text_size(px(14.))
                    .child(
                        TextView::new(&tab.md_view)
                            .selectable(true)
                            .selection_format(gpui_component::text::SelectionFormat::Source),
                    ),
            )
            .child(Scrollbar::new(self.preview_scroll.clone()))
    }
}
