//! 侧边栏「外壳」实体（2026-11 自 shell 的 `app/sidebar/chrome.rs` 迁入并解耦）。
//!
//! [`Sidebar`] = 侧栏域实体：宿主 `EditorApp` 经 `sidebar_view: Entity<Sidebar<EditorApp>>`
//! 持有，渲染期以 `&mut SidebarChromeState` 入参调用（状态在宿主普通字段、域内构建元素），
//! 事件回调（活动栏点击 / 调宽手柄）经 [`crate::host::SidebarHost`] 反调宿主落地状态迁移。
//!
//! 与旧 `impl EditorApp` 的对应关系（逐行迁移，行为零变）：
//!
//! - `render_sidebar` → [`Sidebar::render_sidebar_shell`]（`panel` 由宿主预构建传入：
//!   Toc 走本 crate `toc`、Search/Files 仍留 shell）。
//! - `render_sidebar_activity_bar` / `render_sidebar_resize_handle` → 同名私有方法。
//! - `toggle_sidebar_view` / `update_sidebar_resize` / `end_sidebar_resize` → 本实体方法
//!   （`update/end` 由宿主外层 `on_mouse_move/up` 持 `&mut SidebarChromeState` 调本实体，
//!   `start` 由调宽手柄 on_mouse_down 经宿主服务）；状态纯变换收在 [`crate::state`]。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    div, px, AnimationExt, AnyElement, Context, CursorStyle, Entity, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement,
    Styled, Subscription, WeakEntity, Window,
};
use shell_core::AppModel;
use ui::icon::{Icon, IconName};
use ui::motion::{hover_bg_fade, hover_spring};
use ui::theme::ChromeColors;

use crate::host::SidebarHost;
use crate::prompt::FileTreePromptHost;
use crate::state::{SidebarChromeState, SidebarView};

/// 侧边栏域实体：宿主 `sidebar_view: Entity<Sidebar<H>>` 持有，承载外壳 / 目录 / 文件树的
/// 渲染与事件（`Context<Self>` 内 `cx.listener` 绑定本实体）。不持 `SidebarChromeState`
/// （状态在宿主普通字段，渲染期以 `&mut` 入参传入；事件回调经宿主服务落地）。
/// 文件树右键菜单 / 输入弹层等**实体自有 UI 暂存**亦收在本实体（宿主仅留测试兼容镜像）。
pub struct Sidebar<H: SidebarHost + FileTreePromptHost> {
    /// 宿主模型（与 `EditorApp.model` 同一 `Rc<RefCell>` 共享）：目录视图读 tabs /
    /// reader / md_wy / pdf_bookmarks 与主题明暗，均在域内直取。
    /// `pub(crate)`：`toc.rs` / `files.rs` 拆分模块需直读。
    pub(crate) model: Rc<RefCell<AppModel>>,
    /// 宿主实体弱引用：事件回调 → 宿主服务（改侧栏状态 / notify / persist）。
    host: WeakEntity<H>,
    /// 文件树右键上下文菜单：弹出位置（窗口坐标）。实体自有（`files_ops.rs` 读写）；
    /// 宿主仅镜像 `menu_entity` 供测试断言。
    pub(crate) file_tree_menu_pos: Option<Point<Pixels>>,
    /// 已打开的文件树右键菜单实体（随位置同生同灭）。
    pub(crate) file_tree_menu_entity: Option<Entity<ui::menu::PopupMenu>>,
    /// 文件树右键菜单的关闭订阅（选择/Esc/点击外 → 清位置与实体）。
    pub(crate) file_tree_menu_sub: Option<Subscription>,
    /// 文件树「新建/重命名」输入弹层实体（泛型宿主 `H`）。实体自有（`files_ops.rs`
    /// 读写）；宿主仅镜像供测试。
    pub(crate) file_tree_prompt: Option<Entity<crate::prompt::FileTreePrompt<H>>>,
    /// 文件树输入弹层的关闭订阅（提交成功/取消 → 清引用释放实体）。
    pub(crate) file_tree_prompt_sub: Option<Subscription>,
}

impl<H: SidebarHost + FileTreePromptHost> Sidebar<H> {
    /// 构造：共享模型 + 宿主弱引用。
    pub fn new(model: Rc<RefCell<AppModel>>, host: WeakEntity<H>) -> Self {
        Self {
            model,
            host,
            file_tree_menu_pos: None,
            file_tree_menu_entity: None,
            file_tree_menu_sub: None,
            file_tree_prompt: None,
            file_tree_prompt_sub: None,
        }
    }

    /// 宿主弱引用（活动栏/手柄/toc 主题回调需要；`pub(crate)` 供 `toc.rs`）。
    pub(crate) fn host_weak(&self) -> WeakEntity<H> {
        self.host.clone()
    }

    /// 当前明暗模式对应的外壳配色（`ui::theme::ChromeColors` 单一来源，与宿主
    /// `EditorApp::chrome` 同口径：只读 `model.dark_mode`）。
    /// `pub(crate)`：`toc.rs` 拆分模块需配色（fg/muted/active_bg 等）。
    pub(crate) fn chrome(&self) -> ChromeColors {
        if self.model.borrow().dark_mode {
            ChromeColors::dark()
        } else {
            ChromeColors::light()
        }
    }

    /// Win11 Mica chrome 透明度（亮 0.86 / 暗 0.82）：半透明才能透出系统 Mica 材质。
    fn mica_alpha(&self) -> f32 {
        if self.model.borrow().dark_mode {
            ui::theme::MICA_CHROME_ALPHA_DARK
        } else {
            ui::theme::MICA_CHROME_ALPHA_LIGHT
        }
    }

    /// 工具条/侧栏背景（Mica 半透明版；暗色主题改用 theme `chrome_bg` 不透明统一底色）。
    fn mica_bar_bg(&self) -> Hsla {
        if self.model.borrow().dark_mode {
            ChromeColors::for_mode(true).chrome_bg.into()
        } else {
            self.bar_bg().opacity(self.mica_alpha())
        }
    }

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    pub(crate) fn bar_bg(&self) -> Hsla {
        self.chrome().bar_bg.into()
    }

    /// 工具条分隔边框。
    pub(crate) fn bar_border(&self) -> Hsla {
        self.chrome().bar_border.into()
    }

    /// 区域分隔发丝线（`ChromeColors::hairline` 语义 token、按明暗主题取色，不透明）。
    pub(crate) fn hairline(&self) -> Hsla {
        self.chrome().hairline.into()
    }

    /// 主要文字颜色。
    pub(crate) fn text_primary(&self) -> Hsla {
        self.chrome().text_primary.into()
    }

    /// 次要/弱化文字颜色。
    pub(crate) fn text_minor(&self) -> Hsla {
        self.chrome().text_minor.into()
    }

    /// 激活选项卡背景。
    pub(crate) fn tab_active_bg(&self) -> Hsla {
        self.chrome().tab_active_bg.into()
    }

    /// 切换视图（宿主 `app.toggle_sidebar_view` 测试入口的实体侧实现）：状态纯变换
    /// `sb.toggle_view`。**不 notify**——宿主 forwarder 在自身上下文 `cx.notify()`（实体
    /// 此处若经 `host.update` 再入宿主，会在宿主已持 `&mut` 的 `app.update` 闭包内
    /// RefCell 双借 panic）。
    pub fn toggle_sidebar_view(&mut self, sb: &mut SidebarChromeState, view: SidebarView) {
        sb.toggle_view(view);
    }

    /// 侧边栏整体：左侧为常驻竖列切换栏（活动栏），右侧为可展开的内容面板
    /// （目录/搜索/文件树——`panel` 由宿主预构建传入，Toc 走本 crate `toc`、
    /// Search/Files 仍留 shell）。面板收起时仅剩 46px 活动栏，正文区随之占满。
    ///
    /// 无缝 mica 直连外壳：整块 = 左侧活动栏 + 右侧面板（面板收起时仅活动栏），
    /// 底色用半透明 mica（`mica_bar_bg`）、无圆角、无四周边框，右缘一条发丝线
    /// （`bar_border`）与正文区分隔；右缘内嵌拖拽调宽手柄（见
    /// [`Self::render_sidebar_resize_handle`]，外层 `app-body-with-sidebar` 统一收事件）。
    /// 与旧 `EditorApp::render_sidebar` 逐行对应：先由宿主构建内容（目录视图会自适应
    /// 更新 `sb.width`），再取宽，避免本帧用旧宽。
    pub fn render_sidebar_shell(
        &mut self,
        sb: &mut SidebarChromeState,
        panel: Option<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (side_bg, border) = (self.mica_bar_bg(), self.bar_border());
        let width_px = if sb.panel_open {
            sb.width
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
            // 拖拽调宽事件由外层 app-body-with-sidebar 容器统一处理（见宿主 render_body_sidebar）
            .child(self.render_sidebar_activity_bar(sb, window, border, cx))
            .children(panel)
            .children(self.render_sidebar_resize_handle(sb, window, cx))
            .into_any_element()
    }

    /// 侧边栏右缘拖拽手柄：宽 6px 命中区，视觉 2px 强调细线（悬停/拖拽时经
    /// `hover_spring` 淡入，贴右缘绝对定位——左侧留 4px 余量、右侧 2px 视觉线）。
    /// 面板收起时返回空（无拖拽意义；不留任何 `<sidebar-gap>` 空间隔元素）。
    ///
    /// 命中/拖拽逻辑与现状完全一致：`on_mouse_down` → 宿主 `sidebar_resize_start`
    /// 记 (起始窗口 x, 起始宽)、外层 `app-body-with-sidebar` 的 `on_mouse_move/up(_out)`
    /// 全程跟踪 `update_sidebar_resize`（窗口坐标 δ，与手柄位置无关）；hover →
    /// 宿主 `sidebar_set_resize_hovered` 高亮。
    fn render_sidebar_resize_handle(
        &mut self,
        sb: &mut SidebarChromeState,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if !sb.panel_open {
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
                    |this: &mut Self, hovered: &bool, _w, cx| {
                        if let Some(h) = this.host_weak() .upgrade() {
                            let _ = h.update(cx, |app, cx| app.sidebar_set_resize_hovered(*hovered, cx));
                        }
                    },
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut Self, ev: &gpui::MouseDownEvent, _w, cx| {
                        if let Some(h) = this.host_weak() .upgrade() {
                            let _ = h.update(cx, |app, cx| {
                                app.sidebar_resize_start(ev.position.x.as_f32(), cx)
                            });
                        }
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
                            hover_spring(if sb.resize.is_some() || sb.resize_hovered {
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
        sb: &mut SidebarChromeState,
        window: &mut Window,
        border: Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let accent: Hsla = ui::theme::Theme::global(cx).colors.accent.into();
        let c = self.chrome();
        let hover_fill: Hsla = c.hover_fill.into();
        let pressed_fill: Hsla = c.pressed_fill.into();
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
        let mut btns: Vec<AnyElement> = Vec::with_capacity(3);
        for (view, icon, id, label) in specs {
            let is_active = sb.panel_open && sb.view == view;
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
                .on_click(cx.listener(move |this: &mut Self, _ev, _w, cx| {
                    if let Some(h) = this.host_weak() .upgrade() {
                        let _ = h.update(cx, |app, cx| app.sidebar_toggle_view(view, cx));
                    }
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
