//! 侧边栏：可折叠/展开的文件（标签页）列表，停靠在内容区左侧。
//!
//! 折叠/展开开关放在窗口标题栏右上角（最小化/最大化/关闭按钮旁，见
//! [`EditorApp::render_title_bar`]）。侧边栏只占据内容区（选项卡栏之下、
//! 状态栏之上），不参与标题栏/状态栏布局。

use super::*;

impl EditorApp {
    /// 内容区：侧边栏展开时在左侧停靠（占固定宽），右侧为原有正文 `body`；
    /// 收起时仅返回正文本身。
    pub(super) fn render_body_sidebar(
        &mut self,
        body: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !self.sidebar_open {
            return body;
        }
        div()
            .id("app-body-with-sidebar")
            .debug_selector(|| "app-body-with-sidebar".into())
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            .child(self.render_sidebar(cx))
            .child(body)
            .into_any_element()
    }

    /// 侧边栏内容：列出当前所有已打开标签页，点击切换激活页。
    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (side_bg, border, fg, muted) = (
            self.bar_bg(),
            self.bar_border(),
            self.text_primary(),
            self.text_minor(),
        );
        let active_bg = self.tab_active_bg();
        let active_fg = self.text_primary();
        let n = self.tabs.len();

        let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(n);
        for i in 0..n {
            let name = self.tabs[i]
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let is_active = i == self.active_tab;
            let row_bg = if is_active { active_bg } else { gpui::transparent_black() };
            let row_fg = if is_active { active_fg } else { muted };
            let row = div()
                .id(SharedString::from(format!("sidebar-tab-{i}")))
                .debug_selector(move || format!("sidebar-tab-{i}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .bg(row_bg)
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(if is_active { active_bg } else { border }))
                .on_click(cx.listener(move |this: &mut EditorApp, _, window, cx| {
                    this.activate_tab(i, window, cx);
                }))
                .child(
                    Icon::new(IconName::File)
                        .size(px(14.))
                        .text_color(row_fg)
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(row_fg)
                        .child(name),
                );
            rows.push(row.into_any_element());
        }

        div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .w(px(crate::app::SIDEBAR_WIDTH))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(side_bg)
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .id("sidebar-header")
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .h(px(36.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .child(format!("文件 ({n})")),
                    ),
            )
            .child(
                div()
                    .id("sidebar-list")
                    .debug_selector(|| "sidebar-list".into())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .py_1()
                    .child(
                        div()
                            .id("sidebar-list-inner")
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(rows),
                    ),
            )
    }
}
