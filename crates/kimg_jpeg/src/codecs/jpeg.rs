//! JPEG 解码器（`zune-jpeg` 0.5.15 封装；与 image 0.25.10 使用同一后端，保证字节级输出一致）。

use std::io::{BufRead, Seek};
use std::marker::PhantomData;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace as ZuneColorSpace;
use zune_core::options::DecoderOptions;

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::{
    DecodingError, ImageError, ImageResult, LimitError, LimitErrorKind, UnsupportedError,
    UnsupportedErrorKind,
};
use kimg_core::format::ImageFormat;
use kimg_core::limits::{LimitSupport, Limits};
use kimg_core::metadata::Orientation;
use kimg_core::{ImageDecoder, ImageFormatHint};

/// JPEG 解码器。
pub struct JpegDecoder<R> {
    input: Vec<u8>,
    orig_color_space: ZuneColorSpace,
    width: u32,
    height: u32,
    limits: Limits,
    orientation: Option<Orientation>,
    // 与 image 0.25 保持泛型参数占位。
    phantom: PhantomData<R>,
}

impl<R: BufRead + Seek> JpegDecoder<R> {
    /// 构造并读取头信息（输出颜色空间按输入自适应：CMYK 等转 RGB）。
    pub fn new(mut r: R) -> ImageResult<JpegDecoder<R>> {
        let mut input = Vec::new();
        r.read_to_end(&mut input).map_err(ImageError::from)?;

        let options = DecoderOptions::default()
            .set_strict_mode(false)
            .set_max_width(usize::MAX)
            .set_max_height(usize::MAX);
        let mut decoder =
            zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(input.as_slice()), options);
        decoder.decode_headers().map_err(from_kimg_jpeg)?;

        let (width, height) = decoder
            .dimensions()
            .expect("解码头后尺寸必可用");
        let orig_color_space = decoder.input_colorspace().expect("解码头后必有输入颜色空间");

        // 颜色输出：仅 RGB/RGBA/Luma/LumaA 保留，其余（CMYK 等）解码时转 RGB。
        let target = match orig_color_space {
            ZuneColorSpace::RGB
            | ZuneColorSpace::RGBA
            | ZuneColorSpace::Luma
            | ZuneColorSpace::LumaA => orig_color_space,
            _ => ZuneColorSpace::RGB,
        };
        decoder.set_options(decoder.options().jpeg_set_out_colorspace(target));

        Ok(JpegDecoder {
            input,
            orig_color_space,
            width: width as u32,
            height: height as u32,
            limits: Limits::no_limits(),
            orientation: None,
            phantom: PhantomData,
        })
    }
}

impl<R> ImageDecoder for JpegDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        match self.orig_color_space {
            ZuneColorSpace::RGB => ColorType::Rgb8,
            ZuneColorSpace::RGBA => ColorType::Rgba8,
            ZuneColorSpace::Luma => ColorType::L8,
            ZuneColorSpace::LumaA => ColorType::La8,
            _ => ColorType::Rgb8,
        }
    }

    fn original_color_type(&self) -> ExtendedColorType {
        match self.orig_color_space {
            ZuneColorSpace::RGB => ExtendedColorType::Rgb8,
            ZuneColorSpace::RGBA => ExtendedColorType::Rgba8,
            ZuneColorSpace::Luma => ExtendedColorType::L8,
            ZuneColorSpace::LumaA => ExtendedColorType::La8,
            _ => ExtendedColorType::Cmyk8,
        }
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let mut decoder = new_zune_decoder(&self.input, self.orig_color_space, Limits::no_limits());
        decoder.decode_headers().map_err(from_kimg_jpeg)?;
        Ok(decoder.icc_profile())
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let mut decoder = new_zune_decoder(&self.input, self.orig_color_space, Limits::no_limits());
        decoder.decode_headers().map_err(from_kimg_jpeg)?;
        let exif = decoder.exif().cloned();
        self.orientation = Some(
            exif.as_ref()
                .and_then(|exif| Orientation::from_exif_chunk(exif))
                .unwrap_or(Orientation::NoTransforms),
        );
        Ok(exif)
    }

    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let mut decoder = new_zune_decoder(&self.input, self.orig_color_space, Limits::no_limits());
        decoder.decode_headers().map_err(from_kimg_jpeg)?;
        Ok(decoder.xmp().cloned())
    }

    fn iptc_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let mut decoder = new_zune_decoder(&self.input, self.orig_color_space, Limits::no_limits());
        decoder.decode_headers().map_err(from_kimg_jpeg)?;
        Ok(decoder.iptc().cloned())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        if self.orientation.is_none() {
            let _ = self.exif_metadata()?;
        }
        Ok(self.orientation.unwrap())
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let advertised_len = self.total_bytes();
        if u64::try_from(buf.len()).ok() != Some(advertised_len) {
            return Err(ImageError::Decoding(DecodingError::new(
                ImageFormat::Jpeg.into(),
                format!(
                    "Length of the decoded data {} \
                    doesn't match the advertised dimensions of the image \
                    that imply length {advertised_len}",
                    buf.len(),
                ),
            )));
        }

        let mut decoder = new_zune_decoder(&self.input, self.orig_color_space, self.limits);
        decoder.decode_into(buf).map_err(from_kimg_jpeg)?;
        Ok(())
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        let (width, height) = self.dimensions();
        limits.check_dimensions(width, height)?;
        self.limits = limits;
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// 构造受限度限制的 zune 解码器（宽高限制映射到 zune 的 max_width/max_height）。
fn new_zune_decoder(
    input: &[u8],
    orig_color_space: ZuneColorSpace,
    limits: Limits,
) -> zune_jpeg::JpegDecoder<ZCursor<&[u8]>> {
    let target = match orig_color_space {
        ZuneColorSpace::RGB | ZuneColorSpace::RGBA | ZuneColorSpace::Luma | ZuneColorSpace::LumaA => {
            orig_color_space
        }
        _ => ZuneColorSpace::RGB,
    };
    let mut options = DecoderOptions::default()
        .jpeg_set_out_colorspace(target)
        .set_strict_mode(false);
    options = options.set_max_width(
        limits
            .max_image_width
            .map(|w| w as usize)
            .unwrap_or(usize::MAX),
    );
    options = options.set_max_height(
        limits
            .max_image_height
            .map(|h| h as usize)
            .unwrap_or(usize::MAX),
    );
    zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(input), options)
}

/// 把 zune 解码错误映射为 `ImageError`（ImageError 现属 kimg_core，不能带 inherent method，
/// 故改为 crate 内自由函数）。
pub(crate) fn from_kimg_jpeg(err: zune_jpeg::errors::DecodeErrors) -> ImageError {
    use zune_jpeg::errors::DecodeErrors::*;
    match err {
        Unsupported(desc) => ImageError::Unsupported(
            UnsupportedError::from_format_and_kind(
                ImageFormatHint::Exact(ImageFormat::Jpeg),
                UnsupportedErrorKind::GenericFeature(format!("{desc:?}")),
            ),
        ),
        LargeDimensions(_) => {
            ImageError::Limits(LimitError::from_kind(LimitErrorKind::DimensionError))
        }
        err => ImageError::Decoding(DecodingError::new(ImageFormat::Jpeg.into(), err)),
    }
}
