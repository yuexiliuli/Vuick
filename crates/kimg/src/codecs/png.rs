//! PNG 解码器与编码器。
//!
//! 解码热路径 = 自研引擎 `pngsimd`（SimdCodecs 移植：SIMD 反滤波 + EXPAND 展开，字节级对齐 png crate
//! `Transformations::EXPAND`）；`png` crate 仅保留给编码（`PngEncoder`）与测试对照，不再进解码热路径。
//! 对外 API / 语义与现有一致：`PngDecoder` 的 new/set_limits/dimensions/color_type/total_bytes/
//! read_image/read_image_with_progress/orientation 签名不变。

use std::io::{BufRead, Seek, Write};

use crate::codecs::pngsimd::PngEngine;
use crate::color::{ColorType, ExtendedColorType};
use crate::dynamic_image::DynamicImage;
use crate::error::{
    ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind,
};
use crate::format::ImageFormat;
use crate::limits::{LimitSupport, Limits};
use crate::metadata::Orientation;
use crate::ImageDecoder;

/// PNG 解码器：构造即读取头信息，输出颜色类型与 image 0.25 一致（EXPAND 变换、16bit 保留）。
pub struct PngDecoder<R: BufRead + Seek> {
    color_type: ColorType,
    engine: PngEngine,
    limits: Limits,
    reader: std::marker::PhantomData<R>,
}

impl<R: BufRead + Seek> PngDecoder<R> {
    /// 无限制构造。
    pub fn new(r: R) -> ImageResult<PngDecoder<R>> {
        Self::with_limits(r, Limits::no_limits())
    }

    /// 带限制构造（限制在 `read_image` 前校验尺寸；自研引擎在解析阶段已做防炸弹拦截）。
    pub fn with_limits(mut r: R, limits: Limits) -> ImageResult<PngDecoder<R>> {
        limits.check_support(&LimitSupport::default())?;
        let engine = PngEngine::parse(&mut r)?;
        limits.check_dimensions(engine.width(), engine.height())?;
        let color_type = engine.color_type();
        Ok(PngDecoder {
            color_type,
            engine,
            limits,
            reader: std::marker::PhantomData,
        })
    }
}

impl<R: BufRead + Seek> ImageDecoder for PngDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.engine.width(), self.engine.height())
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.engine.exif())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(self
            .exif_metadata()?
            .and_then(|chunk| Orientation::from_exif_chunk(&chunk))
            .unwrap_or(Orientation::NoTransforms))
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        self.engine.decode(buf, self.limits.max_alloc)?;
        // PNG 为大道在前；16bit 及以上需重排成本机字节序。
        let bpc = self.color_type.bytes_per_pixel() / self.color_type.channel_count();
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
        limits.check_dimensions(self.engine.width(), self.engine.height())?;
        self.limits = limits;
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// PNG 编码器（供 `write_to(Png)` / `save_buffer_with_format(Png)` 使用；沿用 png crate）。
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
