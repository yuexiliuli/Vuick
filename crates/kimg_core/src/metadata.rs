//! EXIF 方向元数据（`Orientation`），与 image 0.25 完全同语义。

/// 描述应用到图像上的变换（与 EXIF orientation 兼容）。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Orientation {
    /// 不做任何变换。
    NoTransforms,
    /// 顺时针旋转 90°。
    Rotate90,
    /// 旋转 180°。
    Rotate180,
    /// 顺时针旋转 270°。
    Rotate270,
    /// 水平翻转。
    FlipHorizontal,
    /// 垂直翻转。
    FlipVertical,
    /// 顺时针旋转 90° 再水平翻转。
    Rotate90FlipH,
    /// 顺时针旋转 270° 再水平翻转。
    Rotate270FlipH,
}

impl Orientation {
    /// 从 EXIF orientation 数值转换。
    pub fn from_exif(exif_orientation: u8) -> Option<Self> {
        match exif_orientation {
            1 => Some(Self::NoTransforms),
            2 => Some(Self::FlipHorizontal),
            3 => Some(Self::Rotate180),
            4 => Some(Self::FlipVertical),
            5 => Some(Self::Rotate90FlipH),
            6 => Some(Self::Rotate90),
            7 => Some(Self::Rotate270FlipH),
            8 => Some(Self::Rotate270),
            0 | 9.. => None,
        }
    }

    /// 转换为 EXIF orientation 数值。
    pub fn to_exif(self) -> u8 {
        match self {
            Self::NoTransforms => 1,
            Self::FlipHorizontal => 2,
            Self::Rotate180 => 3,
            Self::FlipVertical => 4,
            Self::Rotate90FlipH => 5,
            Self::Rotate90 => 6,
            Self::Rotate270FlipH => 7,
            Self::Rotate270 => 8,
        }
    }

    /// 从原始 EXIF 块中解析方向（TIFF 头 + 0x0112 标签）。
    pub fn from_exif_chunk(chunk: &[u8]) -> Option<Self> {
        if chunk.len() < 8 {
            return None;
        }
        let (endianness, ifd_offset) = match &chunk[..4] {
            [0x49, 0x49, 42, 0] => (ExifEndian::Little, read_u32_le(chunk, 4) as u64),
            [0x4d, 0x4d, 0, 42] => (ExifEndian::Big, read_u32_be(chunk, 4) as u64),
            _ => return None,
        };
        // IFD 起始偏移为 TIFF 头起始（EXIF 块第 0 字节）起的绝对偏移。
        let ifd_base = ifd_offset as usize;
        if ifd_base + 2 > chunk.len() {
            return None;
        }
        let entries = match endianness {
            ExifEndian::Little => read_u16_le(chunk, ifd_base),
            ExifEndian::Big => read_u16_be(chunk, ifd_base),
        };
        for i in 0..entries {
            let entry = ifd_base + 2 + i as usize * 12;
            if entry + 12 > chunk.len() {
                return None;
            }
            let tag = match endianness {
                ExifEndian::Little => read_u16_le(chunk, entry),
                ExifEndian::Big => read_u16_be(chunk, entry),
            };
            let format = match endianness {
                ExifEndian::Little => read_u16_le(chunk, entry + 2),
                ExifEndian::Big => read_u16_be(chunk, entry + 2),
            };
            let count = match endianness {
                ExifEndian::Little => read_u32_le(chunk, entry + 4),
                ExifEndian::Big => read_u32_be(chunk, entry + 4),
            };
            let value = match endianness {
                ExifEndian::Little => read_u16_le(chunk, entry + 8),
                ExifEndian::Big => read_u16_be(chunk, entry + 8),
            };
            if tag == 0x112 && format == 3 && count == 1 {
                return Self::from_exif(value.min(255) as u8);
            }
        }
        None
    }
}

#[derive(Copy, Clone)]
enum ExifEndian {
    Big,
    Little,
}

fn read_u16_le(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn read_u16_be(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}
fn read_u32_le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn read_u32_be(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
