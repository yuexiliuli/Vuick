//! 音乐播放器主界面 Windows 风格标题栏：左侧 logo + 标题，右侧窗口控制三钮。
//!
//! 三钮走 `gpui::WindowControlArea`（Min/Max/Close）——与 shell 标题栏
//! （`crates/shell/src/app/chrome.rs`）同款**原生**机制：GPUI 在 WM_NCHITTEST 返回
//! HTMINBUTTON / HTMAXBUTTON / HTCLOSE，由 Windows 系统真实完成最小化 / 最大化（还原）/
//! 关闭（含双击/键盘语义），是仓库已验证的「真实作用于应用窗口」姿势（K17 口径：平台
//! 语义由系统处理；`window.is_maximized()` 做图标/标题切换，对齐 shell `chrome.rs:419`）。
//!
//! 空白/标题区标 `WindowControlArea::Drag`（HTCAPTION）→ 原生拖动窗口 + **双击切换
//! 最大化/还原**（对应原型 v4「双击标题栏空白切换」，且点按钮不触发——按钮是独立
//! non-Drag 命中区；GPUI 对 `window_control_hitboxes` 按插入序取首个命中，父容器不得
//! 兼作 Drag 覆盖子按钮，故拖动区与按钮必须是互不重叠的兄弟元素，见 window.rs 2054）。
//!
//! 本 crate 自含深紫暗色主题（不依赖 ui），窗口控制钮取色逐字对齐 shell
//! `ui::theme::ChromeColors`（close_hover #e81123 / close_pressed #a70d1a /
//! hover_fill 8% 白 / pressed_fill 14% 白，见 [`super::theme`]）。

use gpui::{
    AnyElement, App, ElementId, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea,
    div, px,
};

use super::icon::{IconView, MusicIcon};
use super::theme;
use ui::icon::{Icon, IconName};

/// 标题栏高度（px）：原型 32 / shell 32，取 36（规格 36–40 下沿，容纳 46×36 三钮）。
pub const TITLE_BAR_H: f32 = 36.0;

/// 窗口控制钮类型：其 `window_control_area` 命中区即真实 OS 语义（动作不做应用层重复处理）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctl {
    Minimize,
    Maximize,
    Close,
}

impl Ctl {
    /// 元素 id：与 shell 窗口控制钮对齐（`title-minimize`/`title-maximize`/`title-close`）。
    fn id(self) -> &'static str {
        match self {
            Ctl::Minimize => "title-minimize",
            Ctl::Maximize => "title-maximize",
            Ctl::Close => "title-close",
        }
    }

    fn area(self) -> WindowControlArea {
        match self {
            Ctl::Minimize => WindowControlArea::Min,
            Ctl::Maximize => WindowControlArea::Max,
            Ctl::Close => WindowControlArea::Close,
        }
    }

    fn is_close(self) -> bool {
        matches!(self, Ctl::Close)
    }
}

/// 最大化/还原图标选择（纯函数，测试可双路注入断言）：窗口已最大化 →「向下还原」图标
/// （对齐 shell `chrome.rs:419` 的 `is_maximized` 分支）。
pub fn max_icon(is_maximized: bool) -> MusicIcon {
    if is_maximized {
        MusicIcon::Restore
    } else {
        MusicIcon::Maximize
    }
}

/// 单个窗口控制钮：46×36、hover/按压换底（关闭钮危险红、其余灰底）、图标按注入态换色。
/// **无 on_click**——最小化/最大化/关闭动作由原生 `window_control_area` 完成；这里仅维护
/// hover/按压 keyed 态并通知重绘（同 shell `title_ctrl_button` 姿态；headless 测试可经
/// simulate 命中验证渲染路径，真机动作由系统直通）。
fn ctrl_button(window: &mut Window, cx: &mut App, kind: Ctl) -> AnyElement {
    let view_id = window.current_view();
    let hover_key: ElementId = format!("music-title-{}-hover", kind.id()).into();
    let press_key: ElementId = format!("music-title-{}-press", kind.id()).into();
    let hovered_s = window.use_keyed_state(hover_key, cx, |_, _| false);
    let pressed_s = window.use_keyed_state(press_key, cx, |_, _| false);
    let hovered = *hovered_s.read(cx);
    let pressed = *pressed_s.read(cx);

    // 图标注入色：空闲次要灰 → hover/按压 主前景（关闭钮白 ×，红底白 ×，同 shell
    // `title_ctrl_fg`；注意 IconView/ui 显式 `text_color` 会覆盖父级级联，必须在分支里定）。
    let icon_color = if hovered || pressed {
        theme::text()
    } else {
        theme::text2()
    };
    // 图标组件：Minimize/Maximize/Restore 未迁（Slice2 排除项）仍走本地 IconView；Close 已迁
    // `ui::IconName::WindowClose`（23 项之一）。三钮同尺寸 12、注入色同源。
    let icon: AnyElement = match kind {
        Ctl::Minimize => IconView::new(MusicIcon::Minimize)
            .size(px(12.))
            .text_color(icon_color)
            .into_any_element(),
        Ctl::Maximize => IconView::new(max_icon(window.is_maximized()))
            .size(px(12.))
            .text_color(icon_color)
            .into_any_element(),
        Ctl::Close => Icon::new(IconName::WindowClose)
            .size(px(12.))
            .text_color(icon_color)
            .into_any_element(),
    };
    // 按压底优先于 hover 底（压住不放时保持按压填充，同 shell `active` 与 spring 叠加）。
    let bg = if pressed {
        if kind.is_close() {
            theme::close_pressed()
        } else {
            theme::ctrl_pressed_fill()
        }
    } else if hovered {
        if kind.is_close() {
            theme::close_hover()
        } else {
            theme::ctrl_hover_fill()
        }
    } else {
        gpui::transparent_black()
    };

    div()
        .id(kind.id())
        .debug_selector(move || kind.id().into())
        .w(px(46.))
        .h(px(TITLE_BAR_H))
        .flex()
        .items_center()
        .justify_center()
        // 原生标题钮默认箭头光标（不显式设手型）。
        .bg(bg)
        .window_control_area(kind.area())
        .child(icon)
        .on_hover({
            let st = hovered_s.clone();
            move |hovered, _w, cx| {
                st.update(cx, |h, _| *h = *hovered);
                cx.notify(view_id);
            }
        })
        .on_mouse_down(MouseButton::Left, {
            let st = pressed_s.clone();
            move |_ev, _w, cx| {
                st.update(cx, |p, _| *p = true);
                cx.notify(view_id);
            }
        })
        .on_mouse_up(MouseButton::Left, {
            let st = pressed_s.clone();
            move |_ev, _w, cx| {
                st.update(cx, |p, _| *p = false);
                cx.notify(view_id);
            }
        })
        .on_mouse_up_out(MouseButton::Left, {
            let st = pressed_s.clone();
            move |_ev, _w, cx| {
                st.update(cx, |p, _| *p = false);
                cx.notify(view_id);
            }
        })
        .into_any_element()
}

/// 主界面 Windows 风格标题栏（`MusicView::render` 顶层第一条）：左 logo+标题、中空白
/// 拖动区、右三钮。全屏 overlay（fs）由 render 末尾追加、absolute 覆盖整窗——标题栏在
/// fs 时不新增（与三栏/底栏一样被覆盖）。
pub fn render(window: &mut Window, cx: &mut App) -> AnyElement {
    // 左侧 title-bar-left：logo（accent→pink 渐变小方块 + 白 disc）+ 标题文字；整体即
    // HTCAPTION 拖动区（原型双击标题栏空白切换最大化，点三钮不触发——钮是独立命中区）。
    let left = div()
        .id("music-title-left")
        .debug_selector(|| "music-title-left".into())
        .h(px(TITLE_BAR_H))
        .flex()
        .items_center()
        .gap(px(8.))
        .pl_2()
        .window_control_area(WindowControlArea::Drag)
        .child(
            div()
                .size(px(16.))
                .relative()
                .rounded(px(3.))
                .overflow_hidden()
                .child(theme::gradient_cover((theme::accent(), theme::pink()), 3.0))
                .child(
                    Icon::new(IconName::Disc)
                        .size(px(10.))
                        .text_color(theme::text()),
                ),
        )
        .child(
            div()
                .id("music-title-text")
                .debug_selector(|| "music-title-text".into())
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme::text2())
                .child("Orbitrum Player"),
        );

    // 中间空白拖动区：flex_1 吸满剩余宽度，与左块同为兄弟级 Drag 命中区（不覆盖按钮）。
    let drag_spacer = div()
        .id("music-title-drag")
        .debug_selector(|| "music-title-drag".into())
        .flex_1()
        .h(px(TITLE_BAR_H))
        .window_control_area(WindowControlArea::Drag);

    // 右侧窗口控制三钮（最大化为 最大化/还原 双态图标，随 `window.is_maximized()` 切换）。
    let mut buttons = div().flex().items_center();
    buttons = buttons
        .child(ctrl_button(window, cx, Ctl::Minimize))
        .child(ctrl_button(window, cx, Ctl::Maximize))
        .child(ctrl_button(window, cx, Ctl::Close));

    div()
        .id("music-title-bar")
        .debug_selector(|| "music-title-bar".into())
        .h(px(TITLE_BAR_H))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        // 原型 `.title-bar { background: var(--bg-primary); border-bottom: 1px solid var(--border) }`。
        .bg(theme::bg_primary())
        .border_b_1()
        .border_color(theme::border())
        .child(left)
        .child(drag_spacer)
        .child(buttons)
        .into_any_element()
}
