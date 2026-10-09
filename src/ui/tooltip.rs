//! 悬停说明浮层（tooltip）：VSCode 手感的灰底小卡片，出现在图标/按钮右侧。
//!
//! # 使用
//! 调用点（如活动栏 / 侧栏图标按钮）在 render 期：
//! 1. `tooltip_state(id, window, cx)` 取/建按按钮 id 键控的跨帧状态；
//! 2. 在按钮的 `on_hover` 里调 [`tooltip_hover`]（进入排队延迟、移出即隐藏）；
//! 3. 把 [`tooltip_layer`] 作为按钮的（相对定位）直子挂上，随 `visible` 现隐。
//!
//! # 出没行为
//! 悬停进入先排队 [`TOOLTIP_DELAY_MS`]（200ms）延迟，防止扫过按钮时闪现；
//! 到点仍悬停才真正显示（淡入由 `with_spring` 承担，~120ms 临界阻尼，
//! 自动尊重 `reduce_motion`）；移出立即隐藏并作废在途定时器。
//!
//! # 合成层纪律（对齐 `ui::menu::Dropdown`）
//! 浮层一律 `deferred(anchored(Local))` 独立绘制层、锚在宿主右缘垂直居中，
//! `snap_to_window_with_margin` 兜底防越窗；卡片**无 id / 无事件处理** →
//! 无 hitbox → 不拦截点击、不吸走焦点、命中穿透到下层面板。
//!
//! # 清理策略
//! 状态用 `window.use_keyed_state` 按元素 id 键控：宿主移出元素树（侧栏收起 /
//! 按钮消失）即随元素状态回收；延迟定时器持**弱引用**，且用代序号 `seq`
//! 作废在途任务（移出即 +1），实体释放后任务更新自动空操作，不悬挂、不泄露。

use std::time::Duration;

use gpui::{
    Anchor, AnimationExt, AnyElement, App, Edges, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Styled, anchored, deferred, div, px,
};

use crate::ui::motion::hover_spring;
use crate::ui::theme::Theme;

/// tooltip 出现的悬停防抖（ms）：低于该时长的扫过不触发（初始延迟）。
pub(crate) const TOOLTIP_DELAY_MS: u64 = 200;
/// 浮层相对宿主右缘的水平间隙（px）。
const TIP_GAP: f32 = 4.0;
/// 浮层次序：高于普通 deferred（note card = 1），低于菜单（100）/ toast（200）。
const TIP_LAYER_PRIORITY: usize = 60;

/// 单个 tooltip 的跨帧状态（按宿主元素 id 键控，宿主消失即随元素状态回收）。
#[derive(Default)]
pub(crate) struct TooltipState {
    /// 是否已显示浮层（延迟到期且仍悬停才置 true）。
    visible: bool,
    /// 当前是否悬停在宿主上。
    hanging: bool,
    /// 代序号：悬停进入/移出都 +1，作废在途延迟定时器（防旧定时器误显）。
    seq: u32,
}

/// 在 render 期取/建宿主的 tooltip 状态（按 `id` 键控，幂等）。
pub(crate) fn tooltip_state(id: &str, window: &mut gpui::Window, cx: &mut App) -> Entity<TooltipState> {
    window.use_keyed_state(format!("{id}-tooltip"), cx, |_, _| {
        TooltipState::default()
    })
}

/// 悬停进入/移出统一入口（挂在宿主 `on_hover` 闭包内）：
/// 进入 → 排队延迟显示；移出 → 立即隐藏并作废在途定时器。
pub(crate) fn tooltip_hover(state: &Entity<TooltipState>, hovered: bool, cx: &mut App) {
    if hovered {
        tooltip_enter(state, cx);
    } else {
        tooltip_exit(state, cx);
    }
}

/// 悬停进入：登记悬停并排一单延迟显示；已悬停中不重复排队。
fn tooltip_enter(state: &Entity<TooltipState>, cx: &mut App) {
    let seq = state.update(cx, |s, _| {
        if s.hanging {
            return None;
        }
        s.hanging = true;
        s.seq = s.seq.wrapping_add(1);
        Some(s.seq)
    });
    let Some(seq) = seq else { return };
    let weak = state.downgrade();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(TOOLTIP_DELAY_MS))
            .await;
        // 到点仍悬停且代次未变才真正显示；移出已清 `hanging` / 代次 +1 → 自动作废。
        cx.update(|app| {
            let _ = weak.update(app, |s, cx| {
                if s.seq == seq && s.hanging {
                    s.visible = true;
                    cx.notify();
                }
            });
        });
    })
    .detach();
}

/// 悬停移出：立即隐藏并作废在途延迟定时器。
fn tooltip_exit(state: &Entity<TooltipState>, cx: &mut App) {
    state.update(cx, |s, _| {
        s.hanging = false;
        s.visible = false;
        s.seq = s.seq.wrapping_add(1);
    });
}

/// 当前是否显示浮层（render 期读取）。
pub(crate) fn tooltip_visible(state: &Entity<TooltipState>, cx: &App) -> bool {
    state.read(cx).visible
}

/// 把 tooltip 浮层作为宿主的相对定位直子挂上（合成层纪律：`deferred(anchored(Local))`，
/// 同 `ui::menu::Dropdown`）。锚在宿主右缘、垂直居中，`snap_to_window_with_margin`
/// 兜底防越窗。`show` 为假返回空元素（不移挂）；淡入由 `with_spring` 承担。
///
/// `host_w / host_h`：宿主（按钮）尺寸，用于「贴右缘水平居中」的偏移换算
/// （活动栏按钮 40×38、PDF 导航按钮 28×24 等）。
pub(crate) fn tooltip_layer(
    id: &str,
    text: &str,
    show: bool,
    host_w: f32,
    host_h: f32,
    cx: &App,
) -> AnyElement {
    if !show {
        return div().into_any_element();
    }
    let card = tooltip_card(id, text, cx);
    deferred(
        anchored()
            .position_mode(gpui::AnchoredPositionMode::Local)
            .anchor(Anchor::LeftCenter)
            .offset(gpui::point(px(host_w + TIP_GAP), px(host_h * 0.5)))
            .snap_to_window_with_margin(Edges::all(px(8.)))
            .child(
                card.with_spring(
                    format!("tooltip-{id}-fade"),
                    hover_spring(1.0).from(0.0),
                    |this, v| this.opacity(v),
                ),
            ),
    )
    .with_priority(TIP_LAYER_PRIORITY)
    .into_any_element()
}

/// 灰色说明卡片本体：VSCode 式灰底、细边框 + 弱阴影、8px 圆角、12px muted 文字。
/// 无 id / 无事件处理 → 无 hitbox → 不拦截点击、不抢焦点（命中穿透）。
fn tooltip_card(id: &str, text: &str, cx: &App) -> gpui::Div {
    let theme = Theme::global(cx);
    let colors = theme.colors;
    let bg: Hsla = colors.popover_bg.into();
    let border: Hsla = colors.border.into();
    let fg: Hsla = colors.fg_muted.into();
    let card_sel = format!("tooltip-{id}");
    div()
        .debug_selector(move || card_sel.clone())
        .bg(bg)
        .border_1()
        .border_color(border)
        .rounded_lg()
        // 弱阴影（比菜单双层更轻）：贴图标右侧悬停卡，单层柔和即可。
        .shadow(vec![
            gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.14))
                .blur_radius(px(6.)),
        ])
        .px_2()
        .py_1()
        .whitespace_nowrap()
        .text_size(px(12.))
        .text_color(fg)
        .child(text.to_string())
}
