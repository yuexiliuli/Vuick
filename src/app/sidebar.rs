//! 侧边栏：可折叠/展开，顶部左侧为一竖列「目录 / 搜索」切换栏，右侧为当前视图内容。
//!
//! 折叠/展开开关放在窗口标题栏右上角（见 [`EditorApp::render_title_bar`]）；
//! 「目录」「搜索」两个按钮竖列排布，点击各自调用出对应界面。侧边栏只占据内容区
//! （选项卡栏之下、状态栏之上）。

use super::*;
use gpui_component::input::{Input, RopeExt};

use crate::config::limits::SEARCH_RESULT_CAP;

use std::rc::Rc;

/// 侧边栏视图。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarView {
    /// 当前文档目录（Markdown 标题 / PDF 书签）。
    Toc,
    /// 跨打开文件的搜索。
    Search,
}

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
            // 拖拽会“卡住不灵敏”；挂在整行容器上（与公共 toc 行、splitter 同款）即可全程跟踪。
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
            crate::config::layout::SIDEBAR_ACTIVITY_W
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
                .clamp(crate::config::layout::SIDEBAR_WIDTH_MIN, crate::config::layout::SIDEBAR_WIDTH_MAX);
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
        let accent = rgb(crate::config::colors::RESIZE_ACCENT);
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

    /// 侧边栏左侧竖列切换栏：目录 / 搜索 两个纯图标按钮上下排布。
    /// 活动项**仅图标高亮**（强调蓝、背景不变），并在最左侧边缘出现蓝色竖条
    /// （与目录条目左侧指示条同款），便于快速识别当前面板。
    fn render_sidebar_activity_bar(
        &mut self,
        border: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let accent: gpui::Hsla = rgb(crate::config::colors::RESIZE_ACCENT).into();
        let specs = [
            (SidebarView::Toc, IconName::BookOpen, "sidebar-btn-toc"),
            (SidebarView::Search, IconName::Search, "sidebar-btn-search"),
        ];
        let mut btns: Vec<gpui::AnyElement> = Vec::with_capacity(2);
        for (view, icon, id) in specs {
            let is_active = self.sidebar_panel_open && self.sidebar_view == view;
            // 活动项图标用主前景色（深色主题下近白），不变蓝
            let col = if is_active { fg } else { muted };
            let id_owned = id.to_string();
            let btn = div()
                .id(SharedString::from(id))
                .debug_selector(move || id_owned.clone())
                .relative()
                .w(px(40.))
                .h(px(38.))
                .flex()
                .items_center()
                .justify_center()
                .mx(px(3.))
                .my(px(1.))
                .rounded_md()
                // 背景不随活动状态变化，仅图标着色区分
                .text_color(col)
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(border))
                // 活动项：最左侧蓝色竖条（同目录条目左侧指示条）
                .child(
                    div()
                        .absolute()
                        .top(px(5.))
                        .bottom(px(5.))
                        .left(px(2.))
                        .w(px(2.))
                        .rounded_full()
                        .bg(accent)
                        .opacity(if is_active { 1.0 } else { 0.0 }),
                )
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                    this.toggle_sidebar_view(view, cx);
                }))
                .child(Icon::new(icon).size(px(22.)).text_color(col));
            btns.push(btn.into_any_element());
        }
        div()
            .id("sidebar-activity-bar")
            .debug_selector(|| "sidebar-activity-bar".into())
            .w(px(crate::config::layout::SIDEBAR_ACTIVITY_W))
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
                let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
                crate::toc::toc_node_has_child(&levels, idx)
            }
            Some(t) => {
                let entries = t.md_preview.read(cx).toc_entries().to_vec();
                let levels: Vec<usize> = entries.iter().map(|&(_, l, _)| l as usize).collect();
                crate::toc::toc_node_has_child(&levels, idx)
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
    /// 带面板头（「目录」+ 关闭）与页码，条目样式统一来自公共 [`crate::toc`]（可折叠树）。
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
        if self.sidebar_width_auto
            && let Some(w) = self.compute_sidebar_toc_width(cx) {
                self.sidebar_width = (crate::config::layout::SIDEBAR_ACTIVITY_W + w + 12.0)
                    .clamp(crate::config::layout::SIDEBAR_WIDTH_MIN, crate::config::layout::SIDEBAR_WIDTH_MAX);
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
                            crate::toc::toc_visibility(&levels, &self.sidebar_toc_collapsed);
                        let wk = pdf.clone();
                        for (i, e) in outlines.iter().enumerate() {
                            if !visible[i] {
                                continue;
                            }
                            let page = e.page_index;
                            let wk = wk.clone();
                            let has_child = crate::toc::toc_node_has_child(&levels, i);
                            let is_collapsed =
                                has_child && self.sidebar_toc_collapsed.contains(&i);
                            let idx = i;
                            rows.push(crate::toc::toc_item_frame(
                                format!("sidebar-toc-pdf-{i}"),
                                format!("sidebar-toc-caret-{i}"),
                                e.level,
                                e.title.clone(),
                                has_child,
                                is_collapsed,
                                e.level == 0,
                                (page != usize::MAX).then(|| (page + 1).to_string()),
                                fg,
                                muted,
                                active_bg,
                                move |_, _w, cx| {
                                    if page != usize::MAX {
                                        wk.update(cx, |v, cx| {
                                            v.scroll_to_page(page);
                                            cx.notify();
                                        });
                                    }
                                },
                                cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                                    cx.stop_propagation();
                                    this.toggle_sidebar_toc_collapse(idx, cx);
                                }),
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
                            crate::toc::toc_visibility(&levels, &self.sidebar_toc_collapsed);
                        let wk = tab.md_preview.clone();
                        for (i, &(row_ix, lvl, ref text)) in entries.iter().enumerate() {
                            if !visible[i] {
                                continue;
                            }
                            let wk = wk.clone();
                            let has_child = crate::toc::toc_node_has_child(&levels, i);
                            let is_collapsed =
                                has_child && self.sidebar_toc_collapsed.contains(&i);
                            let idx = i;
                            rows.push(crate::toc::toc_item_frame(
                                format!("sidebar-toc-md-{i}"),
                                format!("sidebar-toc-caret-{i}"),
                                lvl as usize,
                                text.clone(),
                                has_child,
                                is_collapsed,
                                lvl == 1,
                                None,
                                fg,
                                muted,
                                active_bg,
                                move |_, _w, cx| {
                                    wk.update(cx, |v, cx| {
                                        v.scroll_to_row(row_ix);
                                        cx.notify();
                                    });
                                },
                                cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                                    cx.stop_propagation();
                                    this.toggle_sidebar_toc_collapse(idx, cx);
                                }),
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
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_overflow(gpui::TextOverflow::Truncate("…".into()))
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
    /// 扫描结果缓存在 [`Self::search_scan`]：查询词/活动文档未变时，反复渲染帧
    /// 直接 O(1) 复用，不再每帧 O(全文) 重扫（旧逻辑在 render 里逐帧全量扫 + 
    /// 每行 `to_lowercase()` 分配）。
    fn render_sidebar_search_results(
        &mut self,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        // PDF 标签：全文检索由 PdfViewer 后台执行（文本在 pdfium 里，不在 tab.content）。
        // 查询按（标签, 词）键控、变化才推送（阅读器内部 250ms 防抖）；侧边栏只展示
        // 计数/页列表并驱动跳转。结果异步到达 → 经 `_pdf_observe` 重绘本面板。
        if let Some(pdf) = self.tabs.get(self.active_tab).and_then(|t| t.pdf_preview.clone()) {
            let push = self
                .pdf_search
                .as_ref()
                .map(|(ti, q0)| *ti != self.active_tab || q0 != q)
                .unwrap_or(true);
            if push {
                self.pdf_search = Some((self.active_tab, q.to_string()));
                let qo = q.to_string();
                pdf.update(cx, |v, cx| v.set_search(&qo, cx));
            }
            return self.render_sidebar_pdf_search(&pdf, q, cx);
        }
        let scan = self.sidebar_search_scan(q, cx);

        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();
        let mut line_rows: Vec<gpui::AnyElement> = Vec::new();
        let hits = scan.file_tabs.len() + scan.line_hits.len();

        // 文件名匹配（跨所有打开文件）：相当于「打开文件」快速选择器
        for ti in scan.file_tabs.iter().copied() {
            let name = self.tabs[ti]
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let row_fg = if ti == self.active_tab { fg } else { muted };
            let nm = name;
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
        }

        // 行内容命中：只在「当前活动文档」内搜索——若仍扫描所有打开的选项卡，
        // 在 PDF 预览（PDF 无可搜索文本，content 为空）下会冒出别的文件
        // （如 EPUB 完整正文）的内容，看起来像搜索到了错误文档。
        let ti = self.active_tab;
        for hit in &scan.line_hits {
            let li = hit.line;
            let match_range = hit.range.clone();
            let snippet = hit.snippet.clone();
            let q_owned = scan.query.clone();
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
                                let start = ed.offset_for_line_col(li, 0);
                                ed.set_caret_to(start);
                                ed.set_selection_range(match_range.clone(), cx);
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

    /// PDF 标签的搜索视图：顶部一行「n 处匹配」计数（右侧为 ↑/↓ 导航），
    /// 下方逐命中上下文行（`L N  前5字…目标…后5字`，单行显示超长省略，点击跳转到该命中页）。
    /// 检索本身由 [`crate::pdf::PdfViewer::set_search`] 在阅读器后台执行，这里只读状态并驱动跳转。
    fn render_sidebar_pdf_search(
        &mut self,
        pdf: &gpui::Entity<crate::pdf::PdfViewer>,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let (count, truncated, active) = pdf.read(cx).search_summary();
        let snippets: Vec<(usize, String)> = pdf.read(cx).search_snippets();

        // 计数 + ↑/↓ 导航同行排布于顶部一行（计数在左、导航靠右；计数过长尾部省略）
        let mk_nav = |symbol: &'static str,
                      delta: i64,
                      target: gpui::Entity<crate::pdf::PdfViewer>,
                      cx: &mut Context<Self>| {
            let id = if delta < 0 {
                "sidebar-search-pdf-prev"
            } else {
                "sidebar-search-pdf-next"
            };
            div()
                .id(SharedString::from(id))
                .debug_selector(move || SharedString::from(id).into())
                .flex()
                .items_center()
                .justify_center()
                .w(px(28.))
                .h(px(24.))
                .mx(px(2.))
                .rounded_md()
                .text_size(px(16.))
                .text_color(fg)
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(active_bg))
                .on_click(cx.listener(move |_this: &mut EditorApp, _ev, _w, cx| {
                    target.update(cx, |v, cx| v.move_match(delta, cx));
                    cx.notify();
                }))
                .child(symbol)
        };
        let prev_btn = mk_nav("↑", -1, pdf.clone(), cx);
        let next_btn = mk_nav("↓", 1, pdf.clone(), cx);

        let summary: gpui::AnyElement = if count == 0 {
            let hint = if q.trim().is_empty() {
                "在 PDF 全文检索；输入关键字"
            } else {
                "PDF 中没有匹配"
            };
            div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(muted)
                .child(hint)
                .into_any_element()
        } else {
            let text = if truncated {
                format!("{count} 处匹配（已截断）")
            } else {
                format!("{count} 处匹配")
            };
            div()
                .id("sidebar-search-pdf-head")
                .px_3()
                .py_2()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .text_size(px(12.))
                        .text_color(fg)
                        .child(text),
                )
                .child(
                    div()
                        .id("sidebar-search-pdf-nav")
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(prev_btn)
                        .child(next_btn),
                )
                .into_any_element()
        };

        // 命中行（每个命中一行）：`L N  前5字…目标…后5字`，单行显示、超出省略；当前激活命中高亮
        let mut hit_rows: Vec<gpui::AnyElement> = Vec::new();
        for (idx, (page, snippet)) in snippets.into_iter().enumerate() {
            let color = if idx == active { fg } else { muted };
            let row_pdf = pdf.clone();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-pdf-hit-{idx}")))
                .debug_selector(move || format!("sidebar-search-pdf-hit-{idx}"))
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .flex()
                .flex_row()
                .items_center()
                .text_size(px(12.))
                .text_color(color)
                .cursor(CursorStyle::PointingHand)
                .hover(|style| style.bg(active_bg))
                .on_click(cx.listener(move |_this: &mut EditorApp, _ev, _w, cx| {
                    row_pdf.update(cx, |v, cx| v.focus_match_on_page(page, cx));
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .child(format!("L{}  {}", page + 1, snippet)),
                );
            hit_rows.push(r.into_any_element());
        }

        div()
            .id("sidebar-search-results-pdf")
            .debug_selector(|| "sidebar-search-results-pdf".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(summary)
            .child(div().flex().flex_col().py_1().children(hit_rows))
            .into_any_element()
    }

    /// 侧边栏搜索「扫描并缓存」：查询词 + 活动文档未变时命中缓存（O(1) Rc 克隆，
    /// 渲染帧不重扫）；否则对文件名（跨全部打开文件）与活动文档行内容扫一次。
    /// 行内容直接借 `Rope`/`content`（`&str`），不做整份 `to_string()` 物化；
    /// 匹配用无分配 ASCII 大小写不敏感查找（非 ASCII 查询词回退字符级扫描）。
    /// 结果在 [`EditorApp::search_scan`] 缓存，编辑/加载/切文档时清空重扫。
    fn sidebar_search_scan(&mut self, q: &str, cx: &App) -> Rc<SidebarSearchScan> {
        if let Some(s) = &self.search_scan
            && s.tab == self.active_tab && s.query == q {
                return s.clone();
            }
        self.search_scan_count += 1;

        let mut file_tabs: Vec<usize> = Vec::new();
        let mut line_hits: Vec<SearchLineHit> = Vec::new();
        // 文件名匹配（跨所有打开文件）
        for (ti, tab) in self.tabs.iter().enumerate() {
            if file_tabs.len() + line_hits.len() >= SEARCH_RESULT_CAP {
                break;
            }
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if contains_ci(&name, q) {
                file_tabs.push(ti);
            }
        }
        // 行内容命中（仅当前活动文档；readonly 走 content，可编辑走编辑器 Rope）
        if file_tabs.len() < SEARCH_RESULT_CAP
            && let Some(tab) = self.tabs.get(self.active_tab) {
                if tab.readonly_preview {
                    let content: &str = tab.content.as_ref();
                    let mut li = 0usize;
                    let mut doc_offset = 0usize;
                    for line in content.split('\n') {
                        if let Some(r) = find_ci(line, q) {
                            line_hits.push(SearchLineHit {
                                line: li,
                                range: (doc_offset + r.start)..(doc_offset + r.end),
                                snippet: line.trim().to_string(),
                            });
                            if file_tabs.len() + line_hits.len() >= SEARCH_RESULT_CAP {
                                break;
                            }
                        }
                        li += 1;
                        doc_offset += line.len() + 1;
                    }
                } else {
                    let rope = tab.editor.read(cx).text();
                    for (li, lineslice) in rope.iter_lines().enumerate() {
                        // 常规行在单 chunk 内：`as_str()` 零拷贝借出，逐行扫描不做分配；
                        // 罕见跨 chunk 的超长行才整行物化一次（`iter_lines` 的 RopeSlice
                        // 无 Deref<str>，无法直接当 &str 用）。
                        let found = if let Some(s) = lineslice.as_str() {
                            find_ci(s, q).map(|r| (r, s.trim().to_string()))
                        } else {
                            let owned = lineslice.to_string();
                            find_ci(&owned, q).map(|r| (r, owned.trim().to_string()))
                        };
                        let Some((r, snippet)) = found else { continue };
                        let line_start = rope.line_start_offset(li);
                        line_hits.push(SearchLineHit {
                            line: li,
                            range: (line_start + r.start)..(line_start + r.end),
                            snippet,
                        });
                        if file_tabs.len() + line_hits.len() >= SEARCH_RESULT_CAP {
                            break;
                        }
                    }
                }
            }

        let scan = std::rc::Rc::new(SidebarSearchScan {
            query: q.to_string(),
            tab: self.active_tab,
            file_tabs,
            line_hits,
        });
        self.search_scan = Some(scan.clone());
        scan
    }
}

/// 侧边栏搜索一次扫描产出的**命中索引**（渲染复用缓存，不重扫全文）。
#[derive(Debug, Default)]
pub(super) struct SidebarSearchScan {
    /// 归一化后的查询词（trim + to_lowercase，环节点上色与缓存键）。
    pub(super) query: String,
    /// 扫描时的活动选项卡下标（缓存键的一部分：切文档自动失效重扫）。
    pub(super) tab: usize,
    /// 文件名命中的选项卡下标（按打开顺序）。
    pub(super) file_tabs: Vec<usize>,
    /// 当前活动文档中内容命中的行。
    pub(super) line_hits: Vec<SearchLineHit>,
}

/// 一行内容命中：行号 + 首个命中文本的文档字节区间 + 展示条目片段。
#[derive(Debug, Clone)]
pub(super) struct SearchLineHit {
    /// 文档行号（0 基）。
    pub(super) line: usize,
    /// 首个命中文本的文档字节区间（跳转后选中高亮）。
    pub(super) range: std::ops::Range<usize>,
    /// 去掉首尾空白后的行内容（渲染条目显示）。
    pub(super) snippet: String,
}

/// ASCII 大小写不敏感子串查找（无分配），返回首个命中的字节区间。
/// 仅当 `needle` 全 ASCII 时走 `memchr` 加速的字节快路径（用 `memchr2` 同时搜
/// 首字节大小写两种形式，命中再逐字节校验余下部分）：UTF-8 多字节序列的内部字节
/// 都 ≥ 0x80，因此纯 ASCII 命中必然落在字符边界上，返回的区间始终合法。
/// 非 ASCII 查询词回退到与原逻辑等价的字符级大小写折叠扫描（保留原语义）。
pub(super) fn find_ci(haystack: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if needle.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle.as_bytes();
        if n.len() > h.len() {
            return None;
        }
        let first = n[0];
        // 同时搜首字节的 ASCII 大小写两种形式（字母大小写差 0x20）
        let first_alt = if first.is_ascii_alphabetic() {
            first ^ 0x20
        } else {
            first
        };
        let mut pos = 0usize;
        while let Some(rel) = memchr::memchr2(first, first_alt, &h[pos..]) {
            let at = pos + rel;
            if at + n.len() <= h.len() {
                let mut ok = true;
                for (j, &nb) in n.iter().enumerate().skip(1) {
                    if h[at + j].to_ascii_lowercase() != nb {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    return Some(at..(at + n.len()));
                }
            }
            pos = at + 1;
        }
        return None;
    }
    // 非 ASCII needle：字符级大小写折叠比较
    let needle_chars = needle.chars().count();
    let mut i = 0usize;
    while i < haystack.len() {
        let candidate: String = haystack[i..].chars().take(needle_chars).collect();
        if !candidate.is_empty() && candidate.to_lowercase() == needle {
            return Some(i..(i + candidate.len()));
        }
        let ch = haystack[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

/// ASCII 大小写不敏感子串是否存在（无分配；语义与旧 `to_lowercase().contains()` 一致）。
pub(super) fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}
