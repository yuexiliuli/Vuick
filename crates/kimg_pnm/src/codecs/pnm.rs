//! PNM 解码器（手写实现：PBM/PGM/PPM 的 ASCII/二进制及 PAM；MAXVAL 缩放与 image 0.25 一致）。

use std::io::Read;

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::{DecodingError, ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind};
use kimg_core::format::ImageFormat;
use kimg_core::metadata::Orientation;
use crate::{ImageDecoder};

fn pnm_err<T: std::fmt::Display>(msg: T) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormat::Pnm.into(), msg.to_string()))
}

/// PNM 子类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnmSubtype {
    /// 位图（黑白）。
    Bitmap(SampleEncoding),
    /// 灰度图。
    Graymap(SampleEncoding),
    /// 真彩图。
    Pixmap(SampleEncoding),
    /// P7 PAM。
    ArbitraryMap,
}

/// 样本编码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleEncoding {
    /// ASCII 数字。
    Ascii,
    /// 原始二进制。
    Binary,
}

impl PnmSubtype {
    fn sample_encoding(&self) -> SampleEncoding {
        match *self {
            PnmSubtype::Bitmap(e) | PnmSubtype::Graymap(e) | PnmSubtype::Pixmap(e) => e,
            PnmSubtype::ArbitraryMap => SampleEncoding::Binary,
        }
    }
}

#[derive(Clone, Copy)]
enum TupleType {
    PbmBit,
    BWBit,
    GrayU8,
    GrayAlphaU8,
    GrayU16,
    GrayAlphaU16,
    RGBU8,
    RGBAlphaU8,
    RGBU16,
    RGBAlphaU16,
}

/// PNM 解码器。
pub struct PnmDecoder<R> {
    reader: R,
    width: u32,
    height: u32,
    tuple: TupleType,
    maxval: u32,
    subtype: PnmSubtype,
}

impl<R: Read> PnmDecoder<R> {
    /// 构造（读取魔法值、头、尺寸与格式化校验）。
    pub fn new(mut buffered_read: R) -> ImageResult<PnmDecoder<R>> {
        let magic = read_magic(&mut buffered_read)?;
        let (width, height, maxval, tuple, subtype) = match magic {
            [b'P', b'1'] => parse_pbm(&mut buffered_read, SampleEncoding::Ascii)?,
            [b'P', b'2'] => parse_graymap(&mut buffered_read, SampleEncoding::Ascii)?,
            [b'P', b'3'] => parse_pixmap(&mut buffered_read, SampleEncoding::Ascii)?,
            [b'P', b'4'] => parse_pbm(&mut buffered_read, SampleEncoding::Binary)?,
            [b'P', b'5'] => parse_graymap(&mut buffered_read, SampleEncoding::Binary)?,
            [b'P', b'6'] => parse_pixmap(&mut buffered_read, SampleEncoding::Binary)?,
            [b'P', b'7'] => parse_pam(&mut buffered_read)?,
            _ => return Err(pnm_err(format!("magic: {magic:?}"))),
        };

        Ok(PnmDecoder {
            reader: buffered_read,
            width,
            height,
            tuple,
            maxval,
            subtype,
        })
    }

    /// 子类型。
    pub fn subtype(&self) -> PnmSubtype {
        self.subtype
    }
}

fn read_magic<R: Read>(r: &mut R) -> ImageResult<[u8; 2]> {
    let mut magic = [0u8; 2];
    r.read_exact(&mut magic)?;
    Ok(magic)
}

/// 读取一个空白分隔的十进制整数（支持注释与任意空白）。
fn read_next_u32<R: Read>(r: &mut R) -> ImageResult<u32> {
    let mut value: u64 = 0;
    let mut found_digit = false;
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => {
                if found_digit {
                    return Ok(value as u32);
                }
                return Err(ImageError::IoError(std::io::ErrorKind::UnexpectedEof.into()));
            }
            Err(e) => return Err(ImageError::IoError(e)),
            Ok(_) => {
                let c = byte[0];
                if c == b'#' {
                    // 注释到行尾。
                    loop {
                        if r.read(&mut byte)? == 0 {
                            break;
                        }
                        if byte[0] == b'\n' || byte[0] == b'\r' {
                            break;
                        }
                    }
                    continue;
                }
                if is_space(c) {
                    if found_digit {
                        return Ok(value as u32);
                    }
                    continue;
                }
                if c.is_ascii_digit() {
                    value = value
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(u64::from(c - b'0')))
                        .ok_or_else(|| pnm_err("overflow parsing integer"))?;
                    found_digit = true;
                } else {
                    return Err(pnm_err(format!("non-digit byte {c:#04X?} in header")));
                }
            }
        }
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r' | b' ')
}

fn parse_pbm<R: Read>(r: &mut R, encoding: SampleEncoding) -> ImageResult<(u32, u32, u32, TupleType, PnmSubtype)> {
    let width = read_next_u32(r)?;
    let height = read_next_u32(r)?;
    Ok((width, height, 1, TupleType::PbmBit, PnmSubtype::Bitmap(encoding)))
}

fn parse_graymap<R: Read>(r: &mut R, encoding: SampleEncoding) -> ImageResult<(u32, u32, u32, TupleType, PnmSubtype)> {
    let width = read_next_u32(r)?;
    let height = read_next_u32(r)?;
    let maxval = read_next_u32(r)?;
    let tuple = tuple_for_maxval::<1>(maxval)?;
    Ok((width, height, maxval, tuple, PnmSubtype::Graymap(encoding)))
}

fn parse_pixmap<R: Read>(r: &mut R, encoding: SampleEncoding) -> ImageResult<(u32, u32, u32, TupleType, PnmSubtype)> {
    let width = read_next_u32(r)?;
    let height = read_next_u32(r)?;
    let maxval = read_next_u32(r)?;
    let tuple = tuple_for_maxval::<3>(maxval)?;
    Ok((width, height, maxval, tuple, PnmSubtype::Pixmap(encoding)))
}

fn tuple_for_maxval<const N: u32>(maxval: u32) -> ImageResult<TupleType> {
    match (N, maxval) {
        (_, 0) => Err(pnm_err("Image MAXVAL is zero")),
        (1, v) if v <= 0xFF => Ok(TupleType::GrayU8),
        (1, v) if v <= 0xFFFF => Ok(TupleType::GrayU16),
        (3, v) if v <= 0xFF => Ok(TupleType::RGBU8),
        (3, v) if v <= 0xFFFF => Ok(TupleType::RGBU16),
        (_, v) => Err(pnm_err(format!("Image MAXVAL exceeds 0xFFFF: {v}"))),
    }
}

fn parse_pam<R: Read>(r: &mut R) -> ImageResult<(u32, u32, u32, TupleType, PnmSubtype)> {
    // 读取 P7 头：跳过第一个换行后的若干 KEY VALUE 行直到 ENDHDR。
    let mut byte = [0u8; 1];
    loop {
        if r.read(&mut byte)? == 0 {
            return Err(ImageError::IoError(std::io::ErrorKind::UnexpectedEof.into()));
        }
        if byte[0] == b'\n' {
            break;
        }
    }

    let mut height: Option<u32> = None;
    let mut width: Option<u32> = None;
    let mut depth: Option<u32> = None;
    let mut maxval: Option<u32> = None;
    let mut tupltype: Option<String> = None;

    loop {
        let mut line = Vec::new();
        loop {
            if r.read(&mut byte)? == 0 {
                return Err(pnm_err("Unexpected end of PNM header"));
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        let line = String::from_utf8(line).map_err(pnm_err)?;
        if line.is_empty() {
            return Err(pnm_err("Unexpected end of PNM header"));
        }
        if line.starts_with('#') {
            continue;
        }
        if !line.is_ascii() {
            return Err(pnm_err("Non-ASCII in PAM header"));
        }
        let line = line.trim_start();
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        match key {
            "ENDHDR" => break,
            "HEIGHT" => height = Some(parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| pnm_err("bad HEIGHT"))?),
            "WIDTH" => width = Some(parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| pnm_err("bad WIDTH"))?),
            "DEPTH" => depth = Some(parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| pnm_err("bad DEPTH"))?),
            "MAXVAL" => maxval = Some(parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| pnm_err("bad MAXVAL"))?),
            "TUPLTYPE" => {
                let t = parts.collect::<Vec<_>>().join(" ");
                tupltype = Some(t);
            }
            other => return Err(pnm_err(format!("Unknown header line with identifier {other:?}"))),
        }
    }

    let (Some(h), Some(w), Some(d), Some(m)) = (height, width, depth, maxval) else {
        return Err(pnm_err("Missing header line"));
    };

    let tuple: TupleType = match (tupltype.as_deref(), m, d) {
        (_, 0, _) => return Err(pnm_err("Image MAXVAL is zero")),
        (None, _, 1) => Ok(TupleType::GrayU8),
        (None, _, 2) => Ok(TupleType::GrayAlphaU8),
        (None, _, 3) => Ok(TupleType::RGBU8),
        (None, _, 4) => Ok(TupleType::RGBAlphaU8),
        (Some("BLACKANDWHITE"), 1, 1) => Ok(TupleType::BWBit),
        (Some("GRAYSCALE"), v, 1) if v <= 0xFF => Ok(TupleType::GrayU8),
        (Some("GRAYSCALE"), v, 1) if v <= 0xFFFF => Ok(TupleType::GrayU16),
        (Some("RGB"), v, 3) if v <= 0xFF => Ok(TupleType::RGBU8),
        (Some("RGB"), v, 3) if v <= 0xFFFF => Ok(TupleType::RGBU16),
        (Some("GRAYSCALE_ALPHA"), v, 2) if v <= 0xFF => Ok(TupleType::GrayAlphaU8),
        (Some("GRAYSCALE_ALPHA"), v, 2) if v <= 0xFFFF => Ok(TupleType::GrayAlphaU16),
        (Some("RGB_ALPHA"), v, 4) if v <= 0xFF => Ok(TupleType::RGBAlphaU8),
        (Some("RGB_ALPHA"), v, 4) if v <= 0xFFFF => Ok(TupleType::RGBAlphaU16),
        (Some(other), _, _) => {
            return Err(ImageError::Unsupported(
                UnsupportedError::from_format_and_kind(
                    ImageFormat::Pnm.into(),
                    UnsupportedErrorKind::GenericFeature(format!("Tuple type {other:?}")),
                ),
            ))
        }
        _ => Err(pnm_err("Tuple type not recognized")),
    }?;

    Ok((w, h, m, tuple, PnmSubtype::ArbitraryMap))
}

impl<R: Read> PnmDecoder<R> {
    fn read_samples(&mut self, buf: &mut [u8]) -> ImageResult<()> {
        // PBM 单独处理（位流展开 + 0/255 反转）。
        if matches!(self.tuple, TupleType::PbmBit | TupleType::BWBit) {
            return self.read_pbm(buf);
        }

        let is_16 = matches!(
            self.tuple,
            TupleType::GrayU16
                | TupleType::GrayAlphaU16
                | TupleType::RGBU16
                | TupleType::RGBAlphaU16
        );

        match self.subtype.sample_encoding() {
            SampleEncoding::Binary => {
                self.reader.read_exact(buf)?;
                if is_16 {
                    // 二进制 16bit 为大端，转本机端序（与 image 0.25 一致）。
                    for chunk in buf.as_chunks_mut::<2>().0.iter_mut() {
                        let v = u16::from_be_bytes(*chunk);
                        chunk.copy_from_slice(&v.to_ne_bytes());
                    }
                }
            }
            SampleEncoding::Ascii => {
                // 逐样本解析 ASCII 十进制（值不超过 u16）。
                if is_16 {
                    for chunk in buf.as_chunks_mut::<2>().0.iter_mut() {
                        let v = read_separated_ascii(&mut self.reader)?;
                        chunk.copy_from_slice(&v.to_ne_bytes());
                    }
                } else {
                    for b in buf.iter_mut() {
                        *b = read_separated_ascii(&mut self.reader)? as u8;
                    }
                }
            }
        }

        // MAXVAL 缩放（与 image 0.25 相同：不大于目标位深时线性放大）。
        self.scale_samples(buf, is_16);

        Ok(())
    }

    /// 把样本线性缩放到目标位深（8bit→255 / 16bit→65535）。
    fn scale_samples(&self, buf: &mut [u8], is_16: bool) {
        let target_max = if is_16 { 65535u32 } else { 255u32 };
        if self.maxval == target_max {
            return;
        }
        let factor = target_max as f32 / self.maxval as f32;
        if is_16 {
            for chunk in buf.as_chunks_mut::<2>().0.iter_mut() {
                let v = (f32::from(u16::from_ne_bytes(*chunk)) * factor).round() as u16;
                chunk.copy_from_slice(&v.to_ne_bytes());
            }
        } else {
            for v in buf.iter_mut() {
                *v = (f32::from(*v) * factor).round() as u8;
            }
        }
    }

    fn read_pbm(&mut self, buf: &mut [u8]) -> ImageResult<()> {
        let width = self.width as usize;
        let height = self.height as usize;
        match self.subtype.sample_encoding() {
            SampleEncoding::Binary if matches!(self.tuple, TupleType::PbmBit) => {
                let linelen = width.div_ceil(8);
                let bytecount = height * linelen;
                let mut packed = vec![0u8; bytecount];
                self.reader.read_exact(&mut packed)?;
                for y in 0..height {
                    for x in 0..width {
                        let shift = 7 - (x % 8);
                        let v = (packed[y * linelen + x / 8] >> shift) & 0x1;
                        // PBM：1 表示黑 → 0；0 表示白 → 255。
                        buf[y * width + x] = 255 * (1 - v);
                    }
                }
                Ok(())
            }
            _ if matches!(self.tuple, TupleType::BWBit) => {
                // PAM BLACKANDWHITE：单字节样本，0=黑、1=白，线性展宽（不反转）。
                self.reader.read_exact(buf)?;
                if let Some(&v) = buf.iter().find(|&&v| v > 1) {
                    return Err(pnm_err(format!("palette index {v} out of bounds")));
                }
                for b in buf.iter_mut() {
                    *b *= 255;
                }
                Ok(())
            }
            _ => {
                // ASCII PBM（P1）：逐位解析，1 表示黑 → 0；0 表示白 → 255。
                for b in buf.iter_mut() {
                    let v = loop {
                        let mut byte = [0u8; 1];
                        if self.reader.read(&mut byte)? == 0 {
                            return Err(pnm_err("InputTooShort"));
                        }
                        let c = byte[0];
                        if is_space(c) {
                            continue;
                        }
                        match c {
                            b'0' => break 255u8,
                            b'1' => break 0u8,
                            c => return Err(pnm_err(format!("unexpected byte {c:#04X?} in raster"))),
                        }
                    };
                    *b = v;
                }
                Ok(())
            }
        }
    }
}

/// 读取一个 ASCII 十进制样本（空白分隔，值域 u16）。
fn read_separated_ascii(reader: &mut impl Read) -> ImageResult<u16> {
    let mut v: u64 = 0;
    let mut had_any = false;
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                if had_any {
                    break;
                }
                return Err(pnm_err("InputTooShort"));
            }
            Err(e) => return Err(ImageError::IoError(e)),
            Ok(_) => {
                let c = byte[0];
                if is_space(c) {
                    if had_any {
                        break;
                    }
                    continue;
                }
                if c.is_ascii_digit() {
                    v = v
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(u64::from(c - b'0')))
                        .ok_or_else(|| pnm_err("overflow parsing sample"))?;
                    had_any = true;
                } else {
                    return Err(pnm_err(format!("non-digit byte {c:#04X?} in sample raster")));
                }
            }
        }
    }
    if v > u64::from(u16::MAX) {
        return Err(pnm_err("sample overflow"));
    }
    Ok(v as u16)
}

impl<R: Read> ImageDecoder for PnmDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        match self.tuple {
            TupleType::PbmBit | TupleType::BWBit | TupleType::GrayU8 => ColorType::L8,
            TupleType::GrayAlphaU8 => ColorType::La8,
            TupleType::GrayU16 => ColorType::L16,
            TupleType::GrayAlphaU16 => ColorType::La16,
            TupleType::RGBU8 => ColorType::Rgb8,
            TupleType::RGBAlphaU8 => ColorType::Rgba8,
            TupleType::RGBU16 => ColorType::Rgb16,
            TupleType::RGBAlphaU16 => ColorType::Rgba16,
        }
    }

    fn original_color_type(&self) -> ExtendedColorType {
        match self.tuple {
            TupleType::PbmBit | TupleType::BWBit => ExtendedColorType::L1,
            TupleType::GrayU8 => ExtendedColorType::L8,
            TupleType::GrayAlphaU8 => ExtendedColorType::La8,
            TupleType::GrayU16 => ExtendedColorType::L16,
            TupleType::GrayAlphaU16 => ExtendedColorType::La16,
            TupleType::RGBU8 => ExtendedColorType::Rgb8,
            TupleType::RGBAlphaU8 => ExtendedColorType::Rgba8,
            TupleType::RGBU16 => ExtendedColorType::Rgb16,
            TupleType::RGBAlphaU16 => ExtendedColorType::Rgba16,
        }
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        self.read_samples(buf)
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
