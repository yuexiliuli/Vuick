//! `ImageBuffer`：像素矩阵容器（镜像 image 0.25 的语义与别名），及 `GenericImageView`/`GenericImage`。

use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use crate::error::{ImageError, ImageResult, ParameterError, ParameterErrorKind};
use crate::pixel::{Luma, LumaA, Rgb, Rgba};
use crate::traits::Pixel;

/// 只读图像视图。
pub trait GenericImageView {
    /// 像素类型。
    type Pixel: Pixel;

    /// (宽, 高)。
    fn dimensions(&self) -> (u32, u32);

    /// ((0,0), (宽, 高))。
    fn bounds(&self) -> (u32, u32, u32, u32) {
        let (w, h) = self.dimensions();
        (0, 0, w, h)
    }

    /// 读取像素（越界 panic，与 image 0.25 一致）。
    fn get_pixel(&self, x: u32, y: u32) -> &Self::Pixel;

    /// 宽。
    fn width(&self) -> u32 {
        self.dimensions().0
    }

    /// 高。
    fn height(&self) -> u32 {
        self.dimensions().1
    }

    /// 像素迭代器（行主序）。
    fn pixels(&self) -> Pixels<'_, Self>
    where
        Self: Sized,
    {
        let (width, height) = self.dimensions();
        Pixels {
            image: self,
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// 带坐标的像素迭代器。
    fn enumerate_pixels(&self) -> EnumeratePixels<'_, Self>
    where
        Self: Sized,
    {
        let (width, height) = self.dimensions();
        EnumeratePixels {
            image: self,
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// 可变图像视图。
pub trait GenericImage: GenericImageView {
    /// 写入像素（越界 panic）。
    fn put_pixel(&mut self, x: u32, y: u32, pixel: Self::Pixel);

    /// 获取像素可变借用。
    fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut Self::Pixel;

    /// 把 `other` 复制到本图 `(x, y)` 处；越界返回 `ParameterError::DimensionMismatch`。
    fn copy_from<O>(&mut self, other: &O, x: u32, y: u32) -> ImageResult<()>
    where
        O: GenericImageView<Pixel = Self::Pixel>,
    {
        let (src_w, src_h) = other.dimensions();
        let (dst_w, dst_h) = self.dimensions();
        if (x as u64).saturating_add(src_w as u64) > dst_w as u64
            || (y as u64).saturating_add(src_h as u64) > dst_h as u64
        {
            return Err(ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::DimensionMismatch,
            )));
        }
        for (sx, sy, p) in other.enumerate_pixels() {
            self.put_pixel(x + sx, y + sy, *p);
        }
        Ok(())
    }

    /// 像素可变迭代器。
    fn pixels_mut(&mut self) -> PixelsMut<'_, Self>
    where
        Self: Sized,
    {
        let (width, height) = self.dimensions();
        PixelsMut {
            image: self,
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// 带坐标的可变像素迭代器。
    fn enumerate_pixels_mut(&mut self) -> EnumeratePixelsMut<'_, Self>
    where
        Self: Sized,
    {
        let (width, height) = self.dimensions();
        EnumeratePixelsMut {
            image: self,
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// 像素只读迭代器（任意 `GenericImageView`）。
pub struct Pixels<'a, I: ?Sized> {
    image: &'a I,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl<'a, I: GenericImageView> Iterator for Pixels<'a, I> {
    type Item = &'a I::Pixel;

    fn next(&mut self) -> Option<&'a I::Pixel> {
        if self.x >= self.width {
            self.x = 0;
            self.y += 1;
        }
        if self.y >= self.height {
            return None;
        }
        let pixel = self.image.get_pixel(self.x, self.y);
        self.x += 1;
        Some(pixel)
    }
}

/// 像素坐标 + 只读引用迭代器。
pub struct EnumeratePixels<'a, I: ?Sized> {
    image: &'a I,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl<'a, I: GenericImageView> Iterator for EnumeratePixels<'a, I> {
    type Item = (u32, u32, &'a I::Pixel);

    fn next(&mut self) -> Option<(u32, u32, &'a I::Pixel)> {
        if self.x >= self.width {
            self.x = 0;
            self.y += 1;
        }
        if self.y >= self.height {
            return None;
        }
        let (x, y) = (self.x, self.y);
        let pixel = self.image.get_pixel(x, y);
        self.x += 1;
        Some((x, y, pixel))
    }
}

/// 像素可变迭代器（仅对 `ImageBuffer` 具体实现，见下方 `Iterator`）。
pub struct PixelsMut<'a, I: ?Sized> {
    image: &'a mut I,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// 像素坐标 + 可变引用迭代器（仅对 `ImageBuffer` 具体实现）。
pub struct EnumeratePixelsMut<'a, I: ?Sized> {
    image: &'a mut I,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// `ImageBuffer`：以 `P` 为像素、任意连续容器（`Vec<P::Subpixel>` 等）为存储的矩阵。
///
/// 别名：`RgbaImage == ImageBuffer<Rgba<u8>, Vec<u8>>` 等。
#[derive(Debug, PartialEq, Clone)]
pub struct ImageBuffer<P: Pixel, Container> {
    width: u32,
    height: u32,
    data: Container,
    _phantom: PhantomData<P>,
}

/// 灰度图别名。
pub type GrayImage = ImageBuffer<Luma<u8>, Vec<u8>>;
/// 灰度 + alpha 图别名。
pub type GrayAlphaImage = ImageBuffer<LumaA<u8>, Vec<u8>>;
/// RGB 图别名。
pub type RgbImage = ImageBuffer<Rgb<u8>, Vec<u8>>;
/// RGBA 图别名。
pub type RgbaImage = ImageBuffer<Rgba<u8>, Vec<u8>>;
/// 16 位灰度图别名。
pub type Gray16Image = ImageBuffer<Luma<u16>, Vec<u16>>;
/// 16 位灰度 + alpha 图别名。
pub type GrayAlpha16Image = ImageBuffer<LumaA<u16>, Vec<u16>>;
/// 16 位 RGB 图别名。
pub type Rgb16Image = ImageBuffer<Rgb<u16>, Vec<u16>>;
/// 16 位 RGBA 图别名。
pub type Rgba16Image = ImageBuffer<Rgba<u16>, Vec<u16>>;
/// 32 位浮点 RGB 图别名。
pub type Rgb32FImage = ImageBuffer<Rgb<f32>, Vec<f32>>;
/// 32 位浮点 RGBA 图别名。
pub type Rgba32FImage = ImageBuffer<Rgba<f32>, Vec<f32>>;

impl<P: Pixel, Container: Deref<Target = [P::Subpixel]>> ImageBuffer<P, Container> {
    /// 宽。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 高。
    pub fn height(&self) -> u32 {
        self.height
    }

    /// 尺寸元组。
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 用原始数据构造；长度与 `宽×高×通道数` 不符则返回 `None`。
    pub fn from_raw(width: u32, height: u32, buf: Container) -> Option<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(P::CHANNEL_COUNT as usize)?;
        if buf.len() != expected {
            return None;
        }
        Some(Self {
            width,
            height,
            data: buf,
            _phantom: PhantomData,
        })
    }

    /// 返回底层容器引用。
    pub fn as_raw(&self) -> &Container {
        &self.data
    }

    /// 返回底层容器可变引用。
    pub fn as_mut(&mut self) -> &mut Container {
        &mut self.data
    }

    /// 取出底层容器。
    pub fn into_raw(self) -> Container {
        self.data
    }

    /// 像素坐标 → 底层下标。
    #[inline]
    fn pixel_index_unchecked(&self, x: u32, y: u32) -> usize {
        ((y as usize) * (self.width as usize) + (x as usize)) * (P::CHANNEL_COUNT as usize)
    }

    /// 取像素只读引用。
    pub fn get_pixel(&self, x: u32, y: u32) -> &P {
        let index = self.pixel_index_unchecked(x, y);
        let ptr = unsafe { self.data.as_ptr().add(index) as *const P };
        unsafe { &*ptr }
    }
}

impl<P: Pixel, Container: DerefMut<Target = [P::Subpixel]>> ImageBuffer<P, Container> {
    /// 建零值图。
    pub fn new(width: u32, height: u32) -> Self
    where
        Container: From<Vec<P::Subpixel>>,
    {
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(P::CHANNEL_COUNT as usize))
            .unwrap();
        let mut data = Vec::new();
        data.try_reserve_exact(len).unwrap_or_else(|_| {});
        data.resize(len, P::Subpixel::default());
        Self {
            width,
            height,
            data: Container::from(data),
            _phantom: PhantomData,
        }
    }

    /// 建全 `pixel` 的图。
    pub fn from_pixel(width: u32, height: u32, pixel: P) -> Self
    where
        Container: From<Vec<P::Subpixel>>,
    {
        let mut img = Self::new(width, height);
        for p in img.pixels_mut() {
            *p = pixel;
        }
        img
    }

    /// 用生成函数建图。
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> P) -> Self
    where
        Container: From<Vec<P::Subpixel>>,
    {
        let mut img = Self::new(width, height);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = f(x, y);
        }
        img
    }

    /// 取像素可变引用（越界 panic）。
    pub fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut P {
        let index = self.pixel_index_unchecked(x, y);
        let ptr = unsafe { self.data.as_mut_ptr().add(index) as *mut P };
        unsafe { &mut *ptr }
    }

    /// 写像素（越界 panic）。
    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: P) {
        *self.get_pixel_mut(x, y) = pixel;
    }
}

impl<P: Pixel, Container: Deref<Target = [P::Subpixel]>> Deref for ImageBuffer<P, Container> {
    type Target = [P::Subpixel];
    fn deref(&self) -> &[P::Subpixel] {
        &self.data
    }
}

impl<P: Pixel, Container: DerefMut<Target = [P::Subpixel]>> DerefMut
    for ImageBuffer<P, Container>
{
    fn deref_mut(&mut self) -> &mut [P::Subpixel] {
        &mut self.data
    }
}

impl<P: Pixel, Container: Deref<Target = [P::Subpixel]>> GenericImageView
    for ImageBuffer<P, Container>
{
    type Pixel = P;

    fn dimensions(&self) -> (u32, u32) {
        ImageBuffer::dimensions(self)
    }

    fn get_pixel(&self, x: u32, y: u32) -> &P {
        ImageBuffer::get_pixel(self, x, y)
    }
}

impl<P: Pixel, Container: DerefMut<Target = [P::Subpixel]>> GenericImage
    for ImageBuffer<P, Container>
{
    fn put_pixel(&mut self, x: u32, y: u32, pixel: P) {
        ImageBuffer::put_pixel(self, x, y, pixel)
    }

    fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut P {
        ImageBuffer::get_pixel_mut(self, x, y)
    }
}

impl<'a, P: Pixel, Container: DerefMut<Target = [P::Subpixel]> + 'a> Iterator
    for PixelsMut<'a, ImageBuffer<P, Container>>
{
    type Item = &'a mut P;

    fn next(&mut self) -> Option<&'a mut P> {
        if self.x >= self.width {
            self.x = 0;
            self.y += 1;
        }
        if self.y >= self.height {
            return None;
        }
        let index = self.image.pixel_index_unchecked(self.x, self.y);
        self.x += 1;
        let data = self.image.as_mut();
        let ptr = unsafe { data.as_mut_ptr().add(index) as *mut P };
        unsafe { Some(&mut *ptr) }
    }
}

impl<'a, P: Pixel, Container: DerefMut<Target = [P::Subpixel]> + 'a> Iterator
    for EnumeratePixelsMut<'a, ImageBuffer<P, Container>>
{
    type Item = (u32, u32, &'a mut P);

    fn next(&mut self) -> Option<(u32, u32, &'a mut P)> {
        if self.x >= self.width {
            self.x = 0;
            self.y += 1;
        }
        if self.y >= self.height {
            return None;
        }
        let (x, y) = (self.x, self.y);
        let index = self.image.pixel_index_unchecked(x, y);
        self.x += 1;
        let data = self.image.as_mut();
        let ptr = unsafe { data.as_mut_ptr().add(index) as *mut P };
        unsafe { Some((x, y, &mut *ptr)) }
    }
}
