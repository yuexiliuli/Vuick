//! BMP 解码器（手写实现，行为与 image 0.25 对齐：1/2/4/8bit 调色板、16/24/32bit、RLE、bitfields）。

use std::io::{BufRead, Read, Seek, SeekFrom};

use crate::color::ColorType;
use crate::error::{DecodingError, ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind};
use crate::format::ImageFormat;
use crate::metadata::Orientation;
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
    pub(crate) fn new_with_ico_format(reader: R) -> ImageResult<BmpDecoder<R>> {
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

    pub(crate) fn read_image_data(&mut self, buf: &mut [u8]) -> ImageResult<()> {
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
        let padding_len = self.width as usize % 2 * 2;

        for row in self.rows_mut(buf) {
            for pixel in row.chunks_mut(channels) {
                let data = u32::from(read_u16_le(&mut self.reader)?);
                pixel[0] = bf.r.read(data);
                pixel[1] = bf.g.read(data);
                pixel[2] = bf.b.read(data);
                if channels == 4 {
                    pixel[3] = if bf.a.len != 0 { bf.a.read(data) } else { 0xFF };
                }
            }
            let mut pad = [0u8; 2];
            self.reader.read_exact(&mut pad[..padding_len])?;
        }
        Ok(())
    }

    fn read_32bit_bitfields(&mut self, buf: &mut [u8], bf: &Bitfields) -> ImageResult<()> {
        let channels = self.num_channels();
        for row in self.rows_mut(buf) {
            for pixel in row.chunks_mut(channels) {
                let data = read_u32_le(&mut self.reader)?;
                pixel[0] = bf.r.read(data);
                pixel[1] = bf.g.read(data);
                pixel[2] = bf.b.read(data);
                if channels == 4 {
                    pixel[3] = if bf.a.len != 0 { bf.a.read(data) } else { 0xFF };
                }
            }
        }
        Ok(())
    }

    fn read_full_bytes(&mut self, buf: &mut [u8], bpp: usize, skip_bpp4: bool) -> ImageResult<()> {
        let channels = self.num_channels();
        let row_padding = match bpp {
            3 => (4 - (self.width as usize * 3) % 4) % 4,
            _ => 0,
        };

        for row in self.rows_mut(buf) {
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

    /// 供 ICO 读取 AND 掩码用。
    pub(crate) fn reader(&mut self) -> &mut R {
        &mut self.reader
    }
}

fn read_u8_val(reader: &mut impl Read) -> ImageResult<u8> {
    let mut b = [0u8; 1];
    reader.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16_le(reader: &mut impl Read) -> ImageResult<u16> {
    let mut b = [0u8; 2];
    reader.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32_le(reader: &mut impl Read) -> ImageResult<u32> {
    let mut b = [0u8; 4];
    reader.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
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
