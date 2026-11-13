//! TIFF 解码器（自研核心，A1+A2：L8/Rgb8/Rgba8 × Uncompressed/PackBits/LZW/Deflate ×
//! Horizontal predictor(2) × Chunky × 8bit）。
//!
//! 范围墙：本任务不做 16bit/CMYK/La8/palette/CCITT/JPEG/tile/Separate/BigTIFF/
//! Predictor(3) 浮点差分——这些布局对 `new()` 一律显式返回 `Unsupported`（不实现、不 panic）。
//! 字节恒等验收：`crates/kimg_tiff/tests/tiff_parity.rs` 以 tiff crate（dev oracle）逐字节对拍。

use std::io::{BufRead, Seek, SeekFrom};

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::{
    DecodingError, ImageError, ImageResult, LimitError, LimitErrorKind, UnsupportedError,
    UnsupportedErrorKind,
};
use kimg_core::format::ImageFormat;
use kimg_core::limits::{LimitSupport, Limits};
use kimg_core::metadata::Orientation;

use crate::ImageDecoder;

// ---------------------------------------------------------------------------
// 端序与错误
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u16(self, b: &[u8]) -> u16 {
        let a: [u8; 2] = b[..2].try_into().expect("u16 切片长度恒 2");
        match self {
            ByteOrder::Little => u16::from_le_bytes(a),
            ByteOrder::Big => u16::from_be_bytes(a),
        }
    }
    fn u32(self, b: &[u8]) -> u32 {
        let a: [u8; 4] = b[..4].try_into().expect("u32 切片长度恒 4");
        match self {
            ByteOrder::Little => u32::from_le_bytes(a),
            ByteOrder::Big => u32::from_be_bytes(a),
        }
    }
}

fn decoding(msg: impl Into<String>) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormat::Tiff.into(), msg.into()))
}

fn unsupported(msg: impl Into<String>) -> ImageError {
    ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        ImageFormat::Tiff.into(),
        UnsupportedErrorKind::GenericFeature(msg.into()),
    ))
}

fn limits_exceeded() -> ImageError {
    ImageError::Limits(LimitError::from_kind(LimitErrorKind::InsufficientMemory))
}

fn io_eof_err() -> ImageError {
    ImageError::IoError(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "TIFF 数据截断",
    ))
}

fn int_err(_: std::num::TryFromIntError) -> ImageError {
    decoding("TIFF 尺寸超出平台可表示范围")
}

// ---------------------------------------------------------------------------
// IFD 解析
// ---------------------------------------------------------------------------

/// TIFF 字段类型字节数（未知类型返回 None）。
fn type_size(typ: u16) -> Option<usize> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,  // BYTE / ASCII / SBYTE / UNDEFINED
        3 | 8 => 2,          // SHORT / SSHORT
        4 | 9 | 11 | 13 => 4, // LONG / SLONG / FLOAT / IFD
        5 | 10 | 12 => 8,    // RATIONAL / SRATIONAL / DOUBLE
        _ => return None,
    })
}

/// 一条已解析的 IFD 条目：value 为端序原样的原始字节（内联或经偏移读取）。
struct IfdEntry {
    tag: u16,
    typ: u16,
    value: Vec<u8>,
}

fn read_range<R: BufRead + Seek>(
    r: &mut R,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ImageError> {
    r.seek(SeekFrom::Start(offset))?;
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}

/// 解析 IFD0 全部条目（未知类型条目跳过、不报错）。
fn parse_ifd<R: BufRead + Seek>(
    r: &mut R,
    order: ByteOrder,
    ifd0: u64,
    file_len: u64,
) -> Result<Vec<IfdEntry>, ImageError> {
    if ifd0.checked_add(2).map_or(true, |e| e > file_len) {
        return Err(decoding("IFD 偏移越界"));
    }
    let count = order.u16(&read_range(r, ifd0, 2)?) as u64;
    let table_bytes = 12u64
        .checked_mul(count)
        .ok_or_else(|| decoding("IFD 条目数过大"))?;
    if ifd0 + 2 + table_bytes > file_len {
        return Err(decoding("IFD 表越界"));
    }
    let mut entries = Vec::with_capacity(count as usize);
    for i in 0..count {
        let off = ifd0 + 2 + i * 12;
        let raw = read_range(r, off, 12)?;
        let tag = order.u16(&raw[..2]);
        let typ = order.u16(&raw[2..4]);
        let n = u64::from(order.u32(&raw[4..8]));
        let Some(ts) = type_size(typ) else {
            continue; // 未知类型：跳过该条目
        };
        let total = ts as u64 * n;
        let value = if total == 0 {
            Vec::new()
        } else if total <= 4 {
            raw[8..8 + total as usize].to_vec()
        } else {
            let off = u64::from(order.u32(&raw[8..12]));
            if off.checked_add(total).map_or(true, |e| e > file_len) {
                return Err(decoding("IFD 值偏移越界"));
            }
            read_range(r, off, total as usize)?
        };
        entries.push(IfdEntry { tag, typ, value });
    }
    Ok(entries)
}

/// 读取条目为无符号整数数组（BYTE/SHORT/LONG/IFD）。
fn int_values(entry: &IfdEntry, order: ByteOrder) -> ImageResult<Vec<u64>> {
    let v = &entry.value;
    let out = match entry.typ {
        1 | 7 => v.iter().map(|&x| u64::from(x)).collect(),
        3 => {
            if v.len() % 2 != 0 {
                return Err(decoding("SHORT 值长度不是 2 的倍数"));
            }
            v.chunks_exact(2).map(|c| u64::from(order.u16(c))).collect()
        }
        4 | 13 => {
            if v.len() % 4 != 0 {
                return Err(decoding("LONG 值长度不是 4 的倍数"));
            }
            v.chunks_exact(4).map(|c| u64::from(order.u32(c))).collect()
        }
        other => {
            return Err(unsupported(format!(
                "IFD 标签 {} 的字段类型 {} 不支持",
                entry.tag, other
            )))
        }
    };
    Ok(out)
}

// ---------------------------------------------------------------------------
// 解码器
// ---------------------------------------------------------------------------

/// A1 支持的光度布局与灰度反向标志。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhotoLayout {
    /// 灰度（WhiteIsZero 需逐字节反向）。
    Gray { white_is_zero: bool },
    /// RGB3 通道（无附加样本，逐像素直拷）。
    Rgb,
    /// RGBA4 通道（无附加样本 / ExtraSamples=alpha，逐像素 4 字节直拷）。
    Rgba,
    /// 普通 RGB + 1 个非 alpha 附加样本（SPP4 + ExtraSamples=[0]）：输出 RGB3，
    /// 每像素丢弃最后一个样本（与 tiff crate 对 SPP4 非 alpha 的 RGB 压缩行为一致）。
    RgbCompact,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TiffCompression {
    None,
    PackBits,
    Lzw,
    /// 8=AdobeDeflate / 32946=ZlibPkzip：两者均为 zlib 封装 deflate 流。
    Deflate,
}

/// Strip 字节数的两种来源：显式标签（长度受文件界约束）或未压缩满行推断（惰性，不放大分配）。
enum StripByteCounts {
    Explicit(Vec<u64>),
    FullRows,
}

struct StripInfo {
    offset: u64,
    byte_count: u64,
}

/// TIFF 解码器（A1 自研核心）。
pub struct TiffDecoder<R>
where
    R: BufRead + Seek,
{
    reader: R,
    width: u32,
    height: u32,
    color_type: ColorType,
    photo: PhotoLayout,
    /// 每行编码字节数（= width × samples）。
    row_bytes: u32,
    /// 每行样本数（SamplesPerPixel，= row_bytes / width）。
    samples: u32,
    /// Horizontal predictor：1=无，2=水平差分（按行还原）。
    predictor: u8,
    /// 每行输出字节数（= width × 输出通道数；RgbCompact 时每像素丢弃末样本）。
    out_row_bytes: u32,
    rows_per_strip: u32,
    strips: Vec<StripInfo>,
    compression: TiffCompression,
    icc_profile: Option<Vec<u8>>,
    orientation: Orientation,
    limits: Limits,
}

impl<R> TiffDecoder<R>
where
    R: BufRead + Seek,
{
    /// 构造并解析文件头 + IFD0（layout/limits 外的字段均在此校验）。
    pub fn new(mut r: R) -> Result<TiffDecoder<R>, ImageError> {
        r.seek(SeekFrom::Start(0))?;
        let mut header = [0u8; 8];
        r.read_exact(&mut header)?;
        let (order, ifd0) = match &header[..4] {
            [0x49, 0x49, 0x2a, 0x00] => (ByteOrder::Little, u32::from_le_bytes(header[4..8].try_into().unwrap())),
            [0x4d, 0x4d, 0x00, 0x2a] => (ByteOrder::Big, u32::from_be_bytes(header[4..8].try_into().unwrap())),
            [0x49, 0x49, 0x2b, 0x00] | [0x4d, 0x4d, 0x00, 0x2b] => {
                return Err(unsupported("BigTIFF 不支持（后置任务）"))
            }
            _ => return Err(decoding("无效 TIFF 魔数")),
        };
        let file_len = r.seek(SeekFrom::End(0))?;
        if file_len < 8 {
            return Err(decoding("TIFF 文件过短"));
        }
        let entries = parse_ifd(&mut r, order, u64::from(ifd0), file_len)?;

        // ---- 必读基础标签 ----
        let get_uint = |tag: u16| -> ImageResult<Option<u64>> {
            match entries.iter().find(|e| e.tag == tag) {
                Some(e) => {
                    let v = int_values(e, order)?;
                    if v.len() != 1 {
                        Err(decoding(format!("TIFF 标签 {tag} 期望单值")))
                    } else {
                        Ok(Some(v[0]))
                    }
                }
                None => Ok(None),
            }
        };
        let require_uint = |tag: u16, name: &str| -> ImageResult<u64> {
            get_uint(tag)?.ok_or_else(|| decoding(format!("缺少必需标签 {name}({tag})")))
        };
        let get_uints = |tag: u16| -> ImageResult<Option<Vec<u64>>> {
            match entries.iter().find(|e| e.tag == tag) {
                Some(e) => Ok(Some(int_values(e, order)?)),
                None => Ok(None),
            }
        };

        let width = require_uint(256, "ImageWidth")?;
        let height = require_uint(257, "ImageLength")?;
        if width == 0 || height == 0 {
            return Err(decoding(format!("无效尺寸 {width}x{height}")));
        }

        let bits = get_uints(258)?.unwrap_or_else(|| vec![1]);
        let samples = require_uint(277, "SamplesPerPixel")?;
        if samples == 0 {
            return Err(decoding("SamplesPerPixel 为 0"));
        }
        // BitsPerSample：单值应用全部样本；其余必须与 samples 对齐。
        if bits.len() != 1 && bits.len() != samples as usize {
            return Err(decoding("BitsPerSample 长度与 SamplesPerPixel 不一致"));
        }
        let all8 = bits.iter().all(|&b| b == 8);
        if !all8 {
            return Err(unsupported("仅支持 8bit 采样位数（16bit 等放后置）"));
        }

        let extra = get_uints(338)?.unwrap_or_default();
        if extra.len() as u64 > samples {
            return Err(decoding("ExtraSamples 数量超过 SamplesPerPixel"));
        }

        let compression = match get_uint(259)?.unwrap_or(1) {
            1 => TiffCompression::None,
            5 => TiffCompression::Lzw,
            // AdobeDeflate(8) 与 ZlibPkzip(32946) 同为 zlib 封装 deflate 流。
            8 | 32946 => TiffCompression::Deflate,
            32773 => TiffCompression::PackBits,
            other => {
                return Err(unsupported(format!(
                    "压缩方法 {other} 未实现（CCITT/JPEG 放后置）"
                )))
            }
        };

        let planar = get_uint(284)?.unwrap_or(1);
        if planar != 1 {
            if planar == 2 {
                return Err(unsupported("PlanarConfiguration=Separate 不支持"));
            }
            return Err(decoding(format!("未知 PlanarConfiguration {planar}")));
        }
        let predictor = get_uint(317)?.unwrap_or(1);
        match predictor {
            1 => {}
            2 => { /* Horizontal predictor：8bit 各采样通道按行差分还原，见 decode_strip */ }
            3 => return Err(unsupported("浮点差分 Predictor(3) 不支持（放后置）")),
            other => return Err(decoding(format!("未知 Predictor {other}"))),
        }
        let sample_format = get_uints(339)?.unwrap_or_else(|| vec![1]);
        if !sample_format.iter().all(|&f| f == 1) {
            return Err(unsupported("SampleFormat 仅支持 Uint"));
        }

        // ---- 布局 -> 输出色型 ----
        let photo_u16 = require_uint(262, "PhotometricInterpretation")?;
        if !matches!(photo_u16, 0 | 1 | 2) {
            return Err(unsupported(
                "PhotometricInterpretation 仅支持 WhiteIsZero/BlackIsZero/RGB（Palette/CMYK/YCbCr 放后置）",
            ));
        }
        let (color_type, photo_layout) = match photo_u16 {
            0 | 1 => {
                if samples == 1 && extra.is_empty() {
                    (ColorType::L8, PhotoLayout::Gray { white_is_zero: photo_u16 == 0 })
                } else {
                    return Err(unsupported("灰度+附加通道(La8) 放后置"));
                }
            }
            2 => match samples {
                // photometric_samples=3：无额外样本 → RGB；带 alpha 额外样本(RGBA8) → RGBA
                3 if extra.is_empty() => (ColorType::Rgb8, PhotoLayout::Rgb),
                4 => match extra.as_slice() {
                    // photometric_samples=4 → RGBA（SPP4 无额外样本）
                    [] => (ColorType::Rgba8, PhotoLayout::Rgba),
                    // photometric_samples=3 + alpha（Unspecified 除外）→ RGBA（4 字节直拷）
                    [1] | [2] => (ColorType::Rgba8, PhotoLayout::Rgba),
                    // photometric_samples=3 + 非 alpha 附加样本（=Unspecified）→ 输出 RGB，丢弃末样本
                    [0] => (ColorType::Rgb8, PhotoLayout::RgbCompact),
                    _ => return Err(unsupported("SPP4 的 ExtraSamples 组合不支持")),
                },
                _ => return Err(unsupported("RGB 采样数/ExtraSamples 组合不支持")),
            },
            _ => unreachable!(),
        };

        // ---- strip 布局 ----
        let row_bytes_u64 = width as u64 * samples; // 8bit 采样，每样本 1 字节
        let row_bytes = u32::try_from(row_bytes_u64).map_err(int_err)?;
        let out_channels = match photo_layout {
            PhotoLayout::Gray { .. } => 1u32,
            PhotoLayout::Rgb | PhotoLayout::RgbCompact => 3,
            PhotoLayout::Rgba => 4,
        };
        let out_row_bytes = u32::try_from(width as u64 * u64::from(out_channels)).map_err(int_err)?;
        let rps_raw = get_uint(278)?.unwrap_or(height as u64);
        // RowsPerStrip=0 按 TIFF 规范视为整图高度。
        let rps = if rps_raw == 0 { height as u64 } else { rps_raw };
        let rps = u32::try_from(rps).map_err(int_err)?;
        let strip_count = (height as u64).div_ceil(rps as u64);

        // 273 长度受文件界约束（IFD 值长度 ≤ 文件），先于任何规模分配做一致性校验；
        // 与 tiff crate 一致：多 strip 时单值数组不复制，长度不符即 Err（消除内存放大面/静默错解）。
        let offsets = get_uints(273)?
            .ok_or_else(|| decoding("缺少必需标签 StripOffsets(273)"))?;
        if offsets.len() as u64 != strip_count {
            return Err(decoding("StripOffsets 条目数与实际 strip 数不一致"));
        }

        // 279 缺省仅在未压缩时按满行**惰性**推断（不放大分配）；压缩文件缺 279 一律 Err。
        let byte_counts = match get_uints(279)? {
            Some(bc) => {
                if bc.len() as u64 != strip_count {
                    return Err(decoding("StripByteCounts 条目数与实际 strip 数不一致"));
                }
                StripByteCounts::Explicit(bc)
            }
            None => {
                if compression != TiffCompression::None {
                    return Err(decoding("缺少必需标签 StripByteCounts(279)（压缩文件必须有）"));
                }
                StripByteCounts::FullRows
            }
        };

        let strips = offsets
            .iter()
            .enumerate()
            .map(|(i, &offset)| {
                let byte_count = match &byte_counts {
                    StripByteCounts::Explicit(bc) => bc[i],
                    StripByteCounts::FullRows => {
                        let start = i as u64 * rps as u64;
                        let rows = (height as u64 - start).min(rps as u64);
                        rows * row_bytes_u64
                    }
                };
                StripInfo { offset, byte_count }
            })
            .collect::<Vec<_>>();
        // 整体界内校验（防越界读写）。
        for st in &strips {
            if st
                .offset
                .checked_add(st.byte_count)
                .map_or(true, |e| e > file_len)
            {
                return Err(decoding("strip 数据越界"));
            }
        }

        let icc_profile = entries
            .iter()
            .find(|e| e.tag == 34675)
            .map(|e| e.value.clone());
        let orientation = match get_uint(274)? {
            Some(v) => Orientation::from_exif(v.min(255) as u8).unwrap_or(Orientation::NoTransforms),
            None => Orientation::NoTransforms,
        };

        Ok(TiffDecoder {
            reader: r,
            width: u32::try_from(width).map_err(int_err)?,
            height: u32::try_from(height).map_err(int_err)?,
            color_type,
            photo: photo_layout,
            row_bytes,
            samples: u32::try_from(samples).map_err(int_err)?,
            predictor: predictor as u8,
            out_row_bytes,
            rows_per_strip: rps,
            strips,
            compression,
            icc_profile,
            orientation,
            limits: Limits::default(),
        })
    }

    /// 解码单个 strip，按压缩方式还原到输出切片（逐字节对齐 tiff crate）。
    /// 输出 = strip 解压后的编码字节（长度 = data_rows × row_bytes）；Predictor=2 时
    /// 随后按行做水平差分还原（RgbCompact 的样本丢弃发生在 read_image 的压缩步骤，此处保留全样本）。
    fn decode_strip(
        &mut self,
        strip: &StripInfo,
        needed: usize,
        out: &mut [u8],
        max_alloc: Option<u64>,
    ) -> ImageResult<()> {
        match self.compression {
            TiffCompression::None => {
                if strip.byte_count < needed as u64 {
                    return Err(io_eof_err());
                }
                self.reader.seek(SeekFrom::Start(strip.offset))?;
                self.reader.read_exact(out)?;
            }
            TiffCompression::PackBits => {
                if max_alloc.is_some_and(|m| strip.byte_count > m) {
                    return Err(limits_exceeded());
                }
                let bc = usize::try_from(strip.byte_count).map_err(int_err)?;
                let mut packed = vec![0u8; bc];
                self.reader.seek(SeekFrom::Start(strip.offset))?;
                self.reader.read_exact(&mut packed)?;
                decode_packbits(&packed, out)?;
            }
            TiffCompression::Lzw | TiffCompression::Deflate => {
                // 压缩数据整体读入（受 max_alloc 约束，与 tiff crate intermediate_buffer_size 护栏对齐）。
                if max_alloc.is_some_and(|m| strip.byte_count > m) {
                    return Err(limits_exceeded());
                }
                let bc = usize::try_from(strip.byte_count).map_err(int_err)?;
                let mut packed = vec![0u8; bc];
                self.reader.seek(SeekFrom::Start(strip.offset))?;
                self.reader.read_exact(&mut packed)?;
                match self.compression {
                    TiffCompression::Lzw => decode_lzw(&packed, out)?,
                    _ => decode_deflate(&packed, out)?,
                }
            }
        }
        if self.predictor == 2 {
            // 水平差分还原在「压缩层之后、RgbCompact 样本丢弃之前」执行，保留全样本按 N=samples
            // 整行还原 → 之后再丢末样本。
            //
            // 已知分歧（评审裁决：kimg 现状正确、tiff-0.11.3 该组合错像素）：
            // SPP4 + ExtraSamples=[0] + predictor=2（RgbCompact）时，kimg 先按 N=4 对 4 通道整行
            // 还原再压缩出 RGB3（正确：还原发生在编码字节域）；tiff-0.11.3 在该组合走
            // `compact_photometric_bytes` 先丢末样本、再以 N=4 对 3 通道行做预测（错像素）。
            // 因此**勿**为该组合加 oracle parity 断言（必然不等），只能加「期望字节」直测。
            apply_horizontal_predictor(out, self.row_bytes as usize, self.samples as usize);
        }
        Ok(())
    }
}

/// TIFF 变体 PackBits 解压缩（最高位控制字；-128 为 no-op）。
/// 输出达到 `out.len()` 即停止（尾部多余压缩数据忽略）；输入耗尽而输出未满 → `UnexpectedEof`。
fn decode_packbits(src: &[u8], out: &mut [u8]) -> ImageResult<()> {
    let mut si = 0usize;
    let mut oi = 0usize;
    while oi < out.len() {
        if si >= src.len() {
            return Err(io_eof_err());
        }
        let h = src[si] as i8;
        si += 1;
        if h == -128 {
            continue; // no-op
        }
        if h < 0 {
            // 重复段：1-h 份（2..=128）
            if si >= src.len() {
                return Err(io_eof_err());
            }
            let v = src[si];
            si += 1;
            let count = (1 - isize::from(h)) as usize;
            let take = count.min(out.len() - oi);
            out[oi..oi + take].fill(v);
            oi += take;
        } else {
            // 字面段：h+1 份（1..=128）
            let count = h as usize + 1;
            let take = count.min(out.len() - oi);
            if si + take > src.len() {
                return Err(io_eof_err());
            }
            out[oi..oi + take].copy_from_slice(&src[si..si + take]);
            oi += take;
            si += take;
        }
    }
    Ok(())
}

/// TIFF 变体 LZW 解压（与 tiff crate 所用的 weezl `with_tiff_size_switch(Msb, 8)` 逐字节同语义）：
/// - MSB-first 位序，起始码宽 9；ClearCode=256、EOI=257；
/// - TIFF early-change：在为 next_code 建表项时，若 `next_code >= 2^bits - 2` 即提前升宽一位
///   （weezl 判据 `next_code >= code_mask - 1`，code_mask = 2^bits − 1）；
/// - 满表（4096 项）后不再增项、不再升宽，按 12 位继续解，直到遇 ClearCode 才重置；
/// - 输出达到 `out.len()` 即停（对齐 tiff crate `read_exact` 语义，不要求读到 EOI）；
///   EOI 早于输出满（流短）→ Decoding；输入耗尽且 EOI 未现 → UnexpectedEof；无效码 → Decoding。
///
/// 快道/慢道双实现（env `KIMG_TIFF_LZW_FAST=0` 切慢道，默认快道）：只切 LZW 内部实现，
/// 两路状态机语义与输出逐字节一致。快道见 `decode_lzw_fast`，慢道为原始逐码 Vec 版。
fn decode_lzw(src: &[u8], out: &mut [u8]) -> ImageResult<()> {
    if std::env::var("KIMG_TIFF_LZW_FAST")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        decode_lzw_fast(src, out)
    } else {
        decode_lzw_slow(src, out)
    }
}

/// 快道：每码 0 堆分配。平表 `Vec<u16>`(prefix)+`Vec<u8>`(suffix) 免 `Vec<(u16,u8)>` 间接；
/// 链回溯倒填固定栈 `[u8;4097]`（`stack[--sp]=suffix[c]`，回溯完 `stack[sp..]` 即正序）
/// 免 reverse 免 clone，随后 `copy_from_slice` 直写预留 `out`；KwKwK（code==next_code）
/// 逆序先压「上一字首字节」到栈顶（＝新字末字节）再回溯 prev 链，免整词拷贝。
///
/// 栈长 4097 的充分性：回溯链索引严格递减（建表项 always prefix < index，见下），
/// 实际解码码值 ≤ 4095（normal）/ ≤4096（KwKwK 注入），最长普通字 4096 字节、
/// KwKwK 字 = 上一字 +1 首字节 ≤ 4097 → 4097 恰好够；仍有 debug_assert 护栏防越界。
fn decode_lzw_fast(src: &[u8], out: &mut [u8]) -> ImageResult<()> {
    const MAX_ENTRIES: usize = 1 << 12;
    const STACK_CAP: usize = MAX_ENTRIES + 1; // 最长一字 4096 + KwKwK 注入 1

    // 平表：prefix[i]=前置码（字面量回指自身终止回溯）、suffix[i]=末字节。
    // 0..=255 字面量；256/257 为 Clear/EOI 占位（不会被索引到）。prefix.len()==next_code 恒成立。
    let mut prefix: Vec<u16> = Vec::with_capacity(MAX_ENTRIES);
    let mut suffix: Vec<u8> = Vec::with_capacity(MAX_ENTRIES);
    for i in 0u16..=255 {
        prefix.push(i);
        suffix.push(i as u8);
    }
    prefix.push(0); // 256 ClearCode 占位
    suffix.push(0);
    prefix.push(0); // 257 EOI 占位
    suffix.push(0);
    debug_assert_eq!(prefix.len(), 258);

    // MSB 位缓冲读取（大端累积进 bit_buffer，取顶部位）。
    fn refill(src: &[u8], in_pos: &mut usize, bit_buffer: &mut u64, bits: &mut u32) {
        while *bits < 16 && *in_pos < src.len() {
            *bit_buffer = (*bit_buffer << 8) | u64::from(src[*in_pos]);
            *in_pos += 1;
            *bits += 8;
        }
    }
    fn read_code(
        src: &[u8],
        in_pos: &mut usize,
        bit_buffer: &mut u64,
        bits: &mut u32,
        code_size: u32,
    ) -> Option<u32> {
        refill(src, in_pos, bit_buffer, bits);
        if *bits < code_size {
            return None;
        }
        *bits -= code_size;
        let mask = (1u64 << code_size) - 1;
        Some(((*bit_buffer >> *bits) & mask) as u32)
    }

    let mut in_pos = 0usize;
    let mut bit_buffer: u64 = 0;
    let mut bits: u32 = 0;
    let mut code_size: u32 = 9;
    let mut next_code: u32 = 258;
    let mut oi = 0usize;
    let mut prev: Option<(u32, u8)> = None; // (上一码值, 上一码字首字节)

    let mut stack = [0u8; STACK_CAP];

    loop {
        if oi >= out.len() {
            return Ok(()); // 输出已满：read_exact 语义即成功（不要求 EOI）
        }
        let Some(code) = read_code(src, &mut in_pos, &mut bit_buffer, &mut bits, code_size) else {
            return Err(io_eof_err()); // 输入耗尽且 EOI 未现 → 截断
        };

        if code == 256 {
            // ClearCode：重置（含 reset 后首个码不建项；位缓冲状态连带保持原样）
            prefix.truncate(258);
            suffix.truncate(258);
            code_size = 9;
            next_code = 258;
            prev = None;
            continue;
        }
        if code == 257 {
            // EOI：此时 oi < out.len()（输出满已在循环顶返回）→ 流短报错
            return Err(decoding("LZW 流过早结束（EOI 时输出未满）"));
        }

        // 链回溯：倒填栈，`stack[sp..]` 即正序字节；KwKwK 先逆序注入首字节到栈顶。
        let sp = {
            let mut sp = STACK_CAP;
            if code == next_code {
                let Some((prev_code, first)) = prev else {
                    return Err(decoding("LZW 无效码：new_code==next_code 但无前驱字"));
                };
                sp -= 1;
                stack[sp] = first;
                let mut c = prev_code;
                loop {
                    debug_assert!(c < prefix.len() as u32);
                    sp -= 1;
                    stack[sp] = suffix[c as usize];
                    let p = prefix[c as usize];
                    if u32::from(p) == c {
                        break; // 字面量基座：prev 自指
                    }
                    c = u32::from(p);
                }
            } else {
                if code > next_code {
                    return Err(decoding("LZW 无效码"));
                }
                let mut c = code;
                loop {
                    debug_assert!(c < prefix.len() as u32);
                    sp -= 1;
                    stack[sp] = suffix[c as usize];
                    let p = prefix[c as usize];
                    if u32::from(p) == c {
                        break; // 字面量基座：prev 自指
                    }
                    c = u32::from(p);
                }
            }
            sp
        };

        // 写入输出（clamp 到剩余空间）。
        let first = stack[sp];
        let len = STACK_CAP - sp;
        let take = len.min(out.len() - oi);
        out[oi..oi + take].copy_from_slice(&stack[sp..sp + take]);
        oi += take;

        // 每个非首码各建一个表项（满表后不再增项/升宽，继续以 12 位解）。
        if let Some((prev_code, _)) = prev {
            if prefix.len() < MAX_ENTRIES {
                prefix.push(prev_code as u16);
                suffix.push(first);
                // TIFF early-change：表项编号 ≥ 2^bits − 2 即提前升宽。
                if next_code >= (1u32 << code_size) - 2 && code_size < 12 {
                    code_size += 1;
                }
                next_code += 1;
            }
        }

        prev = Some((code, first));
    }
}

/// 慢道（`KIMG_TIFF_LZW_FAST=0`）：原始逐码 `Vec<(u16,u8)>` 表 + 每码一次 `Vec<u8>` 克隆 +
/// 整词 `clone_from` 拷贝。语义与快道一致，仅用于同二进制 A/B 对照与回归兜底。
fn decode_lzw_slow(src: &[u8], out: &mut [u8]) -> ImageResult<()> {
    const MAX_ENTRIES: usize = 1 << 12;

    // 表：(前置码, 字节)。0..=255 字面量（prev=自身 终止回溯）；256/257 为 Clear/EOI 占位。
    let mut table: Vec<(u16, u8)> = Vec::with_capacity(MAX_ENTRIES);
    for i in 0u16..=255 {
        table.push((i, i as u8));
    }
    table.push((0, 0)); // 256 ClearCode
    table.push((0, 0)); // 257 EOI
    debug_assert_eq!(table.len(), 258);

    // MSB 位缓冲读取（大端累积进 bit_buffer，取顶部位）。
    fn refill(src: &[u8], in_pos: &mut usize, bit_buffer: &mut u64, bits: &mut u32) {
        while *bits < 16 && *in_pos < src.len() {
            *bit_buffer = (*bit_buffer << 8) | u64::from(src[*in_pos]);
            *in_pos += 1;
            *bits += 8;
        }
    }
    fn read_code(
        src: &[u8],
        in_pos: &mut usize,
        bit_buffer: &mut u64,
        bits: &mut u32,
        code_size: u32,
    ) -> Option<u32> {
        refill(src, in_pos, bit_buffer, bits);
        if *bits < code_size {
            return None;
        }
        *bits -= code_size;
        let mask = (1u64 << code_size) - 1;
        Some(((*bit_buffer >> *bits) & mask) as u32)
    }

    // 从表回溯重建某码的字节序列（沿 prev 链收集后反转）。
    fn reconstruct_word(code: u32, scratch: &mut Vec<u8>, table: &[(u16, u8)]) {
        scratch.clear();
        let mut c = code;
        loop {
            let (prev, b) = table[c as usize];
            scratch.push(b);
            if u32::from(prev) == c {
                break; // 字面量基座：prev 自指
            }
            c = u32::from(prev);
        }
        scratch.reverse();
    }

    let mut in_pos = 0usize;
    let mut bit_buffer: u64 = 0;
    let mut bits: u32 = 0;
    let mut code_size: u32 = 9;
    let mut next_code: u32 = 258;
    let mut oi = 0usize;
    let mut prev: Option<(u32, u8)> = None; // (上一码值, 上一码字首字节)
    let mut last_word: Vec<u8> = Vec::new(); // 上一码的完整字（KwKwK 新码字 = 它 + 其首字节）
    let mut scratch: Vec<u8> = Vec::new();

    loop {
        if oi >= out.len() {
            return Ok(()); // 输出已满：read_exact 语义即成功（不要求 EOI）
        }
        let Some(code) = read_code(src, &mut in_pos, &mut bit_buffer, &mut bits, code_size) else {
            return Err(io_eof_err()); // 输入耗尽且 EOI 未现 → 截断
        };

        if code == 256 {
            // ClearCode：重置（含 reset 后首个码不建项）
            table.truncate(258);
            code_size = 9;
            next_code = 258;
            prev = None;
            last_word.clear();
            continue;
        }
        if code == 257 {
            // EOI：此时 oi < out.len()（输出满已在循环顶返回）→ 流短报错
            return Err(decoding("LZW 流过早结束（EOI 时输出未满）"));
        }

        let word: Vec<u8> = if code == next_code {
            // KwKwK：新码字 = 上一字 + 上一字首字节（code == next_code，尚不在表内）
            if last_word.is_empty() {
                return Err(decoding("LZW 无效码：new_code==next_code 但无前驱字"));
            }
            scratch.clear();
            scratch.extend_from_slice(&last_word);
            scratch.push(last_word[0]);
            scratch.clone()
        } else {
            if code > next_code {
                return Err(decoding("LZW 无效码"));
            }
            reconstruct_word(code, &mut scratch, &table);
            scratch.clone()
        };

        // 写入输出（clamp 到剩余空间）。
        let take = word.len().min(out.len() - oi);
        out[oi..oi + take].copy_from_slice(&word[..take]);
        oi += take;

        let first = word[0];
        // 每个非首码各建一个表项（满表后不再增项/升宽，继续以 12 位解）。
        if let Some((prev_code, _)) = prev {
            if table.len() < MAX_ENTRIES {
                table.push((prev_code as u16, first));
                // TIFF early-change：表项编号 ≥ 2^bits − 2 即提前升宽。
                if next_code >= (1u32 << code_size) - 2 && code_size < 12 {
                    code_size += 1;
                }
                next_code += 1;
            }
        }

        prev = Some((code, first));
        last_word.clone_from(&word);
    }
}

/// Deflate(8)/ZlibPkzip(32946) 解压：strip 数据为完整 zlib 封装流（0x78 头 + adler32 尾），
/// 交给 fdeflate（与 kimg_png 同版本）。
///
/// 注意：`Decompressor::read(input, out, out_pos, eoi)` 第 4 参是 `end_of_input`（本路径恒 true，
/// 同 `decompress_to_vec_bounded` 姿势），**不是** adler 开关；本路径**从未调 `ignore_adler32()`**，
/// 因此 zlib 尾 4 字节 adler32 真实参与校验（坏校验 → `WrongChecksum`、不足 → `InsufficientInput`），
/// 与 png 热路径（图性能忽略 adler）刻意不同——勿为「提速」顺手补 `ignore_adler32()`，会放掉截断检测。
/// 坏 zlib 头 → `BadZlibHeader`；要求解出长度恒等于 `out.len()`（防 bomb），
/// 多余输出/流未正常结束 → Decoding。
fn decode_deflate(src: &[u8], out: &mut [u8]) -> ImageResult<()> {
    let mut dc = fdeflate::Decompressor::new();
    let mut input_index = 0usize;
    let mut output_index = 0usize;
    loop {
        let (consumed, produced) = dc
            .read(&src[input_index..], out, output_index, true)
            .map_err(|e| decoding(format!("TIFF deflate 解压失败：{e:?}")))?;
        input_index += consumed;
        output_index += produced;
        if dc.is_done() || output_index == out.len() {
            break;
        }
    }
    if !dc.is_done() || output_index != out.len() {
        return Err(decoding(format!(
            "TIFF deflate 流长度不符：期望 {} 字节，解出 {}",
            out.len(),
            output_index
        )));
    }
    Ok(())
}

/// 快道/慢道 env 门控：`KIMG_TIFF_PRED_FAST=0` 强制标量（同二进制 A/B 用），默认快道。
/// `OnceLock` 一次读取（勿每行读 env）。仿 `KIMG_TIFF_LZW_FAST`。
fn pred_fast_enabled() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        std::env::var("KIMG_TIFF_PRED_FAST")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// Horizontal predictor 还原：对每个解码后的行（`row_bytes` 字节）逐采样通道做
/// `row[i] += row[i - samples]`（wrapping mod 256），行首采样保持为 0 差分起点。
/// 与 tiff crate `rev_hpredict_nsamp`（8bit、N=samples）逐字节一致。
///
/// 快道/慢道双路径：samples∈{1,2,3,4} 且宿主支持 SSSE3 且 `KIMG_TIFF_PRED_FAST` 未禁 →
/// 16B 块前缀和 SIMD 快道（见 [`pred_simd`]，K109 探针已证 N=1 逐字节 0 失配、~2.5×）；
/// 否则走下方标量参照路径（dormancy 保留，`KIMG_TIFF_PRED_FAST=0` 同二进制 A/B 用）。
fn apply_horizontal_predictor(buf: &mut [u8], row_bytes: usize, samples: usize) {
    debug_assert!(samples >= 1 && samples <= row_bytes);
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected;
        if pred_fast_enabled()
            && (1..=4).contains(&samples)
            && is_x86_feature_detected!("ssse3")
        {
            return unsafe { pred_simd::predict_all(buf, row_bytes, samples) };
        }
    }
    // 标量参照路径（dormancy；语义 = 逐通道差分链 out[i]=enc[i]+out[i-N]，模 256 回绕）。
    for row in buf.chunks_mut(row_bytes) {
        for i in samples..row.len() {
            row[i] = row[i].wrapping_add(row[i - samples]);
        }
    }
}

/// 16B 块前缀和 SIMD 快道（x86_64）：
/// 每 16B 块先读本块**原始字节** v（in-place 写入前读）→ 按 samples 的 shift 计划做
/// 块内各通道链前缀和 t → `t + pshufb(上块解码输出, MASK_N)`（块间 carry 来自上块**解码**输出
/// 末 N 字节按通道广播）→ 写回，prev = 本块解码输出供下块 carry。每行独立、行首 carry=0。
///
/// 链语义：samples=N 时有 N 条交错链（位置 s, s+N, s+2N, …，0≤s<N），out[i]=enc[i]+out[i-N]；
/// 块内 `_mm_slli_si128` 逐 d 移位累加即块内链前缀和（模 256 回绕天然成立）。块间 carry 必须取
/// 上块**解码输出**末 N 字节（= 链在块前的累计值），不能取原始字节（原始末字节 ≠ 块前缀和）。
/// - shift 计划（`slli_si128` 第二参须编译期立即数，故按 N 特化）：N=1→[1,2,4,8]；
///   N=2→[2,4,8]；N=3→[3,6,12]；N=4→[4,8]。
/// - MASK_N[j] = 16-N+(j mod N)（N=3 为 13,14,15 循环——note：非 12,13,14，前导 carry 偏移
///   由链长 N 定，16-3=13；首块 prev=0 → carry=0）。
/// 整除块循环后尾部（非 16 倍数）标量收尾，自然取本块已压解码值作 carry。
#[cfg(target_arch = "x86_64")]
mod pred_simd {
    use std::arch::x86_64::*;

    const MASK1: [i8; 16] = [15; 16];
    const MASK2: [i8; 16] = [14, 15, 14, 15, 14, 15, 14, 15, 14, 15, 14, 15, 14, 15, 14, 15];
    const MASK3: [i8; 16] = [13, 14, 15, 13, 14, 15, 13, 14, 15, 13, 14, 15, 13, 14, 15, 13];
    const MASK4: [i8; 16] = [12, 13, 14, 15, 12, 13, 14, 15, 12, 13, 14, 15, 12, 13, 14, 15];

    /// 块内链前缀和：依次 `t += slli(t, d)`（模 256 回绕天然成立，无跨子块问题）。
    macro_rules! block_prefix {
        ($v:ident $(, $d:literal)*) => {{
            let mut p = $v;
            $(p = _mm_add_epi8(p, _mm_slli_si128::<$d>(p));)*
            p
        }};
    }

    /// 整体快道：逐行处理（每行独立、行首 carry=0）。
    pub(crate) unsafe fn predict_all(buf: &mut [u8], row_bytes: usize, samples: usize) {
        for row in buf.chunks_mut(row_bytes) {
            unsafe { predict_row(row, samples) };
        }
    }

    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn predict_row(row: &mut [u8], samples: usize) {
        unsafe {
            let n = row.len();
            // 整行不足一块（或未知 samples>4）→ 整行走标量。
            if n < 16 || samples > 4 {
                for i in samples..n {
                    row[i] = row[i].wrapping_add(row[i - samples]);
                }
                return;
            }
            let r = row.as_mut_ptr();
            let mut i = 0usize;
            let mut prev = _mm_setzero_si128(); // 行首 carry=0；逐块更新为上块解码输出
            while i + 16 <= n {
                // 写入前先读本块原始字节（in-place 语义）。
                let v = _mm_loadu_si128(r.add(i) as *const __m128i);
                let maskv =
                    _mm_loadu_si128(match samples {
                        1 => MASK1.as_ptr() as *const __m128i,
                        2 => MASK2.as_ptr() as *const __m128i,
                        3 => MASK3.as_ptr() as *const __m128i,
                        _ => MASK4.as_ptr() as *const __m128i,
                    });
                let carry = _mm_shuffle_epi8(prev, maskv);
                let out = match samples {
                    1 => _mm_add_epi8(block_prefix!(v, 1, 2, 4, 8), carry),
                    2 => _mm_add_epi8(block_prefix!(v, 2, 4, 8), carry),
                    3 => _mm_add_epi8(block_prefix!(v, 3, 6, 12), carry),
                    _ => _mm_add_epi8(block_prefix!(v, 4, 8), carry),
                };
                _mm_storeu_si128(r.add(i) as *mut __m128i, out);
                prev = out;
                i += 16;
            }
            // 标量尾段：i≥16 恒 > samples，row[i-samples] 取已压解码值作 carry。
            while i < n {
                *r.add(i) = row[i].wrapping_add(row[i - samples]);
                i += 1;
            }
        }
    }
}

/// `RgbCompact`：把「每像素 4 样本（R,G,B + 1 附加非 alpha 样本）」的编码行压缩为
/// RGB3 输出（每像素取前 3 样本，丢弃最末附加样本）——与 tiff crate 对
/// `SPP4 + ExtraSamples=[0]`（photometric_samples=3、非 alpha）输出 `ColorType::RGB` 的行为一致。
fn compact_extra_bytes(enc: &[u8], out: &mut [u8]) {
    let mut oi = 0usize;
    for px in enc.chunks_exact(4) {
        out[oi..oi + 3].copy_from_slice(&px[..3]);
        oi += 3;
    }
    debug_assert_eq!(oi, out.len());
}

impl<R> ImageDecoder for TiffDecoder<R>
where
    R: BufRead + Seek,
{
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn original_color_type(&self) -> ExtendedColorType {
        self.color_type.into()
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc_profile.clone())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(self.orientation)
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        let (w, h) = self.dimensions();
        limits.validate_for(&LimitSupport::default(), w, h)?;
        self.limits = limits;
        Ok(())
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        let max_alloc = self.limits.max_alloc;
        if max_alloc.is_some_and(|m| self.total_bytes() > m) {
            return Err(limits_exceeded());
        }

        let height = u64::from(self.height);
        let row_bytes = u64::from(self.row_bytes);
        let out_row_bytes = u64::from(self.out_row_bytes);
        let rps = u64::from(self.rows_per_strip);
        let invert = matches!(self.photo, PhotoLayout::Gray { white_is_zero: true });
        let compact = matches!(self.photo, PhotoLayout::RgbCompact);

        let mut strips = core::mem::take(&mut self.strips);
        for (i, strip) in strips.iter().enumerate() {
            let start_row = i as u64 * rps;
            let data_rows = (height - start_row).min(rps);
            // 编码字节数（含附加样本）与输出字节数（可能经 RgbCompact 压缩）
            let needed = usize::try_from(data_rows * row_bytes).map_err(int_err)?;
            let out_len = usize::try_from(data_rows * out_row_bytes).map_err(int_err)?;
            let out_off = usize::try_from(start_row * out_row_bytes).map_err(int_err)?;
            let out = &mut buf[out_off..out_off + out_len];

            if compact {
                // 每像素丢弃最后一个附加样本：先解到临时缓冲再压缩到输出
                if max_alloc.is_some_and(|m| needed as u64 > m) {
                    return Err(limits_exceeded());
                }
                let mut enc = vec![0u8; needed];
                self.decode_strip(strip, needed, &mut enc, max_alloc)?;
                compact_extra_bytes(&enc, out);
            } else {
                self.decode_strip(strip, needed, out, max_alloc)?;
            }

            if invert {
                for b in out.iter_mut() {
                    *b = !*b;
                }
            }
        }
        strips.clear();
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立标量参考（不经 `apply_horizontal_predictor` 的 SIMD 派发）。
    fn scalar_predict(buf: &mut [u8], row_bytes: usize, samples: usize) {
        for row in buf.chunks_mut(row_bytes) {
            for i in samples..row.len() {
                row[i] = row[i].wrapping_add(row[i - samples]);
            }
        }
    }

    /// 正向水平差分编码（预测器逆运算）：enc[i] = raw[i] - raw[i-N]，行首 N 字节原样。
    /// 恒有 `predict(forward(x)) == x`。
    fn forward_predict(buf: &mut [u8], row_bytes: usize, samples: usize) {
        for row in buf.chunks_mut(row_bytes) {
            for i in (samples..row.len()).rev() {
                row[i] = row[i].wrapping_sub(row[i - samples]);
            }
        }
    }

    /// 确定性伪随机行（LCG，同 pngsimd 测试姿势）。
    fn random_row(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 56) as u8
            })
            .collect()
    }

    /// fast ≡ 标量：随机缓冲覆盖 N=1/2/3/4、行长 1..及非 16 倍数、行高 1/多行。
    /// 直接调 `pred_simd::predict_all` 强制走 SIMD 核（绕开 env/派发），
    /// 故 `KIMG_TIFF_PRED_FAST=0` 重跑本套件时 SIMD 核仍被直接验证。
    #[test]
    fn pred_fast_matches_scalar_random() {
        for samples in 1..=4usize {
            for row_bytes in [
                1usize, 2, 3, 4, 5, 7, 15, 16, 17, 19, 31, 32, 33, 47, 64, 65, 95, 100,
            ] {
                if samples > row_bytes {
                    continue; // debug_assert 要求 samples <= row_bytes（真实行恒满足）
                }
                for rows in [1usize, 3] {
                    let len = row_bytes * rows;
                    let data = random_row(len, 0x9E3779B97F4A7C15 ^ (samples as u64 * 131 + row_bytes as u64 * 7 + rows as u64));
                    // 标量参考
                    let mut scalar = data.clone();
                    scalar_predict(&mut scalar, row_bytes, samples);
                    // 走完整派发（fast 或标量回退，见机行事）
                    let mut dispatched = data.clone();
                    apply_horizontal_predictor(&mut dispatched, row_bytes, samples);
                    assert_eq!(dispatched, scalar, "派发路径不符 (samples={samples} row_bytes={row_bytes} rows={rows})");
                    // 强制 SIMD 核直测
                    #[cfg(target_arch = "x86_64")]
                    {
                        use std::arch::is_x86_feature_detected;
                        if is_x86_feature_detected!("ssse3") {
                            let mut leaf = data.clone();
                            unsafe { pred_simd::predict_all(&mut leaf, row_bytes, samples) };
                            assert_eq!(leaf, scalar, "SIMD 核不符 (samples={samples} row_bytes={row_bytes} rows={rows})");
                        }
                    }
                }
            }
        }
    }

    /// fast ≡ 标量：真实语料 `Benchmarks/corpus/TIFF/*_pred.tif`（解码后的行缓冲再正向差分
    /// 还原预测输入）→ 覆盖真实灰度/RGB/RGBA 行布局与内容。缺语料 → SKIP（不入库惯例）。
    #[test]
    fn pred_fast_matches_scalar_corpus() {
        // 语料装配统一走 kimg_core::testutil::corpus_dir（env `KIMG_TIFF_CORPUS` 覆盖，
        // 与 tiff_bench_ours_vs_tiff 同键；缺省 `<workspace>/Benchmarks/corpus/TIFF`）。
        let Some(corpus) = kimg_core::testutil::corpus_dir("KIMG_TIFF_CORPUS", "TIFF") else {
            eprintln!("[pred-fast-corpus] SKIP: corpus/TIFF 缺失（先跑 Benchmarks/tiff/scripts/run_gen.ps1）");
            return;
        };
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&corpus) {
            for e in rd.flatten() {
                let p = e.path();
                if p.file_name()
                    .map(|n| n.to_string_lossy().ends_with("_pred.tif"))
                    .unwrap_or(false)
                {
                    files.push(p);
                }
            }
        }
        files.sort();
        if files.is_empty() {
            eprintln!("[pred-fast-corpus] SKIP: corpus/TIFF 缺 *_pred.tif（先跑 Benchmarks/tiff/scripts/run_gen.ps1）");
            return;
        }
        let mut checked = 0usize;
        for path in &files {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let data = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[pred-fast-corpus] SKIP {name}: 读文件失败 {e}");
                    continue;
                }
            };
            let dec = match TiffDecoder::new(std::io::Cursor::new(data.as_slice())) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[pred-fast-corpus] SKIP {name}: 自研解码构造失败 {e}");
                    continue;
                }
            };
            let (w, h) = dec.dimensions();
            let total = dec.total_bytes() as usize;
            let mut raw = vec![0u8; total];
            if let Err(e) = dec.read_image(&mut raw) {
                eprintln!("[pred-fast-corpus] SKIP {name}: 自研解码失败 {e}");
                continue;
            }
            let row_bytes = raw.len() / h as usize;
            if row_bytes % w as usize != 0 {
                eprintln!("[pred-fast-corpus] SKIP {name}: 行长不为宽整数倍 row_bytes={row_bytes} w={w}");
                continue;
            }
            let samples = row_bytes / w as usize;
            if !(1..=4).contains(&samples) {
                eprintln!("[pred-fast-corpus] SKIP {name}: 布局推断异常 row_bytes={row_bytes} samples={samples}");
                continue;
            }
            // 正向差分还原预测输入，再跑 fast/标量，均须恢复原始解码字节。
            let mut enc = raw.clone();
            forward_predict(&mut enc, row_bytes, samples);
            let mut fast = enc.clone();
            apply_horizontal_predictor(&mut fast, row_bytes, samples);
            assert_eq!(fast, raw, "[pred-fast-corpus] {name} fast ≠ 原始解码（end-to-end 失配）");
            let mut scalar_buf = enc.clone();
            scalar_predict(&mut scalar_buf, row_bytes, samples);
            assert_eq!(scalar_buf, raw, "[pred-fast-corpus] {name} 标量参考 ≠ 原始解码");
            checked += 1;
        }
        eprintln!("[pred-fast-corpus] parity ok={checked}");
        assert!(checked > 0, "[pred-fast-corpus] 一个 pred 文件都没验上");
    }
}
