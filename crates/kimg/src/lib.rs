//! kimg：纯 Rust 图像解码/编码兼容层。
//!
//! 镜像 image 0.25（本仓库锁定 0.25.10）的「仓库实际使用子集」API，供第 3 阶段整仓替换
//! crates.io 的 `image` 依赖。行为目标为逐位对齐：decode 像素字节与 image 0.25 相同。
//!
//! # 模块路径对齐
//!
//! - 顶层：`Rgba`/`Rgb`/`Luma`/`LumaA`、`ImageBuffer` + 别名、`DynamicImage`、
//!   `Frame`/`Delay`/`AnimationDecoder`、`ImageFormat`/`ImageError`/`Limits`/`ColorType`、
//!   `ImageDecoder`、`ImageReader`、`GenericImageView`。
//! - 自由函数：`open`/`load_from_memory`/`load_from_memory_with_format`/
//!   `save_buffer_with_format`/`guess_format`。
//! - 编解码器：`codecs::png::PngDecoder`、`codecs::gif::GifDecoder`/`GifEncoder`、
//!   `codecs::webp::WebPDecoder`。
//! - 算子：`imageops::resize` 与 `imageops::FilterType`。

pub mod animation;
pub mod buffer;
pub mod codecs;
pub mod color;
pub mod decoder;
pub mod dynamic_image;
pub mod error;
pub mod format;
pub mod free_functions;
pub mod image_reader;
pub mod imageops;
pub mod limits;
pub mod metadata;
pub mod pixel;
pub mod traits;

pub use crate::animation::{AnimationDecoder, Delay, Frame, Frames, LoopCount};
pub use crate::buffer::{
    GenericImage, GenericImageView, GrayAlpha16Image, GrayAlphaImage, Gray16Image, GrayImage,
    ImageBuffer, Rgb16Image, RgbImage, Rgba16Image, Rgba32FImage, Rgb32FImage, RgbaImage,
};
pub use crate::color::{ColorType, ExtendedColorType};
pub use crate::decoder::ImageDecoder;
pub use crate::dynamic_image::DynamicImage;
pub use crate::error::{
    DecodingError, EncodingError, ImageError, ImageFormatHint, ImageResult, LimitError,
    LimitErrorKind, ParameterError, ParameterErrorKind, UnsupportedError, UnsupportedErrorKind,
};
pub use crate::format::ImageFormat;
pub use crate::free_functions::{
    guess_format, load_from_memory, load_from_memory_with_format, open, save_buffer_with_format,
};
#[allow(deprecated)]
pub use crate::image_reader::{ImageReader, Reader};
pub use crate::limits::{LimitSupport, Limits};
pub use crate::metadata::Orientation;
pub use crate::pixel::{Luma, LumaA, Rgb, Rgba};
pub use crate::traits::{Pixel, Primitive};
