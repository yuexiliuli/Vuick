//! kimg_core：kimg 拆分的共享核心 crate。
//!
//! 镜像 image 0.25 中与具体编解码器无关的类型与 trait（pixel/color/buffer/error/format/
//! limits/metadata/decoder/animation/imageops）。被 8 个格式 codec crate（kimg_png /
//! kimg_gif / kimg_webp / kimg_jpeg / kimg_tiff / kimg_bmp / kimg_ico / kimg_pnm）与
//! kimg 门面 crate 共同依赖。本 crate 不含任何解码/编码器实现。

pub mod animation;
pub mod buffer;
pub mod color;
pub mod decoder;
pub mod error;
pub mod format;
pub mod imageops;
pub mod limits;
pub mod metadata;
pub mod pixel;
pub mod traits;

pub use crate::animation::{AnimationDecoder, Delay, Frame, Frames, LoopCount, Ratio};
pub use crate::buffer::{
    GenericImage, GenericImageView, GrayAlpha16Image, GrayAlphaImage, Gray16Image, GrayImage,
    ImageBuffer, Rgb16Image, RgbImage, Rgba16Image, Rgba32FImage, Rgb32FImage, RgbaImage,
};
pub use crate::color::{ColorType, ExtendedColorType};
pub use crate::decoder::ImageDecoder;
pub use crate::error::{
    DecodingError, EncodingError, ImageError, ImageFormatHint, ImageResult, LimitError,
    LimitErrorKind, ParameterError, ParameterErrorKind, UnsupportedError, UnsupportedErrorKind,
};
pub use crate::format::{guess_format, ImageFormat};
pub use crate::imageops::FilterType;
pub use crate::limits::{LimitSupport, Limits};
pub use crate::metadata::Orientation;
pub use crate::pixel::{Luma, LumaA, Rgb, Rgba};
pub use crate::traits::{Pixel, PixelWithColorType, Primitive};
