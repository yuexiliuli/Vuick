//! kimg：图像解码/编码兼容层（门面 crate）。
//!
//! 镜像 image 0.25（本仓库锁定 0.25.10）的「仓库实际使用子集」API。kimg 于 2026-11 拆分为
//! kimg_core（共享类型）+ 8 个格式 crate（kimg_png/kimg_gif/kimg_webp/kimg_jpeg/kimg_tiff/
//! kimg_bmp/kimg_ico/kimg_pnm），本 crate 保留为**门面子集**：聚合分发的 `GenericDecoder`、
//! 门面自由函数（`open`/`load_from_memory`/`save_buffer_with_format`）、`DynamicImage`/
//! `ImageReader`，并对旧 `kimg::` 公共路径全量重导出——消费方（shell 生态 / gpui fork）零改动。
//!
//! # 模块路径对齐
//!
//! - 顶层：`Rgba`/`Rgb`/`Luma`/`LumaA`、`ImageBuffer` + 别名、`DynamicImage`、
//!   `Frame`/`Delay`/`AnimationDecoder`、`ImageFormat`/`ImageError`/`Limits`/`ColorType`、
//!   `ImageDecoder`、`ImageReader`、`GenericImageView`（以上共享类型来自 `kimg_core`）。
//! - 自由函数：`open`/`load_from_memory`/`load_from_memory_with_format`/
//!   `save_buffer_with_format`/`guess_format`。
//! - 编解码器：`codecs::png::PngDecoder`、`codecs::gif::GifDecoder`/`GifEncoder`、
//!   `codecs::webp::WebPDecoder`（自各格式 crate 重导出）。
//! - 算子：`imageops::resize` 与 `imageops::FilterType`。

pub use kimg_core::*;

pub mod codecs;
pub mod dynamic_image;
pub mod free_functions;
pub mod image_reader;
pub use crate::dynamic_image::DynamicImage;

mod image_buf_ext;
pub use crate::image_buf_ext::ImageBufferExt;

#[cfg(test)]
mod bench_rgb8_rgba8;
pub use crate::free_functions::{
    load_from_memory, load_from_memory_with_format, open, save_buffer, save_buffer_with_format,
};
#[allow(deprecated)]
pub use crate::image_reader::{ImageReader, Reader};
