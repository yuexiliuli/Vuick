//! 动效：全套 hover 淡入/淡出与布局宽度过渡的统一弹簧参数与助手（P2-10）。
//!
//! 全站 hover 背景共用一个临界阻尼弹簧（无过冲——欠阻尼会让 alpha 冲过 1
//! 视觉发白），收敛约 60-75ms；需要动的地方用 [`hover_spring`] 包
//! `with_spring`，不要在调用点自行造参数。`reduce_motion` 由 gpui
//! `with_spring` 自动尊重（直接落端点、不调度帧）。
//!
//! [`hoverable`] 把「`use_keyed_state(..-hover..)` + `on_hover{set+notify(view_id)}` +
//! `hover_bg_fade`」三段式样板收敛为一层接线助手（sidebar 目录/搜索/文件树等列表行、
//! 按钮等 ~20 处同构骨架；shell 轮统一迁移调用点，本轮只新增助手）。

use gpui::{
    AnimationExt, App, ElementId, Hsla, IntoElement, SpringAnimation, SpringAnimationElement,
    SpringConfig, StatefulInteractiveElement, Styled, Window,
};

/// hover 淡入刚度 k：omega≈89 rad/s，临界阻尼下 1% 以内约 75ms
///（P4-2 复盘：原 3200 ≈ 120ms 收敛，侧边栏列表 hover 感知迟缓，提速 2.5×）。
pub(crate) const HOVER_STIFFNESS: f32 = 8000.0;
/// 0..1 透明度收敛容差（≈0.2%，肉眼不可辨）。
pub(crate) const HOVER_EPSILON: f32 = 0.002;

/// 0..1 hover 透明度弹簧（临界阻尼：damping=2√k，mass=1，无过冲）。
pub fn hover_spring(target: f32) -> SpringAnimation<f32> {
    SpringAnimation::new(SpringConfig::new(
        HOVER_STIFFNESS,
        2.0 * HOVER_STIFFNESS.sqrt(),
        1.0,
    ))
    .to(target)
    .with_epsilon(HOVER_EPSILON)
}

/// 给元素包一层「hover 背景淡入」：悬停时把背景从透明淡入到 `fill`、
/// 移出淡出。`fill` 自带目标 alpha（`opacity` 是乘性，v=1 时即为 `fill` 原色，
/// 视觉语义不变），只动过渡时序。
pub fn hover_bg_fade<E>(
    el: E,
    id: impl Into<ElementId>,
    fill: Hsla,
    hovered: bool,
) -> SpringAnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    el.with_spring(
        id,
        hover_spring(if hovered { 1.0 } else { 0.0 }),
        move |this, v| this.bg(fill.opacity(v)),
    )
}

/// 把 [`ElementId`] 压成可读标签串（生成兄弟 / 派生 id 用：hover 状态键、弹簧键等）。
///
/// `Name` / `NamedInteger` 取回原始串，其余变体退回 `Debug` 表示（只求唯一可读，
/// 不追求还原）。
pub(crate) fn element_id_label(id: &ElementId) -> String {
    match id {
        ElementId::Name(name) => name.to_string(),
        ElementId::NamedInteger(name, n) => format!("{name}-{n}"),
        other => format!("{other:?}"),
    }
}

/// 悬停接线助手：一轮收敛 shell 内 ~20 处「`use_keyed_state(..-hover..)` +
/// `on_hover{set+notify(view_id)}` + `hover_bg_fade`」三段式样板。
///
/// - `id`：调用方元素的键控 id（列表行 / 按钮等；`el` 应为带该 id 的 stateful 元素，
///   GPUI 命中测试需要元素有 id 才能触发 `on_hover`）；
/// - `fill`：悬停背景 target 色（自身 alpha 保留，透明度由弹簧 0..1 调制，语义同
///   [`hover_bg_fade`]）；
/// - `window` / `cx`：渲染期上下文（读/写 keyed 悬停态、`notify` 宿主视图）。
///
/// 内部悬停状态键与弹簧键由 `id` 派生（`{label}-hover` / `{label}-hover-bg`，
/// [`element_id_label`]），避免与元素自身 id 撞键。返回弹簧包装后的元素：
/// 悬停 `true` 时背景淡入 `fill`、移出淡出，收敛曲线与全站统一（[`hover_spring`]），
/// `reduce_motion` 自动尊重。
pub fn hoverable<E>(
    el: E,
    id: impl Into<ElementId>,
    fill: Hsla,
    window: &mut Window,
    cx: &mut App,
) -> SpringAnimationElement<E>
where
    E: IntoElement + StatefulInteractiveElement + Styled + 'static,
{
    let id = id.into();
    let label = element_id_label(&id);
    let hover_key = ElementId::from(format!("{label}-hover"));
    let spring_key = ElementId::from(format!("{label}-hover-bg"));
    let hover = window.use_keyed_state(hover_key, cx, |_, _| false);
    let hovered = *hover.read(cx);
    let view_id = window.current_view();
    let st = hover.clone();
    el.on_hover(move |h, _window, cx| {
        st.update(cx, |v, _| *v = *h);
        cx.notify(view_id);
    })
    .with_spring(
        spring_key,
        hover_spring(if hovered { 1.0 } else { 0.0 }),
        move |this, v| this.bg(fill.opacity(v)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, InteractiveElement, Modifiers, Render, TestAppContext, Styled, Window, div, point,
        px,
    };

    /// 悬停接线测试宿主：用共享单元格计数宿主 render 次数——`hoverable` 的
    /// `on_hover → notify(view_id)` 会把每次 hover 切换反映成一次宿主重绘。
    struct Harness {
        render_count: std::rc::Rc<std::cell::Cell<usize>>,
        fill: Hsla,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.render_count.set(self.render_count.get() + 1);
            let fill = self.fill;
            hoverable(
                div()
                    .id("hover-target")
                    .absolute()
                    .left(px(60.))
                    .top_0()
                    .size(px(40.)),
                "hover-target",
                fill,
                window,
                cx,
            )
        }
    }

    /// hover 切换：模拟鼠标移入目标中心 → 宿主重绘一次（hover=true）；移出 → 再重绘
    /// 一次（hover=false）。两段都驱动（enter + leave 都接线），且整链路无 panic。
    #[gpui::test]
    fn hover_toggles_notify_host_on_enter_and_leave(cx: &mut TestAppContext) {
        let render_count = std::rc::Rc::new(std::cell::Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let render_count = render_count.clone();
            move |_, _| Harness {
                render_count,
                fill: gpui::hsla(0.60, 0.60, 0.60, 1.0),
            }
        });
        // 目标在 (60..100, 0..40)；初始指针在 (0,0)（窗口左上）在目标外。
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let base = render_count.get();

        // 移入目标中心 (80,20) → hover 过渡在下一拍（重绘推进 `window.defer` 的监听）
        // 触发 on_hover(true) → notify 宿主。
        cx.simulate_mouse_move(point(px(80.), px(20.)), None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            render_count.get() > base,
            "悬停进入应触发宿主重绘（on_hover + notify），base={base} now={}",
            render_count.get()
        );
        let after_enter = render_count.get();

        // 移出到目标外 (200,200)：指针下无元素，悬停过渡靠下一拍重绘（paint 期的
        // defer 监听）触发 on_hover(false) → notify 宿主。
        cx.simulate_mouse_move(point(px(200.), px(200.)), None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            render_count.get() > after_enter,
            "悬停移出应再次触发宿主重绘，after_enter={after_enter} now={}",
            render_count.get()
        );
    }
}
