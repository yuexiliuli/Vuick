//! 自研 JPEG 解码器（`jpeg`，baseline 顺序帧原为 jpeg2，后并入本模块）。
//!
//! 与旧 `zune-jpeg` 后端输出**逐字节恒等**（IDCT/上采样/颜色换算三处取整全等价），
//! 完全不依赖任何外部 JPEG 解码库。模块内：段解析（`segment`）、Huffman（`huffman`）、
//! 位流（`bitstream`）、IDCT（`idct`）、上采样（`upsampler`）、颜色换算（`color`）、
//! MCU 主循环（`mcu`）。
//!
//! 范围：仅 baseline 顺序 JPEG（含灰度/4:4:4/4:2:0/restart marker/非交织多 SOS）。
//! progressive（SOF2）/算术编码/lossless/hierarchical/12-bit → 返回 `Unsupported`。

mod avx2;
mod bitstream;
mod color;
mod cs;
mod cs_avx2;
mod error;
mod huffman;
mod idct;
mod io;
mod mcu;
#[cfg(test)]
mod prescan_probe;
mod segment;
mod upsampler;

use std::borrow::Cow;
use std::io::{BufRead, Seek};

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::{
    DecodingError, ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind,
};
use kimg_core::format::ImageFormat;
use kimg_core::limits::{LimitSupport, Limits};
use kimg_core::metadata::Orientation;
use kimg_core::ImageDecoder;
use kimg_core::ImageFormatHint;

use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::mcu::{decode_baseline, decode_progressive};
use crate::codecs::jpeg::segment::{ColorSpace, Decoder, HeaderState, Stage};

/// 自研 JPEG 解码器。
///
/// 输入字节两种持有方式（C1 零拷贝）：
/// - `from_bytes(&'a [u8])`：直接借用调用方切片（`load_from_memory` 内存路径，零拷贝）；
/// - `new(reader: R)`：自通用读取器读入自有输入（向后兼容旧 API；对内存切片读者是一次复制）。
///
/// 构造（`new`/`from_bytes`）只做一遍 header 解析，结果缓存于 `seed: HeaderState`，
/// `read_image` 复用它构造解码用 `Decoder`（C2 免第二遍解析）。
pub struct JpegDecoder<'a> {
    input: Cow<'a, [u8]>,
    /// 头部解析结果（`extract_header_state`），解码时 `restore_header_state` 复用。
    seed: HeaderState,
    width: u32,
    height: u32,
    color_type: ColorType,
    orig_color_type: ExtendedColorType,
    limits: Limits,
    orientation: Option<Orientation>,
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    iptc: Option<Vec<u8>>,
    /// 输入颜色空间（决定输出颜色类型的映射）。
    input_colorspace: ColorSpace,
}

impl<'a> JpegDecoder<'a> {
    /// 零拷贝构造：借用调用方字节（内存路径，与 C# 参照同口径无输入复制）。
    pub fn from_bytes(data: &'a [u8]) -> ImageResult<JpegDecoder<'a>> {
        JpegDecoder::build(Cow::Borrowed(data))
    }
}

impl JpegDecoder<'static> {
    /// 通用读取器构造（向后兼容旧 `new(reader)`）：`R: BufRead + Seek`，读入自有输入。
    pub fn new<R: BufRead + Seek>(mut r: R) -> ImageResult<JpegDecoder<'static>> {
        let mut input = Vec::new();
        r.read_to_end(&mut input).map_err(ImageError::from)?;
        JpegDecoder::build(Cow::Owned(input))
    }
}

impl<'a> JpegDecoder<'a> {
    /// 共享构造：一遍 header 解析并缓存解码种子（C1 输入不复制、C2 解析只一遍）。
    fn build(input: Cow<'a, [u8]>) -> ImageResult<JpegDecoder<'a>> {
        let mut dec = Decoder::new(input.as_ref());
        dec.decode_headers_internal().map_err(from_kimg_jpeg)?;

        let input_colorspace = dec.input_colorspace;
        let width = dec.width as u32;
        let height = dec.height as u32;

        let orientation = dec
            .exif_data
            .as_ref()
            .and_then(|exif| Orientation::from_exif_chunk(exif))
            .unwrap_or(Orientation::NoTransforms);

        // 解码种子先提取（move 出 components/huffman/量化表），再取 metadata（互不干扰）。
        let seed = dec.extract_header_state();
        let icc = dec.icc_profile();
        let exif = dec.exif_data.take();
        let xmp = dec.xmp_data.take();
        let iptc = dec.iptc_data.take();

        Ok(JpegDecoder {
            color_type: color_type_for(&input_colorspace),
            orig_color_type: orig_color_type_for(&input_colorspace),
            input_colorspace,
            width,
            height,
            limits: Limits::no_limits(),
            orientation: Some(orientation),
            icc,
            exif,
            xmp,
            iptc,
            input,
            seed,
        })
    }
}

impl<'a> ImageDecoder for JpegDecoder<'a> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn original_color_type(&self) -> ExtendedColorType {
        self.orig_color_type
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc.clone())
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.exif.clone())
    }

    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.xmp.clone())
    }

    fn iptc_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.iptc.clone())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        if self.orientation.is_none() {
            self.orientation = self
                .exif
                .as_ref()
                .and_then(|exif| Orientation::from_exif_chunk(exif))
                .or(Some(Orientation::NoTransforms));
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

        self.limits.check_dimensions(self.width, self.height)?;

        let target_out = output_for_input(self.input_colorspace);
        // C2：复用构造期缓存好的头部状态（免第二遍 header 解析）；
        // `decode_baseline/progressive` 内部二次 `decode_headers_internal` 亦短路。
        let mut dec = Decoder::new(&self.input);
        let t_header = dec.stage.timer();
        dec.restore_header_state(self.seed);
        dec.stage.stop(Stage::Header, t_header);
        if dec.is_progressive {
            decode_progressive(&mut dec, target_out, buf).map_err(from_kimg_jpeg)?;
        } else {
            decode_baseline(&mut dec, target_out, buf).map_err(from_kimg_jpeg)?;
        }
        dec.stage.print();
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

/// 输入颜色空间 -> 解码输出 `ColorType`（与旧 wrapper 完全一致）。
fn color_type_for(orig: &ColorSpace) -> ColorType {
    match orig {
        ColorSpace::RGB => ColorType::Rgb8,
        ColorSpace::RGBA => ColorType::Rgba8,
        ColorSpace::Luma => ColorType::L8,
        ColorSpace::LumaA => ColorType::La8,
        _ => ColorType::Rgb8,
    }
}

/// 输入颜色空间 -> 原始 `ExtendedColorType`（与旧 wrapper 一致：YCbCr/CMYK 等输出 Cmyk8）。
fn orig_color_type_for(orig: &ColorSpace) -> ExtendedColorType {
    match orig {
        ColorSpace::RGB => ExtendedColorType::Rgb8,
        ColorSpace::RGBA => ExtendedColorType::Rgba8,
        ColorSpace::Luma => ExtendedColorType::L8,
        ColorSpace::LumaA => ExtendedColorType::La8,
        _ => ExtendedColorType::Cmyk8,
    }
}

/// 由输入颜色空间决定解码输出目标（与旧 wrapper 的 target 选择一致）。
fn output_for_input(orig: ColorSpace) -> ColorSpace {
    match orig {
        ColorSpace::RGB | ColorSpace::RGBA | ColorSpace::Luma | ColorSpace::LumaA => orig,
        _ => ColorSpace::RGB,
    }
}

/// 把内部 `DecodeError` 映射为 `kimg_core::ImageError`（对齐旧 `from_kimg_jpeg`）。
pub(crate) fn from_kimg_jpeg(err: DecodeError) -> ImageError {
    match err {
        DecodeError::Unsupported(desc) => ImageError::Unsupported(
            UnsupportedError::from_format_and_kind(
                ImageFormatHint::Exact(ImageFormat::Jpeg),
                UnsupportedErrorKind::GenericFeature(desc),
            ),
        ),
        err => ImageError::Decoding(DecodingError::new(ImageFormat::Jpeg.into(), err)),
    }
}
