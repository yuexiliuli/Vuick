//! 具体像素类型：`Luma` / `LumaA` / `Rgb` / `Rgba` 及其 `Pixel` 实现。
//!
//! 构造语法与 image 0.25 一致（元组结构 + 公开数组字段，如 `Rgba([r, g, b, a])`）。

use std::ops::{Deref, DerefMut, Index, IndexMut};

use crate::traits::{Pixel, Primitive};

/// 单通道（灰度）像素。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Luma<T: Primitive>(pub [T; 1]);

/// 灰度 + alpha 像素。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LumaA<T: Primitive>(pub [T; 2]);

/// RGB 像素。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rgb<T: Primitive>(pub [T; 3]);

/// RGBA 像素。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rgba<T: Primitive>(pub [T; 4]);

impl<T: Primitive> Deref for Luma<T> {
    type Target = [T; 1];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: Primitive> DerefMut for Luma<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<T: Primitive> Index<usize> for Luma<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.0[index]
    }
}
impl<T: Primitive> IndexMut<usize> for Luma<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.0[index]
    }
}

impl<T: Primitive> Deref for LumaA<T> {
    type Target = [T; 2];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: Primitive> DerefMut for LumaA<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<T: Primitive> Index<usize> for LumaA<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.0[index]
    }
}
impl<T: Primitive> IndexMut<usize> for LumaA<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.0[index]
    }
}

impl<T: Primitive> Deref for Rgb<T> {
    type Target = [T; 3];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: Primitive> DerefMut for Rgb<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<T: Primitive> Index<usize> for Rgb<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.0[index]
    }
}
impl<T: Primitive> IndexMut<usize> for Rgb<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.0[index]
    }
}

impl<T: Primitive> Deref for Rgba<T> {
    type Target = [T; 4];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: Primitive> DerefMut for Rgba<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<T: Primitive> Index<usize> for Rgba<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.0[index]
    }
}
impl<T: Primitive> IndexMut<usize> for Rgba<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.0[index]
    }
}

macro_rules! from_arrays {
    ($($t:ident $n:expr)*) => {$(
        impl<T: Primitive> From<[T; $n]> for $t<T> {
            fn from(v: [T; $n]) -> Self {
                $t(v)
            }
        }
        impl<T: Primitive> From<T> for $t<T> {
            fn from(v: T) -> Self {
                $t([v; $n])
            }
        }
    )*};
}
from_arrays! { Luma 1 LumaA 2 Rgb 3 Rgba 4 }

impl<T: Primitive> From<[T; 3]> for Rgba<T> {
    fn from(v: [T; 3]) -> Self {
        Rgba([v[0], v[1], v[2], T::default()])
    }
}

impl<T: Primitive> From<Luma<T>> for LumaA<T> {
    fn from(v: Luma<T>) -> Self {
        LumaA([v[0], T::default()])
    }
}
impl<T: Primitive> From<Luma<T>> for Rgb<T> {
    fn from(v: Luma<T>) -> Self {
        Rgb([v[0], v[0], v[0]])
    }
}
impl<T: Primitive> From<Luma<T>> for Rgba<T> {
    fn from(v: Luma<T>) -> Self {
        Rgba([v[0], v[0], v[0], T::default()])
    }
}
impl<T: Primitive> From<LumaA<T>> for Rgba<T> {
    fn from(v: LumaA<T>) -> Self {
        Rgba([v[0], v[0], v[0], v[1]])
    }
}
impl<T: Primitive> From<Rgb<T>> for Rgba<T> {
    fn from(v: Rgb<T>) -> Self {
        Rgba([v[0], v[1], v[2], T::default()])
    }
}

/// sRGB → 亮度权重（Rec.709，与 image 0.25 一致）。
const SRGB_LUMA: [u32; 3] = [2126, 7152, 722];
const SRGB_LUMA_DIV: u32 = 10000;

fn rgb_to_luma(rgb: &[u16; 3]) -> u32 {
    (SRGB_LUMA[0] * u32::from(rgb[0])
        + SRGB_LUMA[1] * u32::from(rgb[1])
        + SRGB_LUMA[2] * u32::from(rgb[2]))
        / SRGB_LUMA_DIV
}

impl<T: Primitive> Pixel for Luma<T> {
    type Subpixel = T;
    const CHANNEL_COUNT: u8 = 1;

    fn channels(&self) -> &[T] {
        &self.0
    }
    fn channels_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
    fn from_channels(r: T, _g: T, _b: T, _a: T) -> Self {
        Luma([r])
    }
    fn to_rgba(&self) -> Rgba<u8> {
        let g = self[0].narrow8();
        Rgba([g, g, g, 255])
    }
    fn to_luma(&self) -> Luma<u8> {
        Luma([self[0].narrow8()])
    }
}

impl<T: Primitive> Pixel for LumaA<T> {
    type Subpixel = T;
    const CHANNEL_COUNT: u8 = 2;

    fn channels(&self) -> &[T] {
        &self.0
    }
    fn channels_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
    fn from_channels(r: T, _g: T, _b: T, a: T) -> Self {
        LumaA([r, a])
    }
    fn to_rgba(&self) -> Rgba<u8> {
        let g = self[0].narrow8();
        Rgba([g, g, g, self[1].narrow8()])
    }
    fn to_luma(&self) -> Luma<u8> {
        Luma([self[0].narrow8()])
    }
}

impl<T: Primitive> Pixel for Rgb<T> {
    type Subpixel = T;
    const CHANNEL_COUNT: u8 = 3;

    fn channels(&self) -> &[T] {
        &self.0
    }
    fn channels_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
    fn from_channels(r: T, g: T, b: T, _a: T) -> Self {
        Rgb([r, g, b])
    }
    fn to_rgba(&self) -> Rgba<u8> {
        Rgba([
            self[0].narrow8(),
            self[1].narrow8(),
            self[2].narrow8(),
            255,
        ])
    }
    fn to_luma(&self) -> Luma<u8> {
        let rgb = [self[0].widen16(), self[1].widen16(), self[2].widen16()];
        Luma([rgb_to_luma(&rgb) as u8])
    }
}

impl<T: Primitive> Pixel for Rgba<T> {
    type Subpixel = T;
    const CHANNEL_COUNT: u8 = 4;

    fn channels(&self) -> &[T] {
        &self.0
    }
    fn channels_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
    fn from_channels(r: T, g: T, b: T, a: T) -> Self {
        Rgba([r, g, b, a])
    }
    fn to_rgba(&self) -> Rgba<u8> {
        Rgba([
            self[0].narrow8(),
            self[1].narrow8(),
            self[2].narrow8(),
            self[3].narrow8(),
        ])
    }
    fn to_luma(&self) -> Luma<u8> {
        let rgb = [self[0].widen16(), self[1].widen16(), self[2].widen16()];
        Luma([rgb_to_luma(&rgb) as u8])
    }
}
