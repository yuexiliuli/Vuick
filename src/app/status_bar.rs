//! status_bar.

use super::*;

impl EditorApp {
    pub(super) fn render_status_bar(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tabs.get(self.active_tab);
        let (ln, col) = tab
            .map(|t| (t.cursor_row + 1, t.cursor_col + 1))
            .unwrap_or((1, 1));
        let (line_count, char_count) = tab.map(|t| (t.line_count, t.char_count)).unwrap_or((0, 0));
        let dirty = tab.map(|t| t.dirty).unwrap_or(false);
        let language = self.language_label().to_string();
        let (text_primary, text_minor, chip_text, surface_bg, bar_border) = (
            self.text_primary(),
            self.text_minor(),
            self.chip_text(),
            self.surface_bg(),
            self.bar_border(),
        );

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
                            .text_color(if dirty { rgb(0xdc2626) } else { rgb(0x9ca3af) })
                            .child(if dirty { "●" } else { "" }),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(text_primary)
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

        // 右侧：统计信息
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(chip_text)
                    .child(format!("Ln {ln}, Col {col}")),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(text_minor)
                    .child(format!("{line_count} 行")),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(text_minor)
                    .child(format!("{char_count} 字")),
            );

        div()
            .h(px(28.))
            .bg(surface_bg)
            .border_t_1()
            .border_color(bar_border)
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .child(left)
            .child(right)
    }
}
