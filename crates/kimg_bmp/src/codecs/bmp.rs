//! BMP 解码器（手写实现，行为与 image 0.25 对齐：1/2/4/8bit 调色板、16/24/32bit、RLE、bitfields）。

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

/// 3/4/5/6 bit → 8 bit 展宽表。
static LOOKUP_3BIT: [u8; 8] = [0, 36, 73, 109, 146, 182, 219, 255];
static LOOKUP_4BIT: [u8; 16] = [
    0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255,
];
static LOOKUP_5BIT: [u8; 32] = [
    0, 8, 16, 25, 33, 41, 49, 58, 66, 74, 82, 90, 99, 107, 115, 123, 132, 140, 148, 156, 165, 173,
    181, 189, 197, 206, 214, 222, 230, 239, 247, 255,
];
static LOOKUP_6BIT: [u8; 64] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 45, 49, 53, 57, 61, 65, 69, 73, 77, 81, 85, 89, 93,
    97, 101, 105, 109, 113, 117, 121, 125, 130, 134, 138, 142, 146, 150, 154, 158, 162, 166, 170,
    174, 178, 182, 186, 190, 194, 198, 202, 206, 210, 215, 219, 223, 227, 231, 235, 239, 243, 247,
    251, 255,
];

/// 最大宽高（软上限）。
const MAX_WIDTH_HEIGHT: i32 = 0xFFFF;

const RLE_ESCAPE: u8 = 0;

/// `KIMG_BMP_NO_SIMD=1` 强制 BMP 行内核走标量（量化 SIMD 收益；与 pngsimd/webp 同款姿势）。
/// 首次访问缓存环境变量，之后仅一次 Relaxed 原子读——正常热循环开销可忽略。
fn bmp_no_simd() -> bool {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static OFF: AtomicBool = AtomicBool::new(false);
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let off = std::env::var("KIMG_BMP_NO_SIMD")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        OFF.store(off, Relaxed);
    });
    OFF.load(Relaxed)
}

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

    /// 抽出通道并展宽到 8bit（与 image 0.25 的查找表缩放一致）。
    fn read(&self, data: u32) -> u8 {
        let data = data >> self.shift;
        match self.len {
            1 => ((data & 0b1) * 0xff) as u8,
            2 => ((data & 0b11) * 0x55) as u8,
            3 => LOOKUP_3BIT[(data & 0b00_0111) as usize],
            4 => LOOKUP_4BIT[(data & 0b00_1111) as usize],
            5 => LOOKUP_5BIT[(data & 0b01_1111) as usize],
            6 => LOOKUP_6BIT[(data & 0b11_1111) as usize],
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
        let palette_size = match self.bit_count {
            b => (1usize << b).min(MAX_PALETTE_SIZE),
        };
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
            ImageType::RGB24 => self.read_full_bytes(buf, 3, false),
            ImageType::RGB32 => self.read_full_bytes(buf, 4, true),
            ImageType::RGBA32 => self.read_full_bytes(buf, 4, false),
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
                let (r8, g8, b8, a8) = (
                    Bitfield { shift: 24, len: 8 },
                    Bitfield { shift: 16, len: 8 },
                    Bitfield { shift: 8, len: 8 },
                    Bitfield { shift: 0, len: 8 },
                );
                if bf.r == r8 && bf.g == g8 && bf.b == b8 && bf.a.len == 0 {
                    self.read_full_bytes(buf, 4, true)
                } else if bf.r == (Bitfield { shift: 16, len: 8 })
                    && bf.g == (Bitfield { shift: 8, len: 8 })
                    && bf.b == (Bitfield { shift: 0, len: 8 })
                    && bf.a == a8
                {
                    self.read_full_bytes(buf, 4, false)
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

        for row in self.rows_mut(buf) {
            self.reader.read_exact(&mut indices)?;
            let mut pixel_iter = row.chunks_exact_mut(channels);
            match bit_count {
                1 => {
                    for &idx in &indices {
                        let mut bit = 0x80;
                        loop {
                            if let Some(pixel) = pixel_iter.next() {
                                let rgb = palette[usize::from((idx & bit) != 0)];
                                pixel[..3].copy_from_slice(&rgb);
                            } else {
                                break;
                            }
                            bit >>= 1;
                            if bit == 0 {
                                break;
                            }
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
                8 => {
                    for &idx in &indices {
                        if let Some(pixel) = pixel_iter.next() {
                            let rgb = palette[idx as usize];
                            pixel[..3].copy_from_slice(&rgb);
                        } else {
                            break;
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
        // LOOKUP_3/4/5/6BIT 查表——与算术位重复展宽不同，必须保查表才能字节一致）。
        let rl = bitfield_lut(&bf.r);
        let gl = bitfield_lut(&bf.g);
        let bl = bitfield_lut(&bf.b);
        let al = bitfield_lut(&bf.a);

        // 整行读取再批量展开（消除逐像素 read_u16_le 调用）。
        let mut raw = vec![0u8; width * 2];
        for row in self.rows_mut(buf) {
            self.reader.read_exact(&mut raw)?;
            for (i, pixel) in row.chunks_mut(channels).enumerate() {
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

    fn read_full_bytes(&mut self, buf: &mut [u8], bpp: usize, skip_bpp4: bool) -> ImageResult<()> {
        let channels = self.num_channels();
        let width = self.width as usize;
        let row_padding = match bpp {
            3 => (4 - (width * 3) % 4) % 4,
            _ => 0,
        };
        // SIMD 行内批处理：整行一次性读入（消除逐像素 read_exact 调用），
        // SSSE3/AVX2 shuffle 重排通道，标量尾收齐 SIMD 未覆盖的像素。
        let simd = cfg!(target_arch = "x86_64")
            && !bmp_no_simd()
            && std::arch::is_x86_feature_detected!("ssse3");
        let raw_len = width * bpp;
        let mut raw = vec![0u8; raw_len];
        let mut pad = [0u8; 4];

        for row in self.rows_mut(buf) {
            if !simd {
                // 原始逐像素路径（标量回退，语义与改动前逐字节一致）。
                for pixel in row.chunks_mut(channels) {
                    if skip_bpp4 {
                        read_u8_val(&mut self.reader)?;
                    }
                    self.reader.read_exact(&mut pixel[..3])?;
                    pixel[..3].reverse();
                    if bpp == 4 && !skip_bpp4 {
                        self.reader.read_exact(&mut pixel[3..4])?;
                    } else if channels == 4 {
                        pixel[3] = 0xFF;
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
                        if std::arch::is_x86_feature_detected!("avx2") {
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
                        // RGB32（BI_RGB 32bit）：源每像素 [X,B,G,R] → 输出 [R,G,B]，丢弃 X。
                        self.reader.read_exact(&mut raw)?;
                        let n = unsafe { bgr_ops::xbgr_to_rgb(&raw, row, width) };
                        for i in n..width {
                            row[3 * i] = raw[4 * i + 3];
                            row[3 * i + 1] = raw[4 * i + 2];
                            row[3 * i + 2] = raw[4 * i + 1];
                        }
                    }
                    (4, 4) => {
                        // RGBA32：源每像素 [B,G,R,A] → [R,G,B,A]，整行读入 row 后原位 4→4 重排。
                        self.reader.read_exact(row)?;
                        let mut x = 0;
                        if std::arch::is_x86_feature_detected!("avx2") {
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
                let _ = (bpp, channels, row_padding);
                unreachable!("非 x86_64 上 simd 恒 false，不应进入本分支");
            }
        }
        Ok(())
    }

    fn read_rle(&mut self, buf: &mut [u8], is_8bit: bool) -> ImageResult<()> {
        let channels = self.num_channels();
        let palette = &self.palette;

        let rows: Vec<&mut [u8]> = self.rows_mut(buf).collect();
        let mut row_iter = rows.into_iter().peekable();
        while let Some(row) = row_iter.next() {
            let mut pixel_iter = row.chunks_exact_mut(channels);
            let mut x = 0usize;
            loop {
                let control = read_u8_val(&mut self.reader)?;
                if control == RLE_ESCAPE {
                    let op = read_u8_val(&mut self.reader)?;
                    match op {
                        0 => {
                            // EndOfRow。
                            pixel_iter.for_each(|p| p.fill(0));
                            break;
                        }
                        1 => {
                            // EndOfFile。
                            pixel_iter.for_each(|p| p.fill(0));
                            for r in row_iter {
                                r.fill(0);
                            }
                            return Ok(());
                        }
                        2 => {
                            // Delta。
                            let xd = usize::from(read_u8_val(&mut self.reader)?);
                            let yd = usize::from(read_u8_val(&mut self.reader)?);
                            if yd > 0 {
                                pixel_iter.for_each(|p| p.fill(0));
                                for _ in 1..yd {
                                    let r = row_iter
                                        .next()
                                        .ok_or_else(|| image_err("Corrupt RLE data"))?;
                                    r.fill(0);
                                }
                                let r = row_iter
                                    .next()
                                    .ok_or_else(|| image_err("Corrupt RLE data"))?;
                                pixel_iter = r.chunks_exact_mut(channels);
                                for _ in 0..x {
                                    let p = pixel_iter
                                        .next()
                                        .ok_or_else(|| image_err("Corrupt RLE data"))?;
                                    p.fill(0);
                                }
                            }
                            for _ in 0..xd {
                                let p = pixel_iter
                                    .next()
                                    .ok_or_else(|| image_err("Corrupt RLE data"))?;
                                p.fill(0);
                            }
                            x += xd;
                        }
                        n => {
                            // Absolute。
                            let mut length = n as usize;
                            if !is_8bit {
                                length = length.div_ceil(2);
                            }
                            length += length & 1;
                            let mut data = vec![0u8; length];
                            self.reader.read_exact(&mut data)?;
                            if is_8bit {
                                for &idx in &data {
                                    if let Some(pixel) = pixel_iter.next() {
                                        let rgb = palette[idx as usize];
                                        pixel[..3].copy_from_slice(&rgb);
                                    } else {
                                        return Err(image_err("Corrupt RLE data"));
                                    }
                                }
                            } else {
                                // 4bit 绝对模式。
                                for &idx in &data {
                                    for nib in [idx >> 4, idx & 0xf] {
                                        if let Some(pixel) = pixel_iter.next() {
                                            let rgb = palette[nib as usize];
                                            pixel[..3].copy_from_slice(&rgb);
                                        } else {
                                            return Err(image_err("Corrupt RLE data"));
                                        }
                                    }
                                }
                            }
                            x += n as usize;
                        }
                    }
                } else {
                    let palette_index = read_u8_val(&mut self.reader)?;
                    let n = control as usize;
                    if is_8bit {
                        // 8bit 重复：越界时静默截断（Imagemagick 兼容）。
                        let rgb = palette[palette_index as usize];
                        (&mut pixel_iter).take(n).for_each(|p| {
                            p[0] = rgb[0];
                            p[1] = rgb[1];
                            p[2] = rgb[2];
                        });
                    } else {
                        // 4bit 重复。
                        for _ in 0..n {
                            if let Some(pixel) = pixel_iter.next() {
                                let rgb = palette[palette_index as usize];
                                pixel[..3].copy_from_slice(&rgb);
                            } else {
                                return Err(image_err("Corrupt RLE data"));
                            }
                        }
                    }
                    x += n;
                }
            }
        }
        Ok(())
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

/// 把 `Bitfield` 的抽位+展宽语义预计算为 256 项 LUT：`lut[(data>>shift) & (2^len-1)]`
/// == `bf.read(data)`，其中取指掩码内对齐到通道长度（与 `read` 内部一致）。len==0 通道
/// 不含在查表范围（调用处按旧语义写 0xFF），此处按 `read(0)` 兜底不 panic。
fn bitfield_lut(bf: &Bitfield) -> [u8; 256] {
    let mut lut = [0u8; 256];
    if bf.len == 0 {
        lut.fill(0xFF);
        return lut;
    }
    for i in 0..256usize {
        let idx = i & ((1 << bf.len) - 1);
        lut[i] = bf.read((idx as u32) << bf.shift);
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

    /// RGB32（BI_RGB 32bit）4→3 打包：源每像素 [X,B,G,R] → 输出 [R,G,B]（丢弃 X）。
    /// SSSE3 每批 4 像素：16 字节载入 → 低 12 字节 {3,2,1}/{7,6,5}/{11,10,9}/{15,14,13}，
    /// 字节 12..15 清 0（spill，由后批/标量尾重写）。
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
