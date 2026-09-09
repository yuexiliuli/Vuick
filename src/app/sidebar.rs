//! 侧边栏：可折叠/展开，顶部左侧为一竖列「目录 / 搜索」切换栏，右侧为当前视图内容。
//!
//! 折叠/展开开关放在窗口标题栏右上角（见 [`EditorApp::render_title_bar`]）；
//! 「目录」「搜索」两个按钮竖列排布，点击各自调用出对应界面。侧边栏只占据内容区
//! （选项卡栏之下、状态栏之上）。

use super::*;
use gpui_component::input::{Input, Position};

/// 侧边栏视图。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarView {
    /// 当前文档目录（Markdown 标题 / PDF 书签）。
    Toc,
    /// 跨打开文件的搜索。
    Search,
}

/// 搜索结果显示上限（避免超大文档每键全量铺满）。
const SEARCH_RESULT_CAP: usize = 200;

impl EditorApp {
    /// 切换侧边栏视图。
    pub(super) fn set_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        self.sidebar_view = view;
        cx.notify();
    }

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

    /// 侧边栏整体：左侧为一竖列切换栏（目录/搜索），右侧为当前视图内容。
    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (side_bg, border) = (self.bar_bg(), self.bar_border());
        let content: gpui::AnyElement = match self.sidebar_view {
            SidebarView::Toc => self.render_sidebar_toc(cx),
            SidebarView::Search => self.render_sidebar_search(cx),
        };
        div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .w(px(crate::app::SIDEBAR_WIDTH))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .bg(side_bg)
            .border_r_1()
            .border_color(border)
            .child(self.render_sidebar_activity_bar(border, cx))
            .child(content)
    }

    /// 侧边栏左侧竖列切换栏：目录 / 搜索 两个按钮上下排布（当前项高亮）。
    fn render_sidebar_activity_bar(
        &mut self,
        border: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_bg = self.tab_active_bg();
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let specs = [
            (SidebarView::Toc, IconName::BookOpen, "目录", "sidebar-btn-toc"),
            (SidebarView::Search, IconName::Search, "搜索", "sidebar-btn-search"),
        ];
        let mut btns: Vec<gpui::AnyElement> = Vec::with_capacity(2);
        for (view, icon, label, id) in specs {
            let is_active = self.sidebar_view == view;
            let bg = if is_active { active_bg } else { gpui::transparent_black() };
            let col = if is_active { fg } else { muted };
            let id_owned = id.to_string();
            let btn = div()
                .id(SharedString::from(id))
                .debug_selector(move || id_owned.clone())
                .w(px(40.))
                .h(px(38.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .mx(px(3.))
                .my(px(1.))
                .rounded_md()
                .bg(bg)
                .text_color(col)
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(if is_active { bg } else { border }))
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                    this.set_sidebar_view(view, cx);
                }))
                .child(Icon::new(icon).size(px(17.)).text_color(col))
                .child(div().text_size(px(9.5)).text_color(col).child(label));
            btns.push(btn.into_any_element());
        }
        div()
            .id("sidebar-activity-bar")
            .debug_selector(|| "sidebar-activity-bar".into())
            .w(px(46.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .py_1()
            .border_r_1()
            .border_color(border)
            .children(btns)
    }

    /// 空态提示。
    fn sidebar_empty_hint(&self, text: &str) -> gpui::AnyElement {
        let muted = self.text_minor();
        div()
            .px_3()
            .py_2()
            .text_size(px(12.))
            .text_color(muted)
            .child(text.to_string())
            .into_any_element()
    }

    /// 目录条目行（返回可再挂 on_click 的 Stateful div）。
    fn toc_row_styled(
        &self,
        id_owned: String,
        level: usize,
        title: String,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
    ) -> gpui::Stateful<gpui::Div> {
        let level = level.min(8);
        let id2 = id_owned.clone();
        div()
            .id(SharedString::from(id_owned))
            .debug_selector(move || id2.clone())
            .flex()
            .items_center()
            .pl(px(6. + level as f32 * 12.))
            .pr(px(8.))
            .py(px(4.))
            .mx(px(4.))
            .rounded_md()
            .text_size(px(12.))
            .text_color(fg)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(active_bg))
            .child(
                div()
                    .w(px(4.))
                    .h(px(4.))
                    .rounded_full()
                    .flex_shrink_0()
                    .mr(px(6.))
                    .bg(if level == 0 {
                        rgb(crate::scrollbar::RESIZE_ACCENT).into()
                    } else {
                        muted
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(title),
            )
    }

    /// 「目录」视图：当前文档的标题大纲（Markdown 标题 / PDF 书签）。
    fn render_sidebar_toc(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();

        let inner: gpui::AnyElement = if let Some(tab) = self.tabs.get(self.active_tab) {
            // PDF 书签目录
            if let Some(pdf) = &tab.pdf_preview {
                let outlines = pdf.read(cx).outlines().to_vec();
                if outlines.is_empty() {
                    self.sidebar_empty_hint("当前 PDF 无书签目录")
                } else {
                    let weak = pdf.clone();
                    let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(outlines.len());
                    for (i, e) in outlines.iter().enumerate() {
                        let page = e.page_index;
                        let wk = weak.clone();
                        let mut el = self.toc_row_styled(
                            format!("sidebar-toc-pdf-{i}"),
                            e.level,
                            e.title.clone(),
                            fg,
                            muted,
                            active_bg,
                        );
                        if page != usize::MAX {
                            el = el.on_click(
                                cx.listener(move |_this: &mut EditorApp, _ev, _window, cx| {
                                    let _ = wk.update(cx, |v, cx| {
                                        v.scroll_to_page(page);
                                        cx.notify();
                                    });
                                }),
                            );
                        }
                        rows.push(el.into_any_element());
                    }
                    div().flex().flex_col().py_1().children(rows).into_any_element()
                }
            } else {
                // Markdown 标题
                let entries = tab.md_preview.read(cx).toc_entries().to_vec();
                if entries.is_empty() {
                    self.sidebar_empty_hint("当前文件无目录（无标题）")
                } else {
                    let weak_md = tab.md_preview.clone();
                    let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(entries.len());
                    for (i, &(row_ix, lvl, ref text)) in entries.iter().enumerate() {
                        let wk = weak_md.clone();
                        let mut el = self.toc_row_styled(
                            format!("sidebar-toc-md-{i}"),
                            lvl as usize,
                            text.clone(),
                            fg,
                            muted,
                            active_bg,
                        );
                        el = el.on_click(cx.listener(move |_this: &mut EditorApp, _ev, _window, cx| {
                            let _ = wk.update(cx, |v, cx| {
                                v.scroll_to_row(row_ix);
                                cx.notify();
                            });
                        }));
                        rows.push(el.into_any_element());
                    }
                    div().flex().flex_col().py_1().children(rows).into_any_element()
                }
            }
        } else {
            self.sidebar_empty_hint("当前无打开文档")
        };

        div()
            .id("sidebar-toc")
            .debug_selector(|| "sidebar-toc".into())
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .py_1()
            .child(inner)
            .into_any_element()
    }

    /// 「搜索」视图：关键字输入框 + 跨打开文件的匹配结果。
    fn render_sidebar_search(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = self.text_minor();
        let query = self.search_state.read(cx).value().to_string();
        let search = self.search_state.clone();
        let input = div()
            .id("sidebar-search-input")
            .debug_selector(|| "sidebar-search-input".into())
            .px_3()
            .py_1()
            .w_full()
            .min_h(px(34.))
            .border_b_1()
            .border_color(self.bar_border())
            .child(Input::new(&search));

        let q = query.trim().to_lowercase();
        let result_area: gpui::AnyElement = if q.is_empty() {
            div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(muted)
                .child("输入关键字，搜索所有已打开文件")
                .into_any_element()
        } else {
            self.render_sidebar_search_results(&q, cx)
        };

        div()
            .id("sidebar-search")
            .debug_selector(|| "sidebar-search".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(input)
            .child(result_area)
            .into_any_element()
    }

    /// 计算并渲染搜索结果（文件名命中 + 行内容命中，按打开顺序排列，封顶）。
    fn render_sidebar_search_results(
        &mut self,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();

        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();
        let mut line_rows: Vec<gpui::AnyElement> = Vec::new();
        let mut hits = 0usize;
        'outer: for (ti, tab) in self.tabs.iter().enumerate() {
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            if !name.is_empty() && name.to_lowercase().contains(q) {
                if hits < SEARCH_RESULT_CAP {
                    let row_fg = if ti == self.active_tab { fg } else { muted };
                    let nm = name.clone();
                    let r = div()
                        .id(SharedString::from(format!("sidebar-search-file-{ti}")))
                        .debug_selector(move || format!("sidebar-search-file-{ti}"))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_1()
                        .mx(px(4.))
                        .rounded_md()
                        .text_color(row_fg)
                        .cursor(CursorStyle::PointingHand)
                        .hover(|style| style.bg(active_bg))
                        .on_click(cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                            this.activate_tab(ti, window, cx);
                        }))
                        .child(Icon::new(IconName::File).size(px(13.)).text_color(row_fg))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(12.))
                                .text_color(row_fg)
                                .child(nm),
                        );
                    file_rows.push(r.into_any_element());
                    hits += 1;
                } else {
                    break 'outer;
                }
            }
            let content = tab.content.to_string();
            for (li, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(q) {
                    if hits >= SEARCH_RESULT_CAP {
                        break 'outer;
                    }
                    let snippet = line.trim().to_string();
                    let r = div()
                        .id(SharedString::from(format!("sidebar-search-line-{ti}-{li}")))
                        .debug_selector(move || format!("sidebar-search-line-{ti}-{li}"))
                        .px_2()
                        .py_1()
                        .mx(px(4.))
                        .rounded_md()
                        .text_size(px(12.))
                        .text_color(muted)
                        .cursor(CursorStyle::PointingHand)
                        .hover(|style| style.bg(active_bg))
                        .on_click(cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                            this.activate_tab(ti, window, cx);
                            if let Some(tab) = this.tabs.get(ti) {
                                if !tab.readonly_preview {
                                    let ed = tab.editor.clone();
                                    ed.update(cx, |ed, cx| {
                                        ed.set_cursor_position(Position::new(li as u32, 0), window, cx);
                                    });
                                }
                            }
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(format!("L{}  {}", li + 1, snippet)),
                        );
                    line_rows.push(r.into_any_element());
                    hits += 1;
                }
            }
        }

        let total = hits;
        let summary = if total == 0 {
            div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(muted)
                .child("未找到匹配内容")
                .into_any_element()
        } else {
            div()
                .px_3()
                .py_2()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!("{total} 处匹配"))
                .into_any_element()
        };

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        rows.extend(file_rows);
        rows.extend(line_rows);

        div()
            .id("sidebar-search-results")
            .debug_selector(|| "sidebar-search-results".into())
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(summary)
            .child(div().flex().flex_col().py_1().children(rows))
            .into_any_element()
    }
}
