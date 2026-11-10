//! 侧边栏「外壳」子模块：视图切换 / 正文区整体布局 / 右缘拖拽调宽 / 左侧活动栏。
//! 只做侧栏包裹与分发，各面板内容渲染在 `toc` / `search` / `files` 子模块。

use super::*;

impl EditorApp {
    /// 活动栏按钮点按：未在该面板 → 展开并切到该面板；
    /// 已在该面板再点一次 → **只收起面板**，侧边栏保留为窄活动栏（不留空白宽区）。
    #[doc(hidden)]
    pub fn toggle_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        self.sidebar.toggle_view(view);
        cx.notify();
    }

    /// 内容区整行：侧边栏展开时在左侧停靠（紧贴标题栏下），右侧为一「右列」——
    /// 上方选项卡栏 + 下方正文 `body`（正文块承载）；收起时仅返回右列本身。
    ///
    /// 无缝直连外壳：侧边栏（mica 底色）与右列（选项卡栏 + 正文块）之间无缝隙/圆角，
    /// 仅靠侧边栏右缘一条发丝线分隔；正文右缘贴窗、编辑区直落选项卡栏之下，状态栏
    /// 贴窗底部与正文块直连。右缘调宽手柄内嵌于侧边栏右缘（见 [`Self::render_sidebar`]）。
    pub(crate) fn render_body_sidebar(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // 右列：上方选项卡栏 + 下方编辑区（正文块承载；列容器 flex_col flex_1）。
        let right_col = div()
            .id("app-right-column")
            .debug_selector(|| "app-right-column".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(self.render_tab_bar(window, cx))
            .child(self.render_body_card(body, window, cx));

        if !self.sidebar.open {
            return right_col.into_any_element();
        }
        div()
            .id("app-body-with-sidebar")
            .debug_selector(|| "app-body-with-sidebar".into())
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            // 侧边栏右缘拖拽调宽：鼠标移动/松开事件冒泡到整个内容容器统一处理。
            // （原注释保留，说明不改动；此容器现也包裹右列选项卡栏+编辑区。）
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
            .child(right_col)
            .into_any_element()
    }

    /// 正文区无缝方块：右缘贴窗直角、其它三缘与侧栏/选项卡栏/状态栏零间距直连，
    /// 无圆角、无边框（侧栏右缘发丝线、标题栏/选项卡栏底边发丝线与状态栏顶边负责分隔）。
    /// 卡面用正文底色
    /// （`panel_bg`，与编辑器/预览无缝）。
    fn render_body_card(
        &mut self,
        body: gpui::AnyElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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
            .ml(px(0.))
            .mr(px(0.))
            .mt(px(0.))
            .mb(px(0.))
            // 顶边 1px 发丝线 = 选项卡栏↔编辑区之分隔（由后绘制的编辑区绘制，
            // 保证不受上层选项卡栏遮挡）；其余三边由侧栏右缘/状态栏顶边承担，不重复。
            .border_t_1()
            .border_color(self.hairline())
            .bg(ui::shell::body_card_face(cx))
            .overflow_hidden()
            .child(body)
            .into_any_element()
    }

    /// 侧边栏整体：左侧为常驻竖列切换栏（活动栏），右侧为可展开的内容面板
    /// （目录/搜索）。面板收起时仅剩 46px 活动栏，正文区随之占满，不留空白宽区。
    ///
    /// 无缝 mica 直连外壳：整块 = 左侧活动栏 + 右侧面板（面板收起时仅活动栏），
    /// 底色用半透明 mica（`mica_bar_bg`）、无圆角、无四周边框，右缘一条发丝线
    /// （`bar_border`）与正文区分隔；右缘内嵌拖拽调宽手柄（见
    /// [`Self::render_sidebar_resize_handle`]，外层 `app-body-with-sidebar` 统一收事件）。
    pub(crate) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (side_bg, border) = (self.mica_bar_bg(), self.bar_border());
        // 先构建内容（目录视图会自适应更新 sidebar_width），再取宽，避免本帧用旧宽
        let content: Option<gpui::AnyElement> = if self.sidebar.panel_open {
            Some(match self.sidebar.view {
                SidebarView::Toc => self.render_sidebar_toc(window, cx),
                SidebarView::Search => self.render_sidebar_search(window, cx),
                SidebarView::Files => self.render_sidebar_files(window, cx),
            })
        } else {
            None
        };
        let width_px = if self.sidebar.panel_open {
            self.sidebar.width
        } else {
            config::layout::SIDEBAR_ACTIVITY_W
        };
        div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .w(px(width_px))
            .flex_shrink_0()
            .relative()
            .flex()
            .flex_row()
            // 贴窗左上角：左缘直角、顶缘贴标题栏（mt 0，与选项卡栏对齐）；下缘贴
            // 状态栏（mb 0）。右缘发丝线即与正文的分隔，无间距。
            .ml(px(0.))
            .mt(px(0.))
            .mb(px(0.))
            .bg(side_bg)
            .border_r_1()
            .border_color(self.hairline())
            .overflow_hidden()
            // 拖拽调宽事件由外层 app-body-with-sidebar 容器统一处理（见 render_body_sidebar）
            .child(self.render_sidebar_activity_bar(window, border, cx))
            .children(content)
            .children(self.render_sidebar_resize_handle(cx))
    }

    /// 开始拖拽侧边栏右缘：记录 (起始 x, 起始宽)，关闭自适应由用户接管。
    pub(crate) fn start_sidebar_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        self.sidebar.resize = Some((start_x, self.sidebar.width));
        self.sidebar.width_auto = false;
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新侧边栏宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    pub(crate) fn update_sidebar_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_w)) = self.sidebar.resize {
            self.sidebar.width = (start_w + (x - start_x))
                .clamp(config::layout::SIDEBAR_WIDTH_MIN, config::layout::SIDEBAR_WIDTH_MAX);
            cx.notify();
        }
    }

    /// 结束拖拽：宽度定稿后持久化（`width_auto` 已被关掉，存为手动宽度）。
    pub(crate) fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar.resize.is_some() {
            self.sidebar.resize = None;
            self.persist_prefs();
            cx.notify();
        }
    }

    /// 侧边栏右缘拖拽手柄：宽 6px 命中区，视觉 2px 强调细线（悬停/拖拽时经
    /// `hover_spring` 淡入，贴右缘绝对定位——左侧留 4px 余量、右侧 2px 视觉线）。
    /// 面板收起时返回空（无拖拽意义；不留任何 `<sidebar-gap>` 空间隔元素）。
    ///
    /// 命中/拖拽逻辑与现状完全一致：`start_sidebar_resize` 记 (起始窗口 x, 起始宽)、
    /// 外层 `app-body-with-sidebar` 的 `on_mouse_move/up(_out)` 全程跟踪（窗口坐标 δ，
    /// 与手柄位置无关）。
    fn render_sidebar_resize_handle(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if !self.sidebar.panel_open {
            return Vec::new();
        }
        let drag_line = ui::theme::Theme::global(cx).colors.drag_hover_border;
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
                .child(
                    div()
                        // 贴到右缘，不用 flex 居中（否则蓝色线离边缘差 ~2px 看着有缝）
                        .absolute()
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .w(px(2.))
                        .rounded_full()
                        .bg(drag_line)
                        // P2-10：悬停/拖拽时细线淡入（滚动条滑块同款指数平滑可接受，
                        // 这里用统一 hover 弹簧）。
                        .with_spring(
                            "sidebar-resize-accent",
                            hover_spring(if self.sidebar.resize.is_some()
                                || self.sidebar.resize_hovered
                            {
                                1.0
                            } else {
                                0.0
                            }),
                            |this, v| this.opacity(v),
                        ),
                )
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
        let accent: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
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
            (
                SidebarView::Files,
                IconName::Folder,
                "sidebar-btn-files",
                "文件",
            ),
        ];
        let mut btns: Vec<gpui::AnyElement> = Vec::with_capacity(3);
        for (view, icon, id, label) in specs {
            let is_active = self.sidebar.panel_open && self.sidebar.view == view;
            // 活动项图标用主前景色（深色主题下近白），不变蓝
            let col = if is_active { fg } else { muted };
            let id_owned = id.to_string();
            // P2-10：hover 状态跨帧记录 → 背景淡入。
            let btn_hover = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
            let btn_hovered = *btn_hover.read(cx);
            // P2-9：tooltip 状态（同按钮 id 键控：延迟出现 300ms / 移出消失）。
            let tip = ui::tooltip::tooltip_state(id, window, cx);
            let tip_on = ui::tooltip::tooltip_visible(&tip, cx);
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
                        ui::tooltip::tooltip_hover(&tip, *hovered, cx);
                        cx.notify(view_id);
                    }
                })
                .child(Icon::new(icon).size(px(22.)).text_color(col))
                // P2-9：tooltip 浮层（贴活动栏按钮右缘垂直居中；按钮 40×38）。
                .child(ui::tooltip::tooltip_layer(
                    id, label, tip_on, 40.0, 38.0, cx,
                ));
            btns.push(
                hover_bg_fade(btn, format!("{id}-bg"), hover_fill, btn_hovered).into_any_element(),
            );
        }
        div()
            .id("sidebar-activity-bar")
            .debug_selector(|| "sidebar-activity-bar".into())
            .w(px(config::layout::SIDEBAR_ACTIVITY_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .py_1()
            .border_r_1()
            .border_color(border)
            .children(btns)
    }
}
