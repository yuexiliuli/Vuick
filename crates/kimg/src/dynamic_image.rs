//! `DynamicImage`：多格式动态图像（镜像 image 0.25 的枚举、转换与方向应用）。

use std::io::{Seek, Write};

use crate::buffer::{
    Gray16Image, GrayAlpha16Image, GrayAlphaImage, GrayImage, ImageBuffer, Rgb16Image, RgbImage,
    Rgba16Image, Rgba32FImage, Rgb32FImage, RgbaImage,
};
use crate::codecs::gif::GifEncoder;
use crate::codecs::png;
use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult};
use crate::format::ImageFormat;
use crate::metadata::Orientation;
use crate::pixel::{LumaA, Rgb};
use crate::traits::Pixel;
use crate::ImageDecoder;

/// 动态图像：一个规范化通道序的像素矩阵枚举。
#[derive(Debug, PartialEq, Clone)]
#[non_exhaustive]
pub enum DynamicImage {
    /// 8 位灰度。
    ImageLuma8(GrayImage),
    /// 8 位灰度 + alpha。
    ImageLumaA8(GrayAlphaImage),
    /// 8 位 RGB。
    ImageRgb8(RgbImage),
    /// 8 位 RGBA。
    ImageRgba8(RgbaImage),
    /// 16 位灰度。
    ImageLuma16(Gray16Image),
    /// 16 位灰度 + alpha。
    ImageLumaA16(GrayAlpha16Image),
    /// 16 位 RGB。
    ImageRgb16(Rgb16Image),
    /// 16 位 RGBA。
    ImageRgba16(Rgba16Image),
    /// 32 位浮点 RGB。
    ImageRgb32F(Rgb32FImage),
    /// 32 位浮点 RGBA。
    ImageRgba32F(Rgba32FImage),
}

/// 逐变体运行同一表达式；`p` 绑定各变体内层 `ImageBuffer` 的引用。
/// 用法：`dynamic_map!(self, p, <expr using p>)`（`self` 为 `&DynamicImage`，`p: &ImageBuffer<_>`）。
macro_rules! dynamic_map {
    ($dynimage:expr, $p:ident, $action:expr) => {{
        use DynamicImage::*;
        match $dynimage {
            ImageLuma8($p) => $action,
            ImageLumaA8($p) => $action,
            ImageRgb8($p) => $action,
            ImageRgba8($p) => $action,
            ImageLuma16($p) => $action,
            ImageLumaA16($p) => $action,
            ImageRgb16($p) => $action,
            ImageRgba16($p) => $action,
            ImageRgb32F($p) => $action,
            ImageRgba32F($p) => $action,
        }
    }};
}

impl DynamicImage {
    /// 按颜色类型新建空图。
    pub fn new(w: u32, h: u32, color: ColorType) -> DynamicImage {
        use ColorType::*;
        match color {
            L8 => Self::ImageLuma8(ImageBuffer::new(w, h)),
            La8 => Self::ImageLumaA8(ImageBuffer::new(w, h)),
            Rgb8 => Self::ImageRgb8(ImageBuffer::new(w, h)),
            Rgba8 => Self::ImageRgba8(ImageBuffer::new(w, h)),
            L16 => Self::ImageLuma16(ImageBuffer::new(w, h)),
            La16 => Self::ImageLumaA16(ImageBuffer::new(w, h)),
            Rgb16 => Self::ImageRgb16(ImageBuffer::new(w, h)),
            Rgba16 => Self::ImageRgba16(ImageBuffer::new(w, h)),
            Rgb32F => Self::ImageRgb32F(ImageBuffer::new(w, h)),
            Rgba32F => Self::ImageRgba32F(ImageBuffer::new(w, h)),
        }
    }

    /// 从解码器整图解码为动态图像。
    pub fn from_decoder(decoder: impl ImageDecoder) -> ImageResult<Self> {
        let (w, h) = decoder.dimensions();
        let color = decoder.color_type();
        let total = usize::try_from(decoder.total_bytes()).map_err(|_| {
            ImageError::Limits(crate::error::LimitError::from_kind(
                crate::error::LimitErrorKind::InsufficientMemory,
            ))
        })?;
        let mut buf = vec![0u8; total];
        decoder.read_image(&mut buf)?;

        match color {
            ColorType::L8 => Ok(DynamicImage::ImageLuma8(
                ImageBuffer::from_raw(w, h, buf).expect("buffer size checked"),
            )),
            ColorType::La8 => Ok(DynamicImage::ImageLumaA8(
                ImageBuffer::from_raw(w, h, buf).expect("buffer size checked"),
            )),
            ColorType::Rgb8 => Ok(DynamicImage::ImageRgb8(
                ImageBuffer::from_raw(w, h, buf).expect("buffer size checked"),
            )),
            ColorType::Rgba8 => Ok(DynamicImage::ImageRgba8(
                ImageBuffer::from_raw(w, h, buf).expect("buffer size checked"),
            )),
            ColorType::L16 => Ok(DynamicImage::ImageLuma16(
                ImageBuffer::from_raw(w, h, u16_vec(&buf)).expect("buffer size checked"),
            )),
            ColorType::La16 => Ok(DynamicImage::ImageLumaA16(
                ImageBuffer::from_raw(w, h, u16_vec(&buf)).expect("buffer size checked"),
            )),
            ColorType::Rgb16 => Ok(DynamicImage::ImageRgb16(
                ImageBuffer::from_raw(w, h, u16_vec(&buf)).expect("buffer size checked"),
            )),
            ColorType::Rgba16 => Ok(DynamicImage::ImageRgba16(
                ImageBuffer::from_raw(w, h, u16_vec(&buf)).expect("buffer size checked"),
            )),
            ColorType::Rgb32F => Ok(DynamicImage::ImageRgb32F(
                ImageBuffer::from_raw(w, h, f32_vec(&buf)).expect("buffer size checked"),
            )),
            ColorType::Rgba32F => Ok(DynamicImage::ImageRgba32F(
                ImageBuffer::from_raw(w, h, f32_vec(&buf)).expect("buffer size checked"),
            )),
        }
    }

    /// 颜色类型。
    pub fn color(&self) -> ColorType {
        match self {
            DynamicImage::ImageLuma8(_) => ColorType::L8,
            DynamicImage::ImageLumaA8(_) => ColorType::La8,
            DynamicImage::ImageRgb8(_) => ColorType::Rgb8,
            DynamicImage::ImageRgba8(_) => ColorType::Rgba8,
            DynamicImage::ImageLuma16(_) => ColorType::L16,
            DynamicImage::ImageLumaA16(_) => ColorType::La16,
            DynamicImage::ImageRgb16(_) => ColorType::Rgb16,
            DynamicImage::ImageRgba16(_) => ColorType::Rgba16,
            DynamicImage::ImageRgb32F(_) => ColorType::Rgb32F,
            DynamicImage::ImageRgba32F(_) => ColorType::Rgba32F,
        }
    }

    /// 宽。
    pub fn width(&self) -> u32 {
        dynamic_map!(self, p, p.width())
    }

    /// 高。
    pub fn height(&self) -> u32 {
        dynamic_map!(self, p, p.height())
    }

    /// 尺寸。
    pub fn dimensions(&self) -> (u32, u32) {
        dynamic_map!(self, p, p.dimensions())
    }

    /// 拷贝为 RGB8。
    pub fn to_rgb8(&self) -> RgbImage {
        match self {
            DynamicImage::ImageRgb8(x) => x.clone(),
            x => {
                let (w, h) = x.dimensions();
                let mut out = RgbImage::new(w, h);
                dynamic_map!(x, p, {
                    for (px, out_px) in p.pixels().zip(out.pixels_mut()) {
                        let c = px.to_rgba();
                        *out_px = Rgb([c[0], c[1], c[2]]);
                    }
                });
                out
            }
        }
    }

    /// 拷贝为 RGBA8。
    pub fn to_rgba8(&self) -> RgbaImage {
        match self {
            DynamicImage::ImageRgba8(x) => x.clone(),
            x => {
                let (w, h) = x.dimensions();
                let mut out = RgbaImage::new(w, h);
                dynamic_map!(x, p, {
                    for (px, out_px) in p.pixels().zip(out.pixels_mut()) {
                        *out_px = px.to_rgba();
                    }
                });
                out
            }
        }
    }

    /// 拷贝为 Luma8。
    pub fn to_luma8(&self) -> GrayImage {
        match self {
            DynamicImage::ImageLuma8(x) => x.clone(),
            x => {
                let (w, h) = x.dimensions();
                let mut out = GrayImage::new(w, h);
                dynamic_map!(x, p, {
                    for (px, out_px) in p.pixels().zip(out.pixels_mut()) {
                        *out_px = px.to_luma();
                    }
                });
                out
            }
        }
    }

    /// 拷贝为 LumaA8。
    pub fn to_luma_alpha8(&self) -> GrayAlphaImage {
        match self {
            DynamicImage::ImageLumaA8(x) => x.clone(),
            x => {
                let (w, h) = x.dimensions();
                let mut out = GrayAlphaImage::new(w, h);
                dynamic_map!(x, p, {
                    for (px, out_px) in p.pixels().zip(out.pixels_mut()) {
                        let c = px.to_rgba();
                        *out_px = LumaA([c[0], c[3]]);
                    }
                });
                out
            }
        }
    }

    /// 消费为 RGB8。
    pub fn into_rgb8(self) -> RgbImage {
        match self {
            DynamicImage::ImageRgb8(x) => x,
            x => x.to_rgb8(),
        }
    }

    /// 消费为 RGBA8。
    pub fn into_rgba8(self) -> RgbaImage {
        match self {
            DynamicImage::ImageRgba8(x) => x,
            x => x.to_rgba8(),
        }
    }

    /// 消费为 Luma8。
    pub fn into_luma8(self) -> GrayImage {
        match self {
            DynamicImage::ImageLuma8(x) => x,
            x => x.to_luma8(),
        }
    }

    /// 应用 EXIF 方向。
    pub fn apply_orientation(&mut self, orientation: Orientation) {
        match orientation {
            Orientation::NoTransforms => {}
            Orientation::Rotate90 => {
                let rotated = self.rotate90();
                *self = rotated;
            }
            Orientation::Rotate180 => self.rotate180_in_place(),
            Orientation::Rotate270 => {
                let rotated = self.rotate270();
                *self = rotated;
            }
            Orientation::FlipHorizontal => self.fliph_in_place(),
            Orientation::FlipVertical => self.flipv_in_place(),
            Orientation::Rotate90FlipH => {
                let mut new_image = self.rotate90();
                new_image.fliph_in_place();
                *self = new_image;
            }
            Orientation::Rotate270FlipH => {
                let mut new_image = self.rotate270();
                new_image.fliph_in_place();
                *self = new_image;
            }
        }
    }

    /// 顺时针旋转 90°。
    pub fn rotate90(&self) -> DynamicImage {
        dynamic_map!(self, p, DynamicImage::from(rotate90_image(p)))
    }

    /// 顺时针旋转 270°。
    pub fn rotate270(&self) -> DynamicImage {
        dynamic_map!(self, p, DynamicImage::from(rotate270_image(p)))
    }

    /// 原地旋转 180°（= 水平 + 垂直翻转）。
    pub fn rotate180_in_place(&mut self) {
        self.fliph_in_place();
        self.flipv_in_place();
    }

    /// 原地水平翻转。
    pub fn fliph_in_place(&mut self) {
        let image = self;
        let w = image.width();
        dynamic_map!(image, p, {
            for y in 0..p.height() {
                for x in 0..(w / 2) {
                    let a = *p.get_pixel(x, y);
                    let b = *p.get_pixel(w - 1 - x, y);
                    p.put_pixel(x, y, b);
                    p.put_pixel(w - 1 - x, y, a);
                }
            }
        });
    }

    /// 原地垂直翻转。
    pub fn flipv_in_place(&mut self) {
        let image = self;
        let h = image.height();
        dynamic_map!(image, p, {
            for y in 0..(h / 2) {
                for x in 0..p.width() {
                    let a = *p.get_pixel(x, y);
                    let b = *p.get_pixel(x, h - 1 - y);
                    p.put_pixel(x, y, b);
                    p.put_pixel(x, h - 1 - y, a);
                }
            }
        });
    }

    /// 取出底层字节（按通道直排，与 `color()` 对应）。
    pub fn as_bytes(&self) -> &[u8] {
        dynamic_map!(self, p, bytemuck_view(p.as_raw()))
    }

    /// 编码写入 writer。
    pub fn write_to<W: Write + Seek>(&self, mut w: W, format: ImageFormat) -> ImageResult<()> {
        match format {
            ImageFormat::Png => png::encode_dynamic_png(self, &mut w),
            ImageFormat::Gif => {
                let (data, width, height, color) = self.buffer_and_color();
                if color != ExtendedColorType::Rgba8 {
                    return Err(ImageError::Unsupported(
                        crate::error::UnsupportedError::from_format_and_kind(
                            ImageFormat::Gif.into(),
                            crate::error::UnsupportedErrorKind::Color(color),
                        ),
                    ));
                }
                let frame = crate::animation::Frame::new(
                    ImageBuffer::from_raw(width, height, data.to_vec()).ok_or_else(|| {
                        ImageError::Decoding(crate::error::DecodingError::from_format_hint(
                            ImageFormat::Gif.into(),
                        ))
                    })?,
                );
                let mut enc = GifEncoder::new(&mut w);
                enc.encode_frame(frame)
            }
            other => Err(ImageError::Unsupported(
                crate::error::UnsupportedError::from_format_and_kind(
                    crate::error::ImageFormatHint::Exact(other),
                    crate::error::UnsupportedErrorKind::Format(
                        crate::error::ImageFormatHint::Exact(other),
                    ),
                ),
            )),
        }
    }

    /// 供编码器取用的「字节 + 尺寸 + 颜色类型」表示。
    pub(crate) fn buffer_and_color(&self) -> (&[u8], u32, u32, ExtendedColorType) {
        let (w, h) = self.dimensions();
        let color: ExtendedColorType = self.color().into();
        dynamic_map!(self, p, (bytemuck_view(p.as_raw()), w, h, color))
    }
}

/// 把 `Vec<u8>` 重解释为 `Vec<u16>`（16bit 解码产物）。
fn u16_vec(buf: &[u8]) -> Vec<u16> {
    buf.chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect()
}

/// 把 `Vec<u8>` 重解释为 `Vec<f32>`。
fn f32_vec(buf: &[u8]) -> Vec<f32> {
    buf.chunks_exact(4)
        .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 把 `&[T]` 当作 `&[u8]` 查看（T 为 POD 标量时重解释安全）。
fn bytemuck_view<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn rotate90_image<P: Pixel, C>(img: &ImageBuffer<P, C>) -> ImageBuffer<P, Vec<P::Subpixel>>
where
    C: std::ops::Deref<Target = [P::Subpixel]>,
{
    let (w, h) = img.dimensions();
    ImageBuffer::from_fn(h, w, |x, y| *img.get_pixel(y, w - 1 - x))
}

fn rotate270_image<P: Pixel, C>(img: &ImageBuffer<P, C>) -> ImageBuffer<P, Vec<P::Subpixel>>
where
    C: std::ops::Deref<Target = [P::Subpixel]>,
{
    let (w, h) = img.dimensions();
    ImageBuffer::from_fn(h, w, |x, y| *img.get_pixel(h - 1 - y, x))
}

impl From<GrayImage> for DynamicImage {
    fn from(image: GrayImage) -> Self {
        DynamicImage::ImageLuma8(image)
    }
}
impl From<GrayAlphaImage> for DynamicImage {
    fn from(image: GrayAlphaImage) -> Self {
        DynamicImage::ImageLumaA8(image)
    }
}
impl From<RgbImage> for DynamicImage {
    fn from(image: RgbImage) -> Self {
        DynamicImage::ImageRgb8(image)
    }
}
impl From<RgbaImage> for DynamicImage {
    fn from(image: RgbaImage) -> Self {
        DynamicImage::ImageRgba8(image)
    }
}
impl From<Gray16Image> for DynamicImage {
    fn from(image: Gray16Image) -> Self {
        DynamicImage::ImageLuma16(image)
    }
}
impl From<GrayAlpha16Image> for DynamicImage {
    fn from(image: GrayAlpha16Image) -> Self {
        DynamicImage::ImageLumaA16(image)
    }
}
impl From<Rgb16Image> for DynamicImage {
    fn from(image: Rgb16Image) -> Self {
        DynamicImage::ImageRgb16(image)
    }
}
impl From<Rgba16Image> for DynamicImage {
    fn from(image: Rgba16Image) -> Self {
        DynamicImage::ImageRgba16(image)
    }
}
impl From<Rgb32FImage> for DynamicImage {
    fn from(image: Rgb32FImage) -> Self {
        DynamicImage::ImageRgb32F(image)
    }
}
impl From<Rgba32FImage> for DynamicImage {
    fn from(image: Rgba32FImage) -> Self {
        DynamicImage::ImageRgba32F(image)
    }
}
