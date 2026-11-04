//! ICO 解码器（手写容器解析：目录 → 选最佳条目 → PNG 或 BMP 内嵌条目）。

use std::io::{BufRead, Read, Seek, SeekFrom};

use kimg_core::ImageDecoder;
use kimg_bmp::BmpDecoder;
use kimg_png::PngDecoder;
use kimg_core::color::ColorType;
use kimg_core::error::{
    DecodingError, ImageError, ImageResult, UnsupportedError, UnsupportedErrorKind,
};
use kimg_core::format::ImageFormat;
use kimg_core::metadata::Orientation;

pub(crate) const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

fn ico_err(msg: &str) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormat::Ico.into(), msg.to_string()))
}

/// 目录条目。
#[derive(Clone, Copy, Default)]
struct DirEntry {
    width: u8,
    height: u8,
    bits_per_pixel: u16,
    image_length: u32,
    image_offset: u32,
}

impl DirEntry {
    fn real_width(&self) -> u16 {
        match self.width {
            0 => 256,
            w => u16::from(w),
        }
    }

    fn real_height(&self) -> u16 {
        match self.height {
            0 => 256,
            h => u16::from(h),
        }
    }

    fn matches_dimensions(&self, width: u32, height: u32) -> bool {
        u32::from(self.real_width()) == width.min(256)
            && u32::from(self.real_height()) == height.min(256)
    }
}

fn read_entry<R: Read>(r: &mut R) -> ImageResult<DirEntry> {
    let mut d = DirEntry::default();
    d.width = read_u8(r)?;
    d.height = read_u8(r)?;
    let _color_count = read_u8(r)?;
    let _reserved = read_u8(r)?;
    // 若为大热区（CUR）坐标，校验不超过 256；ICO 的 planes 应为 0/1。
    let planes = read_u16(r)?;
    if planes > 256 {
        return Err(ico_err(
            "ICO image entry has too many color planes or too large hotspot value",
        ));
    }
    d.bits_per_pixel = read_u16(r)?;
    if d.bits_per_pixel > 256 {
        return Err(ico_err(
            "ICO image entry has too many bits per pixel or too large hotspot value",
        ));
    }
    d.image_length = read_u32(r)?;
    d.image_offset = read_u32(r)?;
    Ok(d)
}

/// 选出「位深最大，其次面积最大」的条目（与 image 0.25 相同）。
fn best_entry(mut entries: Vec<DirEntry>) -> ImageResult<DirEntry> {
    let mut best = entries
        .pop()
        .ok_or_else(|| ico_err("ICO directory contains no image"))?;
    let mut best_score = (
        best.bits_per_pixel,
        u32::from(best.real_width()) * u32::from(best.real_height()),
    );
    for entry in entries {
        let score = (
            entry.bits_per_pixel,
            u32::from(entry.real_width()) * u32::from(entry.real_height()),
        );
        if score > best_score {
            best = entry;
            best_score = score;
        }
    }
    Ok(best)
}

enum InnerDecoder<R: BufRead + Seek> {
    Bmp(BmpDecoder<R>),
    Png(Box<PngDecoder<'static, R>>),
}

/// ICO 解码器。
pub struct IcoDecoder<R: BufRead + Seek> {
    selected_entry: DirEntry,
    inner_decoder: InnerDecoder<R>,
}

fn read_u8(r: &mut impl Read) -> ImageResult<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(r: &mut impl Read) -> ImageResult<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(r: &mut impl Read) -> ImageResult<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

impl<R: BufRead + Seek> IcoDecoder<R> {
    /// 构造：解析目录并挑选最佳条目。
    pub fn new(mut r: R) -> ImageResult<IcoDecoder<R>> {
        let _reserved = read_u16(&mut r)?;
        let _image_type = read_u16(&mut r)?;
        let count = read_u16(&mut r)?;
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            entries.push(read_entry(&mut r)?);
        }

        let entry = best_entry(entries)?;
        let decoder = entry.decoder(r)?;

        Ok(IcoDecoder {
            selected_entry: entry,
            inner_decoder: decoder,
        })
    }
}

impl DirEntry {
    fn seek_to_start(&self, r: &mut impl Seek) -> ImageResult<()> {
        r.seek(SeekFrom::Start(u64::from(self.image_offset)))?;
        Ok(())
    }

    fn is_png<R: Read + Seek>(&self, r: &mut R) -> ImageResult<bool> {
        self.seek_to_start(r)?;
        let mut signature = [0u8; 8];
        r.read_exact(&mut signature)?;
        Ok(signature == PNG_SIGNATURE)
    }

    fn decoder<R: BufRead + Seek>(&self, mut r: R) -> ImageResult<InnerDecoder<R>> {
        let is_png = self.is_png(&mut r)?;
        self.seek_to_start(&mut r)?;
        if is_png {
            Ok(InnerDecoder::Png(Box::new(PngDecoder::new(r)?)))
        } else {
            Ok(InnerDecoder::Bmp(BmpDecoder::new_with_ico_format(r)?))
        }
    }
}

impl<R: BufRead + Seek> ImageDecoder for IcoDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        match &self.inner_decoder {
            InnerDecoder::Bmp(d) => d.dimensions(),
            InnerDecoder::Png(d) => d.dimensions(),
        }
    }

    fn color_type(&self) -> ColorType {
        match &self.inner_decoder {
            InnerDecoder::Bmp(d) => d.color_type(),
            InnerDecoder::Png(d) => d.color_type(),
        }
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        match self.inner_decoder {
            InnerDecoder::Png(decoder) => {
                if self.selected_entry.image_length < PNG_SIGNATURE.len() as u32 {
                    return Err(ico_err(
                        "Entry specified a length that is shorter than PNG header!",
                    ));
                }
                let (width, height) = decoder.dimensions();
                if !self.selected_entry.matches_dimensions(width, height) {
                    return Err(ico_err("PNG dimensions do not match entry!"));
                }
                if decoder.color_type() != ColorType::Rgba8 {
                    return Err(ico_err("The PNG is not in RGBA format!"));
                }
                decoder.read_image(buf)
            }
            InnerDecoder::Bmp(mut decoder) => {
                let (width, height) = decoder.dimensions();
                if !self.selected_entry.matches_dimensions(width, height) {
                    return Err(ico_err("BMP dimensions do not match entry!"));
                }
                if decoder.color_type() != ColorType::Rgba8 {
                    return Err(ImageError::Unsupported(
                        UnsupportedError::from_format_and_kind(
                            ImageFormat::Bmp.into(),
                            UnsupportedErrorKind::Color(decoder.color_type().into()),
                        ),
                    ));
                }
                decoder.read_image_data(buf)?;

                let r = decoder.reader();
                let image_end = r.stream_position()?;
                let data_end = u64::from(self.selected_entry.image_offset)
                    + u64::from(self.selected_entry.image_length);

                let mask_row_bytes = width.div_ceil(32) * 4;
                let mask_length = u64::from(mask_row_bytes) * u64::from(height);

                if data_end >= image_end + mask_length {
                    // 有 AND 掩码 → 逐位置透明。
                    for y in 0..height {
                        let mut x = 0;
                        for _ in 0..mask_row_bytes {
                            let mask_byte = read_u8(r)?;
                            for bit in (0..8).rev() {
                                if x >= width {
                                    break;
                                }
                                if mask_byte & (1 << bit) != 0 {
                                    buf[((height - y - 1) * width + x) as usize * 4 + 3] = 0;
                                }
                                x += 1;
                            }
                        }
                    }
                    Ok(())
                } else if data_end == image_end {
                    // 无掩码亦可接受。
                    Ok(())
                } else {
                    Err(ico_err("ICO image data size did not match expected size"))
                }
            }
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
