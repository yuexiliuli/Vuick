//! LaTeX 数学排版（参考 zorite-main 的 `ratex-gpui/src/render.rs`）：`parse → layout →
//! render` 用 RaTeX（KaTeX 字体，纯 CPU 光栅）把 `$$…$$` / `$…$` 排版成
//! `gpui::RenderImage`，取代原来的 Unicode 近似（`∫₋∞∞`、`√(…)`、`a/b` 等真实排版）。
//!
//! RaTeX 只暴露 PNG 编码器；我们把 PNG 解码回像素后重着色：RaTeX 输出黑底白字→不透明，
//! 我们把「明度反转」作为字形 alpha、填充宿主正文色，公式就融入任何主题。
//! 与 zorite 完全同构，仅裁剪掉 `ratex-gpui` 的编辑/光标部分、只留显示管线。

use gpui::{Hsla, RenderImage, Rgba};
use kimg::{Frame, RgbaImage};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parse;
use ratex_render::{RenderOptions, render_to_png};
use std::sync::Arc;

/// 一个排版好的公式：位图 + 逻辑（pre-DPR）px 尺寸。
pub struct Rendered {
    pub image: Arc<RenderImage>,
    pub width: f32,
    pub height: f32,
}

/// 把 LaTeX 排版成 `RenderImage`（`font_size` 单位 px/em，`dpr` 设备像素比）。
/// `None` 当 LaTeX 解析/布局/光栅失败。`padding` 为排版边距（块用
/// [`config::math::MATH_PAD`]，行内用 [`config::math::INLINE_MATH_PAD`]——
/// 正文行高只有 ~19px，大边距会顶高小公式）。
pub fn render_latex_with_pad(
    latex: &str,
    font_size: f32,
    dpr: f32,
    color: Hsla,
    padding: f32,
) -> Option<Rendered> {
    let c: Rgba = color.into();
    let rgb = [
        (c.r * 255.0).round() as u8,
        (c.g * 255.0).round() as u8,
        (c.b * 255.0).round() as u8,
    ];
    // P2-① 唯一性：保留 Hsla 自带的 alpha（不再丢弃）；不透明色（alpha=1）字节恒等。
    let (bgra, w, h) = rasterize(latex, font_size, dpr, rgb, padding, c.a)?;
    let buf = RgbaImage::from_raw(w, h, bgra)?;
    Some(Rendered {
        image: Arc::new(RenderImage::new(vec![Frame::new(buf)])),
        // dpr 已由 rasterize 防御（≤0/NaN/Inf → None），此处除零/NaN 不可达。
        width: w as f32 / dpr,
        height: h as f32 / dpr,
    })
}

/// 与 gpui 无关的一半：LaTeX → BGRA 像素 + 像素尺寸。拆开以便无 gpui 上下文单测。
/// `alpha` ∈ [0,1]：字形透明度（= 颜色 alpha；1 = 不透明，与旧行为字节恒等）。
fn rasterize(
    latex: &str,
    font_size: f32,
    dpr: f32,
    rgb: [u8; 3],
    padding: f32,
    alpha: f32,
) -> Option<(Vec<u8>, u32, u32)> {
    // P2-① 唯一性：dpr ≤0 / NaN / ±Inf 直接失败（`device_pixel_ratio` 与像素换算不可用），
    // 防除零/NaN 尺寸污染下游布局。
    if !dpr.is_finite() || dpr <= 0.0 {
        return None;
    }
    let nodes = parse(latex).ok()?;
    let lbox = layout(&nodes, &LayoutOptions::default());
    let dl = to_display_list(&lbox);
    let opts = RenderOptions {
        font_size,
        padding,
        device_pixel_ratio: dpr,
        ..Default::default()
    };
    let png = render_to_png(&dl, &opts).ok()?;
    let rgba = kimg::load_from_memory(&png).ok()?.into_rgba8();
    let (w, h) = rgba.dimensions();
    let mut bytes = rgba.into_raw();
    // RaTeX 光栅黑底白字 → 重着色为 `rgb`、透明底：像素明度反转成字形 alpha
    // （白底 → 0、黑字形 → 255），填 `rgb`。gpui RenderImage 是 straight（非预乘）
    // BGRA，颜色不能预乘，否则双倍变暗。
    let [r, g, b] = rgb;
    for px in bytes.as_chunks_mut::<4>().0 {
        let a = 255 - px[0]; // 灰度光栅：红通道即明度
        px[0] = b;
        px[1] = g;
        px[2] = r;
        // P2-① 唯一性：字形 alpha 乘颜色 alpha（透明主题色生效；alpha=1 时恒等）。
        px[3] = ((a as f32) * alpha).round() as u8;
    }
    Some((bytes, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::math::{INLINE_MATH_PAD as INLINE_PAD, MATH_PAD as PAD};

    #[test]
    fn rasterizes_to_nonempty_bgra() {
        // 与 zorite `ratex-gpui::render::rasterizes_to_nonempty_bgra` 同款：
        // 纯 CPU 光栅，无需 GPU/窗口，测试环境可直接跑。
        let (bytes, w, h) = rasterize(r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}", 22.0, 2.0, [0, 0, 0], PAD, 1.0)
            .expect("renders integral");
        assert!(w > 0 && h > 0, "non-empty dims, got {w}x{h}");
        assert_eq!(bytes.len(), w as usize * h as usize * 4, "BGRA buffer matches dims");
        // 极限/分式/根号应该真的排出来（上下标不再是正文大小、分式有横线）——
        // 至少位图里有非全透明像素（不是纯空）。
        assert!(bytes.as_chunks::<4>().0.iter().any(|px| px[3] > 0), "glyph pixels present");
    }

    #[test]
    fn inline_pad_reduces_raster_height() {
        // 行内小 PAD 是真收益：`x^2` 在 PAD=8（≈22px 排 16px 边距）下高 ~35px，
        // 收紧到 INLINE_PAD=1.5 后 ≈24px 逻辑高 —— 正文 16px 字号缩放后贴住行高，
        // 不再把行撑高；超高算子（`\int`）另由 textstyle + 高度上限压制。
        let (_, _, h_big) = rasterize("x^2", 22.0, 2.0, [0, 0, 0], PAD, 1.0).unwrap();
        let (_, _, h_tight) = rasterize("x^2", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        assert!(
            h_tight < h_big,
            "行内小 PAD 应显著降高：{h_tight} < {h_big}"
        );
        // 行内缩放后（fs=16 → ×16/22=0.727）应贴住正文行高（≈19px）。
        let scaled = h_tight as f32 / 2.0 * (16.0 / 22.0);
        assert!(scaled < 19.0 + 1.0, "x^2 行内高应 ≤ 行高：{scaled:.1}");
    }

    #[test]
    fn textstyle_shrinks_only_big_operators_not_body() {
        // 用户实测：`$\int_0^1 x^2 dx = \tfrac{1}{3}$` 整图缩放把正文也压小了。
        // 正解 = 行内用 `\textstyle …` 排版：`\int` 极限转上下标、算子变小（h 58→34），
        // `x^2 dx` 等正文字体原样（宽 126→118 只是算子收紧，非整体缩）。
        let (_, w_plain, h_plain) =
            rasterize(r"\int_0^1 x^2 dx = \tfrac{1}{3}", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        let (_, w_ts, h_ts) = rasterize(
            r"\textstyle \int_0^1 x^2 dx = \tfrac{1}{3}",
            22.0,
            2.0,
            [0, 0, 0],
            INLINE_PAD,
            1.0,
        )
        .unwrap();
        assert!(
            h_ts < h_plain,
            "textstyle 应把带上下限的大算子高度压下来：{h_ts} < {h_plain}"
        );
        // 高度降幅主要来自算子；正文字符保持同字号（宽度应接近，仅算子收紧）。
        assert!(
            (w_plain as f32 - w_ts as f32) <= (w_plain as f32) * 0.2,
            "宽度不应大幅缩小（那才是整图缩放）：{w_plain} → {w_ts}"
        );
        // 无算子公式包 textstyle 高度几乎不变（小公式不被改；±3 逻辑 px 容差内）。
        let (_, _, h_x) = rasterize("x^2", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        let (_, _, h_tsx) =
            rasterize(r"\textstyle x^2", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        assert!(
            (h_tsx as i32 - h_x as i32).abs() <= 6,
            "`x^2` 包 textstyle 高度应基本不变：{h_x} → {h_tsx}"
        );
    }

    #[test]
    fn dpr_scales_pixels() {
        let (_, w1, _) = rasterize("x", 22.0, 1.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        let (_, w2, _) = rasterize("x", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        assert!(w2 > w1, "2x DPR yields more pixels ({w2} vs {w1})");
    }

    #[test]
    fn zero_or_non_finite_dpr_returns_none() {
        // P2-① 唯一性：dpr ≤0 / NaN / ±Inf 必须快速失败（防除零 / NaN 尺寸污染下游布局）。
        assert!(rasterize("x", 22.0, 0.0, [0, 0, 0], INLINE_PAD, 1.0).is_none());
        assert!(rasterize("x", 22.0, -1.0, [0, 0, 0], INLINE_PAD, 1.0).is_none());
        assert!(rasterize("x", 22.0, f32::NAN, [0, 0, 0], INLINE_PAD, 1.0).is_none());
        assert!(rasterize("x", 22.0, f32::INFINITY, [0, 0, 0], INLINE_PAD, 1.0).is_none());
        assert!(rasterize("x", 22.0, f32::NEG_INFINITY, [0, 0, 0], INLINE_PAD, 1.0).is_none());
    }

    #[test]
    fn alpha_scales_glyph_opacity_preserving_opaque() {
        // P2-① 唯一性：颜色 alpha 保留到字形透明度；alpha=1 与旧行为字节恒等。
        let (full, _, _) = rasterize("x", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).unwrap();
        let (half, _, _) = rasterize("x", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 0.5).unwrap();
        assert_eq!(full.len(), half.len(), "两遍光栅同尺寸");
        let fp = full.as_chunks::<4>().0;
        let hp = half.as_chunks::<4>().0;
        assert!(fp.iter().any(|px| px[3] > 0), "应有字形像素");
        for (f, h) in fp.iter().zip(hp.iter()) {
            if f[3] == 0 {
                assert_eq!(h[3], 0, "透明像素保持透明");
            } else {
                let expect = (f[3] as f32 * 0.5).round() as i32;
                assert!(
                    (h[3] as i32 - expect).abs() <= 1,
                    "alpha 应减半（±1 舍入）：{} → {}（期望 ≈ {expect}）",
                    f[3],
                    h[3]
                );
            }
        }
    }

    #[test]
    fn failed_latex_is_none() {
        // `\frac` 缺分母 → 布局失败，返回 None（调用方回退 Unicode 排版）。
        assert!(rasterize(r"\frac{1}", 22.0, 2.0, [0, 0, 0], INLINE_PAD, 1.0).is_none());
    }
}
