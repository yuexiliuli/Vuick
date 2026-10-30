//! 颜色类型（`ColorType` / `ExtendedColorType`），镜像 image 0.25 的字段与方法形状。

use std::fmt;

/// 解码器可输出的颜色类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorType {
    /// 8 位灰度。
    L8,
    /// 8 位灰度 + alpha。
    La8,
    /// 8 位 RGB。
    Rgb8,
    /// 8 位 RGBA。
    Rgba8,
    /// 16 位灰度。
    L16,
    /// 16 位灰度 + alpha。
    La16,
    /// 16 位 RGB。
    Rgb16,
    /// 16 位 RGBA。
    Rgba16,
    /// 32 位浮点 RGB。
    Rgb32F,
    /// 32 位浮点 RGBA。
    Rgba32F,
}

impl ColorType {
    /// 每像素字节数。
    pub fn bytes_per_pixel(self) -> u8 {
        match self {
            ColorType::L8 => 1,
            ColorType::La8 => 2,
            ColorType::Rgb8 => 3,
            ColorType::Rgba8 => 4,
            ColorType::L16 => 2,
            ColorType::La16 => 4,
            ColorType::Rgb16 => 6,
            ColorType::Rgba16 => 8,
            ColorType::Rgb32F => 12,
            ColorType::Rgba32F => 16,
        }
    }

    /// 通道数。
    pub fn channel_count(self) -> u8 {
        match self {
            ColorType::L8 | ColorType::L16 => 1,
            ColorType::La8 | ColorType::La16 => 2,
            ColorType::Rgb8 | ColorType::Rgb16 | ColorType::Rgb32F => 3,
            ColorType::Rgba8 | ColorType::Rgba16 | ColorType::Rgba32F => 4,
        }
    }
}

impl fmt::Display for ColorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// 扩展颜色类型：覆盖位深细节与 CMYK 等不能直接存进 `ImageBuffer` 的布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExtendedColorType {
    /// 1 位黑白（位图）。
    L1,
    /// 2 位灰度。
    L2,
    /// 4 位灰度。
    L4,
    /// 8 位灰度。
    L8,
    /// 16 位灰度。
    L16,
    /// 1 位灰度 + 1 位 alpha。
    La1,
    /// 2 位灰度 + 2 位 alpha。
    La2,
    /// 4 位灰度 + 4 位 alpha。
    La4,
    /// 8 位灰度 + alpha。
    La8,
    /// 16 位灰度 + alpha。
    La16,
    /// 1 位 RGB。
    Rgb1,
    /// 2 位 RGB。
    Rgb2,
    /// 4 位 RGB。
    Rgb4,
    /// 8 位 RGB。
    Rgb8,
    /// 16 位 RGB。
    Rgb16,
    /// 32 位浮点 RGB。
    Rgb32F,
    /// 1 位 RGBA。
    Rgba1,
    /// 2 位 RGBA。
    Rgba2,
    /// 4 位 RGBA。
    Rgba4,
    /// 8 位 RGBA。
    Rgba8,
    /// 16 位 RGBA。
    Rgba16,
    /// 32 位浮点 RGBA。
    Rgba32F,
    /// 8 位 CMYK。
    Cmyk8,
    /// 16 位 CMYK。
    Cmyk16,
    /// 未知布局。
    Unknown(u8),
}

impl ExtendedColorType {
    /// 每像素字节数（未知布局按每像素总位数上取整推算）。
    pub fn bytes_per_pixel(self) -> u8 {
        self.bits_per_pixel().div_ceil(8)
    }

    /// 每像素位数。
    pub fn bits_per_pixel(self) -> u8 {
        match self {
            ExtendedColorType::L1 => 1,
            ExtendedColorType::L2 => 2,
            ExtendedColorType::L4 => 4,
            ExtendedColorType::L8 => 8,
            ExtendedColorType::L16 => 16,
            ExtendedColorType::La1 => 2,
            ExtendedColorType::La2 => 4,
            ExtendedColorType::La4 => 8,
            ExtendedColorType::La8 => 16,
            ExtendedColorType::La16 => 32,
            ExtendedColorType::Rgb1 => 3,
            ExtendedColorType::Rgb2 => 6,
            ExtendedColorType::Rgb4 => 12,
            ExtendedColorType::Rgb8 => 24,
            ExtendedColorType::Rgb16 => 48,
            ExtendedColorType::Rgb32F => 96,
            ExtendedColorType::Rgba1 => 4,
            ExtendedColorType::Rgba2 => 8,
            ExtendedColorType::Rgba4 => 16,
            ExtendedColorType::Rgba8 => 32,
            ExtendedColorType::Rgba16 => 64,
            ExtendedColorType::Rgba32F => 128,
            ExtendedColorType::Cmyk8 => 32,
            ExtendedColorType::Cmyk16 => 64,
            ExtendedColorType::Unknown(bits) => bits,
        }
    }

    /// 通道数。
    pub fn channel_count(self) -> u8 {
        match self {
            ExtendedColorType::L1
            | ExtendedColorType::L2
            | ExtendedColorType::L4
            | ExtendedColorType::L8
            | ExtendedColorType::L16 => 1,
            ExtendedColorType::La1
            | ExtendedColorType::La2
            | ExtendedColorType::La4
            | ExtendedColorType::La8
            | ExtendedColorType::La16 => 2,
            ExtendedColorType::Rgb1
            | ExtendedColorType::Rgb2
            | ExtendedColorType::Rgb4
            | ExtendedColorType::Rgb8
            | ExtendedColorType::Rgb16
            | ExtendedColorType::Rgb32F => 3,
            ExtendedColorType::Rgba1
            | ExtendedColorType::Rgba2
            | ExtendedColorType::Rgba4
            | ExtendedColorType::Rgba8
            | ExtendedColorType::Rgba16
            | ExtendedColorType::Rgba32F
            | ExtendedColorType::Cmyk8
            | ExtendedColorType::Cmyk16 => 4,
            ExtendedColorType::Unknown(bits) if bits > 0 => 3,
            ExtendedColorType::Unknown(_) => 1,
        }
    }

    /// 占用的缓冲字节数（宽 × 高 × 每像素字节）。
    pub fn buffer_size(self, width: u32, height: u32) -> u64 {
        u64::from(self.bytes_per_pixel())
            .saturating_mul(u64::from(width))
            .saturating_mul(u64::from(height))
    }
}

impl From<ColorType> for ExtendedColorType {
    fn from(c: ColorType) -> Self {
        match c {
            ColorType::L8 => ExtendedColorType::L8,
            ColorType::La8 => ExtendedColorType::La8,
            ColorType::Rgb8 => ExtendedColorType::Rgb8,
            ColorType::Rgba8 => ExtendedColorType::Rgba8,
            ColorType::L16 => ExtendedColorType::L16,
            ColorType::La16 => ExtendedColorType::La16,
            ColorType::Rgb16 => ExtendedColorType::Rgb16,
            ColorType::Rgba16 => ExtendedColorType::Rgba16,
            ColorType::Rgb32F => ExtendedColorType::Rgb32F,
            ColorType::Rgba32F => ExtendedColorType::Rgba32F,
        }
    }
}
