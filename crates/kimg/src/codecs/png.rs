//! PNG 解码器与编码器（`png` crate 0.17 封装；行为与 image 0.25.10 一致）。

use std::io::{BufRead, Seek, Write};

use crate::color::{ColorType, ExtendedColorType};
use crate::dynamic_image::DynamicImage;
use crate::error::{
    DecodingError, ImageError, ImageResult, LimitError, LimitErrorKind, ParameterError,
    ParameterErrorKind, UnsupportedError, UnsupportedErrorKind,
};
use crate::format::ImageFormat;
use crate::limits::{LimitSupport, Limits};
use crate::metadata::Orientation;
use crate::ImageDecoder;

/// PNG 解码器：构造即读取头信息，输出颜色类型与 image 0.25 一致（EXPAND 变换、16bit 保留）。
pub struct PngDecoder<R: BufRead + Seek> {
    color_type: ColorType,
    reader: png::Reader<R>,
    limits: Limits,
}

impl<R: BufRead + Seek> PngDecoder<R> {
    /// 无限制构造。
    pub fn new(r: R) -> ImageResult<PngDecoder<R>> {
        Self::with_limits(r, Limits::no_limits())
    }

    /// 带限制构造（限制在 `read_info` 前校验尺寸并传给 png crate 字节预算）。
    pub fn with_limits(r: R, limits: Limits) -> ImageResult<PngDecoder<R>> {
        limits.check_support(&LimitSupport::default())?;

        let max_bytes = usize::try_from(limits.max_alloc.unwrap_or(u64::MAX)).unwrap_or(usize::MAX);
        let mut decoder = png::Decoder::new_with_limits(r, png::Limits { bytes: max_bytes });
        decoder.set_ignore_text_chunk(false);

        let info = decoder.read_header_info().map_err(ImageError::from_kimg_png)?;
        limits.check_dimensions(info.width, info.height)?;

        decoder.set_transformations(png::Transformations::EXPAND);
        let reader = decoder.read_info().map_err(ImageError::from_kimg_png)?;
        let (color_type, bits) = reader.output_color_type();
        let color_type = match (color_type, bits) {
            (png::ColorType::Grayscale, png::BitDepth::Eight) => ColorType::L8,
            (png::ColorType::Grayscale, png::BitDepth::Sixteen) => ColorType::L16,
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => ColorType::La8,
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Sixteen) => ColorType::La16,
            (png::ColorType::Rgb, png::BitDepth::Eight) => ColorType::Rgb8,
            (png::ColorType::Rgb, png::BitDepth::Sixteen) => ColorType::Rgb16,
            (png::ColorType::Rgba, png::BitDepth::Eight) => ColorType::Rgba8,
            (png::ColorType::Rgba, png::BitDepth::Sixteen) => ColorType::Rgba16,
            _ => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Png.into(),
                        UnsupportedErrorKind::Color(ExtendedColorType::Unknown(bits as u8)),
                    ),
                ))
            }
        };

        Ok(PngDecoder {
            color_type,
            reader,
            limits,
        })
    }
}

impl<R: BufRead + Seek> ImageDecoder for PngDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        self.reader.info().size()
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self
            .reader
            .info()
            .exif_metadata
            .as_ref()
            .map(|x| x.to_vec()))
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(self
            .exif_metadata()?
            .and_then(|chunk| Orientation::from_exif_chunk(&chunk))
            .unwrap_or(Orientation::NoTransforms))
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        self.reader.next_frame(buf).map_err(ImageError::from_kimg_png)?;
        // PNG 为大道在前；16bit 及以上需重排成本机字节序。
        let bpc = self.color_type().bytes_per_pixel() / self.color_type().channel_count();
        match bpc {
            1 => (),
            2 => buf.as_chunks_mut::<2>().0.iter_mut().for_each(|c| {
                *c = u16::from_be_bytes(*c).to_ne_bytes();
            }),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        let info = self.reader.info();
        limits.check_dimensions(info.width, info.height)?;
        self.limits = limits;
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

impl ImageError {
    /// png crate 错误 → kimg `ImageError`（与 image 0.25 分类一致）。
    pub(crate) fn from_kimg_png(err: png::DecodingError) -> ImageError {
        use png::DecodingError::*;
        match err {
            IoError(err) => ImageError::IoError(err),
            err @ Format(_) => ImageError::Decoding(DecodingError::new(ImageFormat::Png.into(), err)),
            err @ Parameter(_) => ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::Generic(err.to_string()),
            )),
            LimitsExceeded => {
                ImageError::Limits(LimitError::from_kind(LimitErrorKind::InsufficientMemory))
            }
        }
    }
}

/// PNG 编码器（供 `write_to(Png)` / `save_buffer_with_format(Png)` 使用）。
pub struct PngEncoder<W: Write> {
    w: W,
}

impl<W: Write> PngEncoder<W> {
    /// 构造。
    pub fn new(w: W) -> PngEncoder<W> {
        PngEncoder { w }
    }

    /// 把指定颜色类型的缓冲编码为 PNG 写入 `w`（16bit 输入按本机字节序）。
    pub fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color: ExtendedColorType,
    ) -> ImageResult<()> {
        let (ct, bits) = match color {
            ExtendedColorType::L8 => (png::ColorType::Grayscale, png::BitDepth::Eight),
            ExtendedColorType::L16 => (png::ColorType::Grayscale, png::BitDepth::Sixteen),
            ExtendedColorType::La8 => (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight),
            ExtendedColorType::La16 => (png::ColorType::GrayscaleAlpha, png::BitDepth::Sixteen),
            ExtendedColorType::Rgb8 => (png::ColorType::Rgb, png::BitDepth::Eight),
            ExtendedColorType::Rgb16 => (png::ColorType::Rgb, png::BitDepth::Sixteen),
            ExtendedColorType::Rgba8 => (png::ColorType::Rgba, png::BitDepth::Eight),
            ExtendedColorType::Rgba16 => (png::ColorType::Rgba, png::BitDepth::Sixteen),
            _ => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Png.into(),
                        UnsupportedErrorKind::Color(color),
                    ),
                ))
            }
        };

        let expected_buffer_len = color.buffer_size(width, height);
        assert_eq!(
            expected_buffer_len,
            buf.len() as u64,
            "Invalid buffer length: expected {expected_buffer_len} got {} for {width}x{height} image",
            buf.len(),
        );

        // 16bit 输入重排为大道在前（PNG 格式要求）。
        let data: std::borrow::Cow<'_, [u8]> = if matches!(
            bits,
            png::BitDepth::Sixteen
        ) && cfg!(target_endian = "little")
        {
            let mut reordered = Vec::with_capacity(buf.len());
            reordered.extend(buf.as_chunks::<2>().0.iter().flat_map(|le| [le[1], le[0]]));
            std::borrow::Cow::Owned(reordered)
        } else {
            std::borrow::Cow::Borrowed(buf)
        };

        let mut encoder = png::Encoder::new(self.w, width, height);
        encoder.set_color(ct);
        encoder.set_depth(bits);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ImageError::IoError(std::io::Error::other(e.to_string())))?;
        writer
            .write_image_data(&data)
            .map_err(|e| ImageError::IoError(std::io::Error::other(e.to_string())))
    }
}

/// 把 `DynamicImage` 编码为 PNG（`write_to(Png)` 路径用）。
pub(crate) fn encode_dynamic_png(img: &DynamicImage, out: &mut impl Write) -> ImageResult<()> {
    let (data, w, h, color) = img.buffer_and_color();
    PngEncoder::new(out).write_image(data, w, h, color)
}
