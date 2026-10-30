//! 三份查看器（image / video / pdf）共用的「Ctrl+滚轮缩放 + 拖拽平移 +
//! contain 适配 / clamp_pan」controller。
//!
//! 设计原则：
//! - **逐字节相同的「交互数学」收敛到本模块**，一家实现、三家调用。三家原实现里
//!   连公式带阈值（`delta_y.clamp(-1,1)`、`1e-4` 无变化短路、`min(vw/iw, vh/ih).max(0.001)`
//!   contain、`±(图−视口)/2` clamp）逐字一致的部分全部归到此处；
//! - **各家差异留在各自 viewer**：缩放档位区间（image `0.2..8`、video `0.25..8`、pdf
//!   `0.5..4` 来自 `config::pdf`）、坐标口径（image/video 是像素、pdf 是分页列布局，
//!   无 contain/clamp_pan/拖拽）、缩放副作用（重新锚定视口 / 升档重解 / 仅 notify），
//!   一律以**参数与调用方逻辑**承载，绝不统一成某一家行为；
//! - 所有函数都是无副作用的纯函数（只在调用方就地改写字段），便于单测与行为逐字节保真。
//!
//! pdf 只复用本模块的「缩放档位与钳位」部分（分页列布局没有单张图的 contain/clamp/拖拽）；
//! image / video 完整复用全部。

use gpui::{ScrollDelta, ScrollWheelEvent};

/// 滚轮纵轴增量：`Lines` → 格数 / `Pixels` → 像素（三查看器原实现逐字节一致的 match）。
pub fn wheel_delta_y(event: &ScrollWheelEvent) -> f32 {
    match event.delta {
        ScrollDelta::Lines(v) => v.y,
        ScrollDelta::Pixels(v) => f32::from(v.y),
    }
}

/// Ctrl+滚轮缩放因子：放大一档 = `1 + step`；缩小一档 = **精确逆比** `1/(1+step)`。
///
/// ⚠️ 旧实现用对称分数 `1 ± step`（放大 `1+s`、缩小 `1-s`）导致往返 `(1+s)(1-s)=1-s²`
/// 每轮净亏 `s²`（step=0.12 → 每放大再缩小一次页宽 ×0.9856、越滚越小）——用户可见的
/// 「来回缩放页面逐渐缩水、回不到 100%」根源。逆比对称保证放大 N 档再缩小 N 档严格回到
/// 原值（f32 仅 ~1ulp），三家查看器（image/video/pdf）共用本函数一改齐治。
pub fn zoom_factor(delta_y: f32, step: f32) -> f32 {
    let d = delta_y.clamp(-1.0, 1.0);
    if d >= 0.0 {
        1.0 + d * step
    } else {
        1.0 / (1.0 + d.abs() * step)
    }
}

/// 应用缩放并钳位到 `[min, max]`；变化量 < 1e-4 视为无变化（返回 `false`，调用方便可
/// 跳过副作用 / `notify`）。`zoom` 就地更新，返回是否实际变化。
pub fn apply_zoom(zoom: &mut f32, factor: f32, min: f32, max: f32) -> bool {
    let new_zoom = (*zoom * factor).clamp(min, max);
    if (new_zoom - *zoom).abs() < 1e-4 {
        return false;
    }
    *zoom = new_zoom;
    true
}

/// 适应视口（Contain，**贴边无留白**）：取较小缩放比 `min(vw/iw, vh/ih)`（下限 0.001
/// 防除零），一轴正好贴边、另一轴居中留白，保持原始比例。
pub fn contain_fit(iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
    let fit = (vw / iw).min(vh / ih).max(0.001);
    (iw * fit, ih * fit)
}

/// 平移 clamp：放大后（`dw > vw` 或 `dh > vh`）允许在图内滑动，范围 = ±(图 − 视口)/2；
/// 未放大时范围 0（恒居中，拖拽无效果）。以「视口中心 = 图中心」为 0 基准，
/// 缩放/切图后范围变化由 render 逐帧重新 clamp 收敛。
pub fn clamp_pan(pan: (f32, f32), vw: f32, vh: f32, dw: f32, dh: f32) -> (f32, f32) {
    let ax = (dw - vw).max(0.0) * 0.5;
    let ay = (dh - vh).max(0.0) * 0.5;
    (pan.0.clamp(-ax, ax), pan.1.clamp(-ay, ay))
}

/// 拖拽平移会话状态：`(左键按下位置, 按下时 pan 起点)`。image / video 两家同款布局。
pub type PanDrag = ((f32, f32), (f32, f32));

/// 开始拖拽：记录按下位置与按下时 pan 起点（未放大时 clamp 恒 0，拖动无效果）。
pub fn pan_drag_begin(pos: (f32, f32), pan: (f32, f32)) -> PanDrag {
    (pos, pan)
}

/// 拖拽增量 = 当前鼠标位置 − 按下位置。
pub fn pan_drag_delta(drag: &PanDrag, pos: (f32, f32)) -> (f32, f32) {
    (pos.0 - drag.0.0, pos.1 - drag.0.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 共享缩放档位：放大一档 = `1+step`、缩小一档 = `1/(1+step)`（精确逆比，往返闭合）；
    /// 钳位区间来自调用方参数，三家各不相同。
    #[test]
    fn zoom_step_and_clamp_are_parameterized() {
        let near = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(near(zoom_factor(1.0, 0.12), 1.12));
        assert!(near(zoom_factor(-1.0, 0.12), 1.0 / 1.12), "缩小=放大精确逆比");
        // 往返精确闭合：放大 1 档再缩小 1 档回到 1.0（旧 `(1+s)(1-s)=1-s²≈0.9856`
        // 每轮缩水，用户「来回缩放页面越滚越小、回不到 100%」的根源）
        assert!(near(zoom_factor(1.0, 0.12) * zoom_factor(-1.0, 0.12), 1.0), "往返闭合");
        // 分数 delta 同样闭合（连续缩放不累积漂移）
        assert!(near(zoom_factor(0.5, 0.12) * zoom_factor(-0.5, 0.12), 1.0), "分数档闭合");
        // 大于 1 格的 delta 也归一成一档（三家原实现 `clamp(-1,1)` 一致）
        assert!(near(zoom_factor(4.0, 0.12), 1.12));

        // 三家档位区间各不相同：只取决于调用方传入的 min/max；用真实因子（含逆比缩小）
        let cases: [(f32, f32); 3] = [(0.2, 8.0), (0.25, 8.0), (0.5, 4.0)];
        for (min, max) in cases {
            let mut z = 1.0;
            for _ in 0..200 {
                apply_zoom(&mut z, zoom_factor(1.0, 0.12), min, max);
            }
            assert_eq!(z, max, "升到上限 {max}");
            for _ in 0..200 {
                apply_zoom(&mut z, zoom_factor(-1.0, 0.12), min, max);
            }
            assert_eq!(z, min, "降到下限 {min}");
            // 50 次「放大一档+缩小一档」往返后严格回到起点（无系统性缩水）
            let mut z2 = 1.0;
            for _ in 0..50 {
                apply_zoom(&mut z2, zoom_factor(1.0, 0.12), min, max);
                apply_zoom(&mut z2, zoom_factor(-1.0, 0.12), min, max);
            }
            assert!((z2 - 1.0).abs() < 1e-3, "50 次往返不漂移：{z2}");
            // 越界因子（delta=0 情形）不产生变化 → 调用方据此跳过 notify
            assert!(!apply_zoom(&mut z, 1.0, min, max), "delta=0 不应变化");
        }
    }

    /// contain 适配：贴边无留白 + 保持原比例（image/video 共用的几何）。
    #[test]
    fn contain_fit_keeps_aspect_and_touches_one_axis() {
        let (dw, dh) = contain_fit(720.0, 1280.0, 1200.0, 548.0);
        assert!((dw / dh - 720.0 / 1280.0).abs() < 1e-3, "保持竖屏比例");
        assert!((dh - 548.0).abs() < 0.001, "高度贴边 dh={dh}");
        assert!(dw < 1200.0, "宽度留白 dw={dw}");
    }

    /// clamp 与拖拽增量：未放大恒 0；放大后 ±(图−视口)/2 各轴独立。
    #[test]
    fn clamp_pan_and_drag_delta() {
        let cp = clamp_pan;
        assert_eq!(cp((50.0, -30.0), 1000.0, 700.0, 500.0, 300.0), (0.0, 0.0));
        assert_eq!(cp((1000.0, 999.0), 1000.0, 700.0, 1500.0, 700.0), (250.0, 0.0));
        assert_eq!(cp((-1000.0, 200.0), 1000.0, 700.0, 1500.0, 900.0), (-250.0, 100.0));

        let drag = pan_drag_begin((10.0, 20.0), (5.0, -5.0));
        assert_eq!(pan_drag_delta(&drag, (130.0, 100.0)), (120.0, 80.0));
    }
}
