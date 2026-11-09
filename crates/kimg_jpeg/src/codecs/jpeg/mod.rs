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

impl<'a> JpegDecoder<'a> {
    /// 按展示降档分母 **DCT 缩放** 解码（`den∈{2,4,8}`：half-band 抗混叠截断，libjpeg
    /// `jpeg_idct_4x4/2x2/1x1` 模型，见 `cs::idct4x4/idct2x2/idct1x1_dc`）。输出 = 与 box
    /// 网格同构的 `ceil(w/den)×ceil(h/den)` 自然 color_type 字节（走与全解同款
    /// `pack_output_row` 契约；缩后后处理单线程）。输出尺寸/B1 契约由 [`Self::scaled_dimensions`]
    /// 与 [`Self::read_image_scaled`] 一致口径给出。
    ///
    /// 失败语义（上层照既有「Err→全解兜底」零耦合回退）：
    /// - `den ∉ {2,4,8}` → `Unsupported`（其余 den 返回 Unsupported → 上层全解兜底，不降级）；
    /// - `KIMG_JPEG_SCALED=0|false|off` → `Unsupported`（env 门控整收在本入口内，K93 快道+基线）；
    /// - 其余解码错误按常规 `ImageResult` 上报（baseline SOF2 渐进已支持 scaled；
    ///   非交织多 SOS baseline（首 scan 非全分量）仍 `Unsupported` → 全解兜底）。
    ///
    /// 内存如实声明：progressive scaled 内部系数平面（`block`/`prog_mask` 整帧 i16/u64 缓冲）
    /// 按全图尺寸分配——分配与全解同量级、未按 den 独立设界（与全解暴露持平）。
    pub fn read_image_scaled(mut self, den: u8, buf: &mut [u8]) -> ImageResult<()> {
        if !jpeg_scaled_enabled() {
            return Err(unsupported_jpeg_scaled("KIMG_JPEG_SCALED 关闭（回退全解+盒式）"));
        }
        // 入口先校验 den∈{2,4,8}（0/非 2 幂/超限 → Unsupported，不降级）
        if !matches!(den, 2 | 4 | 8) {
            return Err(unsupported_jpeg_scaled(&format!(
                "JPEG DCT 缩放仅支持 den=2/4/8（收到 den={den}）"
            )));
        }
        let (sw, sh) = scaled_dims(self.width, self.height, den);
        let out_bpp = usize::from(self.color_type.bytes_per_pixel());
        let advertised = (sw as usize)
            .saturating_mul(sh as usize)
            .saturating_mul(out_bpp);
        if u64::try_from(buf.len()).ok() != Some(advertised as u64) {
            return Err(ImageError::Decoding(DecodingError::new(
                ImageFormat::Jpeg.into(),
                format!(
                    "Scaled length {} doesn't match predicted length {advertised} \
                    (den={den}: {sw}×{sh} × {out_bpp}B/px)",
                    buf.len(),
                ),
            )));
        }
        // K124 真实护栏：缩放输出字节数须 ≤ `limits.max_alloc`。注意 `check_dimensions`
        // 对 `Limits::default()` 的宽高域是 `None`（恒过）——仅作尺寸上限约束，**不足**为
        // 分配护栏；故这里直接对 `max_alloc` 扣预算（缺省 `no_limits=None` 无约束，由调用方
        // 如 image_viewer 先 `set_limits(Limits::default())` 使默认 512MiB 生效）。超限报
        // `Limits` 错误，绝不先按不可信头尺寸分配（本入口不分配输出，由调用方按本长度预配）。
        self.limits.reserve(advertised as u64)?;

        let target_out = output_for_input(self.input_colorspace);
        let mut dec = Decoder::new(&self.input);
        let t_header = dec.stage.timer();
        dec.restore_header_state(self.seed);
        dec.stage.stop(Stage::Header, t_header);
        dec.scaled_den = Some(den);
        if dec.is_progressive {
            decode_progressive(&mut dec, target_out, buf).map_err(from_kimg_jpeg)?;
        } else {
            decode_baseline(&mut dec, target_out, buf).map_err(from_kimg_jpeg)?;
        }
        dec.stage.print();
        Ok(())
    }

    /// 缩放输出尺寸（与 box 网格精确同构：顶左对齐、`ceil(iw/den)×ceil(ih/den)`，B1）。
    /// `den∈{2,4,8}` 返回 `Some`；其它返回 `None`。
    pub fn scaled_dimensions(&self, den: u8) -> Option<(u32, u32)> {
        matches!(den, 2 | 4 | 8).then(|| scaled_dims(self.width, self.height, den))
    }
}

/// `ceil(w/den)×ceil(h/den)`（den 缩放 / box 网格同构尺寸，顶左对齐；den∈{2,4,8}）。
fn scaled_dims(w: u32, h: u32, den: u8) -> (u32, u32) {
    (w.div_ceil(u32::from(den)), h.div_ceil(u32::from(den)))
}

/// `KIMG_JPEG_SCALED=0|false|off` → 关闭 DCT 缩放（回退全解+盒式），缺省开。
#[inline]
fn jpeg_scaled_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("KIMG_JPEG_SCALED")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off"))
            .unwrap_or(true)
    })
}

/// JPEG DCT 缩放不支持错误（`UnsupportedErrorKind::GenericFeature`；Display 含
/// "does not support" → 上层/测试按 Unsupported 识别回退）。
fn unsupported_jpeg_scaled(desc: &str) -> ImageError {
    ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        ImageFormatHint::Exact(ImageFormat::Jpeg),
        UnsupportedErrorKind::GenericFeature(desc.to_string()),
    ))
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
