//! 顶层自由函数：`open` / `load_from_memory` / `load_from_memory_with_format` / `save_buffer_with_format`。

use std::fs::File;
use std::io::{BufWriter, Cursor, Write};
use std::path::Path;

use crate::ImageDecoder as _;
use crate::buffer::{ImageBuffer, RgbImage, RgbaImage};
use crate::codecs::gif::GifEncoder;
use crate::codecs::png::PngEncoder;
use crate::color::ExtendedColorType;
use crate::dynamic_image::DynamicImage;
use crate::error::{
    ImageError, ImageFormatHint, ImageResult, UnsupportedError, UnsupportedErrorKind,
};
use crate::format::ImageFormat;
use crate::image_reader::ImageReader;
use crate::limits::Limits;
use crate::pixel::Rgba;

pub use crate::format::guess_format;

/// 从文件路径打开并解码（格式按扩展名 + 内容猜测）。
pub fn open<P>(path: P) -> ImageResult<DynamicImage>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    ImageReader::open(path)?.with_guessed_format()?.decode()
}

/// 从内存字节解码（按内容猜测格式）。
pub fn load_from_memory(buf: &[u8]) -> ImageResult<DynamicImage> {
    let format = guess_format(buf)?;
    load_from_memory_with_format(buf, format)
}

/// 从内存字节按指定格式解码。
///
/// A1 零拷贝：内存切片统一经 `GenericDecoder::new_slice` 特判——PNG 直接借用输入切片
/// （[`crate::codecs::png::PngDecoder::from_slice`]，不整份复制），其余格式沿用通用读取路径。
pub fn load_from_memory_with_format(buf: &[u8], format: ImageFormat) -> ImageResult<DynamicImage> {
    if format == ImageFormat::WebP {
        // VP8L 静态族「生产直写」出口（镜像 `ImageReader::decode`）：内存切片已含全量字节，
        // 直接预检 + 直写 RGBA8 唯一缓冲；不可接管 / 解码失败回退既有解码路径。
        let mut limits = Limits::default();
        match crate::codecs::webp::lossless_static_size(buf) {
            Some((w, h, total_bytes)) => {
                // 与既有解码路径严格限制等价：先查严格尺寸（`set_limits` 的
                // `check_dimensions` 口径），再按与现状一致的体积扣 `max_alloc`。
                limits.check_dimensions(w, h)?;
                limits.reserve(total_bytes as u64)?;
                // 未初始化缓冲（免整帧清零）：直写核 Ok 前全量覆写 RGBA8（全覆写不变式）。
                let mut rgba = crate::codecs::webp::uninit_rgba8_vec((w as usize) * (h as usize) * 4);
                match crate::codecs::webp::decode_static_lossless_rgba8_into(buf, &mut rgba) {
                    Ok(true) => {
                        return Ok(DynamicImage::ImageRgba8(
                            ImageBuffer::from_raw(w, h, rgba).expect("缓冲尺寸已校验"),
                        ))
                    }
                    _ => limits.free(total_bytes as u64),
                }
            }
            None => {}
        }
        let reader = Cursor::new(buf);
        let mut decoder = crate::codecs::GenericDecoder::new_slice(format, reader)?;
        limits.reserve(decoder.total_bytes())?;
        decoder.set_limits(limits)?;
        return DynamicImage::from_decoder(decoder);
    }
    let reader = Cursor::new(buf);
    let mut decoder = crate::codecs::GenericDecoder::new_slice(format, reader)?;
    let mut limits = Limits::default();
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    DynamicImage::from_decoder(decoder)
}

/// 按路径与格式把原始缓冲存盘。
pub fn save_buffer_with_format(
    path: impl AsRef<Path>,
    buf: &[u8],
    width: u32,
    height: u32,
    color: impl Into<ExtendedColorType>,
    format: ImageFormat,
) -> ImageResult<()> {
    let color = color.into();
    let mut file = BufWriter::new(File::create(path)?);
    let encoder = encoder_for_format(format, &mut file)?;
    encoder.write_image(buf, width, height, color)
}

/// 按路径扩展名推断格式并把原始缓冲存盘（PNG/GIF 可编码，其余格式返回 `Unsupported`）。
pub fn save_buffer(
    path: impl AsRef<Path>,
    buf: &[u8],
    width: u32,
    height: u32,
    color: impl Into<ExtendedColorType>,
) -> ImageResult<()> {
    let path = path.as_ref();
    let format = ImageFormat::from_path(path)?;
    save_buffer_with_format(path, buf, width, height, color, format)
}

/// 一次性的编码器 enum（覆盖 kimg 已实现的 PNG/GIF 编码）。
pub(crate) enum Encoder<'a, W: Write> {
    Png(PngEncoder<&'a mut W>),
    Gif(GifEncoder<&'a mut W>),
}

impl<'a, W: Write> Encoder<'a, W> {
    pub(crate) fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color: ExtendedColorType,
    ) -> ImageResult<()> {
        match self {
            Encoder::Png(enc) => enc.write_image(buf, width, height, color),
            Encoder::Gif(enc) => {
                if color != ExtendedColorType::Rgba8 && color != ExtendedColorType::Rgb8 {
                    return Err(ImageError::Unsupported(
                        UnsupportedError::from_format_and_kind(
                            ImageFormat::Gif.into(),
                            UnsupportedErrorKind::Color(color),
                        ),
                    ));
                }
                let frame = if color == ExtendedColorType::Rgba8 {
                    let img = RgbaImage::from_raw(width, height, buf.to_vec())
                        .expect("buffer length checked by format");
                    crate::animation::Frame::new(img)
                } else {
                    let rgb = RgbImage::from_raw(width, height, buf.to_vec())
                        .expect("buffer length checked by format");
                    let mut rgba = RgbaImage::new(width, height);
                    for (x, y, p) in rgb.enumerate_pixels() {
                        rgba.put_pixel(x, y, Rgba([p[0], p[1], p[2], 255]));
                    }
                    crate::animation::Frame::new(rgba)
                };
                let mut enc = enc;
                enc.encode_frame(frame)
            }
        }
    }
}

/// 按格式构造编码器。
pub(crate) fn encoder_for_format<'a, W: Write + 'a>(
    format: ImageFormat,
    w: &'a mut W,
) -> ImageResult<Encoder<'a, W>> {
    match format {
        ImageFormat::Png => Ok(Encoder::Png(PngEncoder::new(w))),
        ImageFormat::Gif => Ok(Encoder::Gif(GifEncoder::new(w))),
        other => Err(ImageError::Unsupported(
            UnsupportedError::from_format_and_kind(
                ImageFormatHint::Exact(other),
                UnsupportedErrorKind::Format(ImageFormatHint::Exact(other)),
            ),
        )),
    }
}
