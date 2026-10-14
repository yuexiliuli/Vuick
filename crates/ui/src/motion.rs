//! 动效：全套 hover 淡入/淡出与布局宽度过渡的统一弹簧参数与助手（P2-10）。
//!
//! 全站 hover 背景共用一个临界阻尼弹簧（无过冲——欠阻尼会让 alpha 冲过 1
//! 视觉发白），收敛约 60-75ms；需要动的地方用 [`hover_spring`] 包
//! `with_spring`，不要在调用点自行造参数。`reduce_motion` 由 gpui
//! `with_spring` 自动尊重（直接落端点、不调度帧）。

use gpui::{
    AnimationExt, ElementId, Hsla, IntoElement, SpringAnimation, SpringAnimationElement,
    SpringConfig, Styled,
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
