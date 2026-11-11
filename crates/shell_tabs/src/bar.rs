//! 选项卡栏「域实体」渲染（2026-11-11 自 shell 的 `app/tabs.rs` 迁入并解耦）。
//!
//! [`TabBar`] = 选项卡栏域实体：宿主 `EditorApp` 经 `tab_bar_view: Entity<TabBar<EditorApp>>`
//! 持有，渲染期以「宿主字段引用 + 预取派生数据」为入参调用（`tab_bounds` 拖拽命中槽 /
//! `tab_bar_hscroll` 横向滚动条仍是宿主字段、以 `&mut`/`&` 传入；`preview_on` /
//! `active_is_md` 是渲染期禁入宿主、由宿主预取传入的派生布尔）；事件回调（激活 / 关闭 /
//! 右键菜单项 / 滚轮 / 拖拽幽灵）经 [`crate::host::TabsHost`] 反调宿主落地副作用，行为零变。
//!
//! 与旧 `impl EditorApp` 的对应关系（逐行迁移）：
//!
//! - `render_tab_bar` → [`TabBar::render_tab_bar`]（`tab_bounds` 以 `&mut` 传入重建、
//!   `tab_bar_hscroll` 以 `&` 传入接线）。
//! - `tab_bar_item` → [`TabBar::render_tab_bar_item`]（宿主弱引用于回调内反调动作）。
//! - `tab_eye_button` → [`TabBar::render_tab_eye_button`]（可见性参数 `active_is_md` /
//!   `preview_on` 由宿主预取传入）。
//! - `tab_bar_hscroll_overlay` → `TabBar::render_tab_bar_hscroll_overlay`。
//! - `on_tab_bar_right_click` / `on_tab_right_click` → 同名实体方法（菜单槽位经
//!   [`crate::host::TabsHost`] `tabs_host_open_*`/`tabs_host_dismiss_*` 落宿主）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    div, px, AnimationExt, AppContext, Bounds, Context, CursorStyle, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window,
};
use shell_core::{AppModel, TabDragData, TabDragGhost};
use shell_widgets::scrollbar::{horizontal_scrollbar, TableScrollState};
use shell_widgets::TabBoundsRecorder;
use ui::menu::PopupMenuItem;
use ui::motion::hover_spring;
use ui::theme::ChromeColors;

use crate::host::TabsHost;

/// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
struct TabBarColors {
    text_primary: gpui::Hsla,
    text_minor: gpui::Hsla,
    tab_active_bg: gpui::Hsla,
    tab_active_border: gpui::Hsla,
    hover_fill: gpui::Hsla,
}

/// 拖拽排序的白色插入指示线画在哪一侧：`Left` = 本选项卡**左侧**边界（插到它之前）；
/// `Right` = 本选项卡**右侧**边界（插到它之后 / 末尾）。由 `render_tab_bar` 按
/// `tab_reorder_target` 指派给对应选项卡元素自行绘制，天然随横向滚动对齐。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReorderSide {
    Left,
    Right,
}

/// 选项卡栏域实体：宿主 `tab_bar_view: Entity<TabBar<H>>` 持有，承载选项卡栏的渲染与
/// 事件（`Context<Self>` 内 `cx.listener` 绑定本实体）。共享模型（`model`）域内直读；
/// 其余宿主状态（`tab_bounds` / `tab_bar_hscroll`）渲染期以引用入参、副作用反调宿主。
pub struct TabBar<H: TabsHost> {
    /// 宿主模型（与 `EditorApp.model` 同一 `Rc<RefCell>` 共享）：选项卡栏读 `tabs` /
    /// `active_tab` / `dark_mode` / `tab_dragging` / `tab_reorder_target` 与写
    /// `tab_drag_grab`（拖拽开始），均在域内直取（同 `shell_sidebar::Sidebar::model`）。
    pub(crate) model: Rc<RefCell<AppModel>>,
    /// 宿主实体弱引用：事件回调 → 宿主服务（动作 / 滚动条 / 菜单槽位 / 幽灵窗）。
    host: WeakEntity<H>,
}

impl<H: TabsHost> TabBar<H> {
    /// 构造：共享模型 + 宿主弱引用（宿主 `init.rs` 于窗口构造时创建）。
    pub fn new(model: Rc<RefCell<AppModel>>, host: WeakEntity<H>) -> Self {
        Self { model, host }
    }

    /// 宿主弱点（事件回调经它反调宿主）。
    pub(crate) fn host_weak(&self) -> WeakEntity<H> {
        self.host.clone()
    }

    /// 当前明暗模式对应的外壳配色（`ui::theme::ChromeColors` 单一来源，与宿主
    /// `EditorApp::chrome` 同口径：只读 `model.dark_mode`）。
    fn chrome(&self) -> ChromeColors {
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

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    fn bar_bg(&self) -> gpui::Hsla {
        self.chrome().bar_bg.into()
    }

    /// 标题栏/选项卡栏/侧栏工具条背景（Mica 半透明版；暗色主题改用 theme `chrome_bg`
    /// #191A1B 的不透明统一底色）。
    fn mica_bar_bg(&self) -> gpui::Hsla {
        if self.model.borrow().dark_mode {
            ui::theme::ChromeColors::for_mode(true).chrome_bg.into()
        } else {
            self.bar_bg().opacity(self.mica_alpha())
        }
    }

    /// 主要文字颜色。
    fn text_primary(&self) -> gpui::Hsla {
        self.chrome().text_primary.into()
    }

    /// 次要/弱化文字颜色。
    fn text_minor(&self) -> gpui::Hsla {
        self.chrome().text_minor.into()
    }

    /// 激活选项卡背景。
    fn tab_active_bg(&self) -> gpui::Hsla {
        self.chrome().tab_active_bg.into()
    }

    /// 激活选项卡边框。
    fn tab_active_border(&self) -> gpui::Hsla {
        self.chrome().tab_active_border.into()
    }

    /// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
    fn bar_colors(&self, _cx: &mut Context<Self>) -> TabBarColors {
        let c = self.chrome();
        TabBarColors {
            text_primary: self.text_primary(),
            text_minor: self.text_minor(),
            tab_active_bg: self.tab_active_bg(),
            tab_active_border: self.tab_active_border(),
            hover_fill: c.hover_fill.into(),
        }
    }

    /// 单个选项卡元素：名称/脏标记/关闭钮/激活顶线 + 拖拽幽灵 + 点击激活。
    /// `reorder_line` = 拖拽排序时本选项卡要绘制的白色插入指示线侧（None = 不画）；
    /// `bounds_cell` = 每帧真实 laid-out 边界写入点（宿主 `tab_bounds` 槽位，排序命中用）。
    /// `host` = 宿主弱引用（回调反调宿主动作）。
    fn render_tab_bar_item(
        name: &str,
        dirty: bool,
        path_hint: Option<String>,
        idx: usize,
        is_active: bool,
        reorder_line: Option<ReorderSide>,
        bounds_cell: Rc<Cell<Bounds<Pixels>>>,
        colors: &TabBarColors,
        host: WeakEntity<H>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = name.to_string();

        // 元素级 hover 状态：跨帧记住「该 tab 是否被悬停」，关闭 × 因此在
        // 激活项恒定可见、非激活项悬停显现（VS Code 观感）。
        let view_id = window.current_view();
        let tab_hover = window.use_keyed_state(("tab", idx), cx, |_, _| false);
        let hovered = *tab_hover.read(cx);

        // P2-9 鼠标跟随（2026-11-10）：记录下方路径 tooltip 的**窗口坐标锚点**（水平 = 鼠标
        // 窗口 x，垂直 = tab 底缘 y）。锚点在 `on_mouse_move`（事件期）算——此刻 `bounds_cell`
        // 才持有上一帧 laid-out 边界；render 期 `render_tab_bar` 每帧重建 cell（宽恒 0）不可用。
        // key 与既有 ("tab", idx) hover 态区分，互不冲突。
        let tab_mx = window.use_keyed_state(("tab-mx", idx), cx, |_, _| None::<gpui::Point<Pixels>>);
        let anchor = *tab_mx.read(cx);

        // P2-9 tooltip（2026-11-10 由状态栏文件名迁移）：每个 tab 独立 id 键控；悬停
        // 显示该 tab 自身完整路径；未命名（无路径）悬殊停恒不显示浮层。
        let path_hint = path_hint.unwrap_or_default();
        let has_path = !path_hint.is_empty();
        let tip_id = format!("tab-tip-{idx}");
        let tip = ui::tooltip::tooltip_state(&tip_id, window, cx);
        let tip_on = ui::tooltip::tooltip_visible(&tip, cx);

        // 浮层锚在 tab 下方、水平中心随鼠标 x 移动（`tooltip_layer_placed` Window 定位，锚=
        // 事件期算出的窗口坐标点；不盖相邻选项卡）：host_w = 内容宽近似 = px_2 双向 16 +
        // 脏点宽 + 名宽 + 两个 gap_1 8 + 关闭钮 18（仅 None 回退居中使用）。measure 闭包
        // 姿势同 `status_bar`（`window.text_style().font()` + `shape_line(..., px(12.), ...)`）。
        let base_font = window.text_style().font();
        let measure = |text: SharedString, weight: gpui::FontWeight| -> f32 {
            let len = text.len();
            let mut font = base_font.clone();
            font.weight = weight;
            f32::from(
                window
                    .text_system()
                    .shape_line(
                        text,
                        px(12.),
                        &[gpui::TextRun {
                            len,
                            font,
                            color: gpui::Hsla::default(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    )
                    .width(),
            )
        };
        let dirty_dot_w = if dirty {
            measure("●".into(), gpui::FontWeight::NORMAL) + 4.0
        } else {
            0.0
        };
        let name_w = measure(name.clone().into(), gpui::FontWeight::NORMAL);
        let host_w = 16.0 + dirty_dot_w + name_w + 8.0 + 18.0;

        let host_close = host.clone();
        let close = div()
            .id(SharedString::from(format!("tab-close-{idx}")))
            // H1 评审回归：debug_selector 供 app::tests::tab_bar 断言关闭钮渲染
            // （`debug_bounds` 只收录带 debug_selector 的元素；行为零影响）。
            .debug_selector(move || format!("tab-close-{idx}"))
            .w(px(18.))
            .h(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(13.))
            .text_color(colors.text_minor)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(colors.hover_fill))
            .child("×")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_this: &mut Self, _ev, window, cx| {
                    if let Some(h) = host_close.upgrade() {
                        let _ = h.update(cx, |app, cx| {
                            app.tabs_host_close_tab(idx, window, cx);
                        });
                    }
                    cx.stop_propagation();
                }),
            )
            // P2-10：仅视觉的 × 悬停显现淡入（激活项恒定、非激活项随行 hover）。
            .with_spring(
                format!("tab-close-op-{idx}"),
                hover_spring(if is_active || hovered { 1.0 } else { 0.0 }),
                move |this, v| this.opacity(v),
            );

        let mut el = div()
            .id(SharedString::from(format!("tab-{idx}")))
            .debug_selector(move || format!("tab-{idx}"))
            .h(px(26.))
            .my(px(3.))
            .px_2()
            .flex()
            .items_center()
            .flex_shrink(1.)
            .relative()
            .cursor(CursorStyle::PointingHand)
            .text_size(px(12.))
            .text_color(colors.text_primary)
            // 几何硬约束（2026-11-10）：[脏点, 名, 关闭钮] 包进内层 flex_row + gap_1，
            // 外层去掉 gap_1——tooltip_layer 空 div 作外层直子零宽度，tab 的 border-box
            // 与改动前逐像素一致；浮层锚在 tab 下方水平居中（host_w=内容宽近似）。
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(if dirty { "● " } else { "" })
                    .child(SharedString::from(name.clone()))
                    .child(close),
            )
            // 路径提示浮层（锚在 tab 下方、水平中心随鼠标 x 移动、不盖相邻选项卡；无路径
            // （未命名）恒不显示）：host_w = 内容宽近似 = px_2 双向 16 + 脏点宽 + 名宽 + 两个
            // gap_1 8 + 关闭钮 18。compact 密度（更小内边距/字号）仅本 tooltip 使用。
            .child(ui::tooltip::tooltip_layer_placed(
                &tip_id,
                &path_hint,
                has_path && tip_on,
                host_w,
                26.0,
                ui::tooltip::TooltipPlacement::Below,
                ui::tooltip::TooltipTone::Compact,
                anchor,
                cx,
            ));
        // 激活项 = VS Code 卡片式：26px 圆角白卡浮在灰带上（浅色下）；
        // 非激活项透明底 + hover 填充（背景由底部弹簧统一淡入，P2-10）——非激活项同样加
        // `rounded_md()`（**不加** `border_1/border_color`，边框仅激活卡片有，非激活透明底
        // 有边框会变方形框），保证 hover 高亮为**圆角矩形**而非尖角方矩形；border-radius
        // 不改 border-box 尺寸，既有几何测试不受影响。
        el = if is_active {
            el.rounded_md()
                .border_1()
                .border_color(colors.tab_active_border)
        } else {
            el.rounded_md()
        };
        // hover 状态跨帧记录，驱动关闭 × 悬停显现与行背景淡入；同时驱动路径 tooltip 出没。
        el = el.on_hover({
            let st = tab_hover.clone();
            let tip = tip.clone();
            move |hovered, _window, cx| {
                st.update(cx, |h, _| *h = *hovered);
                ui::tooltip::tooltip_hover(&tip, *hovered, cx);
                cx.notify(view_id);
            }
        });
        // 鼠标 x 跨帧记录（事件期）：此刻 `bounds_cell` 持上一帧 laid-out 边界（render 期被
        // 每帧重建、不可用）。只在 tooltip **尚未显示**时记录锚点（出现时对齐鼠标当时所在
        // x、垂直 tab 底缘）；一旦显示即冻结——`tip_on` 为真不再记录、也不再 notify 重渲染，
        // 后续任何重渲染都读到同一锚点（真冻结）。移出隐藏后重新进入会重新记录（恢复）。
        el = el.on_mouse_move({
            let st = tab_mx.clone();
            let bcell = bounds_cell.clone();
            move |ev: &gpui::MouseMoveEvent, _window, cx| {
                if !tip_on {
                    let b = bcell.get();
                    if b.size.width > px(0.) {
                        st.update(cx, |m, _| {
                            *m = Some(gpui::point(
                                px(ev.position.x.as_f32()),
                                b.bottom_right().y,
                            ))
                        });
                    }
                }
            }
        });
        // 多窗口拖拽：按住选项卡拖动 → 跟出“胶囊”幽灵；
        // 在窗口外松开由宿主根视图决定「拆独立窗口 / 还原回主窗口」。
        // 真实 laid-out 尺寸记录器：包住最终元素，每帧把选项卡 border-box 抄进 size_cell，
        // 供拖拽预览 / 幽灵窗以真实尺寸渲染（不缩不被裁）。
        let size_cell = Rc::new(Cell::new(gpui::Size {
            width: px(0.),
            height: px(0.),
        }));
        let drag_data = TabDragData {
            idx,
            name: SharedString::from(name),
            size: size_cell.clone(),
        };
        let ghost_name = drag_data.name.clone();
        // 拖拽开始时 gpui 把「抓取偏移」（按下点在点击矩形内的偏移）交给构造器；窗外
        // 跟手幽灵窗按同一偏移对齐（picture 恒画在鼠标-偏移处），保证窗口边缘无缝。
        // 拖拽开始即把抓取偏移写共享模型、并反调宿主预建窗外幽灵窗（`prime_drag_ghost`）。
        let tab_weak = cx.weak_entity();
        let host_for_drag = host.clone();
        // P2-10：背景弹簧——激活=白卡轻淡入、非激活=hover 填充淡入（同一曲线）；
        // 激活底色 / hover 底色自带 alpha，`opacity` 乘性，v=1 即原色，语义不变。
        let card = if is_active {
            colors.tab_active_bg
        } else {
            colors.hover_fill
        };
        let el = el.on_drag(
            drag_data,
            move |_value: &TabDragData, offset, window, cx| {
                // 拖拽开始：先记抓取偏移（共享模型，`tab_drag_grab`），再经宿主预建幽灵窗
                // （`prime_drag_ghost` 内部亦置 `tab_dragging`）——顺序与迁移前逐字一致。
                if let Some(tb) = tab_weak.upgrade() {
                    let _ = tb.update(cx, |this, _cx| {
                        this.model.borrow_mut().tab_drag_grab = Some(offset);
                    });
                }
                if let Some(h) = host_for_drag.upgrade() {
                    let _ = h.update(cx, |app, cx| {
                        app.tabs_host_prime_drag_ghost(
                            _value.idx,
                            _value.name.clone(),
                            dirty,
                            _value.size.clone(),
                            window,
                            cx,
                        );
                    });
                }
                cx.new(|_| TabDragGhost {
                    name: ghost_name.clone(),
                    dirty,
                    size: (_value).size.clone(),
                    ghost: false,
                })
            },
        )
        .on_mouse_down(
            MouseButton::Left,
            {
                let host = host.clone();
                cx.listener(move |_this: &mut Self, _ev, window, cx| {
                    if let Some(h) = host.upgrade() {
                        let _ = h.update(cx, |app, cx| {
                            app.tabs_host_activate_tab(idx, window, cx);
                        });
                    }
                })
            },
        )
        // 选项卡右键 → 选项卡管理菜单（关闭组 / 移入·复制新窗口组）。
        // `stop_propagation` 在 `on_tab_right_click` 内：不冒泡到栏级右键（新建/打开）、
        // 也不触发图片/视频媒体右键菜单。
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this: &mut Self, ev, window, cx| {
                this.on_tab_right_click(idx, ev, window, cx);
            }),
        );
        // 拖拽排序：本次拖拽光标落在「插入到本选项卡左/右边界」→ 在本选项卡边界画白色
        // 插入指示线（absolute 子元素定位于**本选项卡自身**边界，随横向滚动天然对齐；
        // 拖拽 picture 覆盖其上为半透明胶囊，不遮蔽整条指示线）。
        let el = if let Some(side) = reorder_line {
            let line = div()
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(2.))
                .bg(gpui::white())
                .rounded_full()
                .debug_selector(|| "tab-reorder-line".into());
            match side {
                ReorderSide::Left => el.child(line.left_0()),
                ReorderSide::Right => el.child(line.right_0()),
            }
        } else {
            el
        };
        let el: gpui::AnyElement = el
            .with_spring(
                format!("tab-bg-{idx}"),
                hover_spring(if is_active || hovered { 1.0 } else { 0.0 }),
                move |this, v| this.bg(card.opacity(v)),
            )
            .into_any_element();
        // 包上尺寸记录器：由 recorder 持有 size_cell / bounds_cell（拖拽侧另有 Clone），
        // 每帧把真实 laid-out 尺寸 / 边界写入 → 拖拽预览 / 幽灵窗按真实尺寸渲染、
        // 栏内排序命中使用。
        TabBoundsRecorder {
            size: size_cell,
            bounds: bounds_cell,
            child: el,
        }
        .into_any_element()
    }

    /// 整条选项卡栏最右侧的眼睛图标：仅当"当前选中的文件"是 Markdown（.md / .markdown）
    /// 时才显示（`active_is_md` 由宿主预取传入，域不依赖分类），点击切换当前选项卡的
    /// Markdown 预览（与「查看 → 预览」一致）。用纯 div 画出（椭圆外框 + 中心瞳孔），
    /// 避免依赖 emoji 字体；蓝色=已开，灰色=已关。
    fn render_tab_eye_button(
        &self,
        active_is_md: bool,
        preview_on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !active_is_md {
            return None;
        }
        let eye_color: gpui::Hsla = if preview_on {
            self.chrome().pill_fg.into()
        } else {
            self.chrome().text_tertiary.into()
        };
        let hover_fill: gpui::Hsla = self.chrome().hover_fill.into();
        // P2-10：hover 状态跨帧记录 → 背景淡入。
        let hover = window.use_keyed_state("tab-eye-hover", cx, |_, _| false);
        let hovered = *hover.read(cx);
        let view_id = window.current_view();
        Some(
            div()
                .id("tab-bar-eye")
                .debug_selector(|| "tab-bar-eye".into())
                .w(px(22.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .child(
                    div()
                        .w(px(16.))
                        .h(px(10.))
                        .rounded_full()
                        .border_1()
                        .border_color(eye_color)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().w(px(5.)).h(px(5.)).rounded_full().bg(eye_color)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _ev, window, cx| {
                        let idx = this.model.borrow().active_tab;
                        let host = this.host_weak();
                        if let Some(h) = host.upgrade() {
                            let _ = h.update(cx, |app, cx| {
                                app.tabs_host_toggle_preview_for_tab(idx, window, cx);
                            });
                        }
                    }),
                )
                .with_spring(
                    "tab-eye-bg",
                    hover_spring(if hovered { 1.0 } else { 0.0 }),
                    move |this, v| this.bg(hover_fill.opacity(v)),
                )
                .into_any_element(),
        )
    }

    /// 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现；
    /// 半透明、贴着标签底部、浮在上方（不额外占高度）。
    fn render_tab_bar_hscroll_overlay(&self, hscroll: &TableScrollState) -> Option<gpui::AnyElement> {
        let hscroll_dragging = hscroll.drag.borrow().is_some();
        if hscroll.handle.max_offset().x > px(0.)
            && (*hscroll.visible.borrow() || hscroll_dragging)
        {
            // 滑块底色取主题语义 token（ChromeColors::scrollbar_thumb，亮暗同值）。
            let thumb = ChromeColors::for_mode(self.model.borrow().dark_mode).scrollbar_thumb;
            Some(
                div()
                    .absolute()
                    .debug_selector(|| "tab-bar-hscroll-overlay".into())
                    .left(px(0.))
                    .right(px(0.))
                    .bottom(px(0.))
                    .h(px(3.))
                    .child(horizontal_scrollbar(
                        hscroll,
                        "tab-bar-hscroll",
                        thumb,
                    ))
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    /// 选项卡栏：横跨右列顶部的选项卡行，左侧滚动区 + （按需）眼睛按钮；
    /// 右键整条栏弹「新建文件 / 打开文件」上下文菜单（取代已移除的「+」按钮）。
    /// 底边绘一条右列宽发丝线（`bar_border`），分隔下方编辑区正文。
    /// 零选项卡（`tabs.is_empty()`，即欢迎页呈现时）提前返回空元素——选项卡栏
    /// 在 flex 列中占零高度，欢迎页从标题栏下直接铺到状态栏。
    /// `tab_bounds` 以 `&mut` 传入：每帧重建边界槽（排序命中测试读上一帧值；记录器在
    /// prepaint 写入）——宿主 `tab_drag.rs` 的 `tab_insert_index`/`tab_drag_over_row` 直读
    /// 宿主该字段（本阶段 tab_drag 仍走宿主原路径）。`hscroll` 为宿主横向滚动条状态
    /// （`app/tests/tab_bar.rs` 直读宿主字段，故留宿主）。`active_is_md` / `preview_on`
    /// 渲染期禁入宿主、由宿主按模型预取传入（眼睛钮用）。
    pub fn render_tab_bar(
        &mut self,
        tab_bounds: &mut Vec<Rc<Cell<Bounds<Pixels>>>>,
        hscroll: &TableScrollState,
        active_is_md: bool,
        preview_on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.model.borrow().tabs.is_empty() {
            tab_bounds.clear();
            return div().into_any_element();
        }
        let active = self.model.borrow().active_tab;
        let colors = self.bar_colors(cx);
        // 重建每帧的页卡边界槽（排序命中测试读上一帧值；记录器在 prepaint 写入）。
        *tab_bounds = (0..self.model.borrow().tabs.len())
            .map(|_| Rc::new(Cell::new(Bounds::default())))
            .collect();
        // 拖拽排序插入指示线：仅拖拽中按 `tab_reorder_target` 指派——目标 < n → 该目标
        // 选项卡左缘一线；目标 == n（末尾）→ 最后一个选项卡右缘一线。
        let reorder_target = if self.model.borrow().tab_dragging.is_some() {
            self.model.borrow().tab_reorder_target
        } else {
            None
        };
        // 借用纪律（SC-5）：先一次 borrow 快照每个选项卡渲染所需的廉价字段（显示名 +
        // 脏标记 + 完整路径），guard 随即释放——构造期不再持 `tabs` 长借用。
        // `render_tab_bar_item` 只消费这三项派生数据，改动零渲染差异（未命名路径为 None）。
        let tab_meta: Vec<(String, bool, Option<String>)> = {
            let model = self.model.borrow();
            model
                .tabs
                .iter()
                .map(|t| {
                    let name = t
                        .file_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "未命名".to_string());
                    let path_hint = t
                        .file_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned());
                    (name, t.dirty, path_hint)
                })
                .collect()
        };
        let tabs_len = tab_meta.len();
        let mut tabs: Vec<gpui::AnyElement> = Vec::new();
        let host = self.host_weak();
        for (i, (name, dirty, path_hint)) in tab_meta.into_iter().enumerate() {
            let line = match reorder_target {
                Some(t) if i == t => Some(ReorderSide::Left),
                Some(t) if t == tabs_len && i + 1 == tabs_len => Some(ReorderSide::Right),
                _ => None,
            };
            tabs.push(Self::render_tab_bar_item(
                &name,
                dirty,
                path_hint,
                i,
                i == active,
                line,
                tab_bounds[i].clone(),
                &colors,
                host.clone(),
                window,
                cx,
            ));
        }

        // 选项卡栏右侧：固定放（按需）眼睛图标；标签过多时由左侧滚动区横向滚动
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(eye) = self.render_tab_eye_button(active_is_md, preview_on, window, cx) {
            right.push(eye);
        }

        // 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现。
        let mut hscroll_overlay: Vec<gpui::AnyElement> = Vec::new();
        if let Some(o) = self.render_tab_bar_hscroll_overlay(hscroll) {
            hscroll_overlay.push(o);
        }

        div()
            .id("tab-bar")
            .debug_selector(|| "tab-bar".into())
            .relative()
            .w_full()
            // 左侧不留内边距：第一个选项卡紧贴窗口左边缘；右侧保留 px_2 给眼睛按钮
            .pl_0()
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(self.mica_bar_bg())
            // 右键整条栏（含点在各选项卡上，事件冒泡到这）→ 「新建文件 / 打开文件」上下文菜单
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::on_tab_bar_right_click),
            )
            // 选项卡栏↔编辑区的发丝线由下方编辑区顶边绘制（见 render_body_card，
            // 此处不画底边框，避免被后绘制的编辑区面板盖住）。
            // 选项卡横向滚动区：占满剩余宽度；标签不收缩，过多即横向溢出可滚轮/拖拽滚动。
            // 横向滚动条浮在标签底部（见 hscroll_overlay），不改变整体高度。
            .child(
                div()
                    .id("tab-bar-scroll")
                    .relative()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(32.))
                    .child(
                        div()
                            .id("tab-bar-tabs")
                            .debug_selector(|| "tab-bar-tabs".into())
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .overflow_x_scroll()
                            .track_scroll(&hscroll.handle)
                            // 滚动时点亮横向滚动条（自动隐藏计时由此重启）
                            .on_scroll_wheel(cx.listener(
                                |this: &mut Self, _ev, _window, cx| {
                                    let host = this.host_weak();
                                    if let Some(h) = host.upgrade() {
                                        let _ = h.update(cx, |app, cx| {
                                            app.tabs_host_poke_tab_hscroll(cx);
                                        });
                                    }
                                },
                            ))
                            .children(tabs),
                    )
                    .children(hscroll_overlay),
            )
            .children(right)
            .into_any_element()
    }

    /// 选项卡栏右键 → 在光标处弹「新建文件 / 打开文件」上下文菜单（取代已移除的「+」
    /// 按钮）。菜单槽位（pos/实体/订阅）仍落宿主（`tab_bar_menu_*` 测试直读 + 前置弹窗
    /// 互关直清），经 [`crate::host::TabsHost`] 落地。关闭（选择/Esc/点击外）经
    /// `DismissEvent` 订阅清理；先收掉上一份菜单（若有）再在光标处新建（旧订阅带 emitter
    /// 身份校验，不会误清新菜单——身份校验收在宿主 `tabs_host_dismiss_tab_bar_menu`）。
    /// 装配/焦点/订阅闭环收敛自 `ui::menu::spawn_context_menu`。
    pub fn on_tab_bar_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 事件继续冒泡到树根会额外触发图片/视频右键「锁定」菜单（on_media_right_click），
        // 此处拦截，保证右键选项卡栏只弹本菜单。
        cx.stop_propagation();
        let weak = self.host_weak();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                menu.item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("新建文件")
                            .debug_selector(|| "tab-bar-menu-item-new".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.tabs_host_new_tab(window, cx));
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("打开文件")
                            .debug_selector(|| "tab-bar-menu-item-open".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.tabs_host_open_dialog(window, cx));
                    })
                })
            },
            |this, emitter, cx| {
                // 关闭（选择/Esc/点击外）→ 宿主身份校验 + 清理槽位；emitter 身份校验避免
                // 旧菜单的关闭误清新菜单。
                let host = this.host_weak();
                if let Some(h) = host.upgrade() {
                    let _ = h.update(cx, |app, cx| {
                        app.tabs_host_dismiss_tab_bar_menu(emitter, cx);
                    });
                }
            },
        );
        let host = self.host_weak();
        if let Some(h) = host.upgrade() {
            let _ = h.update(cx, |app, cx| {
                app.tabs_host_open_tab_bar_menu(ev.position, menu, sub, cx);
            });
        }
    }

    // -----------------------------------------------------------------
    // 选项卡右键管理菜单 + 批量关闭 + 移入/复制新窗口
    // -----------------------------------------------------------------

    /// 选项卡右键 → 在光标处弹**选项卡管理菜单**（复用宿主 `tab_bar_menu_*` 槽位与
    /// `render_tab_bar_menu` Overlay，同一时刻只开一个）。菜单分组：
    /// 关闭 / 关闭其他 / 关闭右侧 / 关闭已保存 / 关闭全部 ── 分割线 ──
    /// 移入新窗口 / 复制到新窗口。
    /// `stop_propagation`：不冒泡到选项卡栏空白区右键（新建/打开），也不触发媒体右键菜单。
    pub fn on_tab_right_click(
        &mut self,
        idx: usize,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let weak = self.host_weak();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                menu.item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭")
                            .debug_selector(|| "tab-menu-close".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_close_tab(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭其他")
                            .debug_selector(|| "tab-menu-close-others".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_close_others(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭右侧")
                            .debug_selector(|| "tab-menu-close-right".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_close_right(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭已保存")
                            .debug_selector(|| "tab-menu-close-saved".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_close_saved(window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭全部")
                            .debug_selector(|| "tab-menu-close-all".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_close_all(window, cx);
                        });
                    })
                })
                .separator()
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("移入新窗口")
                            .debug_selector(|| "tab-menu-move-new-window".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_move_to_new_window(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("复制到新窗口")
                            .debug_selector(|| "tab-menu-copy-new-window".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tabs_host_copy_to_new_window(idx, window, cx);
                        });
                    })
                })
            },
            |this, emitter, cx| {
                // 关闭（选择/Esc/点击外）→ 宿主身份校验 + 清理槽位。
                let host = this.host_weak();
                if let Some(h) = host.upgrade() {
                    let _ = h.update(cx, |app, cx| {
                        app.tabs_host_dismiss_tab_bar_menu(emitter, cx);
                    });
                }
            },
        );
        let host = self.host_weak();
        if let Some(h) = host.upgrade() {
            let _ = h.update(cx, |app, cx| {
                app.tabs_host_open_tab_bar_menu(ev.position, menu, sub, cx);
            });
        }
    }
}
