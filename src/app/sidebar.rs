//! 侧边栏：可折叠/展开，顶部左侧为一竖列「目录 / 搜索」切换栏，右侧为当前视图内容。
//!
//! 折叠/展开开关放在窗口标题栏右上角（见 [`EditorApp::render_title_bar`]）；
//! 「目录」「搜索」两个按钮竖列排布，点击各自调用出对应界面。侧边栏只占据内容区
//! （选项卡栏之下、状态栏之上）。

use super::*;
use crate::ui::input::Input;
use crate::editor::rope_ext::RopeExt;

use crate::config::limits::SEARCH_RESULT_CAP;
use crate::ui::motion::{hover_bg_fade, hover_spring};

use std::rc::Rc;

/// 侧边栏视图。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarView {
    /// 当前文档目录（Markdown 标题 / PDF 书签）。
    Toc,
    /// 跨打开文件的搜索。
    Search,
}

/// 侧边栏「外壳」状态：可见性、当前视图与目录（TOC）面板的宽度/折叠/右缘拖拽。
/// 与「检索输入 + 缓存 + 订阅」分属（后者仍在 `EditorApp`，后续洋葱继续剥）。
/// 全部 `pub(crate)`：跨 `app` 子模块读写 + `app::tests` 直读。
pub(crate) struct SidebarChromeState {
    /// 侧边栏是否展开。
    pub(crate) open: bool,
    /// 侧边栏当前视图（目录 / 搜索）。
    pub(crate) view: SidebarView,
    /// 侧边栏内容面板是否展开（false = 只留窄活动栏，目录/搜索面板收起）。
    pub(crate) panel_open: bool,
    /// 侧边栏目录折叠集合（仅当前激活文档，切换文档时清空）。
    pub(crate) toc_collapsed: std::collections::HashSet<usize>,
    /// 侧边栏目录折叠集合对应的选项卡下标（切换文档即失效清空）。
    pub(crate) toc_tab: Option<usize>,
    /// 侧边栏当前宽度（px）：目录视图随内容自适应，也可右缘拖拽手动调整。
    pub(crate) width: f32,
    /// 是否仍随目录内容自适应宽度（手动拖拽后关闭，切换文档重开）。
    pub(crate) width_auto: bool,
    /// 侧边栏右缘拖拽：`Some((起始 x, 起始宽))`。
    pub(crate) resize: Option<(f32, f32)>,
    /// 侧边栏右缘是否被鼠标悬停（悬停时高亮提示可拖拽）。
    pub(crate) resize_hovered: bool,
}

impl SidebarChromeState {
    /// 切换视图：未在该面板 → 展开并切到；已在该面板再点 → 只收起面板。
    /// （折叠/展开逻辑此前栖身在 `toggle_sidebar_view`，随状态一起收编。）
    pub(super) fn toggle_view(&mut self, view: SidebarView) {
        if self.panel_open && self.view == view {
            self.panel_open = false;
        } else {
            self.open = true;
            self.panel_open = true;
            self.view = view;
        }
    }
}

impl EditorApp {
    /// 活动栏按钮点按：未在该面板 → 展开并切到该面板；
    /// 已在该面板再点一次 → **只收起面板**，侧边栏保留为窄活动栏（不留空白宽区）。
    pub(super) fn toggle_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        self.sidebar.toggle_view(view);
        cx.notify();
    }

    /// 内容区：侧边栏展开时在左侧停靠（占固定宽），右侧为原有正文 `body`；
    /// 收起时仅返回正文本身。
    ///
    /// 外壳卡片化：正文 `body` 一律套「正文卡」（右缘贴窗直角、左/上/下圆角 + 间距）；
    /// 侧边栏展开时在其与正文之间插一条 `CARD_GAP` 宽的**分隔带**（面板展开时带
    /// 三点手柄 + 右缘调宽命中区）。
    pub(super) fn render_body_sidebar(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !self.sidebar.open {
            return self.render_body_card(body, window, cx);
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
            .child(self.render_sidebar(window, cx))
            .children(self.render_sidebar_resize_handle(cx))
            .child(self.render_body_card(body, window, cx))
            .into_any_element()
    }

    /// 正文区卡片：右缘贴窗直角、左/上/下圆角 + 与侧栏/状态栏的 `CARD_GAP` 间距，
    /// 浮在自绘材质层之上。卡面用正文底色（`panel_bg`，与编辑器/预览无缝）；卡
    /// 内边距会略缩「面板可用宽」（换行/滚动条按新可用宽走，属预期）。
    fn render_body_card(
        &mut self,
        body: gpui::AnyElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::ui::shell::{CARD_GAP, CARD_RADIUS};
        div()
            .id("app-main-card")
            .debug_selector(|| "app-main-card".into())
            // 关键：包装层必须是 flex 容器（列向），否则内部 render_body 的 .flex_1()
            // 在非 flex 的块盒里失效 → 编辑区高度坍缩为内容高，滚动条/点击命中全部错位。
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            // 侧栏展开时 `app-body-with-sidebar` 已为其左侧留出分隔带（ml 0）；
            // 收拢时正文卡左缘直接贴窗左侧的间距（ml = CARD_GAP）。
            .ml(px(if self.sidebar.open { 0.0 } else { CARD_GAP }))
            .mr(px(0.))
            .mt(px(CARD_GAP))
            // 下缘间距由状态栏卡顶边（mt = CARD_GAP）单侧承担，避免 6+6=12px 双倍缝。
            .mb(px(0.))
            .rounded_tl(px(CARD_RADIUS))
            .rounded_bl(px(CARD_RADIUS))
            .rounded_tr(px(0.))
            .rounded_br(px(0.))
            .border_1()
            .border_color(crate::ui::shell::card_border(cx))
            .bg(crate::ui::shell::body_card_face(cx))
            .overflow_hidden()
            .child(body)
            .into_any_element()
    }

    /// 侧边栏整体：左侧为常驻竖列切换栏（活动栏），右侧为可展开的内容面板
    /// （目录/搜索）。面板收起时仅剩 46px 活动栏，正文区随之占满，不留空白宽区。
    ///
    /// 外壳卡片化：整块 = 一张「侧栏卡」（左缘贴窗直角、上/下/右缘圆角 + 与正文的
    /// 间距），卡面用浮层表面 token（theme `surface`）而非半透明 mica。
    pub(super) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use crate::ui::shell::{CARD_GAP, CARD_RADIUS};
        let border = self.bar_border();
        // 先构建内容（目录视图会自适应更新 sidebar_width），再取宽，避免本帧用旧宽
        let content: Option<gpui::AnyElement> = if self.sidebar.panel_open {
            Some(match self.sidebar.view {
                SidebarView::Toc => self.render_sidebar_toc(window, cx),
                SidebarView::Search => self.render_sidebar_search(window, cx),
            })
        } else {
            None
        };
        let width_px = if self.sidebar.panel_open {
            self.sidebar.width
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
            // 贴窗左侧：左缘直角；上下留卡间间距（右缘间距由分隔带承担，见
            // render_body_sidebar 的 render_sidebar_resize_handle）。
            .ml(px(0.))
            .mt(px(CARD_GAP))
            // 下缘间距由状态栏卡顶边（mt = CARD_GAP）单侧承担，避免 6+6=12px 双倍缝。
            .mb(px(0.))
            .rounded_tl(px(0.))
            .rounded_bl(px(0.))
            .rounded_tr(px(CARD_RADIUS))
            .rounded_br(px(CARD_RADIUS))
            .border_1()
            .border_color(crate::ui::shell::card_border(cx))
            .bg(crate::ui::shell::card_surface(cx))
            .overflow_hidden()
            // 拖拽调宽事件由外层 app-body-with-sidebar 容器统一处理（见 render_body_sidebar）
            .child(self.render_sidebar_activity_bar(window, border, cx))
            .children(content)
    }

    /// 开始拖拽侧边栏右缘：记录 (起始 x, 起始宽)，关闭自适应由用户接管。
    pub(super) fn start_sidebar_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        self.sidebar.resize = Some((start_x, self.sidebar.width));
        self.sidebar.width_auto = false;
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新侧边栏宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    pub(super) fn update_sidebar_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_w)) = self.sidebar.resize {
            self.sidebar.width = (start_w + (x - start_x))
                .clamp(crate::config::layout::SIDEBAR_WIDTH_MIN, crate::config::layout::SIDEBAR_WIDTH_MAX);
            cx.notify();
        }
    }

    /// 结束拖拽：宽度定稿后持久化（`width_auto` 已被关掉，存为手动宽度）。
    pub(super) fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar.resize.is_some() {
            self.sidebar.resize = None;
            self.persist_prefs();
            cx.notify();
        }
    }

    /// 侧边栏右缘「分隔带」：`CARD_GAP` 宽的弹层留白（卡片化后它是「侧栏卡 ↔ 正文卡」
    /// 之间的间距），本身即整条可拖拽命中区 + 居中三点手柄。
    ///
    /// - 面板展开（`panel_open`）：分隔带 = 调宽命中区（投影 `sidebar-resize`），中央挂
    ///   [`crate::ui::shell::three_dot_grip`] 三点手柄（hover/拖拽 → accent 淡入）；
    /// - 面板收起（仅活动栏）：只保留空间隔 `sidebar-gap`，无拖拽意义（与旧行为一致：
    ///   右缘调宽只对目录/搜索面板展开时开放）。
    ///
    /// 命中/拖拽逻辑与现状完全一致：`start_sidebar_resize` 记 (起始窗口 x, 起始宽)、
    /// 外层 `app-body-with-sidebar` 的 `on_mouse_move/up(_out)` 全程跟踪（窗口坐标 δ，
    /// 与手柄位置无关）。
    fn render_sidebar_resize_handle(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        use crate::ui::shell::CARD_GAP;
        if !self.sidebar.panel_open {
            return vec![
                div()
                    .id("sidebar-gap")
                    .debug_selector(|| "sidebar-gap".into())
                    .w(px(CARD_GAP))
                    .flex_shrink_0()
                    .into_any_element(),
            ];
        }
        let grip = crate::ui::shell::three_dot_grip(
            "sidebar-resize-grip",
            self.sidebar.resize.is_some() || self.sidebar.resize_hovered,
            cx,
        );
        vec![
            div()
                .id("sidebar-resize")
                .debug_selector(|| "sidebar-resize".into())
                .w(px(CARD_GAP))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor(CursorStyle::ResizeColumn)
                .on_hover(cx.listener(
                    |this: &mut EditorApp, hovered: &bool, _w, cx| {
                        this.sidebar.resize_hovered = *hovered;
                        cx.notify();
                    },
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut EditorApp, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.start_sidebar_resize(ev.position.x.as_f32(), cx);
                    }),
                )
                .child(grip.into_any_element())
                .into_any_element(),
        ]
    }

    /// 侧边栏左侧竖列切换栏：目录 / 搜索 两个纯图标按钮上下排布。
    /// 活动项**仅图标高亮**（强调蓝、背景不变），并在最左侧边缘出现蓝色竖条
    /// （与目录条目左侧指示条同款），便于快速识别当前面板。
    fn render_sidebar_activity_bar(
        &mut self,
        window: &mut Window,
        border: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let accent: gpui::Hsla = crate::ui::theme::Theme::global(cx).colors.accent.into();
        let c = self.chrome();
        let hover_fill: gpui::Hsla = c.hover_fill.into();
        let pressed_fill: gpui::Hsla = c.pressed_fill.into();
        let specs = [
            (SidebarView::Toc, IconName::BookOpen, "sidebar-btn-toc", "目录"),
            (
                SidebarView::Search,
                IconName::Search,
                "sidebar-btn-search",
                "搜索",
            ),
        ];
        let mut btns: Vec<gpui::AnyElement> = Vec::with_capacity(2);
        for (view, icon, id, label) in specs {
            let is_active = self.sidebar.panel_open && self.sidebar.view == view;
            // 活动项图标用主前景色（深色主题下近白），不变蓝
            let col = if is_active { fg } else { muted };
            let id_owned = id.to_string();
            // P2-10：hover 状态跨帧记录 → 背景淡入。
            let btn_hover = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
            let btn_hovered = *btn_hover.read(cx);
            // P2-9：tooltip 状态（同按钮 id 键控：延迟出现 300ms / 移出消失）。
            let tip = crate::ui::tooltip::tooltip_state(id, window, cx);
            let tip_on = crate::ui::tooltip::tooltip_visible(&tip, cx);
            let view_id = window.current_view();
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
                .rounded_sm()
                // 背景不随活动状态变化，仅图标着色区分
                .text_color(col)
                .cursor(CursorStyle::PointingHand)
                .active(|style| style.bg(pressed_fill))
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
                .on_hover({
                    let st = btn_hover.clone();
                    let tip = tip.clone();
                    move |hovered, _w, cx| {
                        st.update(cx, |v, _| *v = *hovered);
                        crate::ui::tooltip::tooltip_hover(&tip, *hovered, cx);
                        cx.notify(view_id);
                    }
                })
                .child(Icon::new(icon).size(px(22.)).text_color(col))
                // P2-9：tooltip 浮层（贴活动栏按钮右缘垂直居中；按钮 40×38）。
                .child(crate::ui::tooltip::tooltip_layer(
                    id, label, tip_on, 40.0, 38.0, cx,
                ));
            btns.push(
                hover_bg_fade(btn, format!("{id}-bg"), hover_fill, btn_hovered).into_any_element(),
            );
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

    /// 折叠/展开某个目录父项（仅「有下级」的条目可折叠）。
    pub(super) fn toggle_sidebar_toc_collapse(&mut self, idx: usize, cx: &mut Context<Self>) {
        let has_child = match self.tabs.get(self.active_tab) {
            Some(_t) if self.tabs[self.active_tab].pdf_preview.is_some() => {
                let outlines = self.tabs[self.active_tab].pdf_preview.as_ref().unwrap().read(cx).outlines().to_vec();
                let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
                crate::toc::toc_node_has_child(&levels, idx)
            }
            Some(_t) => {
                // A4：MD 统一预览走 WyEditor、EPUB 走 MarkdownPreview（同一数据源口径）。
                let levels: Vec<usize> = self
                    .md_toc_source(cx)
                    .map(|(entries, _)| entries.iter().map(|&(_, l, _)| l as usize).collect())
                    .unwrap_or_default();
                crate::toc::toc_node_has_child(&levels, idx)
            }
            None => false,
        };
        if !has_child {
            return;
        }
        if !self.sidebar.toc_collapsed.remove(&idx) {
            self.sidebar.toc_collapsed.insert(idx);
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
            // A4：MD 统一预览走 WyEditor、EPUB 走 MarkdownPreview（同一数据源口径）。
            let entries = self
                .md_toc_source(cx)
                .map(|(e, _)| e)
                .unwrap_or_default();
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
    fn render_sidebar_toc(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let accent = crate::ui::theme::Theme::global(cx).colors.accent.into();

        // 折叠状态按文档缓存：切换文档即清空，并重新按新文档内容自适应宽度
        let active = self.active_tab;
        if self.sidebar.toc_tab != Some(active) {
            self.sidebar.toc_tab = Some(active);
            self.sidebar.toc_collapsed.clear();
            self.sidebar.width_auto = true;
        }

        // 目录视图自适应宽：按当前文档最宽目录条目估算（未手动拖拽时）
        if self.sidebar.width_auto
            && let Some(w) = self.compute_sidebar_toc_width(cx) {
                self.sidebar.width = (crate::config::layout::SIDEBAR_ACTIVITY_W + w + 12.0)
                    .clamp(crate::config::layout::SIDEBAR_WIDTH_MIN, crate::config::layout::SIDEBAR_WIDTH_MAX);
            }

        let inner: gpui::AnyElement = match self.tabs.get(self.active_tab) {
            Some(tab) => {
                let collapsed = self.sidebar.toc_collapsed.clone();
                // PDF 书签目录
                if let Some(pdf) = &tab.pdf_preview {
                    let outlines = pdf.read(cx).outlines().to_vec();
                    if outlines.is_empty() {
                        crate::ui::empty_state::compact_empty_state(
                            "sidebar-toc-empty",
                            IconName::File,
                            "当前 PDF 无书签目录",
                            cx,
                        )
                    } else {
                        let rows = self.sidebar_toc_pdf_rows(
                            window, &outlines, &collapsed, fg, muted, active_bg, accent,
                            pdf.clone(), cx,
                        );
                        div().flex().flex_col().py_1().children(rows).into_any_element()
                    }
                } else {
                    // Markdown 标题（A4：MD 统一预览走 WyEditor、EPUB 走 MarkdownPreview）。
                    match self.md_toc_source(cx) {
                        Some((entries, scroll)) if !entries.is_empty() => {
                            let rows = self.sidebar_toc_md_rows(
                                window, &entries, &collapsed, fg, muted, active_bg, accent,
                                scroll, cx,
                            );
                            div().flex().flex_col().py_1().children(rows).into_any_element()
                        }
                        _ => crate::ui::empty_state::compact_empty_state(
                            "sidebar-toc-empty",
                            IconName::BookOpen,
                            "当前文件无目录（无标题）",
                            cx,
                        ),
                    }
                }
            }
            None => crate::ui::empty_state::compact_empty_state(
                "sidebar-toc-empty",
                IconName::File,
                "当前无打开文档",
                cx,
            ),
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
            // P2-10：面板开合轻淡入（宽度动画会让「自适应宽 + 布局断言」测试抖动，
            // 改为只动绘制、不碰布局的透明度淡入）。
            .with_spring(
                "sidebar-toc-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// PDF 书签目录条目（可折叠树；可见性按当前折叠集合过滤，点击跳转页码）。
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_pdf_rows(
        &self,
        window: &mut Window,
        outlines: &[crate::pdf::PdfOutlineEntry],
        collapsed: &std::collections::HashSet<usize>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        accent: gpui::Hsla,
        pdf: gpui::Entity<crate::pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
        let visible = crate::toc::toc_visibility(&levels, collapsed);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, e) in outlines.iter().enumerate() {
            if !visible[i] {
                continue;
            }
            let page = e.page_index;
            let wk = pdf.clone();
            let has_child = crate::toc::toc_node_has_child(&levels, i);
            let is_collapsed = has_child && collapsed.contains(&i);
            let idx = i;
            // 折叠回调先物化（与下方 `window, cx` 参数不可同表达式借用 cx）。
            let toggle_collapse = cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_toc_collapse(idx, cx);
            });
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
                accent,
                window,
                cx,
                move |_, _w, cx| {
                    if page != usize::MAX {
                        wk.update(cx, |v, cx| {
                            v.scroll_to_page(page);
                            cx.notify();
                        });
                    }
                },
                toggle_collapse,
            ));
        }
        rows
    }

    /// Markdown 标题目录条目（可折叠树；可见性按当前折叠集合过滤）。
    /// A4：`scroll` 区分跳转目标——`Legacy`（MarkdownPreview，渲染行列）用 `scroll_to_row`，
    /// `Unified`（右栏 WyEditor 只读预览，源码行）用 `scroll_to_line` 滚动统一组件宿主。
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_md_rows(
        &self,
        window: &mut Window,
        entries: &[(usize, u8, SharedString)],
        collapsed: &std::collections::HashSet<usize>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        accent: gpui::Hsla,
        scroll: MdTocScroll,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let levels: Vec<usize> = entries.iter().map(|&(_, l, _)| l as usize).collect();
        let visible = crate::toc::toc_visibility(&levels, collapsed);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, &(tgt, lvl, ref text)) in entries.iter().enumerate() {
            if !visible[i] {
                continue;
            }
            let sc = scroll.clone();
            let has_child = crate::toc::toc_node_has_child(&levels, i);
            let is_collapsed = has_child && collapsed.contains(&i);
            let idx = i;
            // 折叠回调先物化（与下方 `window, cx` 参数不可同表达式借用 cx）。
            let toggle_collapse = cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_toc_collapse(idx, cx);
            });
            rows.push(crate::toc::toc_item_frame(
                format!("sidebar-toc-md-{i}"),
                format!("sidebar-toc-caret-{i}"),
                lvl as usize,
                text.to_string(),
                has_child,
                is_collapsed,
                lvl == 1,
                None,
                fg,
                muted,
                active_bg,
                accent,
                window,
                cx,
                move |_, window, cx| match &sc {
                    MdTocScroll::Legacy(md) => {
                        md.update(cx, |v, cx| {
                            v.scroll_to_row(tgt);
                            cx.notify();
                        });
                    }
                    MdTocScroll::Unified(wy) => {
                        wy.update(cx, |v, cx| {
                            v.scroll_to_line(tgt, window);
                            cx.notify();
                        });
                    }
                },
                toggle_collapse,
            ));
        }
        rows
    }

    /// 「搜索」视图：关键字输入框 + 当前文档的匹配结果（文件名跨打开文件匹配）。
    fn render_sidebar_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let muted = self.text_minor();
        let query = self.search_state.read(cx).value().to_string();
        let search = self.search_state.clone();
        // Fluent 文本框：整圈细边框 + 内缩边距（背景随 chrome 保持透明）；
        // 聚焦时边框点亮为强调色。
        let focused = window.focused(cx) == Some(self.search_state.read(cx).focus_handle(cx));
        let border_color: gpui::Hsla = if focused {
            crate::ui::theme::Theme::global(cx).colors.focus_ring.into()
        } else {
            self.bar_border()
        };
        let input = div()
            .id("sidebar-search-input")
            .debug_selector(|| "sidebar-search-input".into())
            .px_2()
            .py_1()
            .mx_2()
            .my_1()
            .w_full()
            .min_h(px(34.))
            .rounded_sm()
            .border_1()
            .border_color(border_color)
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
            self.render_sidebar_search_results(window, &q, cx)
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
            // P2-10：面板开合轻淡入（同目录面板，见 `sidebar-toc-fade`）。
            .with_spring(
                "sidebar-search-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// 计算并渲染搜索结果（文件名命中 + 行内容命中，按打开顺序排列，封顶）。
    /// 扫描结果缓存在 [`Self::search_scan`]：查询词/活动文档未变时，反复渲染帧
    /// 直接 O(1) 复用，不再每帧 O(全文) 重扫（旧逻辑在 render 里逐帧全量扫 + 
    /// 每行 `to_lowercase()` 分配）。
    fn render_sidebar_search_results(
        &mut self,
        window: &mut Window,
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
            return self.render_sidebar_pdf_search(window, &pdf, q, cx);
        }
        let scan = self.sidebar_search_scan(q, cx);
        let hits = scan.file_tabs.len() + scan.line_hits.len();
        // 无命中 → 居中「灰色搜索图标 + 文案」空态占位（与目录/正文区空态同款视觉）。
        if hits == 0 {
            return div()
                .id("sidebar-search-results")
                .debug_selector(|| "sidebar-search-results".into())
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .overflow_y_scroll()
                .child(crate::ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Search,
                    "未找到匹配内容",
                    cx,
                ))
                .into_any_element();
        }
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        rows.extend(self.sidebar_search_file_rows(
            window,
            &scan,
            fg,
            muted,
            active_bg,
            cx,
        ));
        rows.extend(self.sidebar_search_line_rows(window, &scan, muted, active_bg, cx));

        div()
            .id("sidebar-search-results")
            .debug_selector(|| "sidebar-search-results".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(self.sidebar_search_summary(hits, muted))
            .child(div().flex().flex_col().py_1().children(rows))
            .into_any_element()
    }

    /// 文件名命中（跨所有打开文件）：相当于「打开文件」快速选择器，点击激活选项卡。
    fn sidebar_search_file_rows(
        &self,
        window: &mut Window,
        scan: &SidebarSearchScan,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();
        for ti in scan.file_tabs.iter().copied() {
            let name = self.tabs[ti]
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let row_fg = if ti == self.active_tab { fg } else { muted };
            let nm = name;
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover = window.use_keyed_state(format!("ss-file-{ti}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
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
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
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
            file_rows.push(
                hover_bg_fade(r, format!("ss-file-{ti}-bg"), active_bg, hovered)
                    .into_any_element(),
            );
        }
        file_rows
    }

    /// 行内容命中（只在「当前活动文档」内搜索）——若仍扫描所有打开的选项卡，
    /// 在 PDF 预览（无可搜索文本，content 为空）下会冒出别的文件（如 EPUB 完整正文）
    /// 的内容，看起来像搜索到了错误文档。点击：激活文档 + 光标/预览滚到命中行并高亮。
    fn sidebar_search_line_rows(
        &self,
        window: &mut Window,
        scan: &SidebarSearchScan,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let ti = self.active_tab;
        let mut line_rows: Vec<gpui::AnyElement> = Vec::new();
        for hit in &scan.line_hits {
            let li = hit.line;
            let match_range = hit.range.clone();
            let snippet = hit.snippet.clone();
            let q_owned = scan.query.clone();
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover =
                window.use_keyed_state(format!("ss-line-{ti}-{li}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
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
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
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
                            // 预览可见（分屏/全屏）且仍是 MarkdownPreview（EPUB/手动预览，
                            // md_wy 缺席）时高亮预览中的命中文本——MD 已切统一组件由下面
                            // `md_wy.highlight_search` 承担（A4）。
                            if (tab.preview_on || tab.preview_fullscreen) && tab.md_wy.is_none() {
                                let md = tab.md_preview.clone();
                                md.update(cx, |v, cx| {
                                    v.highlight_search(li, &q_owned, cx);
                                });
                            }
                            // 统一组件（live2 WYSIWYG）也在场时：命中文本同步高亮
                            // （预览独有能力搬入 `WyEditor::highlight_search`，同款语义）。
                            if let Some(l2) = tab.live2.clone() {
                                l2.update(cx, |v, cx| {
                                    v.highlight_search(li, &q_owned, cx);
                                });
                            }
                            // A2：右栏 WyEditor 预览（MD 统一组件，只读）在场时同样高亮
                            // 命中文本（预览从 MarkdownPreview 切到它之后的跳转高亮同步）。
                            if let Some(wy) = tab.md_wy.clone() {
                                wy.update(cx, |v, cx| {
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
            line_rows.push(
                hover_bg_fade(r, format!("ss-line-{ti}-{li}-bg"), active_bg, hovered)
                    .into_any_element(),
            );
        }
        line_rows
    }

    /// 搜索结果头：0 命中提示 / n 处匹配。
    fn sidebar_search_summary(&self, total: usize, muted: gpui::Hsla) -> gpui::AnyElement {
        if total == 0 {
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
        }
    }

    /// PDF 标签的搜索视图：顶部一行「n 处匹配」计数（右侧为 ↑/↓ 导航），
    /// 下方逐命中上下文行（`L N  前5字…目标…后5字`，单行显示超长省略，点击跳转到该命中页）。
    /// 检索本身由 [`crate::pdf::PdfViewer::set_search`] 在阅读器后台执行，这里只读状态并驱动跳转。
    fn render_sidebar_pdf_search(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<crate::pdf::PdfViewer>,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let (count, truncated, active) = pdf.read(cx).search_summary();
        let snippets: Vec<(usize, String)> = pdf.read(cx).search_snippets();

        let prev_btn = self.sidebar_pdf_nav_btn(window, "↑", -1, pdf.clone(), fg, active_bg, cx);
        let next_btn = self.sidebar_pdf_nav_btn(window, "↓", 1, pdf.clone(), fg, active_bg, cx);

        // 无匹配 → 居中空态占位（灰色搜索图标 + 提示文案），与普通搜索空态同款视觉。
        let summary: gpui::AnyElement = if count == 0 {
            let hint = if q.trim().is_empty() {
                "在 PDF 全文检索；输入关键字"
            } else {
                "PDF 中没有匹配"
            };
            crate::ui::empty_state::compact_empty_state(
                "sidebar-search-pdf-empty",
                IconName::Search,
                hint,
                cx,
            )
        } else {
            Self::sidebar_pdf_search_summary(count, truncated, fg, prev_btn, next_btn)
        };
        let hit_rows =
            self.sidebar_pdf_hit_rows(window, pdf, &snippets, active, fg, muted, active_bg, cx);

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

    /// PDF 搜索头部「上一处 / 下一处」导航按钮（↑/↓，点击在命中间移动）。
    /// 返回 owned `AnyElement`（避免 RPIT 捕获 `cx` 生命周期导致连续两按钮借用冲突）。
    fn sidebar_pdf_nav_btn(
        &self,
        window: &mut Window,
        symbol: &'static str,
        delta: i64,
        target: gpui::Entity<crate::pdf::PdfViewer>,
        fg: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = if delta < 0 {
            "sidebar-search-pdf-prev"
        } else {
            "sidebar-search-pdf-next"
        };
        // P2-9：tooltip 文案（上一处 / 下一处匹配）。
        let label = if delta < 0 { "上一处匹配" } else { "下一处匹配" };
        // P2-10：hover 状态跨帧记录 → 背景淡入。
        let hover = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
        let hovered = *hover.read(cx);
        // P2-9：tooltip 状态（同按钮 id 键控）。
        let tip = crate::ui::tooltip::tooltip_state(id, window, cx);
        let tip_on = crate::ui::tooltip::tooltip_visible(&tip, cx);
        let view_id = window.current_view();
        let btn = div()
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
            .on_hover({
                let st = hover.clone();
                let tip = tip.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    crate::ui::tooltip::tooltip_hover(&tip, *h, cx);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(move |_this: &mut EditorApp, _ev, _w, cx| {
                target.update(cx, |v, cx| v.move_match(delta, cx));
                cx.notify();
            }))
            .child(symbol)
            // P2-9：tooltip 浮层（贴导航按钮右缘垂直居中；按钮 28×24）。
            .child(crate::ui::tooltip::tooltip_layer(
                id, label, tip_on, 28.0, 24.0, cx,
            ));
        hover_bg_fade(btn, format!("{id}-bg"), active_bg, hovered).into_any_element()
    }

    /// PDF 搜索头部「n 处匹配」计数（计数在左、导航靠右；计数过长尾部省略）。
    /// 调用方已处理 `count == 0` 的空态分支，这里只渲染命中计数头。
    #[allow(clippy::too_many_arguments)] // 双色 + 计数来源 + 导航按钮，与 render_* 同构
    fn sidebar_pdf_search_summary(
        count: usize,
        truncated: bool,
        fg: gpui::Hsla,
        prev: impl IntoElement,
        next: impl IntoElement,
    ) -> gpui::AnyElement {
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
                    .child(prev)
                    .child(next),
            )
            .into_any_element()
    }

    /// PDF 搜索命中行（每个命中一行）：`L N  前5字…目标…后5字`，单行显示、超出省略；
    /// 当前激活命中高亮，点击跳转到该命中页。
    #[allow(clippy::too_many_arguments)] // 三色 + 数据源 + 命中列表，与 render_* 同构
    fn sidebar_pdf_hit_rows(
        &self,
        window: &mut Window,
        pdf: &gpui::Entity<crate::pdf::PdfViewer>,
        snippets: &[(usize, String)],
        active: usize,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut hit_rows: Vec<gpui::AnyElement> = Vec::new();
        for (idx, (page, snippet)) in snippets.iter().enumerate() {
            let page = *page;
            let color = if idx == active { fg } else { muted };
            let row_pdf = pdf.clone();
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover = window.use_keyed_state(format!("ss-pdf-hit-{idx}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
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
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
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
            hit_rows.push(
                hover_bg_fade(r, format!("ss-pdf-hit-{idx}-bg"), active_bg, hovered)
                    .into_any_element(),
            );
        }
        hit_rows
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
                    let mut doc_offset = 0usize;
                    for (li, line) in content.split('\n').enumerate() {
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
