//! 应用图标资产：变色龙 288×288 PNG（调色板 8bpp）→ 进程内一次性解码为
//! `gpui::RenderImage`，供自绘标题栏最左图标与欢迎页（空态）插图共用。
//!
//! 资产存于 `assets/app-icon-228x228.png`（被 git 跟踪的入库资产；文件名标 228，
//! IHDR 实际 288×288，以解码尺寸为准）；编译期 `include_bytes!` 内嵌 → 零运行时文件 IO。
//!
//! 与 `crates/tray::APP_ICO`（同 PNG → 64×64 HICON）同源：本模块只做屏幕渲染
//! （gpui 位图），不碰 Win32；两者各自内嵌、互不依赖。
//!
//! 注意：Cargo 不追踪包外 `include_bytes!` 的文件改动（PNG 位于本 crate 之外）→
//! 换图标需 touch 本 crate 任一个 .rs 触发重建。

use std::sync::{Arc, OnceLock};

use gpui::{ImageSource, InteractiveElement, IntoElement, ParentElement, RenderImage, Styled};
use image::{Frame, GenericImageView, ImageReader};

/// 应用图标 PNG 字节。
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/app-icon-228x228.png");

/// 进程内一次性解码应用图标 PNG → `gpui::RenderImage`（位图纹理上传在渲染期由 GPU 完成）。
pub fn app_icon_render() -> Arc<RenderImage> {
    static RENDER: OnceLock<Arc<RenderImage>> = OnceLock::new();
    RENDER
        .get_or_init(|| {
            let img = ImageReader::new(std::io::Cursor::new(APP_ICON_PNG))
                .with_guessed_format()
                .expect("应用图标 PNG 格式识别失败")
                .decode()
                .expect("应用图标 PNG 解码失败");
            let (w, h) = img.dimensions();
            debug_assert_eq!((w, h), (288, 288), "PNG 解码尺寸不对");
            let rgba = img.to_rgba8();
            // gpui `RenderImage` 文档明示缓冲按 **BGRA** 字节序消费（pdf 同款姿势：
            // 把 bgra 字节标成 RgbaImage 喂入即正确显示）；`image` 解码给的是 RGBA →
            // 交换 R/B 再包 `Frame`，否则上屏 R/B 互换（青蓝变橙黄，真机取证确认）。
            let bgra = image::ImageBuffer::from_fn(w, h, |x, y| {
                let p = rgba.get_pixel(x, y);
                image::Rgba([p[2], p[1], p[0], p[3]])
            });
            Arc::new(RenderImage::new(vec![Frame::new(bgra)]))
        })
        .clone()
}

/// 应用图标元素：给定边长的方块 `img`（源为正方形，勿拉伸——标题栏最左用 ~18px）。
pub fn app_icon_element(size: gpui::Pixels) -> gpui::AnyElement {
    let img = gpui::img(ImageSource::Render(app_icon_render()))
        .w(size)
        .h(size);
    gpui::div()
        .id("app-icon")
        .debug_selector(|| "app-icon".into())
        .w(size)
        .h(size)
        .flex_shrink_0()
        .child(img)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 图标资产可解码且尺寸正确（288×288）——标题栏/欢迎页/托盘 HICON 同源的
    /// 渲染回归护栏：换/错放资产会在此红。
    #[test]
    fn app_icon_decodes_288x288() {
        let img = ImageReader::new(std::io::Cursor::new(APP_ICON_PNG))
            .with_guessed_format()
            .expect("应用图标 PNG 格式识别失败")
            .decode()
            .expect("应用图标 PNG 解码失败");
        assert_eq!(img.dimensions(), (288, 288), "应用图标 PNG 尺寸应为 288×288");
        // 至少一帧非空且含不透明像素（真图，非空白占位）：直接扫 BGRA 原始字节。
        let rgba = img.to_rgba8();
        assert!(
            rgba.as_raw().chunks_exact(4).any(|px| px[3] > 0),
            "应用图标应含不透明像素"
        );
    }
}
