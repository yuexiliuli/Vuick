//! 封面图像解码：把封面文件解码成可上屏的 `RenderImage`（BGRA 字节序），供全屏封面 /
//! 正在播放大图 / 底栏缩略图 / 列表缩略图复用。解码可能较慢（几十 ms 级），由视图层放
//! 后台线程执行并缓存 `Arc<RenderImage>`，本模块只做纯解码。
//!
//! 姿势照抄 `crates/image_viewer/src/viewer.rs`（`decode_static_first_frame` +
//! `swap_rb_in_place`）：`image` crate 全解 → RGBA → 原地交换 R/B（GPUI 渲染格式为
//! BGRA）→ 单帧 `RenderImage`。GIF/动图封面按首帧静态处理，足够封面场景。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use gpui::RenderImage;

/// 解码图片字节为 BGRA `RenderImage`（单帧静态；失败给可读错误不是 panic）。
pub(crate) fn decode_to_render(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    let img = kimg::load_from_memory(bytes).map_err(|e| format!("无法解码封面：{e}"))?;
    let mut rgba = img.to_rgba8();
    swap_rb_in_place(rgba.as_mut());
    let frame = kimg::Frame::new(rgba);
    Ok(Arc::new(RenderImage::new(vec![frame])))
}

/// 读封面文件并解码。
pub(crate) fn cover_from_file(path: &Path) -> Result<Arc<RenderImage>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取封面失败：{e}"))?;
    decode_to_render(&bytes)
}

/// RGBA → BGRA（原地交换每像素 R/B）。GPUI `RenderImage` 字节序为 BGRA；不交换会红蓝互换。
fn swap_rb_in_place(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成 2×1 的纯红 PNG（R=255, G=B=0）字节，供解码回归用。
    fn tiny_red_png() -> Vec<u8> {
        let img: kimg::RgbaImage =
            kimg::ImageBuffer::from_pixel(2, 1, kimg::Rgba([255u8, 0, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        kimg::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, kimg::ImageFormat::Png)
            .expect("png 编码应成功");
        out.into_inner()
    }

    #[test]
    fn decode_png_to_render_is_bgra_dimensions() {
        let render = decode_to_render(&tiny_red_png()).expect("应解码成功");
        assert_eq!(render.frame_count(), 1);
        let bytes = render.as_bytes(0).expect("首帧字节");
        assert_eq!(bytes.len(), 2 * 1 * 4);
        // 纯红 Rgba(255,0,0,255) → 交换 R/B 后应为 [B=0, G=0, R=255, A=255]
        assert_eq!(bytes[0], 0, "机制成立：R/B 已交换（首字节是 B）");
        assert_eq!(bytes[1], 0, "G 不变");
        assert_eq!(bytes[2], 255, "R 分量（原 B=0 处）");
        assert_eq!(bytes[3], 255, "alpha 不变");
    }

    #[test]
    fn decode_empty_bytes_fails_gracefully() {
        assert!(decode_to_render(b"not-an-image").is_err(), "损坏字节应报错而非 panic");
    }
}
