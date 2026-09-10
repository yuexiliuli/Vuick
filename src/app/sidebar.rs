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
    /// 活动栏按钮点按：未在该面板 → 展开并切到该面板；
    /// 已在该面板再点一次 → **只收起面板**，侧边栏保留为窄活动栏（不留空白宽区）。
    pub(super) fn toggle_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        if self.sidebar_panel_open && self.sidebar_view == view {
            self.sidebar_panel_open = false;
        } else {
            self.sidebar_open = true;
            self.sidebar_panel_open = true;
            self.sidebar_view = view;
        }
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
            // 侧边栏右缘拖拽调宽：鼠标移动/松开事件冒泡到整个内容容器统一处理。
            // 若挂在 sidebar 自身，指针一旦移出侧边栏右缘（进入正文区）就不再派发，
            // 拖拽会“卡住不灵敏”；挂在整行容器上（与 pdf::toc、splitter 同款）即可全程跟踪。
            .on_mouse_move(cx.listener(
                |this: &mut EditorApp, ev: &gpui::MouseMoveEvent, _w, cx| {
                    this.update_sidebar_resize(ev.position.x.as_f32(), cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                    this.end_sidebar_resize(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                    this.end_sidebar_resize(cx);
                }),
            )
            .child(self.render_sidebar(cx))
            .child(body)
            .into_any_element()
    }

    /// 侧边栏整体：左侧为常驻竖列切换栏（活动栏），右侧为可展开的内容面板
    /// （目录/搜索）。面板收起时仅剩 46px 活动栏，正文区随之占满，不留空白宽区。
    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (side_bg, border) = (self.bar_bg(), self.bar_border());
        // 先构建内容（目录视图会自适应更新 sidebar_width），再取宽，避免本帧用旧宽
        let content: Option<gpui::AnyElement> = if self.sidebar_panel_open {
            Some(match self.sidebar_view {
                SidebarView::Toc => self.render_sidebar_toc(cx),
                SidebarView::Search => self.render_sidebar_search(cx),
            })
        } else {
            None
        };
        let width_px = if self.sidebar_panel_open {
            self.sidebar_width
        } else {
            crate::app::SIDEBAR_ACTIVITY_W
        };
        div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .w(px(width_px))
            .flex_shrink_0()
            .relative()
            .flex()
            .flex_row()
            .bg(side_bg)
            .border_r_1()
            .border_color(border)
            // 拖拽调宽事件由外层 app-body-with-sidebar 容器统一处理（见 render_body_sidebar）
            .child(self.render_sidebar_activity_bar(border, cx))
            .children(content)
            .children(self.render_sidebar_resize_handle(cx))
    }

    /// 开始拖拽侧边栏右缘：记录 (起始 x, 起始宽)，关闭自适应由用户接管。
    pub(super) fn start_sidebar_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        self.sidebar_resize = Some((start_x, self.sidebar_width));
        self.sidebar_width_auto = false;
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新侧边栏宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    pub(super) fn update_sidebar_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_w)) = self.sidebar_resize {
            self.sidebar_width = (start_w + (x - start_x))
                .clamp(crate::app::SIDEBAR_WIDTH_MIN, crate::app::SIDEBAR_WIDTH_MAX);
            cx.notify();
        }
    }

    /// 结束拖拽。
    pub(super) fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_resize.is_some() {
            self.sidebar_resize = None;
            cx.notify();
        }
    }

    /// 侧边栏右缘拖拽手柄：宽 6px 命中区，视觉 2px 强调细线（悬停/拖拽时显现）。
    /// 面板收起时返回空（无拖拽意义）。
    fn render_sidebar_resize_handle(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if !self.sidebar_panel_open {
            return Vec::new();
        }
        let accent = rgb(crate::scrollbar::RESIZE_ACCENT);
        vec![
            div()
                .id("sidebar-resize")
                .debug_selector(|| "sidebar-resize".into())
                .absolute()
                .right(px(0.))
                .top(px(0.))
                .bottom(px(0.))
                .w(px(6.))
                .cursor(CursorStyle::ResizeColumn)
                .on_hover(cx.listener(
                    |this: &mut EditorApp, hovered: &bool, _w, cx| {
                        this.sidebar_resize_hovered = *hovered;
                        cx.notify();
                    },
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut EditorApp, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.start_sidebar_resize(ev.position.x.as_f32(), cx);
                    }),
                )
                .child(
                    div()
                        // 贴到右缘，不用 flex 居中（否则蓝色线离边缘差 ~2px 看着有缝）
                        .absolute()
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .w(px(2.))
                        .rounded_full()
                        .opacity(
                            if self.sidebar_resize.is_some() || self.sidebar_resize_hovered {
                                1.0
                            } else {
                                0.0
                            },
                        )
                        .bg(accent),
                )
                .into_any_element(),
        ]
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
            let is_active = self.sidebar_panel_open && self.sidebar_view == view;
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
                    this.toggle_sidebar_view(view, cx);
                }))
                .child(Icon::new(icon).size(px(17.)).text_color(col))
                .child(div().text_size(px(9.5)).text_color(col).child(label));
            btns.push(btn.into_any_element());
        }
        div()
            .id("sidebar-activity-bar")
            .debug_selector(|| "sidebar-activity-bar".into())
            .w(px(crate::app::SIDEBAR_ACTIVITY_W))
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

    /// 折叠/展开某个目录父项（仅「有下级」的条目可折叠）。
    pub(super) fn toggle_sidebar_toc_collapse(&mut self, idx: usize, cx: &mut Context<Self>) {
        let has_child = match self.tabs.get(self.active_tab) {
            Some(t) if t.pdf_preview.is_some() => {
                let outlines = t.pdf_preview.as_ref().unwrap().read(cx).outlines().to_vec();
                crate::pdf::toc_node_has_child(&outlines, idx)
            }
            Some(t) => {
                let entries = t.md_preview.read(cx).toc_entries().to_vec();
                idx + 1 < entries.len() && entries[idx + 1].1 as usize > entries[idx].1 as usize
            }
            None => false,
        };
        if !has_child {
            return;
        }
        if !self.sidebar_toc_collapsed.remove(&idx) {
            self.sidebar_toc_collapsed.insert(idx);
        }
        cx.notify();
    }

    /// 目录条目可视件（样式对齐 `pdf::toc` 的条目：组 hover 左侧指示条 + 折叠箭头列 +
    /// 圆点 + 标题 + 右侧页码；整行可点跳转，箭头只折叠/展开）。`top_accent`：顶级条目用
    /// 强调色圆点；`page_label`：右侧页码文本（PDF 用页+1，Markdown 无页传 `None`）。
    #[allow(clippy::too_many_arguments)]
    fn toc_item_frame(
        &self,
        id: String,
        level: usize,
        title: String,
        has_child: bool,
        is_collapsed: bool,
        top_accent: bool,
        page_label: Option<String>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        hover_bg: gpui::Hsla,
        idx: usize,
        on_select: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id2 = id.clone();
        let mut row = div()
            .id(SharedString::from(id))
            .debug_selector(move || id2.clone())
            .pl(px(6. + level as f32 * 14.))
            .pr(px(8.))
            .py(px(5.))
            .w_full()
            .relative()
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor(CursorStyle::PointingHand)
            .group("toc-item")
            .rounded(px(4.))
            .hover(|style| style.bg(hover_bg).text_color(fg))
            .active(|style| style.bg(hover_bg).opacity(0.85));

        // 左侧当前项指示条（hover 时显现，统一强调蓝）
        let indicator = div()
            .absolute()
            .top(px(5.))
            .bottom(px(5.))
            .left(px(2.))
            .w(px(2.))
            .rounded_full()
            .bg(rgb(crate::scrollbar::RESIZE_ACCENT))
            .opacity(0.0)
            .group_hover("toc-item", |style| style.opacity(1.0));
        row = row.child(indicator);

        // 折叠箭头（有下级时）：点击只折叠/展开，不跳转
        if has_child {
            let chevron = if is_collapsed {
                Icon::new(IconName::ChevronRight)
            } else {
                Icon::new(IconName::ChevronDown)
            }
            .size(px(12.))
            .text_color(muted);
            let caret = div()
                .id(SharedString::from(format!("sidebar-toc-caret-{idx}")))
                .debug_selector(move || format!("sidebar-toc-caret-{idx}"))
                .w(px(16.))
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .text_color(muted)
                .group_hover("toc-item", |style| style.text_color(fg))
                .hover(|style| style.bg(hover_bg))
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                    cx.stop_propagation();
                    this.toggle_sidebar_toc_collapse(idx, cx);
                }))
                .child(chevron);
            row = row.child(caret);
        } else {
            // 无子项：占位宽度对齐箭头列
            row = row.child(div().w(px(16.)));
        }

        row = row
            .child(
                div()
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
                    .flex_shrink_0()
                    .bg(if top_accent {
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
                    .text_size(px(13.))
                    .text_color(fg)
                    .group_hover("toc-item", |style| style.text_color(fg))
                    .child(title),
            );

        // 右侧页码（PDF 书签）：承前对齐 pdf::toc 的灰色小号页码
        if let Some(page) = page_label {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .pl_2()
                    .text_size(px(10.5))
                    .text_color(muted)
                    .child(page),
            );
        }

        row.on_click(on_select).into_any_element()
    }

    /// 估算目录文字宽度（px）：CJK 取全宽、ASCII 取 0.6 倍字号。
    fn toc_text_width(text: &str, font_px: f32) -> f32 {
        text.chars()
            .map(|c| font_px * if c.is_ascii() { 0.6 } else { 1.0 })
            .sum()
    }

    /// 目录条目行所需宽度（px）：起始缩进 + 层级缩进 + 箭头列 + 圆点 + 标题 + 右侧留白。
    fn toc_row_width(level: usize, title: &str, font_px: f32) -> f32 {
        Self::toc_text_width(title, font_px)
            + 6.0              // pl 起始缩进
            + level as f32 * 14.0
            + 16.0             // 箭头列（无箭头也占位对齐）
            + 6.0 + 5.0 + 6.0  // gap + 圆点 + gap
            + 8.0              // pr
    }

    /// 计算当前激活文档目录所需的最大行宽（px）；无文档/无标题返回 `None`。
    fn compute_sidebar_toc_width(&self, cx: &gpui::App) -> Option<f32> {
        let tab = self.tabs.get(self.active_tab)?;
        let font_px = 13.0f32;
        // 右侧页码列宽（PDF 书签有页码，Markdown 无）：预留以免标题被截断
        let page_col_w = 34.0f32;
        let max_row = if let Some(pdf) = &tab.pdf_preview {
            let outlines = pdf.read(cx).outlines();
            outlines
                .iter()
                .map(|e| Self::toc_row_width(e.level, &e.title, font_px) + page_col_w)
                .fold(0.0f32, f32::max)
        } else {
            let entries = tab.md_preview.read(cx).toc_entries();
            entries
                .iter()
                .map(|&(_, l, ref t)| Self::toc_row_width(l as usize, t, font_px))
                .fold(0.0f32, f32::max)
        };
        (max_row > 0.0).then_some(max_row)
    }

    /// 目录视图顶栏：「目录」标题。开关交给左侧竖列「目录」按钮（再点一次收拢侧边栏）。
    fn render_sidebar_toc_header(&mut self) -> impl IntoElement {
        let (fg, _muted) = (self.text_primary(), self.text_minor());
        let border = self.bar_border();
        div()
            .id("sidebar-toc-header")
            .debug_selector(|| "sidebar-toc-header".into())
            .flex()
            .items_center()
            .px(px(10.))
            .h(px(36.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child("目录"),
            )
    }

    /// 「目录」视图：当前文档的标题大纲（Markdown 标题 / PDF 书签），
    /// 带面板头（「目录」+ 关闭）与页码，条目样式对齐 `pdf::toc`（可折叠树）。
    fn render_sidebar_toc(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();

        // 折叠状态按文档缓存：切换文档即清空，并重新按新文档内容自适应宽度
        let active = self.active_tab;
        if self.sidebar_toc_tab != Some(active) {
            self.sidebar_toc_tab = Some(active);
            self.sidebar_toc_collapsed.clear();
            self.sidebar_width_auto = true;
        }

        // 目录视图自适应宽：按当前文档最宽目录条目估算（未手动拖拽时）
        if self.sidebar_width_auto {
            if let Some(w) = self.compute_sidebar_toc_width(cx) {
                self.sidebar_width = (crate::app::SIDEBAR_ACTIVITY_W + w + 12.0)
                    .clamp(crate::app::SIDEBAR_WIDTH_MIN, crate::app::SIDEBAR_WIDTH_MAX);
            }
        }

        let inner: gpui::AnyElement = match self.tabs.get(self.active_tab) {
            Some(tab) => {
                let mut rows: Vec<gpui::AnyElement> = Vec::new();
                // PDF 书签目录
                if let Some(pdf) = &tab.pdf_preview {
                    let outlines = pdf.read(cx).outlines().to_vec();
                    if outlines.is_empty() {
                        self.sidebar_empty_hint("当前 PDF 无书签目录")
                    } else {
                        let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
                        let visible =
                            crate::pdf::toc_visibility(&levels, &self.sidebar_toc_collapsed);
                        let wk = pdf.clone();
                        for (i, e) in outlines.iter().enumerate() {
                            if !visible[i] {
                                continue;
                            }
                            let page = e.page_index;
                            let wk = wk.clone();
                            let has_child = crate::pdf::toc_node_has_child(&outlines, i);
                            let is_collapsed =
                                has_child && self.sidebar_toc_collapsed.contains(&i);
                            rows.push(self.toc_item_frame(
                                format!("sidebar-toc-pdf-{i}"),
                                e.level,
                                e.title.clone(),
                                has_child,
                                is_collapsed,
                                e.level == 0,
                                (page != usize::MAX).then(|| (page + 1).to_string()),
                                fg,
                                muted,
                                active_bg,
                                i,
                                move |_, _w, cx| {
                                    if page != usize::MAX {
                                        let _ = wk.update(cx, |v, cx| {
                                            v.scroll_to_page(page);
                                            cx.notify();
                                        });
                                    }
                                },
                                cx,
                            ));
                        }
                        div().flex().flex_col().py_1().children(rows).into_any_element()
                    }
                } else {
                    // Markdown 标题
                    let entries = tab.md_preview.read(cx).toc_entries().to_vec();
                    if entries.is_empty() {
                        self.sidebar_empty_hint("当前文件无目录（无标题）")
                    } else {
                        let levels: Vec<usize> = entries.iter().map(|&(_, l, _)| l as usize).collect();
                        let visible =
                            crate::pdf::toc_visibility(&levels, &self.sidebar_toc_collapsed);
                        let wk = tab.md_preview.clone();
                        for (i, &(row_ix, lvl, ref text)) in entries.iter().enumerate() {
                            if !visible[i] {
                                continue;
                            }
                            let wk = wk.clone();
                            let has_child = levels.get(i + 1).is_some_and(|&n| n > levels[i]);
                            let is_collapsed =
                                has_child && self.sidebar_toc_collapsed.contains(&i);
                            rows.push(self.toc_item_frame(
                                format!("sidebar-toc-md-{i}"),
                                lvl as usize,
                                text.clone(),
                                has_child,
                                is_collapsed,
                                lvl == 1,
                                None,
                                fg,
                                muted,
                                active_bg,
                                i,
                                move |_, _w, cx| {
                                    let _ = wk.update(cx, |v, cx| {
                                        v.scroll_to_row(row_ix);
                                        cx.notify();
                                    });
                                },
                                cx,
                            ));
                        }
                        div().flex().flex_col().py_1().children(rows).into_any_element()
                    }
                }
            }
            None => self.sidebar_empty_hint("当前无打开文档"),
        };

        div()
            .id("sidebar-toc")
            .debug_selector(|| "sidebar-toc".into())
            .flex_1()
            // 允许面板缩到比最宽条目还窄（否则 min-width:auto 拒绝缩窄，拖窄后
            // 面板溢出侧边栏右缘，条目 hover/点击仍响应编辑器区）
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.render_sidebar_toc_header())
            .child(
                div()
                    .id("sidebar-toc-scroll")
                    .debug_selector(|| "sidebar-toc-scroll".into())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .py_1()
                    .child(inner),
            )
            .into_any_element()
    }

    /// 「搜索」视图：关键字输入框 + 当前文档的匹配结果（文件名跨打开文件匹配）。
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
                .child("输入关键字，搜索当前文件内容；文件名匹配所有打开文件")
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
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
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

        // 文件名匹配（跨所有打开文件）：相当于「打开文件」快速选择器
        for (ti, tab) in self.tabs.iter().enumerate() {
            if hits >= SEARCH_RESULT_CAP {
                break;
            }
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            if !name.is_empty() && name.to_lowercase().contains(q) {
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
            }
        }

        // 行内容匹配：只在「当前活动文档」内搜索——若仍扫描所有打开的选项卡，
        // 在 PDF 预览（PDF 无可搜索文本，content 为空）下会冒出别的文件
        // （如 EPUB 完整正文）的内容，看起来像搜索到了错误文档。
        if hits < SEARCH_RESULT_CAP {
            if let Some(tab) = self.tabs.get(self.active_tab) {
                let content = tab.content.to_string();
                let ti = self.active_tab;
                for (li, line) in content.lines().enumerate() {
                    if line.to_lowercase().contains(q) {
                        if hits >= SEARCH_RESULT_CAP {
                            break;
                        }
                        // 命中文本在全文中的字节区间：跳转后选中它作为高亮提示
                        let match_range = find_match_range(&content, q, li);
                        let q_owned = q.to_string();
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
                                    if tab.readonly_preview {
                                        // EPUB 等只读预览：编辑区为空，跳转 = 滚动预览到命中行并高亮命中文本
                                        let md = tab.md_preview.clone();
                                        md.update(cx, |v, cx| {
                                            v.scroll_to_line(li);
                                            v.highlight_search(li, &q_owned, cx);
                                        });
                                    } else {
                                        // 预览可见（分屏/全屏）时同步高亮预览中的命中文本
                                        if tab.preview_on || tab.preview_fullscreen {
                                            let md = tab.md_preview.clone();
                                            md.update(cx, |v, cx| {
                                                v.highlight_search(li, &q_owned, cx);
                                            });
                                        }
                                        let ed = tab.editor.clone();
                                        ed.update(cx, |ed, cx| {
                                            // 先落光标（滚动到该行），再选中命中文本作为高亮
                                            ed.set_cursor_position(
                                                Position::new(li as u32, 0),
                                                window,
                                                cx,
                                            );
                                            if let Some(range) = match_range.clone() {
                                                ed.set_selected_range(range, cx);
                                            }
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
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(summary)
            .child(div().flex().flex_col().py_1().children(rows))
            .into_any_element()
    }
}

/// 在 `content` 的第 `line` 行（0 基）内查找第一个忽略大小写命中 `q` 的
/// 文档级字节区间（用于搜索跳转后选中命中文本作为高亮）；未命中返回 `None`。
fn find_match_range(content: &str, q: &str, line: usize) -> Option<std::ops::Range<usize>> {
    let needle = q.to_lowercase();
    let needle_chars = needle.chars().count();
    if needle_chars == 0 {
        return None;
    }
    let mut doc_pos = 0usize;
    for (li, l) in content.split('\n').enumerate() {
        if li == line {
            let mut i = 0usize;
            while i < l.len() {
                let Some(ch) = l[i..].chars().next() else {
                    break;
                };
                let candidate: String = l[i..].chars().take(needle_chars).collect();
                if !candidate.is_empty() && candidate.to_lowercase() == needle {
                    return Some((doc_pos + i)..(doc_pos + i + candidate.len()));
                }
                i += ch.len_utf8();
            }
            return None;
        }
        doc_pos += l.len() + 1;
    }
    None
}
