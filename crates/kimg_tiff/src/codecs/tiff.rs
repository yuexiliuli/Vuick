//! TIFF 解码器（`tiff` crate 0.11 封装；覆盖仓库实际使用的常规布局）。

use std::io::{BufRead, Seek};

use tiff::decoder::{DecodingResult, Decoder};
use tiff::tags::Tag;

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::{DecodingError, ImageError, ImageResult, LimitError, LimitErrorKind};
use kimg_core::format::ImageFormat;
use kimg_core::limits::{LimitSupport, Limits};
use kimg_core::metadata::Orientation;
use crate::{ImageDecoder};

/// TIFF 解码器。
pub struct TiffDecoder<R>
where
    R: BufRead + Seek,
{
    dimensions: (u32, u32),
    color_type: ColorType,
    original_color_type: ExtendedColorType,
    inner: Option<Decoder<R>>,
    buffer: DecodingResult,
}

impl<R> TiffDecoder<R>
where
    R: BufRead + Seek,
{
    /// 构造并读取头信息。
    pub fn new(r: R) -> Result<TiffDecoder<R>, ImageError> {
        let mut inner = Decoder::new(r).map_err(from_kimg_tiff)?;

        let dimensions = inner.dimensions().map_err(from_kimg_tiff)?;
        let tiff_color_type = inner.colortype().map_err(from_kimg_tiff)?;

        let color_type = match tiff_color_type {
            tiff::ColorType::Gray(1) | tiff::ColorType::Gray(8) => ColorType::L8,
            tiff::ColorType::Gray(16) => ColorType::L16,
            tiff::ColorType::GrayA(8) => ColorType::La8,
            tiff::ColorType::GrayA(16) => ColorType::La16,
            tiff::ColorType::RGB(8) => ColorType::Rgb8,
            tiff::ColorType::RGB(16) => ColorType::Rgb16,
            tiff::ColorType::RGBA(8) => ColorType::Rgba8,
            tiff::ColorType::RGBA(16) => ColorType::Rgba16,
            tiff::ColorType::CMYK(8) => ColorType::Rgb8,
            tiff::ColorType::CMYK(16) => ColorType::Rgb16,
            _ => {
                return Err(ImageError::Unsupported(
                    kimg_core::error::UnsupportedError::from_format_and_kind(
                        ImageFormat::Tiff.into(),
                        kimg_core::error::UnsupportedErrorKind::Color(ExtendedColorType::Unknown(0)),
                    ),
                ))
            }
        };

        let original_color_type = match tiff_color_type {
            tiff::ColorType::Gray(1) => ExtendedColorType::L1,
            tiff::ColorType::CMYK(8) => ExtendedColorType::Cmyk8,
            tiff::ColorType::CMYK(16) => ExtendedColorType::Cmyk16,
            _ => color_type.into(),
        };

        Ok(TiffDecoder {
            dimensions,
            color_type,
            original_color_type,
            inner: Some(inner),
            buffer: DecodingResult::U8(vec![]),
        })
    }
}

impl<R> ImageDecoder for TiffDecoder<R>
where
    R: BufRead + Seek,
{
    fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn original_color_type(&self) -> ExtendedColorType {
        self.original_color_type
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        if let Some(decoder) = &mut self.inner {
            Ok(decoder.get_tag_u8_vec(Tag::IccProfile).ok())
        } else {
            Ok(None)
        }
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        if let Some(decoder) = &mut self.inner {
            Ok(decoder
                .find_tag(Tag::Orientation)
                .map_err(from_kimg_tiff)?
                .and_then(|v| Orientation::from_exif(v.into_u16().ok()?.min(255) as u8))
                .unwrap_or(Orientation::NoTransforms))
        } else {
            Ok(Orientation::NoTransforms)
        }
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        let (width, height) = self.dimensions();
        limits.check_dimensions(width, height)?;

        let max_alloc = limits.max_alloc.unwrap_or(u64::MAX);
        let total_bytes = u64::from(self.total_bytes());
        let max_intermediate_alloc = max_alloc.saturating_sub(total_bytes);

        let mut tiff_limits: tiff::decoder::Limits = Default::default();
        tiff_limits.decoding_buffer_size =
            usize::try_from(max_alloc - max_intermediate_alloc).unwrap_or(usize::MAX);
        tiff_limits.intermediate_buffer_size =
            usize::try_from(max_intermediate_alloc).unwrap_or(usize::MAX);
        tiff_limits.ifd_value_size = tiff_limits.intermediate_buffer_size;
        self.inner = Some(self.inner.take().unwrap().with_limits(tiff_limits));
        Ok(())
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));

        self.inner
            .as_mut()
            .unwrap()
            .read_image_to_buffer(&mut self.buffer)
            .map_err(from_kimg_tiff)?;

        match self.buffer {
            DecodingResult::U8(v) if self.original_color_type == ExtendedColorType::Cmyk8 => {
                for (cmyk, out) in v.as_chunks::<4>().0.iter().zip(buf.chunks_exact_mut(3)) {
                    out.copy_from_slice(&cmyk_to_rgb(cmyk));
                }
            }
            DecodingResult::U16(v) if self.original_color_type == ExtendedColorType::Cmyk16 => {
                for (cmyk, out) in v
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(buf.chunks_exact_mut(6))
                {
                    out.copy_from_slice(bytemuck_cast::<u16>(&cmyk_to_rgb16(cmyk)));
                }
            }
            DecodingResult::U8(v) if self.original_color_type == ExtendedColorType::L1 => {
                let width = self.dimensions.0;
                let row_bytes = width.div_ceil(8);
                for (in_row, out_row) in v
                    .chunks_exact(row_bytes as usize)
                    .zip(buf.chunks_exact_mut(width as usize))
                {
                    expand_bits_into(in_row, out_row);
                }
            }
            DecodingResult::U8(v) => buf.copy_from_slice(&v),
            DecodingResult::U16(v) => buf.copy_from_slice(as_u8(&v)),
            DecodingResult::U32(v) => buf.copy_from_slice(as_u8(&v)),
            DecodingResult::I8(v) => buf.copy_from_slice(as_u8(&v)),
            DecodingResult::I16(v) => buf.copy_from_slice(as_u8(&v)),
            DecodingResult::I32(v) => buf.copy_from_slice(as_u8(&v)),
            DecodingResult::F32(v) => buf.copy_from_slice(as_u8(&v)),
            _ => {
                return Err(ImageError::Unsupported(
                    kimg_core::error::UnsupportedError::from_format_and_kind(
                        ImageFormat::Tiff.into(),
                        kimg_core::error::UnsupportedErrorKind::GenericFeature(
                            "TIFF 位深/类型组合未实现".into(),
                        ),
                    ),
                ))
            }
        }

        if let Some(_) = buf.len().checked_sub(buf.len()) {
            // 占位避免死代码告警（完整长度由 total_bytes 断言保证）。
        }

        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

fn cmyk_to_rgb(cmyk: &[u8; 4]) -> [u8; 3] {
    let c = f32::from(cmyk[0]);
    let m = f32::from(cmyk[1]);
    let y = f32::from(cmyk[2]);
    let kf = 1. - f32::from(cmyk[3]) / 255.;
    [
        ((255. - c) * kf) as u8,
        ((255. - m) * kf) as u8,
        ((255. - y) * kf) as u8,
    ]
}

fn cmyk_to_rgb16(cmyk: &[u16; 4]) -> [u16; 3] {
    let c = f32::from(cmyk[0]);
    let m = f32::from(cmyk[1]);
    let y = f32::from(cmyk[2]);
    let kf = 1. - f32::from(cmyk[3]) / 65535.;
    [
        ((65535. - c) * kf) as u16,
        ((65535. - m) * kf) as u16,
        ((65535. - y) * kf) as u16,
    ]
}

/// 1bit 位图按行展开为 0/255 字节（行宽由 out_row 长度确定）。
fn expand_bits_into(in_row: &[u8], out_row: &mut [u8]) {
    for (x, out) in out_row.iter_mut().enumerate() {
        let byte = in_row[x / 8];
        let bit = (byte >> (7 - (x % 8))) & 0x1;
        *out = if bit == 1 { 255 } else { 0 };
    }
}

fn bytemuck_cast<T: Copy>(v: &[T]) -> &[u8] {
    // T 规模已知（u16/f32 类），用指针重解释拷贝。
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn as_u8<T: Copy>(v: &[T]) -> &[u8] {
    bytemuck_cast(v)
}

/// 把 tiff crate 错误映射为 `ImageError`（ImageError 现属 kimg_core，不能带 inherent method，
/// 故改为 crate 内自由函数）。
pub(crate) fn from_kimg_tiff(err: tiff::TiffError) -> ImageError {
    match err {
        tiff::TiffError::IoError(err) => ImageError::IoError(err),
        err @ (tiff::TiffError::FormatError(_)
        | tiff::TiffError::IntSizeError
        | tiff::TiffError::UsageError(_)) => {
            ImageError::Decoding(DecodingError::new(ImageFormat::Tiff.into(), err))
        }
        tiff::TiffError::UnsupportedError(desc) => {
            ImageError::Unsupported(kimg_core::error::UnsupportedError::from_format_and_kind(
                ImageFormat::Tiff.into(),
                kimg_core::error::UnsupportedErrorKind::GenericFeature(desc.to_string()),
            ))
        }
        tiff::TiffError::LimitsExceeded => {
            ImageError::Limits(LimitError::from_kind(LimitErrorKind::InsufficientMemory))
        }
    }
}
