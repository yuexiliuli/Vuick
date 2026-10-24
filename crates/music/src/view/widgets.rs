//! 各子视图复用的纯视觉绘制件：均衡器小柱动画、黑胶圈旋转、圆形主按钮。
//!
//! 均**自含于本 crate**：GPUI 无 CSS `animation`/`transform` 于 `div`，均衡器用
//! [`AnimationExt::with_animation`]（`Repeat` 循环）驱动小柱高度变化 + 相位错开；
//! 黑胶圈用 `svg().with_transformation(Transformation::rotate(..))` 作圆环旋转
//! （裸 `div` 无 transform 支持）。`playing=false` 时返回静态形态（挂上即停）。

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Hsla, IntoElement, ParentElement as _, Styled as _,
    Transformation, div, px, radians, svg,
};

/// 均衡器小柱组（当前播放行「♪」指示）：四根小柱以不同高度基线 + 相位跳动，
/// 三角波 easing 让高度在基线 ↔ 峰值间往返（近似原型 `eq-bounce` scaleY）。
/// `prefix` 用于区分同一屏内的多组 eq（元素动画 id 须全局唯一）。
pub fn eq_bars(playing: bool, accent: Hsla, prefix: String) -> AnyElement {
    // (基线高, 相位偏移)；相位用 easing 折叠：同一 animator 输出错开的高度。
    let specs = [(6.0f32, 0.00f32), (10.0, 0.30), (4.0, 0.55), (8.0, 0.15)];
    div()
        .flex()
        .items_end()
        .gap_x(px(2.))
        .justify_center()
        .h(px(14.))
        .children(specs.into_iter().enumerate().map(move |(i, (base, phase))| {
            let bar = div().w(px(3.)).rounded(px(1.)).bg(accent);
            let bar: AnyElement = if playing {
                bar.with_animation(
                    format!("{prefix}-eq-{i}"),
                    Animation::new(Duration::from_millis(650)).repeat().with_easing({
                        let p = phase;
                        move |x| {
                            // 三角波 0..1：先升后降，相位错开让四柱像「跳动」而非同起同落。
                            let t = (x + p).fract();
                            let wave = if t < 0.5 { t * 2.0 } else { (1.0 - t) * 2.0 };
                            0.35 + 0.65 * wave
                        }
                    }),
                    move |this, e| this.h(px(base * (0.45 + 0.55 * e))),
                )
                .into_any_element()
            } else {
                bar.h(px(6.)).into_any_element()
            };
            bar
        }))
        .into_any_element()
}

/// 黑胶圈：给定直径的圆环 + 顶端缺口点（让旋转可见）。`playing` 时绕中心匀速旋转
/// （`svg().with_transformation`，`div` 无 transform；`prefix` 保证动画 id 唯一）。
/// 默认 8s 一圈顺时针（legacy 姿势）。
pub fn vinyl_ring(diameter: f32, playing: bool, prefix: String) -> AnyElement {
    vinyl_ring_at(diameter, playing, prefix, Duration::from_secs(8), false)
}

/// 黑胶圈（可配周期/转向）：全屏 v2 封面三层圈用不同周期 + 反向旋转增强层次感。
pub fn vinyl_ring_at(
    diameter: f32,
    playing: bool,
    prefix: String,
    period: Duration,
    reverse: bool,
) -> AnyElement {
    let mark = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"
  fill="none" stroke="currentColor" stroke-width="0.7" stroke-linecap="round">
  <circle cx="12" cy="12" r="11"/>
  <circle cx="12" cy="1.4" r="0.8" fill="currentColor" stroke="currentColor"/>
</svg>"#
    );
    let ring = svg()
        .w(px(diameter))
        .h(px(diameter))
        .path(mark)
        .text_color(gpui::white().opacity(0.06));
    if playing {
        ring.with_animation(prefix, Animation::new(period).repeat(), move |this, d| {
            let deg = if reverse { -d * 360.0 } else { d * 360.0 };
            this.with_transformation(Transformation::rotate(radians(deg)))
        })
        .into_any_element()
    } else {
        ring.with_transformation(Transformation::rotate(radians(0.0)))
            .into_any_element()
    }
}
