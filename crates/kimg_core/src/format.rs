//! `ImageFormat` 枚举与魔数嗅探（与 image 0.25 一致）。

use std::ffi::OsStr;
use std::path::Path;

use crate::error::{ImageError, ImageFormatHint, ImageResult, UnsupportedError};

/// 支持的图像格式枚举（含编码/解码能力差异）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[non_exhaustive]
pub enum ImageFormat {
    /// PNG。
    Png,
    /// JPEG。
    Jpeg,
    /// GIF。
    Gif,
    /// WebP。
    WebP,
    /// 一般 PNM（PBM/PGM/PPM/PAM）。
    Pnm,
    /// TIFF。
    Tiff,
    /// BMP。
    Bmp,
    /// ICO。
    Ico,
    /// TGA（仅扩展名识别，未实现解码）。
    Tga,
    /// DDS（仅扩展名识别，未实现解码）。
    Dds,
    /// Radiance HDR（仅扩展名识别）。
    Hdr,
    /// OpenEXR（仅扩展名识别）。
    OpenExr,
    /// farbfeld（仅扩展名识别）。
    Farbfeld,
    /// AVIF（仅扩展名识别）。
    Avif,
    /// QOI（仅扩展名识别）。
    Qoi,
    /// PCX（仅扩展名识别）。
    Pcx,
    /// HEIF / HEIC（ISOBMFF item 容器 + HEVC；kimg_heif 支持识别/读尺寸，解码逐步落地）。
    Heic,
}

impl ImageFormat {
    /// 由扩展名识别格式（大小写不敏感）。
    pub fn from_extension<S>(ext: S) -> Option<Self>
    where
        S: AsRef<OsStr>,
    {
        let ext = ext.as_ref().to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "avif" => ImageFormat::Avif,
            "heic" | "heif" => ImageFormat::Heic,
            "jpg" | "jpeg" | "jfif" => ImageFormat::Jpeg,
            "png" | "apng" => ImageFormat::Png,
            "gif" => ImageFormat::Gif,
            "webp" => ImageFormat::WebP,
            "tif" | "tiff" => ImageFormat::Tiff,
            "tga" => ImageFormat::Tga,
            "dds" => ImageFormat::Dds,
            "bmp" => ImageFormat::Bmp,
            "ico" => ImageFormat::Ico,
            "hdr" => ImageFormat::Hdr,
            "exr" => ImageFormat::OpenExr,
            "pbm" | "pam" | "ppm" | "pgm" | "pnm" => ImageFormat::Pnm,
            "ff" => ImageFormat::Farbfeld,
            "qoi" => ImageFormat::Qoi,
            "pcx" => ImageFormat::Pcx,
            _ => return None,
        })
    }

    /// 由路径扩展名识别格式。
    pub fn from_path<P>(path: P) -> ImageResult<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        path.extension()
            .and_then(Self::from_extension)
            .ok_or_else(|| {
                let format_hint = match path.extension() {
                    None => ImageFormatHint::Unknown,
                    Some(os) => ImageFormatHint::PathExtension(os.into()),
                };
                ImageError::Unsupported(format_hint.into())
            })
    }

    /// 本格式是否支持读取。
    pub fn can_read(&self) -> bool {
        matches!(
            self,
            ImageFormat::Png
                | ImageFormat::Jpeg
                | ImageFormat::Gif
                | ImageFormat::WebP
                | ImageFormat::Tiff
                | ImageFormat::Bmp
                | ImageFormat::Ico
                | ImageFormat::Pnm
                | ImageFormat::Heic
        )
    }

    /// 本格式是否支持写入（kimg 仅实现 PNG/GIF 编码）。
    pub fn can_write(&self) -> bool {
        matches!(self, ImageFormat::Png | ImageFormat::Gif)
    }
}

/// 魔数嗅探表：(签名, 掩码, 格式)。掩码为空的按前缀匹配；非空则逐字节与掩码比对。
static MAGIC_BYTES: [(&[u8], &[u8], ImageFormat); 22] = [
    (b"\x89PNG\r\n\x1a\n", b"", ImageFormat::Png),
    (&[0xff, 0xd8, 0xff], b"", ImageFormat::Jpeg),
    (b"GIF89a", b"", ImageFormat::Gif),
    (b"GIF87a", b"", ImageFormat::Gif),
    (
        b"RIFF\0\0\0\0WEBP",
        b"\xFF\xFF\xFF\xFF\0\0\0\0",
        ImageFormat::WebP,
    ),
    (b"MM\x00*", b"", ImageFormat::Tiff),
    (b"II*\x00", b"", ImageFormat::Tiff),
    (b"DDS ", b"", ImageFormat::Dds),
    (b"BM", b"", ImageFormat::Bmp),
    (&[0, 0, 1, 0], b"", ImageFormat::Ico),
    (b"#?RADIANCE", b"", ImageFormat::Hdr),
    (b"\0\0\0\0ftypavif", b"\xFF\xFF\0\0", ImageFormat::Avif),
    (&[0x76, 0x2f, 0x31, 0x01], b"", ImageFormat::OpenExr),
    (b"qoif", b"", ImageFormat::Qoi),
    (b"P1", b"", ImageFormat::Pnm),
    (b"P2", b"", ImageFormat::Pnm),
    (b"P3", b"", ImageFormat::Pnm),
    (b"P4", b"", ImageFormat::Pnm),
    (b"P5", b"", ImageFormat::Pnm),
    (b"P6", b"", ImageFormat::Pnm),
    (b"P7", b"", ImageFormat::Pnm),
    (b"farbfeld", b"", ImageFormat::Farbfeld),
];

/// 根据内存块开头的魔数猜测格式（不检验整块有效性）。
pub fn guess_format(buffer: &[u8]) -> ImageResult<ImageFormat> {
    match guess_format_impl(buffer) {
        Some(format) => Ok(format),
        None => Err(ImageError::Unsupported(ImageFormatHint::Unknown.into())),
    }
}

/// 内部魔数匹配实现（返回 `None` 表示无法判断）。
pub(crate) fn guess_format_impl(buffer: &[u8]) -> Option<ImageFormat> {
    for &(signature, mask, format) in &MAGIC_BYTES {
        if mask.is_empty() {
            if buffer.starts_with(signature) {
                return Some(format);
            }
        } else if buffer.len() >= signature.len()
            && buffer
                .iter()
                .zip(signature.iter())
                .zip(mask.iter().chain(std::iter::repeat(&0xFF)))
                .all(|((&byte, &sig), &mask)| byte & mask == sig)
        {
            return Some(format);
        }
    }
    // HEIF/HEIC：IPv4/ISOBMFF `ftyp` box 列出 HEIF brand（且无 AVIF brand）。
    if heif_brands(buffer) {
        return Some(ImageFormat::Heic);
    }
    None
}

/// 与 image/C# `HeifImageDecoder.CanDecode` 同步的 HEIF brand 检测：
/// `ftyp` 里列出 HEIF brand（heic/heix/heim/heis/hevc/hevx/mif1/msf1）且不含 AVIF brand
/// （avif/avis，它们常连同 mif1 列出，但不是 HEVC）。
fn heif_brands(header: &[u8]) -> bool {
    if header.len() < 16 || &header[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]])
        .min(header.len() as u32) as usize;
    let mut heif = false;
    let mut at = 8usize;
    loop {
        if at + 4 > size {
            break;
        }
        let brand = &header[at..at + 4];
        if brand == b"avif" || brand == b"avis" {
            return false;
        }
        heif |= brand == b"heic"
            || brand == b"heix"
            || brand == b"heim"
            || brand == b"heis"
            || brand == b"hevc"
            || brand == b"hevx"
            || brand == b"mif1"
            || brand == b"msf1";
        // 跳过 major brand 后的 minor version（at==8 时 +8，之后 +4）。
        at += if at == 8 { 8 } else { 4 };
    }
    heif
}

// 供 format_hint 的 From 转换使用（避免未用导入告警）。
#[allow(unused_imports)]
use UnsupportedError as _;
