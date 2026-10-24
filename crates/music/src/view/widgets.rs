//! 各子视图复用的纯视觉绘制件：均衡器小柱动画、黑胶圈旋转、圆形主按钮。
//!
//! 均**自含于本 crate**：GPUI 无 CSS `animation`/`transform` 于 `div`，均衡器用
//! [`AnimationExt::with_animation`]（`Repeat` 循环）驱动小柱高度变化 + 相位错开；
//! 黑胶圈用 `svg().with_transformation(Transformation::rotate(..))` 作圆环旋转
//! （裸 `div` 无 transform 支持）。`playing=false` 时返回静态形态（挂上即停）。
//!
//! **播放中动画降频**（性能优化第一步）：`Repeat` 动画默认每帧 `request_animation_frame()` →
//! 每动画帧重绘整棵 MusicView 树（侧栏+主列表+右栏+底栏+全屏），播放中 ≈60fps 整树重建常驻
//! =「全局迟缓」。故循环动画统一 `repeat_synced().with_max_fps(15.)`：共享时钟协同各元素相位，
//! `max_fps` 走 timer 驱动 notify 而非每帧，播放中整树重建降到 ≤15fps。`reduce_motion` 由 gpui
//! 自动尊重（渲染静态起态、不再请求帧）。

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Entity, Hsla, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Transformation, Window, div, px, radians, svg,
};

use crate::player::MusicPlayer;

use super::theme;

/// 均衡器小柱组（当前播放行「♪」指示）：四根小柱以不同高度基线 + 相位跳动，
/// 三角波 easing 让高度在基线 ↔ 峰值间往返（近似原型 `eq-bounce` scaleY）。
/// `prefix` 用于区分同一屏内的多组 eq（元素动画 id 须全局唯一）。
/// 循环动画节流到 15fps（`repeat_synced` + `with_max_fps`——本仓库性能优化第一步，防
/// 播放中 ~60fps 整树重建；`reduce_motion` 由 gpui 尊重）。
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
                    Animation::new(Duration::from_millis(650))
                        .repeat_synced()
                        .with_max_fps(15.)
                        .with_easing({
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
/// 已被 [`VinylView`] 子视图取代（dormancy：保留公共绘制件，调用方全走子视图动画）。
#[allow(dead_code)]
pub fn vinyl_ring(diameter: f32, playing: bool, prefix: String) -> AnyElement {
    vinyl_ring_at(diameter, playing, prefix, Duration::from_secs(8), false)
}

/// 黑胶圈（可配周期/转向）：全屏 v2 封面三层圈用不同周期 + 反向旋转增强层次感。
/// 循环动画节流到 15fps（`repeat_synced` + `with_max_fps`，同上性能优化；`reduce_motion`
/// 由 gpui 尊重）。
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
        ring.with_animation(
            prefix,
            Animation::new(period).repeat_synced().with_max_fps(15.),
            move |this, d| {
                let deg = if reverse { -d * 360.0 } else { d * 360.0 };
                this.with_transformation(Transformation::rotate(radians(deg)))
            },
        )
        .into_any_element()
    } else {
        ring.with_transformation(Transformation::rotate(radians(0.0)))
            .into_any_element()
    }
}

// ---- 高频重绘热区 → 纯展示子视图（性能优化第三步 A 段） ----
//
// GPUI 动画的 notify 目标 = 包含动画元素的 `window.current_view()`（见 gpui
// `AnimationElement::request_layout` 的 `with_max_fps` 分支：`let view =
// window.current_view()` → 到点 `cx.notify(view)`）。旧代码动画在父 MusicView 的 render
// 里构建 → current_view = 整棵 MusicView → 动画每帧 notify 整树（播放中 15fps×42ms 整树
// 重建）。把动画元素放进下面的子视图后，current_view = 子视图 → 动画每帧只重绘本视图，
// 父树不再被动画帧带动整树 notify。`repeat_synced` 走 App 级共享时钟，跨多个子视图实例
// 的相位仍协同（不破坏既有视觉）。

/// 均衡器小柱动画纯展示子视图：自身 render 构建 `eq_bars` 动画元素，无事件回调、
/// 只读播放器态（`playing` 直接从 `player` 读，不做回写）；动画 notify 只落在本视图。
/// `prefix` 须全局唯一（动画元素 id 全局唯一；主区当前行与队列当前项分设实例互不干扰）。
pub struct EqBarsView {
    player: Entity<MusicPlayer>,
    prefix: SharedString,
}

impl EqBarsView {
    pub(crate) fn new(player: Entity<MusicPlayer>, prefix: &str) -> Self {
        Self {
            player,
            prefix: prefix.into(),
        }
    }
}

impl Render for EqBarsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing = self.player.read(cx).playing();
        eq_bars(playing, theme::accent(), self.prefix.to_string())
    }
}

/// 黑胶圈动画纯展示子视图：可配多圈（直径比例/周期/转向/前缀），渲染为 right.rs 与
/// fullscreen.rs 旧实现同款的「absolute 满布 flex 容器、中心对齐多圈」；无事件回调、
/// 只读播放器态；动画 notify 只落在本视图（多圈同容器 → 一次 notify 即全圈）。
pub struct VinylView {
    player: Entity<MusicPlayer>,
    /// 封面基准边长（px）：各圈直径 = `base × 比例`。
    base: f32,
    /// (直径比例, 周期, 反向, 前缀)；prefix 须全局唯一。
    rings: Vec<(f32, Duration, bool, SharedString)>,
}

impl VinylView {
    pub(crate) fn new(player: Entity<MusicPlayer>, base: f32) -> Self {
        Self {
            player,
            base,
            rings: Vec::new(),
        }
    }

    /// 追加一圈（按序叠放；prefix 全局唯一，防动画元素 id 冲突）。
    pub(crate) fn ring(
        mut self,
        frac: f32,
        period: Duration,
        reverse: bool,
        prefix: &str,
    ) -> Self {
        self.rings.push((frac, period, reverse, prefix.into()));
        self
    }
}

impl Render for VinylView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing = self.player.read(cx).playing();
        let base = self.base;
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .children(self.rings.iter().map(move |(frac, period, reverse, prefix)| {
                vinyl_ring_at(base * frac, playing, prefix.to_string(), *period, *reverse)
            }))
    }
}
