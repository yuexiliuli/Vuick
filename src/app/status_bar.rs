//! status_bar.

use super::*;

impl EditorApp {
    /// 切换当前激活项的 PDF/EPUB 左侧目录面板「打开/收起」。
    fn toggle_doc_toc(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if let Some(pdf) = &tab.pdf_preview {
            let pv = pdf.clone();
            pv.update(cx, |v, cx| v.toggle_toc(cx));
        } else {
            let md = tab.md_preview.clone();
            md.update(cx, |v, cx| v.toggle_toc_panel(cx));
        }
        cx.notify();
    }

    pub(super) fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
        let mut left = div()
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

        // PDF/EPUB 目录「打开/收起」开关：放在状态栏左侧、语言徽标之后。
        // PDF → 书签目录（pdf_preview）；EPUB → PDF 式左侧目录面板（md_preview）。
        let toc_open = tab.and_then(|t| {
            if let Some(pdf) = &t.pdf_preview {
                let p = pdf.read(cx);
                (!p.outlines().is_empty()).then_some(p.toc_open)
            } else {
                let md = t.md_preview.read(cx);
                (md.toc_panel_enabled() && md.toc_has_content()).then_some(md.toc_open_state())
            }
        });
        if let Some(open) = toc_open {
            let btn = div()
                .id("status-toc-btn")
                .debug_selector(|| "status-toc-btn".into())
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(0xeef2ff))
                .text_size(px(12.))
                .text_color(rgb(0x4338ca))
                .cursor(CursorStyle::PointingHand)
                .child(if open { "☰ 收起" } else { "☰ 目录" })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                        this.toggle_doc_toc(cx);
                    }),
                );
            left = left.child(btn);
        }

        // 右侧：阅读/统计按钮
        // PDF / EPUB 等只读预览自带全屏阅读，不适用「全屏/还原」「换行」切换，故隐藏。
        let preview_on = tab.map(|t| t.preview_on).unwrap_or(false);
        // 只读预览文档（按后缀识别为 PDF/EPUB 时置位）→ 隐藏阅读控制按钮
        let readonly_preview = tab.map(|t| t.readonly_preview).unwrap_or(false);
        let fullscreen = tab.map(|t| t.preview_fullscreen).unwrap_or(false);
        let wrap_on = tab
            .map(|t| t.md_preview.read(cx).is_wrap())
            .unwrap_or(false);
        let mut right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0();

        // 全屏/还原按钮：仅在开启预览且非只读预览（PDF/EPUB）时显示
        if preview_on && !readonly_preview {
            let btn = div()
                .id("status-fullscreen-btn")
                .debug_selector(|| "status-fullscreen-btn".into())
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(0xeef2ff))
                .text_size(px(12.))
                .text_color(rgb(0x4338ca))
                .cursor(CursorStyle::PointingHand)
                .child(if fullscreen { "还原" } else { "全屏" })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(
                        |this: &mut EditorApp, _ev, window, cx| {
                            this.toggle_preview_fullscreen(window, cx);
                        },
                    ),
                );
            right = right.child(btn);
        }

        // 换行开关按钮：仅非只读预览（非 PDF/EPUB）且非 Markdown 时显示。
        // Markdown 编辑区与预览区默认/始终自动换行，不提供切换（隐藏该按钮）。
        let is_markdown = tab
            .map(|t| t.file_path.as_ref().map(|p| Self::is_markdown_path(p.as_path())))
            .flatten()
            .unwrap_or(false);
        if !readonly_preview && !is_markdown {
            let btn = div()
                .id("status-wrap-btn")
                .debug_selector(|| "status-wrap-btn".into())
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(0xeef2ff))
                .text_size(px(12.))
                .text_color(rgb(0x4338ca))
                .cursor(CursorStyle::PointingHand)
                .child(if wrap_on { "换行" } else { "不换行" })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                        this.toggle_wrap(window, cx);
                    }),
                );
            right = right.child(btn);
        }

        let right = right
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
