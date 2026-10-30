//! 顶层自由函数：`open` / `load_from_memory` / `load_from_memory_with_format` / `save_buffer_with_format`。

use std::fs::File;
use std::io::{BufWriter, Cursor, Write};
use std::path::Path;

use crate::buffer::{RgbImage, RgbaImage};
use crate::codecs::gif::GifEncoder;
use crate::codecs::png::PngEncoder;
use crate::color::ExtendedColorType;
use crate::dynamic_image::DynamicImage;
use crate::error::{ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind, ImageFormatHint};
use crate::format::ImageFormat;
use crate::image_reader::ImageReader;
use crate::pixel::Rgba;
use crate::GenericImageView;

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
pub fn load_from_memory_with_format(buf: &[u8], format: ImageFormat) -> ImageResult<DynamicImage> {
    let reader = ImageReader::with_format(Cursor::new(buf), format);
    reader.decode()
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
