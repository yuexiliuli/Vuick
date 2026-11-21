//! 基础特质：`Primitive` 子像素类型标记与 `Pixel` 像素抽象（镜像 image 0.25 所需子集）。

use std::fmt::Debug;

use crate::color::ExtendedColorType;
use crate::pixel::{Luma, LumaA, Rgb, Rgba};

/// 可作为像素通道的标量类型（u8/u16/f32），并附带位深换算。
pub trait Primitive: Copy + Clone + Default + Debug + PartialEq + Send + Sync + 'static {
    /// 归一化到 8 位（u16 用 `(v + 128) / 257`，f32 用 `round(clamp(0,1) * 255)`）。
    fn narrow8(self) -> u8;

    /// 归一到 16 位（u8 用 `(v << 8) | v`）。
    fn widen16(self) -> u16;
}

impl Primitive for u8 {
    fn narrow8(self) -> u8 {
        self
    }
    fn widen16(self) -> u16 {
        (u32::from(self) << 8 | u32::from(self)) as u16
    }
}

impl Primitive for u16 {
    fn narrow8(self) -> u8 {
        ((u32::from(self) + 128) / 257) as u8
    }
    fn widen16(self) -> u16 {
        self
    }
}

impl Primitive for f32 {
    fn narrow8(self) -> u8 {
        let clamped = if self.is_nan() || self >= 1.0 { 1.0 } else { self.max(0.0) };
        (clamped * 255.0).round() as u8
    }
    fn widen16(self) -> u16 {
        let clamped = if self.is_nan() || self >= 1.0 { 1.0 } else { self.max(0.0) };
        (clamped * 65535.0).round() as u16
    }
}

/// 像素抽象：固定通道数的值类型，可被 `ImageBuffer<P, _>` 用作像素。
pub trait Pixel: Copy + Clone + Debug + PartialEq + Send + Sync + 'static {
    /// 子像素类型。
    type Subpixel: Primitive;

    /// 通道数。
    const CHANNEL_COUNT: u8;

    /// 底层连续通道切片。
    fn channels(&self) -> &[Self::Subpixel];

    /// 底层连续通道可变切片。
    fn channels_mut(&mut self) -> &mut [Self::Subpixel];

    /// 由最大 4 个通道构造（不足的通道忽略）。
    fn from_channels(
        r: Self::Subpixel,
        g: Self::Subpixel,
        b: Self::Subpixel,
        a: Self::Subpixel,
    ) -> Self;

    /// 归一化到 8 位 RGBA（供 `to_rgba8` 等转换使用）。
    fn to_rgba(&self) -> Rgba<u8>;

    /// 归一化到 8 位灰度。
    fn to_luma(&self) -> Luma<u8>;
}

/// 可写入像素类型标记：为 `ImageBuffer::save`/`save_with_format` 提供编码用颜色类型。
/// 镜像 image 0.25 的 `PixelWithColorType`。只对 kimg 支持编码的像素类型实现。
pub trait PixelWithColorType: Pixel {
    /// 编码时使用的扩展颜色类型。
    const COLOR_TYPE: ExtendedColorType;
}

macro_rules! impl_pixel_with_color_type {
    ($($px:ty => $ct:ident),+ $(,)?) => {
        $(
            impl PixelWithColorType for $px {
                const COLOR_TYPE: ExtendedColorType = ExtendedColorType::$ct;
            }
        )+
    };
}

impl_pixel_with_color_type! {
    Luma<u8> => L8,
    Luma<u16> => L16,
    LumaA<u8> => La8,
    LumaA<u16> => La16,
    Rgb<u8> => Rgb8,
    Rgb<u16> => Rgb16,
    Rgba<u8> => Rgba8,
    Rgba<u16> => Rgba16,
    Rgb<f32> => Rgb32F,
    Rgba<f32> => Rgba32F,
}
