//! `ImageDecoder` trait（镜像 image 0.25 的抽象；`read_image` 采用原生字节序输出）。

use crate::color::{ColorType, ExtendedColorType};
use crate::error::ImageResult;
use crate::limits::{LimitSupport, Limits};
use crate::metadata::Orientation;

/// 解码器 trait：所有解码器实现本 trait。
pub trait ImageDecoder {
    /// 图像 (宽, 高)。
    fn dimensions(&self) -> (u32, u32);

    /// 解码输出的颜色类型。
    fn color_type(&self) -> ColorType;

    /// 解码前的原始颜色类型（默认等于解码后）。
    fn original_color_type(&self) -> ExtendedColorType {
        self.color_type().into()
    }

    /// 内嵌 ICC profile。
    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(None)
    }

    /// 原始 EXIF 块。
    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(None)
    }

    /// 原始 XMP 数据。
    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(None)
    }

    /// 原始 IPTC 数据。
    fn iptc_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(None)
    }

    /// 图像方向（通常取自 EXIF）。
    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(self
            .exif_metadata()?
            .and_then(|chunk| Orientation::from_exif_chunk(&chunk))
            .unwrap_or(Orientation::NoTransforms))
    }

    /// 解码后图像占用的字节总数（`buf` 需不小于其放入 `read_image`）。
    fn total_bytes(&self) -> u64 {
        let dimensions = self.dimensions();
        let total_pixels = u64::from(dimensions.0) * u64::from(dimensions.1);
        let bytes_per_pixel = u64::from(self.color_type().bytes_per_pixel());
        total_pixels.saturating_mul(bytes_per_pixel)
    }

    /// 把整图像素读入 `buf`（长度必须等于 `total_bytes`，原生字节序）。
    fn read_image(self, buf: &mut [u8]) -> ImageResult<()>
    where
        Self: Sized;

    /// 设置解码限制。
    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        let (width, height) = self.dimensions();
        limits.check_dimensions(width, height)?;
        Ok(())
    }

    /// `Box<dyn ImageDecoder>` 的桥接。
    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()>;
}
