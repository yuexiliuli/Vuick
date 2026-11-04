//! 门面扩展：`ImageBuffer::save` / `save_with_format`。
//!
//! 拆分后 `ImageBuffer` 本体在 kimg_core（不含任何编码器），存盘编码分发在门面层
//! （[`crate::free_functions::save_buffer_with_format`]，仅 PNG/GIF 可编码），故这两个方法以
//! 扩展 trait（`kimg::ImageBufferExt`）形态挂在门面，调用 `img.save(path)` 的消费方需 `use kimg::ImageBufferExt;`。

use std::ops::Deref;
use std::path::Path;

use crate::buffer::ImageBuffer;
use crate::error::ImageResult;
use crate::format::ImageFormat;
use crate::free_functions::save_buffer_with_format;
use crate::traits::{Pixel, PixelWithColorType, Primitive};

/// `ImageBuffer::save` 的扩展 trait（kimg 拆门面后从 kimg_core 的 inherent method 移来）。
pub trait ImageBufferExt {
    /// 按扩展名推断格式存盘。仅 PNG/GIF 可编码，其余格式返回 `Unsupported`。
    fn save<Q: AsRef<Path>>(&self, path: Q) -> ImageResult<()>;
    /// 按指定格式存盘。
    fn save_with_format<Q: AsRef<Path>>(&self, path: Q, format: ImageFormat) -> ImageResult<()>;
}

impl<P: Pixel + PixelWithColorType, C: Deref<Target = [P::Subpixel]>> ImageBufferExt
    for ImageBuffer<P, C>
{
    fn save<Q: AsRef<Path>>(&self, path: Q) -> ImageResult<()> {
        let path = path.as_ref();
        let format = ImageFormat::from_path(path)?;
        self.save_with_format(path, format)
    }

    fn save_with_format<Q: AsRef<Path>>(&self, path: Q, format: ImageFormat) -> ImageResult<()> {
        save_buffer_with_format(
            path,
            pixel_bytes(self.as_raw()),
            self.width(),
            self.height(),
            <P as PixelWithColorType>::COLOR_TYPE,
            format,
        )
    }
}

/// 把 `&[Subpixel]` 按内存布局视为 `&[u8]`（u8/u16/f32 均是无填充标量，与 image 的
/// `EncodableLayout` 语义一致；16bit 输入由 PngEncoder 内部重排为大端）。
fn pixel_bytes<T: Primitive>(data: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), std::mem::size_of_val(data))
    }
}
