//! BMP 解码器（手写实现，行为与 image 0.25 对齐：1/2/4/8bit 调色板、16/24/32bit、RLE、bitfields）。

// 解码内核逐游标传参 + LUT 按索引循环（字节恒等硬约束）→ 放行。
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]

use std::io::{BufRead, Read, Seek, SeekFrom};

use kimg_core::color::ColorType;
use kimg_core::error::{DecodingError, ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind};
use kimg_core::format::ImageFormat;
use kimg_core::metadata::Orientation;
use crate::{ImageDecoder};

const BITMAPCOREHEADER_SIZE: u32 = 12;
const BITMAPINFOHEADER_SIZE: u32 = 40;
const BITMAPV2HEADER_SIZE: u32 = 52;
const BITMAPV3HEADER_SIZE: u32 = 56;
const BITMAPV4HEADER_SIZE: u32 = 108;
const BITMAPV5HEADER_SIZE: u32 = 124;

/// 3/4 bit → 8 bit 展宽表（3/4bit 的位重复展开恰等于查表 i*255/max 舍入，两者相等用表）。
/// 5/6 bit 走位重复算术（见 `Bitfield::read`），与 C# Sdcb 的 Widen(bits) 逐位一致。
static LOOKUP_3BIT: [u8; 8] = [0, 36, 73, 109, 146, 182, 219, 255];
static LOOKUP_4BIT: [u8; 16] = [
    0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255,
];

/// 最大宽高（软上限）。
const MAX_WIDTH_HEIGHT: i32 = 0xFFFF;

const RLE_ESCAPE: u8 = 0;

#[derive(Clone, Copy, PartialEq)]
enum ImageType {
    Palette,
    RGB16,
    RGB24,
    RGB32,
    RGBA32,
    RLE8,
    RLE4,
    Bitfields16,
    Bitfields32,
}

/// `read_full_bytes` 的 32bit「无 alpha 输出」来源布局（决定丢弃哪个字节）；
/// 24bit / RGBA32 输出不涉及（传 `None`）。
#[derive(Clone, Copy, PartialEq)]
enum DropByte {
    /// 源 [B,G,R,X]（BI_RGB 32bit：B 在低字节，X 为第 4 字节）→ 丢 byte3。
    Last,
    /// 源 [X,B,G,R]（888 BITFIELDS 快路径：r/g/b 掩码落在 byte3/2/1，X 为 byte0）→ 丢 byte0。
    First,
}

/// BMP 头类型。
#[derive(Clone, Copy, PartialEq)]
enum HeaderType {
    Core,
    Info,
    V2,
    V3,
    V4,
    V5,
}

#[derive(Clone, Copy, PartialEq)]
struct Bitfield {
    shift: u32,
    len: u32,
}

impl Bitfield {
    fn from_mask(mask: u32, max_len: u32) -> ImageResult<Bitfield> {
        if mask == 0 {
            return Ok(Bitfield { shift: 0, len: 0 });
        }
        let mut shift = mask.trailing_zeros();
        let mut len = (!(mask >> shift)).trailing_zeros();
        if len != mask.count_ones() {
            return Err(image_err("Non-contiguous bitfield mask"));
        }
        if len + shift > max_len {
            return Err(image_err("Invalid bitfield mask"));
        }
        if len > 8 {
            shift += len - 8;
            len = 8;
        }
        Ok(Bitfield { shift, len })
    }

    /// 抽出通道并展宽到 8bit（5/6 bit 用**位重复**，与 C# Sdcb 的 `Widen` 逐位一致：
    /// 5bit `(v<<3)|(v>>2)`、6bit `(v<<2)|(v>>4)`；3/4bit 位重复恰等于 image 查表缩放，仍用表）。
    fn read(&self, data: u32) -> u8 {
        let data = data >> self.shift;
        match self.len {
            1 => ((data & 0b1) * 0xff) as u8,
            2 => ((data & 0b11) * 0x55) as u8,
            3 => LOOKUP_3BIT[(data & 0b00_0111) as usize],
            4 => LOOKUP_4BIT[(data & 0b00_1111) as usize],
            5 => (((data & 0x1f) << 3) | ((data & 0x1f) >> 2)) as u8,
            6 => (((data & 0x3f) << 2) | ((data & 0x3f) >> 4)) as u8,
            7 => (((data & 0x7f) << 1) | ((data & 0x7f) >> 6)) as u8,
            8 => (data & 0xff) as u8,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Copy)]
struct Bitfields {
    r: Bitfield,
    g: Bitfield,
    b: Bitfield,
    a: Bitfield,
}

impl Bitfields {
    fn from_masks(r: u32, g: u32, b: u32, a: u32, max_len: u32) -> ImageResult<Bitfields> {
        let bf = Bitfields {
            r: Bitfield::from_mask(r, max_len)?,
            g: Bitfield::from_mask(g, max_len)?,
            b: Bitfield::from_mask(b, max_len)?,
            a: Bitfield::from_mask(a, max_len)?,
        };
        if bf.r.len == 0 || bf.g.len == 0 || bf.b.len == 0 {
            return Err(image_err("Missing bitfield mask"));
        }
        Ok(bf)
    }
}

const R5_G5_B5: Bitfields = Bitfields {
    r: Bitfield { len: 5, shift: 10 },
    g: Bitfield { len: 5, shift: 5 },
    b: Bitfield { len: 5, shift: 0 },
    a: Bitfield { len: 0, shift: 0 },
};

/// 标准 565 掩码（BITFIELDS16 常见的 r=0xF800 g=0x07E0 b=0x001F）。
const R5_G6_B5: Bitfields = Bitfields {
    r: Bitfield { len: 5, shift: 11 },
    g: Bitfield { len: 6, shift: 5 },
    b: Bitfield { len: 5, shift: 0 },
    a: Bitfield { len: 0, shift: 0 },
};

/// 16bpp 标准布局标记：仅这两种精确掩码（且 alpha 缺席）走 SIMD 行内核，
/// 其余任意掩码（444、rgb565-444 等）保持通用 LUT 路径。
#[derive(Clone, Copy)]
enum Bmp16Fmt {
    Rgb555,
    Rgb565,
}

fn image_err(msg: impl std::fmt::Display) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormat::Bmp.into(),
        msg.to_string(),
    ))
}

/// BMP 解码器。
pub struct BmpDecoder<R> {
    reader: R,
    width: i32,
    height: i32,
    data_offset: u64,
    top_down: bool,
    no_file_header: bool,
    add_alpha_channel: bool,
    image_type: ImageType,
    bit_count: u16,
    palette: Vec<[u8; 3]>,
    bitfields: Option<Bitfields>,
    header_type: HeaderType,
}

impl<R: BufRead + Seek> BmpDecoder<R> {
    /// 构造（读取标准 BITMAPFILEHEADER）。
    pub fn new(reader: R) -> ImageResult<BmpDecoder<R>> {
        let mut d = Self::empty(reader);
        d.read_metadata(false)?;
        Ok(d)
    }

    /// 以 ICO 内嵌图片形式构造（无文件头、强制 alpha、高度折半）。
    /// 拆分后由 kimg_ico 的 `IcoDecoder` 跨 crate 调用，故为 `pub`。
    pub fn new_with_ico_format(reader: R) -> ImageResult<BmpDecoder<R>> {
        let mut d = Self::empty(reader);
        d.no_file_header = true;
        d.add_alpha_channel = true;
        d.read_metadata(true)?;
        d.height /= 2;
        Ok(d)
    }

    fn empty(reader: R) -> BmpDecoder<R> {
        BmpDecoder {
            reader,
            width: 0,
            height: 0,
            data_offset: 0,
            top_down: false,
            no_file_header: false,
            add_alpha_channel: false,
            image_type: ImageType::Palette,
            bit_count: 0,
            palette: Vec::new(),
            bitfields: None,
            header_type: HeaderType::Info,
        }
    }

    fn read_file_header(&mut self) -> ImageResult<()> {
        if self.no_file_header {
            return Ok(());
        }
        let mut signature = [0; 2];
        self.reader.read_exact(&mut signature)?;
        if signature != *b"BM" {
            return Err(image_err("BMP signature not found"));
        }
        // 文件大小 + 保留字。
        self.read_u32()?;
        self.read_u32()?;
        self.data_offset = u64::from(self.read_u32()?);
        Ok(())
    }

    fn read_u16(&mut self) -> ImageResult<u16> {
        let mut b = [0u8; 2];
        self.reader.read_exact(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn read_u32(&mut self) -> ImageResult<u32> {
        let mut b = [0u8; 4];
        self.reader.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_i32(&mut self) -> ImageResult<i32> {
        Ok(self.read_u32()? as i32)
    }

    fn check_overflow(&self) -> ImageResult<()> {
        if self.width <= 0 || self.height <= 0 {
            return Err(image_err("invalid dimensions"));
        }
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(self.num_channels()))
            .ok_or_else(|| {
                ImageError::Unsupported(UnsupportedError::from_format_and_kind(
                    ImageFormat::Bmp.into(),
                    UnsupportedErrorKind::GenericFeature(format!(
                        "Image dimensions ({}x{}) are too large",
                        self.width, self.height
                    )),
                ))
            })?;
        Ok(())
    }

    fn num_channels(&self) -> usize {
        if self.add_alpha_channel {
            4
        } else {
            3
        }
    }

    fn read_core_header(&mut self) -> ImageResult<()> {
        self.width = i32::from(self.read_u16()?);
        self.height = i32::from(self.read_u16()?);
        self.check_overflow()?;
        if self.read_u16()? != 1 {
            return Err(image_err("More than one plane"));
        }
        self.bit_count = self.read_u16()?;
        self.image_type = match self.bit_count {
            1 | 4 | 8 => ImageType::Palette,
            24 => ImageType::RGB24,
            _ => return Err(image_err("Invalid channel bit count")),
        };
        Ok(())
    }

    fn read_info_header(&mut self) -> ImageResult<()> {
        self.width = self.read_i32()?;
        self.height = self.read_i32()?;

        if self.width < 0 {
            return Err(image_err("Negative width"));
        } else if self.width > MAX_WIDTH_HEIGHT || self.height > MAX_WIDTH_HEIGHT {
            return Err(image_err(format!(
                "Image too large (one of ({}, {}) > soft limit of {MAX_WIDTH_HEIGHT})",
                self.width, self.height
            )));
        }
        if self.height == i32::MIN {
            return Err(image_err("Invalid height"));
        }
        if self.height < 0 {
            self.height *= -1;
            self.top_down = true;
        }
        self.check_overflow()?;

        if self.read_u16()? != 1 {
            return Err(image_err("More than one plane"));
        }
        self.bit_count = self.read_u16()?;
        let compression = self.read_u32()?;

        if self.top_down && compression != 0 && compression != 3 {
            return Err(image_err(format!(
                "Invalid image type {compression} for top-down image."
            )));
        }
        self.image_type = match compression {
            0 => match self.bit_count {
                1 | 2 | 4 | 8 => ImageType::Palette,
                16 => ImageType::RGB16,
                24 => ImageType::RGB24,
                32 if self.add_alpha_channel => ImageType::RGBA32,
                32 => ImageType::RGB32,
                _ => return Err(image_err("Invalid channel bit count")),
            },
            1 => match self.bit_count {
                8 => ImageType::RLE8,
                _ => return Err(image_err("Invalid RLE8 bit count")),
            },
            2 => match self.bit_count {
                4 => ImageType::RLE4,
                _ => return Err(image_err("Invalid RLE4 bit count")),
            },
            3 => match self.bit_count {
                16 => ImageType::Bitfields16,
                32 => ImageType::Bitfields32,
                _ => return Err(image_err("Invalid bitfields bit count")),
            },
            4 => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Bmp.into(),
                        UnsupportedErrorKind::GenericFeature("JPEG compression".to_owned()),
                    ),
                ))
            }
            5 => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Bmp.into(),
                        UnsupportedErrorKind::GenericFeature("PNG compression".to_owned()),
                    ),
                ))
            }
            11..=13 => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Bmp.into(),
                        UnsupportedErrorKind::GenericFeature("CMYK format".to_owned()),
                    ),
                ))
            }
            t => return Err(image_err(format!("Unknown image compression type {t}"))),
        };

        // 图像数据大小、水平/垂直分辨率。
        self.read_u32()?;
        self.read_u32()?;
        self.read_u32()?;
        // colors used / important colors。
        self.read_u32()?;
        self.read_u32()?;
        Ok(())
    }

    fn read_bitmasks(&mut self) -> ImageResult<()> {
        let r = self.read_u32()?;
        let g = self.read_u32()?;
        let b = self.read_u32()?;
        let a = match self.header_type {
            HeaderType::V3 | HeaderType::V4 | HeaderType::V5 => self.read_u32()?,
            _ => 0,
        };
        self.bitfields = match self.image_type {
            ImageType::Bitfields16 => {
                Some(Bitfields::from_masks(r, g, b, a, 16)?)
            }
            ImageType::Bitfields32 => {
                Some(Bitfields::from_masks(r, g, b, a, 32)?)
            }
            _ => None,
        };
        if self.bitfields.is_some() && a != 0 {
            self.add_alpha_channel = true;
        }
        Ok(())
    }

    fn read_metadata(&mut self, ico_format: bool) -> ImageResult<()> {
        self.read_file_header()?;
        let header_offset = self.reader.stream_position()?;
        let header_size = self.read_u32()?;
        let header_end = header_offset + u64::from(header_size);

        self.header_type = match header_size {
            BITMAPCOREHEADER_SIZE => HeaderType::Core,
            BITMAPINFOHEADER_SIZE => HeaderType::Info,
            BITMAPV2HEADER_SIZE => HeaderType::V2,
            BITMAPV3HEADER_SIZE => HeaderType::V3,
            BITMAPV4HEADER_SIZE => HeaderType::V4,
            BITMAPV5HEADER_SIZE => HeaderType::V5,
            s if s < BITMAPCOREHEADER_SIZE => return Err(image_err("Bitmap header too small")),
            _ => {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Bmp.into(),
                        UnsupportedErrorKind::GenericFeature(format!(
                            "Unknown bitmap header type (size={header_size})"
                        )),
                    ),
                ))
            }
        };

        match self.header_type {
            HeaderType::Core => self.read_core_header()?,
            _ => self.read_info_header()?,
        }

        let mut bitmask_offset = 0;
        if self.image_type == ImageType::Bitfields16 || self.image_type == ImageType::Bitfields32 {
            self.read_bitmasks()?;
            // Info/V4/V5 的 bitfields 掩码接在头之后本应被跳过——但它们在 40 头下紧随头数据。
            if matches!(
                self.header_type,
                HeaderType::Info | HeaderType::V4 | HeaderType::V5
            ) {
                bitmask_offset = 12;
            }
        }

        self.reader
            .seek(SeekFrom::Start(header_end + bitmask_offset))?;

        match self.image_type {
            ImageType::Palette | ImageType::RLE4 | ImageType::RLE8 => self.read_palette()?,
            _ => {}
        }

        if self.no_file_header {
            self.data_offset = self.reader.stream_position()?;
        }

        if ico_format {
            self.add_alpha_channel = true;
        }

        Ok(())
    }

    fn bytes_per_color(&self) -> usize {
        match self.header_type {
            HeaderType::Core => 3,
            _ => 4,
        }
    }

    fn read_palette(&mut self) -> ImageResult<()> {
        const MAX_PALETTE_SIZE: usize = 256;
        let bpc = self.bytes_per_color();
        let palette_size = (1usize << self.bit_count).min(MAX_PALETTE_SIZE);
        let length = palette_size * bpc;
        let mut buf = vec![0u8; bpc * MAX_PALETTE_SIZE];
        let n = length.min(buf.len());
        self.reader.read_exact(&mut buf[..n])?;
        if length > MAX_PALETTE_SIZE * bpc {
            self.reader
                .seek(SeekFrom::Current((length - MAX_PALETTE_SIZE * bpc) as i64))?;
        }

        self.palette = (0..MAX_PALETTE_SIZE)
            .map(|i| {
                let b = buf[bpc * i];
                let g = buf[bpc * i + 1];
                let r = buf[bpc * i + 2];
                [r, g, b]
            })
            .collect();
        Ok(())
    }

    /// 按行方向产出行切片迭代器（底 → 顶，或顶 → 底）。
    fn rows_mut<'a>(
        &self,
        buf: &'a mut [u8],
    ) -> Box<dyn Iterator<Item = &'a mut [u8]> + 'a> {
        let row_width = (self.width as usize) * self.num_channels();
        if self.top_down {
            Box::new(buf.chunks_mut(row_width))
        } else {
            Box::new(buf.chunks_mut(row_width).rev())
        }
    }

    /// 像素数据读入 `buf`（`buf` 长度须等于 `total_bytes`）；供 BmpDecoder::read_image 与
    /// kimg_ico（读后接 AND 掩码）跨 crate 调用，故为 `pub`。
    pub fn read_image_data(&mut self, buf: &mut [u8]) -> ImageResult<()> {
        self.reader.seek(SeekFrom::Start(self.data_offset))?;
        match self.image_type {
            ImageType::Palette => self.read_palette_buf(buf),
            ImageType::RGB16 => self.read_16bit(buf, Some(&R5_G5_B5)),
            ImageType::RGB24 => self.read_full_bytes(buf, 3, None),
            ImageType::RGB32 => self.read_full_bytes(buf, 4, Some(DropByte::Last)),
            ImageType::RGBA32 => self.read_full_bytes(buf, 4, None),
            ImageType::RLE8 => self.read_rle(buf, true),
            ImageType::RLE4 => self.read_rle(buf, false),
            ImageType::Bitfields16 => {
                let bf = self
                    .bitfields
                    .ok_or_else(|| image_err("missing mask"))?;
                self.read_16bit(buf, Some(&bf))
            }
            ImageType::Bitfields32 => {
                let bf = self
                    .bitfields
                    .ok_or_else(|| image_err("missing mask"))?;
                // 常见的 888/8888 掩码直接按字节流快读，其余按位移抽位。
                let (r8, g8, b8) = (
                    Bitfield { shift: 24, len: 8 },
                    Bitfield { shift: 16, len: 8 },
                    Bitfield { shift: 8, len: 8 },
                );
                if bf.r == r8 && bf.g == g8 && bf.b == b8 && bf.a.len == 0 {
                    // r/g/b 掩码落在 byte3/2/1 → 源 [X,B,G,R]，丢 byte0。
                    self.read_full_bytes(buf, 4, Some(DropByte::First))
                } else if bf.r == (Bitfield { shift: 16, len: 8 })
                    && bf.g == (Bitfield { shift: 8, len: 8 })
                    && bf.b == (Bitfield { shift: 0, len: 8 })
                    && bf.a == r8
                {
                    // A8R8G8B8（r/g/b/a 掩码落在 byte2/1/0/3）→ 源 [B,G,R,A] 直出 RGBA：
                    // (4,4) 行内核 bgra_to_rgba_* SIMD 原位重排。alpha 掩码须恰为 byte3
                    // （== r8 的 shift 24）；原 `bf.a == a8`（byte0）与 b 同掩码层永不命中，
                    // 使 argb32-v4 类文件一直走通用抽位路径（≈2.5× 于 C#）。
                    self.read_full_bytes(buf, 4, None)
                } else {
                    self.read_32bit_bitfields(buf, &bf)
                }
            }
        }
    }

    fn read_palette_buf(&mut self, buf: &mut [u8]) -> ImageResult<()> {
        let channels = self.num_channels();
        let row_byte_length = ((i32::from(self.bit_count) * self.width + 31) / 32 * 4) as usize;
        let mut indices = vec![0u8; row_byte_length];
        if channels == 4 {
            buf.chunks_exact_mut(4).for_each(|c| c[3] = 0xFF);
        }
        let bit_count = self.bit_count;
        let width = self.width as usize;
        let palette = &self.palette;

        // 8bit 调色板主量走 AVX2 gather 行内核（`pshufb` 取指仅 4 位 = 16 项，256 色任
        // 意表须 gather）。LUT 展开为扁平 RGBA dword 表（1KB L1 常驻），逐行复用。
        let simd8 = bit_count == 8 && kimg_core::simd::gate("KIMG_BMP_NO_SIMD", "avx2");
        let mut lut8 = [0u8; 1024];
        if simd8 {
            for (i, rgb) in palette.iter().enumerate() {
                lut8[4 * i..4 * i + 3].copy_from_slice(rgb);
                lut8[4 * i + 3] = 0xFF;
            }
        }

        // 4bit 调色板 = 16 项，SSSE3 `pshufb` 恰可单张查表（低 4 位取指）；分 R/G/B 三张
        // 16 字节 LUT。
        let simd4 = bit_count == 4 && kimg_core::simd::gate("KIMG_BMP_NO_SIMD", "ssse3");
        let mut lut4 = [[0u8; 16]; 3];
        if simd4 {
            for (i, rgb) in palette.iter().take(16).enumerate() {
                lut4[0][i] = rgb[0];
                lut4[1][i] = rgb[1];
                lut4[2][i] = rgb[2];
            }
        }

        for row in self.rows_mut(buf) {
            self.reader.read_exact(&mut indices)?;
            let mut pixel_iter = row.chunks_exact_mut(channels);
            match bit_count {
                1 => {
                    for &idx in &indices {
                        for (bit_shift, pixel) in pixel_iter.by_ref().take(8).enumerate() {
                            let bit = 0x80u8 >> bit_shift;
                            let rgb = palette[usize::from((idx & bit) != 0)];
                            pixel[..3].copy_from_slice(&rgb);
                        }
                    }
                }
                2 => {
                    for &idx in &indices {
                        for nib in [idx >> 6 & 0x3, idx >> 4 & 0x3, idx >> 2 & 0x3, idx & 0x3] {
                            if let Some(pixel) = pixel_iter.next() {
                                let rgb = palette[nib as usize];
                                pixel[..3].copy_from_slice(&rgb);
                            } else {
                                break;
                            }
                        }
                    }
                }
                4 => {
                    if simd4 {
                        #[cfg(target_arch = "x86_64")]
                        {
                            let start = unsafe {
                                bgr_ops::expand_palette4_ssse3(
                                    &indices,
                                    row,
                                    width,
                                    channels,
                                    &lut4,
                                )
                            };
                            // 标量尾：SIMD 吃 0..start（start 为 16 的倍数 → 整索引字节
                            // start/2 起始），余下按旧语义逐 nibble 补齐（字节恒等）。
                            let mut iter = row.chunks_mut(channels).enumerate().skip(start);
                            for &idx in &indices[start / 2..] {
                                for nib in [idx >> 4, idx & 0xf] {
                                    if let Some((_, pixel)) = iter.next() {
                                        let rgb = palette[nib as usize];
                                        pixel[..3].copy_from_slice(&rgb);
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                        #[cfg(not(target_arch = "x86_64"))]
                        {
                            unreachable!("非 x86_64 上 simd4 恒 false，不应进入本分支");
                        }
                    } else {
                        for &idx in &indices {
                            for nib in [idx >> 4, idx & 0xf] {
                                if let Some(pixel) = pixel_iter.next() {
                                    let rgb = palette[nib as usize];
                                    pixel[..3].copy_from_slice(&rgb);
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
                8 => {
                    if simd8 {
                        #[cfg(target_arch = "x86_64")]
                        {
                            let start = unsafe {
                                bgr_ops::expand_palette8_avx2(
                                    &indices,
                                    row,
                                    width,
                                    channels,
                                    lut8.as_ptr(),
                                )
                            };
                            for (i, pixel) in row.chunks_mut(channels).enumerate().skip(start) {
                                let rgb = palette[indices[i] as usize];
                                pixel[..3].copy_from_slice(&rgb);
                            }
                        }
                        #[cfg(not(target_arch = "x86_64"))]
                        {
                            unreachable!("非 x86_64 上 simd8 恒 false，不应进入本分支");
                        }
                    } else {
                        for &idx in &indices {
                            if let Some(pixel) = pixel_iter.next() {
                                let rgb = palette[idx as usize];
                                pixel[..3].copy_from_slice(&rgb);
                            } else {
                                break;
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
            let _ = width;
        }

        Ok(())
    }

    fn read_16bit(&mut self, buf: &mut [u8], bf: Option<&Bitfields>) -> ImageResult<()> {
        let bf = match bf {
            Some(b) => b,
            None => self.bitfields.as_ref().unwrap(),
        };
        let channels = self.num_channels();
        let width = self.width as usize;
        let padding_len = width % 2 * 2;

        // 每通道预计算 LUT（覆盖该掩码全部取指），语义与 Bitfield::read 逐项一致（含
        // 3/4bit 查表展宽——SIMD 行内核只接管 5/6bit 位重复的标准 555/565，故可混用）。
        let rl = bitfield_lut(&bf.r);
        let gl = bitfield_lut(&bf.g);
        let bl = bitfield_lut(&bf.b);
        let al = bitfield_lut(&bf.a);

        // 标准 555/565 掩码（alpha 缺席）走 SIMD 行内核：展宽为位重复，与标量逐位一致；
        // 其余任意掩码（444、rgb565-444 等）保持通用 LUT 路径，字节恒等闸覆盖。
        let fmt = if bf.r == R5_G5_B5.r
            && bf.g == R5_G5_B5.g
            && bf.b == R5_G5_B5.b
            && bf.a.len == 0
        {
            Some(Bmp16Fmt::Rgb555)
        } else if bf.r == R5_G6_B5.r
            && bf.g == R5_G6_B5.g
            && bf.b == R5_G6_B5.b
            && bf.a.len == 0
        {
            Some(Bmp16Fmt::Rgb565)
        } else {
            None
        };
        let simd = kimg_core::simd::gate("KIMG_BMP_NO_SIMD", "ssse3");

        // 整行读取再批量展开（消除逐像素 read_u16_le 调用）。
        let mut raw = vec![0u8; width * 2];
        for row in self.rows_mut(buf) {
            self.reader.read_exact(&mut raw)?;
            let mut start = 0usize;
            // 行内核先吃掉宽行主径，剩余 <4 像素交标量尾（字节恒等）。
            if simd
                && let Some(f) = fmt {
                    #[cfg(target_arch = "x86_64")]
                    {
                        start = unsafe { bgr_ops::expand_16bit_simd(&raw, row, width, channels, f) };
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        let _ = f;
                        let _ = channels;
                        unreachable!("非 x86_64 上 simd 恒 false，不应进入本分支");
                    }
                }
            for (i, pixel) in row.chunks_mut(channels).enumerate().skip(start) {
                let data = u16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]);
                pixel[0] = rl[((data >> bf.r.shift) & ((1 << bf.r.len) - 1)) as usize];
                pixel[1] = gl[((data >> bf.g.shift) & ((1 << bf.g.len) - 1)) as usize];
                pixel[2] = bl[((data >> bf.b.shift) & ((1 << bf.b.len) - 1)) as usize];
                if channels == 4 {
                    pixel[3] = if bf.a.len != 0 {
                        al[((data >> bf.a.shift) & ((1 << bf.a.len) - 1)) as usize]
                    } else {
                        0xFF
                    };
                }
            }
            let mut pad = [0u8; 2];
            self.reader.read_exact(&mut pad[..padding_len])?;
        }
        Ok(())
    }

    fn read_32bit_bitfields(&mut self, buf: &mut [u8], bf: &Bitfields) -> ImageResult<()> {
        let channels = self.num_channels();
        let width = self.width as usize;

        let rl = bitfield_lut(&bf.r);
        let gl = bitfield_lut(&bf.g);
        let bl = bitfield_lut(&bf.b);
        let al = bitfield_lut(&bf.a);

        // 32bit 行无填充（行宽必为 4 倍数），整行读取批量展开。
        let mut raw = vec![0u8; width * 4];
        for row in self.rows_mut(buf) {
            self.reader.read_exact(&mut raw)?;
            for (i, pixel) in row.chunks_mut(channels).enumerate() {
                let data = u32::from_le_bytes([
                    raw[4 * i],
                    raw[4 * i + 1],
                    raw[4 * i + 2],
                    raw[4 * i + 3],
                ]);
                pixel[0] = rl[((data >> bf.r.shift) & ((1 << bf.r.len) - 1)) as usize];
                pixel[1] = gl[((data >> bf.g.shift) & ((1 << bf.g.len) - 1)) as usize];
                pixel[2] = bl[((data >> bf.b.shift) & ((1 << bf.b.len) - 1)) as usize];
                if channels == 4 {
                    pixel[3] = if bf.a.len != 0 {
                        al[((data >> bf.a.shift) & ((1 << bf.a.len) - 1)) as usize]
                    } else {
                        0xFF
                    };
                }
            }
        }
        Ok(())
    }

    fn read_full_bytes(
        &mut self,
        buf: &mut [u8],
        bpp: usize,
        drop: Option<DropByte>,
    ) -> ImageResult<()> {
        let channels = self.num_channels();
        let width = self.width as usize;
        let row_padding = match bpp {
            3 => (4 - (width * 3) % 4) % 4,
            _ => 0,
        };
        // 32bit 无 alpha 输出的通道选位（drop 布局决定）：Last=[B,G,R,X]→R/G/B=byte2/1/0；
        // First=[X,B,G,R]→R/G/B=byte3/2/1。24bit / RGBA32 不用。
        let (r_i, g_i, b_i) = match drop {
            Some(DropByte::Last) => (2usize, 1usize, 0usize),
            Some(DropByte::First) => (3usize, 2usize, 1usize),
            None => (0, 0, 0),
        };
        // SIMD 行内批处理：整行一次性读入（消除逐像素 read_exact 调用），
        // SSSE3/AVX2 shuffle 重排通道，标量尾收齐 SIMD 未覆盖的像素。
        let simd = kimg_core::simd::gate("KIMG_BMP_NO_SIMD", "ssse3");
        let raw_len = width * bpp;
        let mut raw = vec![0u8; raw_len];
        let mut pad = [0u8; 4];

        for row in self.rows_mut(buf) {
            if !simd {
                if drop.is_some() {
                    // 32bit 无 alpha：读 4 字节按选位出 [R,G,B]（保持不透明语义）。
                    let mut px = [0u8; 4];
                    for pixel in row.chunks_mut(channels) {
                        self.reader.read_exact(&mut px)?;
                        pixel[0] = px[r_i];
                        pixel[1] = px[g_i];
                        pixel[2] = px[b_i];
                    }
                } else {
                    for pixel in row.chunks_mut(channels) {
                        self.reader.read_exact(&mut pixel[..3])?;
                        pixel[..3].reverse();
                        if bpp == 4 {
                            // RGBA32：源 [B,G,R,A] → [R,G,B] + 读 alpha。
                            self.reader.read_exact(&mut pixel[3..4])?;
                        } else if channels == 4 {
                            pixel[3] = 0xFF;
                        }
                    }
                }
                let mut pad = [0u8; 4];
                self.reader.read_exact(&mut pad[..row_padding])?;
                continue;
            }

            #[cfg(target_arch = "x86_64")]
            {
                match (bpp, channels) {
                    (3, 3) => {
                        // BGR→RGB 3→3（源为独立 raw 缓冲，无回写冲突）。
                        self.reader.read_exact(&mut raw)?;
                        let n = unsafe { bgr_ops::bgr24_to_rgb(&raw, row, width) };
                        for i in n..width {
                            row[3 * i] = raw[3 * i + 2];
                            row[3 * i + 1] = raw[3 * i + 1];
                            row[3 * i + 2] = raw[3 * i];
                        }
                    }
                    (3, 4) => {
                        // BGR→RGBA（alpha=255），供 ICO/强制 alpha 的 24bit 输出。
                        self.reader.read_exact(&mut raw)?;
                        let mut x = 0;
                        if kimg_core::simd::detect("avx2") {
                            x += unsafe { bgr_ops::bgr24_to_rgba_avx2(&raw, row, width) };
                        }
                        if x < width {
                            x += unsafe {
                                bgr_ops::bgr24_to_rgba_ssse3(
                                    &raw[3 * x..],
                                    &mut row[4 * x..],
                                    width - x,
                                )
                            };
                        }
                        for i in x..width {
                            row[4 * i] = raw[3 * i + 2];
                            row[4 * i + 1] = raw[3 * i + 1];
                            row[4 * i + 2] = raw[3 * i];
                            row[4 * i + 3] = 0xFF;
                        }
                    }
                    (4, 3) => {
                        // 32bit 无 alpha 4→3 打包：按 drop 布局选 SIMD 内核与标量尾选位。
                        self.reader.read_exact(&mut raw)?;
                        let n = match drop {
                            Some(DropByte::Last) => {
                                unsafe { bgr_ops::bgra_to_rgb(&raw, row, width) }
                            }
                            Some(DropByte::First) => {
                                unsafe { bgr_ops::xbgr_to_rgb(&raw, row, width) }
                            }
                            None => unreachable!("(4,3) 必为无 alpha 布局"),
                        };
                        for i in n..width {
                            row[3 * i] = raw[4 * i + r_i];
                            row[3 * i + 1] = raw[4 * i + g_i];
                            row[3 * i + 2] = raw[4 * i + b_i];
                        }
                    }
                    (4, 4) => {
                        // RGBA32：源每像素 [B,G,R,A] → [R,G,B,A]，整行读入 row 后原位 4→4 重排。
                        debug_assert!(drop.is_none());
                        self.reader.read_exact(row)?;
                        let mut x = 0;
                        if kimg_core::simd::detect("avx2") {
                            x += unsafe { bgr_ops::bgra_to_rgba_avx2(row, width) };
                        }
                        if x < width {
                            x += unsafe { bgr_ops::bgra_to_rgba_ssse3(&mut row[4 * x..], width - x) };
                        }
                        for i in x..width {
                            row.swap(4 * i, 4 * i + 2);
                        }
                    }
                    _ => unreachable!(),
                }
                self.reader.read_exact(&mut pad[..row_padding])?;
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                let _ = (bpp, channels, row_padding, drop);
                unreachable!("非 x86_64 上 simd 恒 false，不应进入本分支");
            }
        }
        Ok(())
    }

    fn read_rle(&mut self, buf: &mut [u8], is_8bit: bool) -> ImageResult<()> {
        let channels = self.num_channels();
        let palette = &self.palette;
        let w = self.width as usize;

        // 3 通道（RLE 文件自然输出）→ 索引缓冲 + 行末整行 SIMD 展开（复用 R4 palette 行内核，
        // RLE8 absolute/run 只写 1 字节索引、RLE4 逐像素 4bit 值，行末一次展开，替代逐像素
        // 调色板查表 + RGB 拷贝）；4 通道（仅 ICO 可达）与其余环境走旧逐像素直写路径，
        // 语义逐字节不变（索引路径的 SIMD 展开仅按 channels==3 推理，alpha 直写语义不涉）。
        // `KIMG_BMP_RLE_LEGACY=1` 强制旧逐像素直写路径（供同二进制 A/B 量化，默认关）。
        let force_legacy_rle = std::env::var("KIMG_BMP_RLE_LEGACY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let indexed = channels == 3 && cfg!(target_arch = "x86_64") && !force_legacy_rle;

        let simd8 = indexed
            && is_8bit
            && kimg_core::simd::gate("KIMG_BMP_NO_SIMD", "avx2");
        let simd4 = indexed
            && !is_8bit
            && kimg_core::simd::gate("KIMG_BMP_NO_SIMD", "ssse3");
        // LUT 预计算（姿势同 read_palette_buf）：8bit 扁平 RGBA dword 表 / 4bit 三通道 16 项表。
        let mut lut8 = [0u8; 1024];
        if simd8 {
            for (i, rgb) in palette.iter().enumerate() {
                lut8[4 * i..4 * i + 3].copy_from_slice(rgb);
                lut8[4 * i + 3] = 0xFF;
            }
        }
        let mut lut4 = [[0u8; 16]; 3];
        if simd4 {
            for (i, rgb) in palette.iter().take(16).enumerate() {
                lut4[0][i] = rgb[0];
                lut4[1][i] = rgb[1];
                lut4[2][i] = rgb[2];
            }
        }

        // 行索引缓冲：只记「已解码像素」的调色板索引（RLE8 = 源字节原样；RLE4 = 每像素低 4 位），
        // 行末按已解码段整段 SIMD 展开。delta/EOL/EOF/跳行产生的「填零区」不进缓冲，
        // 仍在行上直接 fill(0)（与旧语义一致，零是 RGBA 全零而非 palette[0]）。
        let mut idx = vec![0u8; w];
        let mut segs: Vec<(usize, usize)> = Vec::new(); // 本行已解码连续段 (start, len)。

        let rows: Vec<&mut [u8]> = self.rows_mut(buf).collect();
        let mut row_iter = rows.into_iter();
        // 首行（0 高/空缓冲直接收尾）。
        let mut row: &mut [u8] = match row_iter.next() {
            Some(r) => r,
            None => return Ok(()),
        };
        loop {
            // 当前行已写像素列（delta 换行时沿用列位置）。
            let mut x = 0usize;
            segs.clear();
            'row: loop {
                let control = read_u8_val(&mut self.reader)?;
                if control == RLE_ESCAPE {
                    let op = read_u8_val(&mut self.reader)?;
                    match op {
                        0 => {
                            // EndOfRow：行尾余白填零（run 越界写满后无余白，保持静默）。
                            if x < w {
                                row[x * channels..].fill(0);
                            }
                            break 'row;
                        }
                        1 => {
                            // EndOfFile：当前行余白 + 全部剩余行填零。
                            if x < w {
                                row[x * channels..].fill(0);
                            }
                            if indexed {
                                render_rle_segments(
                                    row,
                                    &idx,
                                    &segs,
                                    is_8bit,
                                    palette,
                                    &lut8,
                                    &lut4,
                                    simd8,
                                    simd4,
                                );
                            }
                            for r in row_iter {
                                r.fill(0);
                            }
                            return Ok(());
                        }
                        2 => {
                            // Delta（dx,dy）。
                            let xd = usize::from(read_u8_val(&mut self.reader)?);
                            let yd = usize::from(read_u8_val(&mut self.reader)?);
                            if yd > 0 {
                                if x < w {
                                    row[x * channels..].fill(0);
                                }
                                // 当前行在此放弃 → 先展开本行已解码段再换行。
                                if indexed {
                                    render_rle_segments(
                                        row,
                                        &idx,
                                        &segs,
                                        is_8bit,
                                        palette,
                                        &lut8,
                                        &lut4,
                                        simd8,
                                        simd4,
                                    );
                                }
                                for _ in 1..yd {
                                    let r = row_iter
                                        .next()
                                        .ok_or_else(|| image_err("Corrupt RLE data"))?;
                                    r.fill(0);
                                }
                                row = row_iter
                                    .next()
                                    .ok_or_else(|| image_err("Corrupt RLE data"))?;
                                // 跳过新行前 x 列（原逐像素 next 越界即 Err，保持该语义）。
                                if x > w {
                                    return Err(image_err("Corrupt RLE data"));
                                }
                                row[..x * channels].fill(0);
                                segs.clear();
                            }
                            if x + xd > w {
                                return Err(image_err("Corrupt RLE data"));
                            }
                            row[x * channels..(x + xd) * channels].fill(0);
                            x += xd;
                        }
                        _ => {
                            // Absolute：`n` 个字面像素（RLE8 = n 字节 / RLE4 = ceil(n/2) 字节，
                            // 每段按 16 位字边界补偶——补偶字节**非像素**，只跳过不解为像素；
                            // 与 C# Sdcb 一致）。
                            let n_pixels = op as usize;
                            let payload = if is_8bit {
                                n_pixels
                            } else {
                                n_pixels.div_ceil(2)
                            };
                            // 栈内定长缓冲（op ≤ 255 → payload ≤ 255/128），免每段堆分配。
                            let mut data = [0u8; 256];
                            self.reader.read_exact(&mut data[..payload])?;
                            if x + n_pixels > w {
                                return Err(image_err("Corrupt RLE data"));
                            }
                            if indexed {
                                // 索引缓冲累积：仅 ≥ 阈值的绝对段走「索引 + 行末整段展开」
                                // （RLE8 ≥8px / RLE4 ≥32px——调过之后 16–31px 的 4bit 段直接 RGB
                                // 直写：每段 kernel 调用须重载 LUT/掩码，段太小不抵 setup 开销；
                                // 低于阈值同样直写，免索引写 + 展开二次触达）。RLE8 索引 = 源
                                // 字节原样、RLE4 每字节高→低 nibble 顺次为像素——与 C# 一致。
                                let min_simd = if is_8bit { 8 } else { 32 };
                                if n_pixels >= min_simd {
                                    if is_8bit {
                                        idx[x..x + n_pixels].copy_from_slice(&data[..n_pixels]);
                                    } else {
                                        let mut xi = x;
                                        for (k, &b) in data[..payload].iter().enumerate() {
                                            idx[xi] = b >> 4;
                                            xi += 1;
                                            // 奇数尾像素只写高 nibble（低 nibble 丢弃）。
                                            if 2 * k + 1 < n_pixels {
                                                idx[xi] = b & 0x0f;
                                                xi += 1;
                                            }
                                        }
                                    }
                                    push_seg(&mut segs, x, n_pixels);
                                } else if is_8bit {
                                    let start = x * channels;
                                    for i in 0..n_pixels {
                                        let rgb = palette[data[i] as usize];
                                        row[start + i * channels..start + (i + 1) * channels][..3]
                                            .copy_from_slice(&rgb);
                                    }
                                } else {
                                    let mut off = x * channels;
                                    for (k, &b) in data[..payload].iter().enumerate() {
                                        let hi = &palette[(b >> 4) as usize];
                                        row[off..off + 3].copy_from_slice(&hi[..3]);
                                        off += channels;
                                        if 2 * k + 1 < n_pixels {
                                            let lo = &palette[(b & 0x0f) as usize];
                                            row[off..off + 3].copy_from_slice(&lo[..3]);
                                            off += channels;
                                        }
                                    }
                                }
                            } else {
                                // 旧逐像素直写（4ch / 非 x86_64），字节语义不变。
                                let start = x * channels;
                                if is_8bit {
                                    for i in 0..n_pixels {
                                        let rgb = palette[data[i] as usize];
                                        row[start + i * channels..start + (i + 1) * channels][..3]
                                            .copy_from_slice(&rgb);
                                    }
                                } else {
                                    let mut off = start;
                                    for (k, &b) in data[..payload].iter().enumerate() {
                                        let hi = &palette[(b >> 4) as usize];
                                        row[off..off + 3].copy_from_slice(&hi[..3]);
                                        off += channels;
                                        if 2 * k + 1 < n_pixels {
                                            let lo = &palette[(b & 0x0f) as usize];
                                            row[off..off + 3].copy_from_slice(&lo[..3]);
                                            off += channels;
                                        }
                                    }
                                }
                            }
                            x += n_pixels;
                            // 段后补偶字节（奇数 payload 才有）：非像素，尽力跳过（文件尾缺失可容忍）。
                            // 注：与补偶「严格」语义相比此处是宽容路径——若用 read_exact 全量读，
                            // 文件截断（含补偶缺失）会直接 Err；现用 `read` 尽力跳过、忽略返回。
                            if payload & 1 == 1 {
                                let mut pad = [0u8; 1];
                                let _ = self.reader.read(&mut pad);
                            }
                        }
                    }
                } else {
                    let palette_index = read_u8_val(&mut self.reader)?;
                    let n = control as usize;
                    if is_8bit {
                        // 8bit 重复：越界时静默截断（Imagemagick 兼容）。
                        if x < w {
                            let n_out = n.min(w - x);
                            if indexed {
                                // run 不走索引缓冲：fill_repeat 已是紧批量复制，直写 RGB
                                // 免「索引写 + 行末再展开」的二次触达（run 多的截图尤甚）。
                                let rgb = palette[palette_index as usize];
                                let pat = [rgb[0], rgb[1], rgb[2]];
                                fill_repeat(
                                    &mut row[x * channels..x * channels + n_out * channels],
                                    &pat,
                                );
                            } else {
                                let rgb = palette[palette_index as usize];
                                let start = x * channels;
                                // alpha（第 4 通道）恒 0：批量翻倍填充 [r,g,b,(0)]*，与逐像素写等价。
                                let pat = [rgb[0], rgb[1], rgb[2], 0u8];
                                fill_repeat(
                                    &mut row[start..start + n_out * channels],
                                    &pat[..channels],
                                );
                            }
                        }
                        x += n;
                    } else {
                        // 4bit 重复（与 C# Sdcb 一致）：第二字节高 nibble = 奇数位颜色、低 nibble =
                        // 偶数位颜色，游程内两色交替；缺省低 nibble=0 时即「高 nibble 重复」。
                        if x + n > w {
                            return Err(image_err("Corrupt RLE data"));
                        }
                        let hi = palette_index >> 4;
                        let lo = palette_index & 0x0f;
                        if indexed {
                            // 同 RLE8 run：直写 RGB（hi/lo 交替模式），不走索引缓冲。
                            let hicol = &palette[hi as usize];
                            let locol = &palette[lo as usize];
                            let pat = [
                                hicol[0], hicol[1], hicol[2], locol[0], locol[1], locol[2],
                            ];
                            fill_repeat(&mut row[x * channels..x * channels + n * channels], &pat);
                        } else {
                            let hicol = &palette[hi as usize];
                            let locol = &palette[lo as usize];
                            let start = x * channels;
                            if channels == 4 {
                                let pat = [
                                    hicol[0], hicol[1], hicol[2], 0, locol[0], locol[1], locol[2],
                                    0,
                                ];
                                fill_repeat(&mut row[start..start + n * channels], &pat);
                            } else {
                                let pat = [
                                    hicol[0], hicol[1], hicol[2], locol[0], locol[1], locol[2],
                                ];
                                fill_repeat(&mut row[start..start + n * channels], &pat);
                            }
                        }
                        x += n;
                    }
                }
            }
            // 行完成（EOL break）：展开本行已解码段。
            if indexed {
                render_rle_segments(
                    row,
                    &idx,
                    &segs,
                    is_8bit,
                    palette,
                    &lut8,
                    &lut4,
                    simd8,
                    simd4,
                );
            }
            // 取下一行；无则结束。
            row = match row_iter.next() {
                Some(r) => r,
                None => return Ok(()),
            };
        }
    }

    /// 供 ICO 读取 AND 掩码用（拆分后由 kimg_ico 跨 crate 调用，故为 `pub`）。
    pub fn reader(&mut self) -> &mut R {
        &mut self.reader
    }
}

fn read_u8_val(reader: &mut impl Read) -> ImageResult<u8> {
    let mut b = [0u8; 1];
    reader.read_exact(&mut b)?;
    Ok(b[0])
}

/// 记录一个已解码连续段（start, len）到段表；若与当前末段紧邻则合并（无洞 = 整段展开）。
#[inline]
fn push_seg(segs: &mut Vec<(usize, usize)>, x: usize, len: usize) {
    if len == 0 {
        return;
    }
    if let Some(last) = segs.last_mut()
        && last.0 + last.1 == x {
            last.1 += len;
            return;
        }
    segs.push((x, len));
}

/// 把本行全部已解码段的索引缓冲整段展开为 RGB（复用 R4 palette SIMD 行内核 + 标量尾，
/// 与 read_palette_buf 相同姿势，字节恒等）。
/// 「填零区」（EOL/EOF 行尾、delta 前导/平移洞）由调用点直接在行上 fill(0)，与展开段
/// 不相交，此处只展开 segs 覆盖的已解码像素。
#[inline]
fn render_rle_segments(
    row: &mut [u8],
    idx: &[u8],
    segs: &[(usize, usize)],
    is_8bit: bool,
    palette: &[[u8; 3]],
    lut8: &[u8; 1024],
    lut4: &[[u8; 16]; 3],
    simd8: bool,
    simd4: bool,
) {
    let channels = 3usize;
    for &(s, len) in segs {
        if len == 0 {
            continue;
        }
        let dst = &mut row[s * channels..(s + len) * channels];
        let src = &idx[s..s + len];
        if is_8bit {
            // RLE8：idx 字节即调色板索引。len<8 无 SIMD 批（kernel 空转），直接标量省调用。
            if simd8 && len >= 8 {
                let start = unsafe {
                    bgr_ops::expand_palette8_avx2(src, dst, len, channels, lut8.as_ptr())
                };
                for i in start..len {
                    let rgb = palette[src[i] as usize];
                    dst[i * channels..i * channels + 3].copy_from_slice(&rgb[..3]);
                }
            } else {
                for (i, &ix) in src.iter().enumerate() {
                    let rgb = palette[ix as usize];
                    dst[i * channels..i * channels + 3].copy_from_slice(&rgb[..3]);
                }
            }
        } else {
            // RLE4：len<16 无 SIMD 批 → 直接按字节索引标量展开；len>=16 用字节索引 SIMD 核
            // （免打包 pass）。标量尾逐字节索引直接展开。
            if simd4 && len >= 16 {
                let start = unsafe {
                    bgr_ops::expand_palette4_idx_ssse3(src, dst, len, channels, lut4)
                };
                for i in start..len {
                    let rgb = palette[(src[i] & 0x0f) as usize];
                    dst[i * channels..i * channels + 3].copy_from_slice(&rgb[..3]);
                }
            } else {
                for (i, &ix) in src.iter().enumerate() {
                    let rgb = palette[(ix & 0x0f) as usize];
                    dst[i * channels..i * channels + 3].copy_from_slice(&rgb[..3]);
                }
            }
        }
    }
}

/// 用 `pattern` 重复填充 `dst`：先写一次 pattern，随后按 2 的幂翻倍复制已填充前缀
/// （`copy_within` 为 memmove 语义，重叠安全）。与逐元素复制等价，仅批量优化调用点。
#[inline]
fn fill_repeat(dst: &mut [u8], pattern: &[u8]) {
    let n = dst.len();
    if n == 0 {
        return;
    }
    let plen = pattern.len();
    let init = plen.min(n);
    dst[..init].copy_from_slice(&pattern[..init]);
    let mut filled = init;
    while filled < n {
        let step = filled.min(n - filled);
        dst.copy_within(..step, filled);
        filled += step;
    }
}

/// 把 `Bitfield` 的抽位+展宽语义预计算为 256 项 LUT：`lut[(data>>shift) & (2^len-1)]`
/// == `bf.read(data)`，其中取指掩码内对齐到通道长度（与 `read` 内部一致）。len==0 通道
/// 不含在查表范围（调用处按旧语义写 0xFF），此处按 `read(0)` 兜底不 panic。
fn bitfield_lut(bf: &Bitfield) -> [u8; 256] {
    let mut lut = [0u8; 256];
    if bf.len == 0 {
        lut.fill(0xFF);
        return lut;
    }
    for (i, e) in lut.iter_mut().enumerate() {
        let idx = i & ((1 << bf.len) - 1);
        *e = bf.read((idx as u32) << bf.shift);
    }
    lut
}

/// SIMD 行内核（x86_64；非 x86_64 一律走标量回退）。每核返回「已处理的像素数」，
/// 调用方用标量尾（= 旧语义逐像素姿势）补齐向量主径未覆盖的像素，保证字节级一致。
#[cfg(target_arch = "x86_64")]
mod bgr_ops {
    use std::arch::x86_64::*;

    /// BGR→RGB（3 字节/像素，3→3）：源每像素 [B,G,R] → 输出 [R,G,B]。SSSE3 每批 4 像素
    /// （16 字节载/存；输出字节 12..15 为 spill，由后批/标量尾重写，源为独立 raw 缓冲无冲突）。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn bgr24_to_rgb(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(
                2, 1, 0, 5, 4, 3, 8, 7, 6, 11, 10, 9, -128, -128, -128, -128,
            );
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while 3 * x + 16 <= 3 * width {
                let v = _mm_loadu_si128(s.add(3 * x) as *const __m128i);
                _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm_shuffle_epi8(v, m));
                x += 4;
            }
            x
        }
    }

    /// BGR→RGBA（alpha=255）：SSSE3 每批 4 像素。掩码 {2,1,0,-128} ×4 + OR 0xFF000000。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn bgr24_to_rgba_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(
                2, 1, 0, -128, 5, 4, 3, -128, 8, 7, 6, -128, 11, 10, 9, -128,
            );
            let a = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while 3 * x + 16 <= 3 * width {
                let v = _mm_loadu_si128(s.add(3 * x) as *const __m128i);
                let out = _mm_or_si128(_mm_shuffle_epi8(v, m), a);
                _mm_storeu_si128(d.add(4 * x) as *mut __m128i, out);
                x += 4;
            }
            x
        }
    }

    /// BGR→RGBA（alpha=255）AVX2：每批 8 像素（两次 16 字节载入各出 4 RGBA，拼 256 位一次写）。
    /// 移位边条件与 pngsimd `rgb8_to_rgba_avx2` 同款：`3x+28 <= 3w` 保证两次载入整段在行内。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn bgr24_to_rgba_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(
                2, 1, 0, -128, 5, 4, 3, -128, 8, 7, 6, -128, 11, 10, 9, -128,
            );
            let a = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while 3 * x + 28 <= 3 * width {
                let v0 = _mm_loadu_si128(s.add(3 * x) as *const __m128i);
                let v1 = _mm_loadu_si128(s.add(3 * x + 12) as *const __m128i);
                let o0 = _mm_or_si128(_mm_shuffle_epi8(v0, m), a);
                let o1 = _mm_or_si128(_mm_shuffle_epi8(v1, m), a);
                _mm256_storeu_si256(d.add(4 * x) as *mut __m256i, _mm256_set_m128i(o1, o0));
                x += 8;
            }
            x
        }
    }

    /// RGB32（BI_RGB 32bit）4→3 打包：源每像素 [B,G,R,X] → 输出 [R,G,B]（丢弃第 4 字节 X），
    /// 与 C# Sdcb 对 BI_RGB 32bit（缺省掩码 R=byte2/G=byte1/B=byte0）的抽取一致。
    /// SSSE3 每批 4 像素：16 字节载入 → 低 12 字节 {2,1,0}/{6,5,4}/{10,9,8}/{14,13,12}，
    /// 字节 12..15 清 0（spill，由后批/标量尾重写）。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn bgra_to_rgb(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(
                2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -128, -128, -128, -128,
            );
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width && 3 * x + 16 <= 3 * width {
                let v = _mm_loadu_si128(s.add(4 * x) as *const __m128i);
                _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm_shuffle_epi8(v, m));
                x += 4;
            }
            x
        }
    }

    /// BITFIELDS 888 无 alpha 快路径（r/g/b 掩码落在 byte3/2/1）4→3 打包：源每像素 [X,B,G,R] →
    /// 输出 [R,G,B]（丢弃 byte0 X）。与 BI_RGB 32bit 的 [B,G,R,X] 布局不同，掩码位序决定选位。
    /// SSSE3 每批 4 像素：16 字节载入 → 低 12 字节 {3,2,1}/{7,6,5}/{11,10,9}/{15,14,13}。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn xbgr_to_rgb(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(
                3, 2, 1, 7, 6, 5, 11, 10, 9, 15, 14, 13, -128, -128, -128, -128,
            );
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width && 3 * x + 16 <= 3 * width {
                let v = _mm_loadu_si128(s.add(4 * x) as *const __m128i);
                _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm_shuffle_epi8(v, m));
                x += 4;
            }
            x
        }
    }

    /// RGBA32 原位 4→4 重排 [B,G,R,A]→[R,G,B,A]：SSSE3 每批 4 像素（载/写同一 16 字节组，
    /// 组间无重叠，原位安全）。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn bgra_to_rgba_ssse3(buf: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(
                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
            );
            let d = buf.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width {
                let v = _mm_loadu_si128(d.add(4 * x) as *const __m128i);
                _mm_storeu_si128(d.add(4 * x) as *mut __m128i, _mm_shuffle_epi8(v, m));
                x += 4;
            }
            x
        }
    }

    /// RGBA32 原位 4→4 重排 AVX2：每批 8 像素。注意 `_mm256_shuffle_epi8` 按 128 位 lane
    /// 独立取指，故 256 掩码须为同一 16 字节模式复制两份（lane 内相对下标）。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn bgra_to_rgba_avx2(buf: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm256_setr_epi8(
                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, //
                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
            );
            let d = buf.as_mut_ptr();
            let mut x = 0usize;
            while x + 8 <= width {
                let v = _mm256_loadu_si256(d.add(4 * x) as *const __m256i);
                _mm256_storeu_si256(
                    d.add(4 * x) as *mut __m256i,
                    _mm256_shuffle_epi8(v, m),
                );
                x += 8;
            }
            x
        }
    }

    // ---------------------------------------------------------------------------
    // 16bpp（RGB555/RGB565）→ RGB(A)8 行内核
    // ---------------------------------------------------------------------------
    // 位布局：16bit 小端 u16 样本，u16 lane 字节序天然即像素（低字节在前）。展宽严格按
    // `Bitfield::read` 的位重复：5bit `(v<<3)|(v>>2)`、6bit `(v<<2)|(v>>4)`，与 C# Widen 一致。
    // 每内核只处理「整组」像素并返回已处理数；调用方用标量尾补齐余量，保证字节级一致。
    // 公共：src/dst 为整行切片，内核内部按比例推进指针。

    /// 16bit→RGB8 主分发：先 AVX2（16px/批），再 SSSE3（4px/批），余量交标量尾。
    pub(crate) unsafe fn expand_16bit_simd(
        src: &[u8],
        dst: &mut [u8],
        width: usize,
        channels: usize,
        fmt: super::Bmp16Fmt,
    ) -> usize {
        unsafe {
            let avx2 = kimg_core::simd::detect("avx2");
            let mut x = 0usize;
            match (fmt, channels) {
                (super::Bmp16Fmt::Rgb555, 3) => {
                    if avx2 {
                        x += rgb555_to_rgb_avx2(&src[2 * x..], &mut dst[3 * x..], width - x);
                    }
                    x += rgb555_to_rgb_ssse3(&src[2 * x..], &mut dst[3 * x..], width - x);
                }
                (super::Bmp16Fmt::Rgb565, 3) => {
                    if avx2 {
                        x += rgb565_to_rgb_avx2(&src[2 * x..], &mut dst[3 * x..], width - x);
                    }
                    x += rgb565_to_rgb_ssse3(&src[2 * x..], &mut dst[3 * x..], width - x);
                }
                (super::Bmp16Fmt::Rgb555, 4) => {
                    if avx2 {
                        x += rgb555_to_rgba_avx2(&src[2 * x..], &mut dst[4 * x..], width - x);
                    }
                    x += rgb555_to_rgba_ssse3(&src[2 * x..], &mut dst[4 * x..], width - x);
                }
                (super::Bmp16Fmt::Rgb565, 4) => {
                    if avx2 {
                        x += rgb565_to_rgba_avx2(&src[2 * x..], &mut dst[4 * x..], width - x);
                    }
                    x += rgb565_to_rgba_ssse3(&src[2 * x..], &mut dst[4 * x..], width - x);
                }
                _ => unreachable!("expand_16bit_simd 仅接收 3/4 通道的标准 555/565"),
            }
            x
        }
    }

    /// RGB555→RGB（3B/px）：SSSE3 每批 4 像素。8 字节载入 → u16 lane=像素 → 位重复展宽 →
    /// [R,B,G,0]×4 → 压平为 12 字节连续 [R,G,B]×4 + 4 字节零 spill（由后批/标量尾重写；
    /// 守 `3x+16<=3w` 防越界）。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn rgb555_to_rgb_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let pix = _mm_setr_epi8(
                0, 1, 2, 3, 4, 5, 6, 7, -128, -128, -128, -128, -128, -128, -128, -128,
            );
            let pack = _mm_setr_epi8(
                0, 2, 1, 4, 6, 5, 8, 10, 9, 12, 14, 13, -128, -128, -128, -128,
            );
            let m5 = _mm_set1_epi16(31);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width && 3 * x + 16 <= 3 * width {
                let v = _mm_loadl_epi64(s.add(2 * x) as *const __m128i);
                let p = _mm_shuffle_epi8(v, pix);
                let r = _mm_and_si128(_mm_srli_epi16(p, 10), m5);
                let g = _mm_and_si128(_mm_srli_epi16(p, 5), m5);
                let b = _mm_and_si128(p, m5);
                let r = _mm_or_si128(_mm_slli_epi16(r, 3), _mm_srli_epi16(r, 2));
                let g = _mm_or_si128(_mm_slli_epi16(g, 3), _mm_srli_epi16(g, 2));
                let b = _mm_or_si128(_mm_slli_epi16(b, 3), _mm_srli_epi16(b, 2));
                let lo = _mm_or_si128(r, _mm_slli_epi16(g, 8));
                let hi = b;
                let t = _mm_unpacklo_epi8(lo, hi);
                _mm_storeu_si128(
                    d.add(3 * x) as *mut __m128i,
                    _mm_shuffle_epi8(t, pack),
                );
                x += 4;
            }
            x
        }
    }

    /// RGB565→RGB（3B/px）：同 `rgb555_to_rgb_ssse3`，仅红移 11、绿 6bit 展宽不同。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn rgb565_to_rgb_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let pix = _mm_setr_epi8(
                0, 1, 2, 3, 4, 5, 6, 7, -128, -128, -128, -128, -128, -128, -128, -128,
            );
            let pack = _mm_setr_epi8(
                0, 2, 1, 4, 6, 5, 8, 10, 9, 12, 14, 13, -128, -128, -128, -128,
            );
            let m5 = _mm_set1_epi16(31);
            let m6 = _mm_set1_epi16(63);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width && 3 * x + 16 <= 3 * width {
                let v = _mm_loadl_epi64(s.add(2 * x) as *const __m128i);
                let p = _mm_shuffle_epi8(v, pix);
                let r = _mm_and_si128(_mm_srli_epi16(p, 11), m5);
                let g = _mm_and_si128(_mm_srli_epi16(p, 5), m6);
                let b = _mm_and_si128(p, m5);
                let r = _mm_or_si128(_mm_slli_epi16(r, 3), _mm_srli_epi16(r, 2));
                let g = _mm_or_si128(_mm_slli_epi16(g, 2), _mm_srli_epi16(g, 4));
                let b = _mm_or_si128(_mm_slli_epi16(b, 3), _mm_srli_epi16(b, 2));
                let lo = _mm_or_si128(r, _mm_slli_epi16(g, 8));
                let hi = b;
                let t = _mm_unpacklo_epi8(lo, hi);
                _mm_storeu_si128(
                    d.add(3 * x) as *mut __m128i,
                    _mm_shuffle_epi8(t, pack),
                );
                x += 4;
            }
            x
        }
    }

    /// RGB555→RGB AVX2：每批 16 像素（32 字节载入 → [R,B,G,0]×16 → 压平为 4×16B 出，
    /// 相邻存储 12 字节步进重叠 4 字节级联；守 `3x+52<=3w` 让末次 spill 不越界）。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn rgb555_to_rgb_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let pack = _mm256_setr_epi8(
                0, 2, 1, 4, 6, 5, 8, 10, 9, 12, 14, 13, -128, -128, -128, -128, //
                0, 2, 1, 4, 6, 5, 8, 10, 9, 12, 14, 13, -128, -128, -128, -128,
            );
            let m5 = _mm256_set1_epi16(31);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 16 <= width && 3 * x + 52 <= 3 * width {
                let v = _mm256_loadu_si256(s.add(2 * x) as *const __m256i);
                let r = _mm256_and_si256(_mm256_srli_epi16(v, 10), m5);
                let g = _mm256_and_si256(_mm256_srli_epi16(v, 5), m5);
                let b = _mm256_and_si256(v, m5);
                let r = _mm256_or_si256(_mm256_slli_epi16(r, 3), _mm256_srli_epi16(r, 2));
                let g = _mm256_or_si256(_mm256_slli_epi16(g, 3), _mm256_srli_epi16(g, 2));
                let b = _mm256_or_si256(_mm256_slli_epi16(b, 3), _mm256_srli_epi16(b, 2));
                let lo = _mm256_or_si256(r, _mm256_slli_epi16(g, 8));
                let t0 = _mm256_unpacklo_epi8(lo, b);
                let t1 = _mm256_unpackhi_epi8(lo, b);
                let p0 = _mm256_shuffle_epi8(t0, pack);
                let p1 = _mm256_shuffle_epi8(t1, pack);
                _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm256_castsi256_si128(p0));
                _mm_storeu_si128(d.add(3 * x + 12) as *mut __m128i, _mm256_castsi256_si128(p1));
                _mm_storeu_si128(
                    d.add(3 * x + 24) as *mut __m128i,
                    _mm256_extracti128_si256(p0, 1),
                );
                _mm_storeu_si128(
                    d.add(3 * x + 36) as *mut __m128i,
                    _mm256_extracti128_si256(p1, 1),
                );
                x += 16;
            }
            x
        }
    }

    /// RGB565→RGB AVX2：同 `rgb555_to_rgb_avx2`，红移 11、绿 6bit 展宽。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn rgb565_to_rgb_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let pack = _mm256_setr_epi8(
                0, 2, 1, 4, 6, 5, 8, 10, 9, 12, 14, 13, -128, -128, -128, -128, //
                0, 2, 1, 4, 6, 5, 8, 10, 9, 12, 14, 13, -128, -128, -128, -128,
            );
            let m5 = _mm256_set1_epi16(31);
            let m6 = _mm256_set1_epi16(63);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 16 <= width && 3 * x + 52 <= 3 * width {
                let v = _mm256_loadu_si256(s.add(2 * x) as *const __m256i);
                let r = _mm256_and_si256(_mm256_srli_epi16(v, 11), m5);
                let g = _mm256_and_si256(_mm256_srli_epi16(v, 5), m6);
                let b = _mm256_and_si256(v, m5);
                let r = _mm256_or_si256(_mm256_slli_epi16(r, 3), _mm256_srli_epi16(r, 2));
                let g = _mm256_or_si256(_mm256_slli_epi16(g, 2), _mm256_srli_epi16(g, 4));
                let b = _mm256_or_si256(_mm256_slli_epi16(b, 3), _mm256_srli_epi16(b, 2));
                let lo = _mm256_or_si256(r, _mm256_slli_epi16(g, 8));
                let t0 = _mm256_unpacklo_epi8(lo, b);
                let t1 = _mm256_unpackhi_epi8(lo, b);
                let p0 = _mm256_shuffle_epi8(t0, pack);
                let p1 = _mm256_shuffle_epi8(t1, pack);
                _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm256_castsi256_si128(p0));
                _mm_storeu_si128(d.add(3 * x + 12) as *mut __m128i, _mm256_castsi256_si128(p1));
                _mm_storeu_si128(
                    d.add(3 * x + 24) as *mut __m128i,
                    _mm256_extracti128_si256(p0, 1),
                );
                _mm_storeu_si128(
                    d.add(3 * x + 36) as *mut __m128i,
                    _mm256_extracti128_si256(p1, 1),
                );
                x += 16;
            }
            x
        }
    }

    /// RGB555→RGBA（4B/px, alpha=FF）：SSSE3 每批 4 像素，输出 [R,G,B,FF]。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn rgb555_to_rgba_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let pix = _mm_setr_epi8(
                0, 1, 2, 3, 4, 5, 6, 7, -128, -128, -128, -128, -128, -128, -128, -128,
            );
            let swz = _mm_setr_epi8(
                0, 2, 1, 3, 4, 6, 5, 7, 8, 10, 9, 11, 12, 14, 13, 15,
            );
            let m5 = _mm_set1_epi16(31);
            let aff = _mm_set1_epi16(-256);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width {
                let v = _mm_loadl_epi64(s.add(2 * x) as *const __m128i);
                let p = _mm_shuffle_epi8(v, pix);
                let r = _mm_and_si128(_mm_srli_epi16(p, 10), m5);
                let g = _mm_and_si128(_mm_srli_epi16(p, 5), m5);
                let b = _mm_and_si128(p, m5);
                let r = _mm_or_si128(_mm_slli_epi16(r, 3), _mm_srli_epi16(r, 2));
                let g = _mm_or_si128(_mm_slli_epi16(g, 3), _mm_srli_epi16(g, 2));
                let b = _mm_or_si128(_mm_slli_epi16(b, 3), _mm_srli_epi16(b, 2));
                let lo = _mm_or_si128(r, _mm_slli_epi16(g, 8));
                let hi = _mm_or_si128(b, aff);
                let t = _mm_unpacklo_epi8(lo, hi);
                _mm_storeu_si128(
                    d.add(4 * x) as *mut __m128i,
                    _mm_shuffle_epi8(t, swz),
                );
                x += 4;
            }
            x
        }
    }

    /// RGB565→RGBA（4B/px, alpha=FF）：同 `rgb555_to_rgba_ssse3`，红移 11、绿 6bit 展宽。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn rgb565_to_rgba_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let pix = _mm_setr_epi8(
                0, 1, 2, 3, 4, 5, 6, 7, -128, -128, -128, -128, -128, -128, -128, -128,
            );
            let swz = _mm_setr_epi8(
                0, 2, 1, 3, 4, 6, 5, 7, 8, 10, 9, 11, 12, 14, 13, 15,
            );
            let m5 = _mm_set1_epi16(31);
            let m6 = _mm_set1_epi16(63);
            let aff = _mm_set1_epi16(-256);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width {
                let v = _mm_loadl_epi64(s.add(2 * x) as *const __m128i);
                let p = _mm_shuffle_epi8(v, pix);
                let r = _mm_and_si128(_mm_srli_epi16(p, 11), m5);
                let g = _mm_and_si128(_mm_srli_epi16(p, 5), m6);
                let b = _mm_and_si128(p, m5);
                let r = _mm_or_si128(_mm_slli_epi16(r, 3), _mm_srli_epi16(r, 2));
                let g = _mm_or_si128(_mm_slli_epi16(g, 2), _mm_srli_epi16(g, 4));
                let b = _mm_or_si128(_mm_slli_epi16(b, 3), _mm_srli_epi16(b, 2));
                let lo = _mm_or_si128(r, _mm_slli_epi16(g, 8));
                let hi = _mm_or_si128(b, aff);
                let t = _mm_unpacklo_epi8(lo, hi);
                _mm_storeu_si128(
                    d.add(4 * x) as *mut __m128i,
                    _mm_shuffle_epi8(t, swz),
                );
                x += 4;
            }
            x
        }
    }

    /// RGB555→RGBA AVX2：每批 8 像素（16 字节载入 → 2×16 字节出）。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn rgb555_to_rgba_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let swz = _mm_setr_epi8(
                0, 2, 1, 3, 4, 6, 5, 7, 8, 10, 9, 11, 12, 14, 13, 15,
            );
            let m5 = _mm_set1_epi16(31);
            let aff = _mm_set1_epi16(-256);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 8 <= width {
                let v = _mm_loadu_si128(s.add(2 * x) as *const __m128i);
                let r = _mm_and_si128(_mm_srli_epi16(v, 10), m5);
                let g = _mm_and_si128(_mm_srli_epi16(v, 5), m5);
                let b = _mm_and_si128(v, m5);
                let r = _mm_or_si128(_mm_slli_epi16(r, 3), _mm_srli_epi16(r, 2));
                let g = _mm_or_si128(_mm_slli_epi16(g, 3), _mm_srli_epi16(g, 2));
                let b = _mm_or_si128(_mm_slli_epi16(b, 3), _mm_srli_epi16(b, 2));
                let lo = _mm_or_si128(r, _mm_slli_epi16(g, 8));
                let hi = _mm_or_si128(b, aff);
                let t0 = _mm_unpacklo_epi8(lo, hi);
                let t1 = _mm_unpackhi_epi8(lo, hi);
                let o0 = _mm_shuffle_epi8(t0, swz);
                let o1 = _mm_shuffle_epi8(t1, swz);
                _mm_storeu_si128(d.add(4 * x) as *mut __m128i, o0);
                _mm_storeu_si128(d.add(4 * x + 16) as *mut __m128i, o1);
                x += 8;
            }
            x
        }
    }

    /// RGB565→RGBA AVX2：同 `rgb555_to_rgba_avx2`，红移 11、绿 6bit 展宽。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn rgb565_to_rgba_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let swz = _mm_setr_epi8(
                0, 2, 1, 3, 4, 6, 5, 7, 8, 10, 9, 11, 12, 14, 13, 15,
            );
            let m5 = _mm_set1_epi16(31);
            let m6 = _mm_set1_epi16(63);
            let aff = _mm_set1_epi16(-256);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while x + 8 <= width {
                let v = _mm_loadu_si128(s.add(2 * x) as *const __m128i);
                let r = _mm_and_si128(_mm_srli_epi16(v, 11), m5);
                let g = _mm_and_si128(_mm_srli_epi16(v, 5), m6);
                let b = _mm_and_si128(v, m5);
                let r = _mm_or_si128(_mm_slli_epi16(r, 3), _mm_srli_epi16(r, 2));
                let g = _mm_or_si128(_mm_slli_epi16(g, 2), _mm_srli_epi16(g, 4));
                let b = _mm_or_si128(_mm_slli_epi16(b, 3), _mm_srli_epi16(b, 2));
                let lo = _mm_or_si128(r, _mm_slli_epi16(g, 8));
                let hi = _mm_or_si128(b, aff);
                let t0 = _mm_unpacklo_epi8(lo, hi);
                let t1 = _mm_unpackhi_epi8(lo, hi);
                let o0 = _mm_shuffle_epi8(t0, swz);
                let o1 = _mm_shuffle_epi8(t1, swz);
                _mm_storeu_si128(d.add(4 * x) as *mut __m128i, o0);
                _mm_storeu_si128(d.add(4 * x + 16) as *mut __m128i, o1);
                x += 8;
            }
            x
        }
    }

    // ---------------------------------------------------------------------------
    // 8bit 调色板 → RGB(A)8（AVX2 gather）
    // ---------------------------------------------------------------------------
    // `pshufb` 取指下标只有低 4 位（16 项），256 色任意调色板无法靠单张 shufb 查表；
    // 改用 `vpgatherdd` 按调色板项直取扁平 RGBA dword（小端 [R,G,B,A]，表 1KB 常驻 L1）。
    // 返回已处理像素数，余量交调用方标量尾（字节恒等）。

    /// 8bit 调色板 → RGB(A)8。每批 8 像素：
    /// - channels==3：gather 出 8 个 dword → 128 位半裁各压「12B 有效 + 4B spill」，
    ///   两次 16B 步进 12 交叠存储（spill 由下一批/标量尾重写）；守 `3x+28<=3w` 令末批
    ///   spill 不出行。
    /// - channels==4：dword 即 [R,G,B,A]（表项 alpha=FF），256 位整存 32B，无 spill。
    #[target_feature(enable = "avx2", enable = "sse4.1", enable = "ssse3")]
    pub(crate) unsafe fn expand_palette8_avx2(
        src: &[u8],
        dst: &mut [u8],
        width: usize,
        channels: usize,
        lut: *const u8,
    ) -> usize {
        unsafe {
            let pack = _mm_setr_epi8(
                0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -128, -128, -128, -128,
            );
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let base = lut as *const i32;
            let mut x = 0usize;
            if channels == 4 {
                while x + 8 <= width {
                    let lo = _mm_cvtepu8_epi32(_mm_loadl_epi64(s.add(x) as *const __m128i));
                    let hi = _mm_cvtepu8_epi32(_mm_loadl_epi64(s.add(x + 4) as *const __m128i));
                    let idx = _mm256_set_m128i(hi, lo);
                    let g = _mm256_i32gather_epi32(base, idx, 4);
                    _mm256_storeu_si256(d.add(4 * x) as *mut __m256i, g);
                    x += 8;
                }
            } else {
                while x + 8 <= width && 3 * x + 28 <= 3 * width {
                    let lo = _mm_cvtepu8_epi32(_mm_loadl_epi64(s.add(x) as *const __m128i));
                    let hi = _mm_cvtepu8_epi32(_mm_loadl_epi64(s.add(x + 4) as *const __m128i));
                    let idx = _mm256_set_m128i(hi, lo);
                    let g = _mm256_i32gather_epi32(base, idx, 4);
                    let g0 = _mm256_castsi256_si128(g);
                    let g1 = _mm256_extracti128_si256(g, 1);
                    _mm_storeu_si128(
                        d.add(3 * x) as *mut __m128i,
                        _mm_shuffle_epi8(g0, pack),
                    );
                    _mm_storeu_si128(
                        d.add(3 * x + 12) as *mut __m128i,
                        _mm_shuffle_epi8(g1, pack),
                    );
                    x += 8;
                }
            }
            x
        }
    }

    // ---------------------------------------------------------------------------
    // 4bit 调色板 → RGB(A)8（SSSE3，16 项任意调色板恰可单张 shufb 查表）
    // ---------------------------------------------------------------------------
    // 每字节 = 2 像素（高 nibble 先行）。高/低 nibble 各自成流（`srli_epi16(and(0xF0),4)`
    // 恰得 [hi_0..hi_{2k+1}...]，`and(0x0F)` 得 [lo_0..]）→ unpacklo 交回像素序 →
    // 16 项 R/G/B LUT 各 shufb（shufb 取指 = 低 4 位，正是 4bit 值）→ unpacklo/hi 交织 +
    // pack 压「12B 有效 + 4B spill」四次步进 12 交叠存储（同 8bit 交叠链，spill 由下一批/
    // 标量尾重写）。返回已处理像素数（16 的倍数）。

    /// 4bit 调色板 → RGB(A)8。每批 16 像素 = 8 索引字节。
    /// - channels==3：4 次 16B 步进 12 交叠存储；守 `3x+52<=3w` 令末批 spill 不出行。
    /// - channels==4：[R,G,B,A] 直出，4 次整 16B 存储，无 spill。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn expand_palette4_ssse3(
        src: &[u8],
        dst: &mut [u8],
        width: usize,
        channels: usize,
        lut: &[[u8; 16]; 3],
    ) -> usize {
        unsafe {
            let pack = _mm_setr_epi8(
                0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -128, -128, -128, -128,
            );
            let rl = _mm_loadu_si128(lut[0].as_ptr() as *const __m128i);
            let gl = _mm_loadu_si128(lut[1].as_ptr() as *const __m128i);
            let bl = _mm_loadu_si128(lut[2].as_ptr() as *const __m128i);
            let a = _mm_set1_epi8(-1); // 0xFF
            let m_f0 = _mm_set1_epi8(-16); // 0xF0
            let m_0f = _mm_set1_epi8(0x0F);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            if channels == 3 {
                while x + 16 <= width && 3 * x + 52 <= 3 * width {
                    let v = _mm_loadl_epi64(s.add(x / 2) as *const __m128i);
                    let hi = _mm_srli_epi16(_mm_and_si128(v, m_f0), 4);
                    let lo = _mm_and_si128(v, m_0f);
                    let p = _mm_unpacklo_epi8(hi, lo);
                    let r = _mm_shuffle_epi8(rl, p);
                    let g = _mm_shuffle_epi8(gl, p);
                    let b = _mm_shuffle_epi8(bl, p);
                    // 交织成 RGBA：须 (r,b) 与 (g,a) 成对内插再外插——(r,g)+(b,a) 会得
                    // R,B,G,A（G/B 换位）。
                    let t0 = _mm_unpacklo_epi8(r, b);
                    let t1 = _mm_unpacklo_epi8(g, a);
                    let t2 = _mm_unpackhi_epi8(r, b);
                    let t3 = _mm_unpackhi_epi8(g, a);
                    let o0 = _mm_unpacklo_epi8(t0, t1);
                    let o1 = _mm_unpackhi_epi8(t0, t1);
                    let o2 = _mm_unpacklo_epi8(t2, t3);
                    let o3 = _mm_unpackhi_epi8(t2, t3);
                    _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm_shuffle_epi8(o0, pack));
                    _mm_storeu_si128(
                        d.add(3 * x + 12) as *mut __m128i,
                        _mm_shuffle_epi8(o1, pack),
                    );
                    _mm_storeu_si128(
                        d.add(3 * x + 24) as *mut __m128i,
                        _mm_shuffle_epi8(o2, pack),
                    );
                    _mm_storeu_si128(
                        d.add(3 * x + 36) as *mut __m128i,
                        _mm_shuffle_epi8(o3, pack),
                    );
                    x += 16;
                }
            } else {
                while x + 16 <= width {
                    let v = _mm_loadl_epi64(s.add(x / 2) as *const __m128i);
                    let hi = _mm_srli_epi16(_mm_and_si128(v, m_f0), 4);
                    let lo = _mm_and_si128(v, m_0f);
                    let p = _mm_unpacklo_epi8(hi, lo);
                    let r = _mm_shuffle_epi8(rl, p);
                    let g = _mm_shuffle_epi8(gl, p);
                    let b = _mm_shuffle_epi8(bl, p);
                    let t0 = _mm_unpacklo_epi8(r, b);
                    let t1 = _mm_unpacklo_epi8(g, a);
                    let t2 = _mm_unpackhi_epi8(r, b);
                    let t3 = _mm_unpackhi_epi8(g, a);
                    let o0 = _mm_unpacklo_epi8(t0, t1);
                    let o1 = _mm_unpackhi_epi8(t0, t1);
                    let o2 = _mm_unpacklo_epi8(t2, t3);
                    let o3 = _mm_unpackhi_epi8(t2, t3);
                    _mm_storeu_si128(d.add(4 * x) as *mut __m128i, o0);
                    _mm_storeu_si128(d.add(4 * x + 16) as *mut __m128i, o1);
                    _mm_storeu_si128(d.add(4 * x + 32) as *mut __m128i, o2);
                    _mm_storeu_si128(d.add(4 * x + 48) as *mut __m128i, o3);
                    x += 16;
                }
            }
            x
        }
    }

    /// 4bit 调色板 → RGB(A)8（**字节索引源**：每像素 1 字节、低 4 位即索引）。
    /// 独立于 `expand_palette4_ssse3`（nibble 打包源）存在的原因：read_rle 的索引缓冲是
    /// 逐像素字节，允许「任意奇像素偏移的段」直接按 `&idx[s..s+len]` 切片——而 nibble
    /// 打包核要求源按每 2 像素偶对齐、奇数偏移的段要先做 nibble 错位搬运，错位即错像素。
    /// 逐像素字节直切免打包 pass。`pshufb` 取指 = 字节低 4 位，恰为 4bit 索引（无需掩码）。
    /// 每批 16 像素；3ch 按 `3x+52<=3w` 步进 12 交叠存储，4ch 整 16B 存储。
    /// 返回已处理像素数（16 的倍数），余量交调用方标量尾（字节恒等）。
    ///
    /// ⚠️ channels==4 分支在本 crate **不可达**：本核唯一调用方 render_rle_segments 恒
    /// channels==3；RLE 的 4ch（仅 ICO `new_with_ico_format` 可达）走旧逐像素直写路径
    /// （`read_rle` 里 `indexed` 恒为 false），不调用本核。该 4ch 分支仅与兄弟核保持
    /// 对称保留，勿据它推理 4ch 语义，也勿在 render 侧放开 channels 参数。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn expand_palette4_idx_ssse3(
        src: &[u8],
        dst: &mut [u8],
        width: usize,
        channels: usize,
        lut: &[[u8; 16]; 3],
    ) -> usize {
        unsafe {
            let pack = _mm_setr_epi8(
                0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -128, -128, -128, -128,
            );
            let rl = _mm_loadu_si128(lut[0].as_ptr() as *const __m128i);
            let gl = _mm_loadu_si128(lut[1].as_ptr() as *const __m128i);
            let bl = _mm_loadu_si128(lut[2].as_ptr() as *const __m128i);
            let a = _mm_set1_epi8(-1); // 0xFF
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            if channels == 3 {
                while x + 16 <= width && 3 * x + 52 <= 3 * width {
                    let p = _mm_loadu_si128(s.add(x) as *const __m128i);
                    let r = _mm_shuffle_epi8(rl, p);
                    let g = _mm_shuffle_epi8(gl, p);
                    let b = _mm_shuffle_epi8(bl, p);
                    // 与 expand_palette4_ssse3 同款交织+打包存储（(r,b)+(g,a) 成对内插外插）。
                    let t0 = _mm_unpacklo_epi8(r, b);
                    let t1 = _mm_unpacklo_epi8(g, a);
                    let t2 = _mm_unpackhi_epi8(r, b);
                    let t3 = _mm_unpackhi_epi8(g, a);
                    let o0 = _mm_unpacklo_epi8(t0, t1);
                    let o1 = _mm_unpackhi_epi8(t0, t1);
                    let o2 = _mm_unpacklo_epi8(t2, t3);
                    let o3 = _mm_unpackhi_epi8(t2, t3);
                    _mm_storeu_si128(d.add(3 * x) as *mut __m128i, _mm_shuffle_epi8(o0, pack));
                    _mm_storeu_si128(d.add(3 * x + 12) as *mut __m128i, _mm_shuffle_epi8(o1, pack));
                    _mm_storeu_si128(d.add(3 * x + 24) as *mut __m128i, _mm_shuffle_epi8(o2, pack));
                    _mm_storeu_si128(d.add(3 * x + 36) as *mut __m128i, _mm_shuffle_epi8(o3, pack));
                    x += 16;
                }
            } else {
                while x + 16 <= width {
                    let p = _mm_loadu_si128(s.add(x) as *const __m128i);
                    let r = _mm_shuffle_epi8(rl, p);
                    let g = _mm_shuffle_epi8(gl, p);
                    let b = _mm_shuffle_epi8(bl, p);
                    let t0 = _mm_unpacklo_epi8(r, b);
                    let t1 = _mm_unpacklo_epi8(g, a);
                    let t2 = _mm_unpackhi_epi8(r, b);
                    let t3 = _mm_unpackhi_epi8(g, a);
                    let o0 = _mm_unpacklo_epi8(t0, t1);
                    let o1 = _mm_unpackhi_epi8(t0, t1);
                    let o2 = _mm_unpacklo_epi8(t2, t3);
                    let o3 = _mm_unpackhi_epi8(t2, t3);
                    _mm_storeu_si128(d.add(4 * x) as *mut __m128i, o0);
                    _mm_storeu_si128(d.add(4 * x + 16) as *mut __m128i, o1);
                    _mm_storeu_si128(d.add(4 * x + 32) as *mut __m128i, o2);
                    _mm_storeu_si128(d.add(4 * x + 48) as *mut __m128i, o3);
                    x += 16;
                }
            }
            x
        }
    }
}

impl<R: BufRead + Seek> ImageDecoder for BmpDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    fn color_type(&self) -> ColorType {
        if self.add_alpha_channel {
            ColorType::Rgba8
        } else {
            ColorType::Rgb8
        }
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        self.read_image_data(buf)
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
