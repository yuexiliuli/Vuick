//! 解码器分发门面：按格式聚合各格式 crate 的解码器（kimg_png / kimg_gif / kimg_webp /
//! kimg_jpeg / kimg_tiff / kimg_bmp / kimg_ico / kimg_pnm）并提供 `GenericDecoder`。

pub mod bmp {
    pub use kimg_bmp::BmpDecoder;
}
pub mod gif {
    pub use kimg_gif::{GifDecoder, GifEncoder, Repeat};
}
pub mod ico {
    pub use kimg_ico::IcoDecoder;
}
pub mod jpeg {
    pub use kimg_jpeg::JpegDecoder;
}
pub mod png {
    pub use kimg_png::{PngDecoder, PngEncoder};
}
pub mod pnm {
    pub use kimg_pnm::{PnmDecoder, PnmSubtype, SampleEncoding};
}
pub mod tiff {
    pub use kimg_tiff::TiffDecoder;
}
pub mod webp {
    pub use kimg_webp::{
        set_webp_backend, BACKEND_AUTO, BACKEND_OURS, WebPDecoder,
        decode_static_lossless_rgba8_into, decode_static_lossy_rgba8_into,
    };
    // 静态族直写辅助：kimg_webp 根面未 re-export，从 `codecs::webp` 子模块取。
    pub use kimg_webp::codecs::webp::{lossless_static_size, uninit_rgba8_vec};
    #[cfg(feature = "stage_profile")]
    pub use kimg_webp::{
        vp8_extract_frame, vp8_probe_clock_costs, vp8_probe_entropy_op_costs,
        vp8_probe_i4_op_costs, vp8_stage_decode, vp8l_extract_bitstream, vp8l_stage_decode,
        I4OpCosts, Vp8StageProfile, Vp8lStageProfile,
    };
}
pub mod heif {
    pub use kimg_heif::{read_info, HeicDecoder, HeifInfo};
    pub use kimg_heif::codecs::heif::probe::{heif_probe, ProbeInfo};
}

use std::io::{BufRead, Cursor, Seek};

use crate::color::ColorType;
use crate::error::{ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind};
use crate::format::ImageFormat;
use crate::limits::Limits;
use crate::metadata::Orientation;
use crate::{ImageDecoder, ImageFormatHint};

/// 按格式分发到具体解码器的通用解码器（供 `ImageReader` 使用）。
///
/// 生命周期 `'a` 仅为 PNG/JPEG 零拷贝路径携带（Png/Jpeg 变体可借用外部输入切片）；
/// 其余格式一律自有输入，`'a` 仅协变透传，不影响使用。
/// 大变体按设计内联（调用面以枚举值传递，不装箱破坏零拷贝借用面）→ 放行。
#[allow(clippy::large_enum_variant)]
pub enum GenericDecoder<'a, R: BufRead + Seek> {
    Png(png::PngDecoder<'a, R>),
    Jpeg(jpeg::JpegDecoder<'a>),
    Gif(gif::GifDecoder<R>),
    WebP(webp::WebPDecoder<R>),
    Tiff(tiff::TiffDecoder<R>),
    Bmp(bmp::BmpDecoder<R>),
    Ico(ico::IcoDecoder<R>),
    Pnm(pnm::PnmDecoder<R>),
    Heic(heif::HeicDecoder<R>),
}

impl<'a, R: BufRead + Seek> GenericDecoder<'a, R> {
    /// 按格式构造解码器；不支持的格式返回 `Unsupported`。
    /// PNG 走「通用读取器 → 读入自有输入」路径（内存切片读者用 [`Self::new_slice`] 零拷贝）。
    pub(crate) fn new(format: ImageFormat, reader: R) -> ImageResult<Self> {
        use ImageFormat::*;
        match format {
            Png => Ok(GenericDecoder::Png(png::PngDecoder::new(reader)?)),
            Jpeg => Ok(GenericDecoder::Jpeg(jpeg::JpegDecoder::new(reader)?)),
            Gif => Ok(GenericDecoder::Gif(gif::GifDecoder::new(reader)?)),
            WebP => Ok(GenericDecoder::WebP(webp::WebPDecoder::new(reader)?)),
            Tiff => Ok(GenericDecoder::Tiff(tiff::TiffDecoder::new(reader)?)),
            Bmp => Ok(GenericDecoder::Bmp(bmp::BmpDecoder::new(reader)?)),
            Ico => Ok(GenericDecoder::Ico(ico::IcoDecoder::new(reader)?)),
            Pnm => Ok(GenericDecoder::Pnm(pnm::PnmDecoder::new(reader)?)),
            Heic => Ok(GenericDecoder::Heic(heif::HeicDecoder::new(reader)?)),
            other => Err(ImageError::Unsupported(
                UnsupportedError::from_format_and_kind(
                    ImageFormatHint::Exact(other),
                    UnsupportedErrorKind::Format(ImageFormatHint::Exact(other)),
                ),
            )),
        }
    }
}

/// 特判：内存切片读者 `Cursor<&[u8]>` 的零拷贝构造入口（A1，对应 `load_from_memory` 的
/// `&[u8]` 输入经 `Cursor` 包装的路径）。PNG/JPEG 直接借用输入切片（不整份复制，省一次 MB 级
/// memcpy 与峰值内存）；其余格式沿用 [`Self::new`] 通用读取路径。
/// （裸 `&[u8]` 不实现 `Seek`，故泛型 `GenericDecoder<R: BufRead+Seek>` 不实例化它——
/// 裸切片由 `PngDecoder::from_slice` 直接承接，见 `free_functions.rs`。）
impl<'a> GenericDecoder<'a, Cursor<&'a [u8]>> {
    /// 由 `Cursor<&[u8]>` 构造（`load_from_memory` 的内存字节路径）。
    pub(crate) fn new_slice(format: ImageFormat, reader: Cursor<&'a [u8]>) -> ImageResult<Self> {
        match format {
            ImageFormat::Png => {
                // 借用 reader 里的外部切片（数据在调用方，'a = 调用方 buf 存活期）。
                let dec = png::PngDecoder::from_slice(reader.get_ref())?;
                Ok(GenericDecoder::Png(dec))
            }
            ImageFormat::Jpeg => {
                // 同 PNG：借用输入切片零拷贝构建（C1）。
                let dec = jpeg::JpegDecoder::from_bytes(reader.get_ref())?;
                Ok(GenericDecoder::Jpeg(dec))
            }
            _ => GenericDecoder::new(format, reader),
        }
    }
}

impl<'a, R: BufRead + Seek> ImageDecoder for GenericDecoder<'a, R> {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            GenericDecoder::Png(d) => d.dimensions(),
            GenericDecoder::Jpeg(d) => d.dimensions(),
            GenericDecoder::Gif(d) => d.dimensions(),
            GenericDecoder::WebP(d) => d.dimensions(),
            GenericDecoder::Tiff(d) => d.dimensions(),
            GenericDecoder::Bmp(d) => d.dimensions(),
            GenericDecoder::Ico(d) => d.dimensions(),
            GenericDecoder::Pnm(d) => d.dimensions(),
            GenericDecoder::Heic(d) => d.dimensions(),
        }
    }

    fn color_type(&self) -> ColorType {
        match self {
            GenericDecoder::Png(d) => d.color_type(),
            GenericDecoder::Jpeg(d) => d.color_type(),
            GenericDecoder::Gif(d) => d.color_type(),
            GenericDecoder::WebP(d) => d.color_type(),
            GenericDecoder::Tiff(d) => d.color_type(),
            GenericDecoder::Bmp(d) => d.color_type(),
            GenericDecoder::Ico(d) => d.color_type(),
            GenericDecoder::Pnm(d) => d.color_type(),
            GenericDecoder::Heic(d) => d.color_type(),
        }
    }

    fn original_color_type(&self) -> crate::color::ExtendedColorType {
        match self {
            GenericDecoder::Png(d) => d.original_color_type(),
            GenericDecoder::Jpeg(d) => d.original_color_type(),
            GenericDecoder::Gif(d) => d.original_color_type(),
            GenericDecoder::WebP(d) => d.original_color_type(),
            GenericDecoder::Tiff(d) => d.original_color_type(),
            GenericDecoder::Bmp(d) => d.original_color_type(),
            GenericDecoder::Ico(d) => d.original_color_type(),
            GenericDecoder::Pnm(d) => d.original_color_type(),
            GenericDecoder::Heic(d) => d.original_color_type(),
        }
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        match self {
            GenericDecoder::Png(d) => d.exif_metadata(),
            GenericDecoder::Jpeg(d) => d.exif_metadata(),
            GenericDecoder::Gif(d) => d.exif_metadata(),
            GenericDecoder::WebP(d) => d.exif_metadata(),
            GenericDecoder::Tiff(d) => d.exif_metadata(),
            GenericDecoder::Bmp(d) => d.exif_metadata(),
            GenericDecoder::Ico(d) => d.exif_metadata(),
            GenericDecoder::Pnm(d) => d.exif_metadata(),
            GenericDecoder::Heic(d) => d.exif_metadata(),
        }
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        match self {
            GenericDecoder::Png(d) => d.orientation(),
            GenericDecoder::Jpeg(d) => d.orientation(),
            GenericDecoder::Gif(d) => d.orientation(),
            GenericDecoder::WebP(d) => d.orientation(),
            GenericDecoder::Tiff(d) => d.orientation(),
            GenericDecoder::Bmp(d) => d.orientation(),
            GenericDecoder::Ico(d) => d.orientation(),
            GenericDecoder::Pnm(d) => d.orientation(),
            GenericDecoder::Heic(d) => d.orientation(),
        }
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        match self {
            GenericDecoder::Png(d) => d.read_image(buf),
            GenericDecoder::Jpeg(d) => d.read_image(buf),
            GenericDecoder::Gif(d) => d.read_image(buf),
            GenericDecoder::WebP(d) => d.read_image(buf),
            GenericDecoder::Tiff(d) => d.read_image(buf),
            GenericDecoder::Bmp(d) => d.read_image(buf),
            GenericDecoder::Ico(d) => d.read_image(buf),
            GenericDecoder::Pnm(d) => d.read_image(buf),
            GenericDecoder::Heic(d) => d.read_image(buf),
        }
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        match self {
            GenericDecoder::Png(d) => d.set_limits(limits),
            GenericDecoder::Jpeg(d) => d.set_limits(limits),
            GenericDecoder::Gif(d) => d.set_limits(limits),
            GenericDecoder::WebP(d) => d.set_limits(limits),
            GenericDecoder::Tiff(d) => d.set_limits(limits),
            GenericDecoder::Bmp(d) => d.set_limits(limits),
            GenericDecoder::Ico(d) => d.set_limits(limits),
            GenericDecoder::Pnm(d) => d.set_limits(limits),
            GenericDecoder::Heic(d) => d.set_limits(limits),
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
