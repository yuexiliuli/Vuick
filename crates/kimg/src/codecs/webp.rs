//! WebP 解码器（`image-webp` crate 0.2.4 封装；动画帧表示与 image 0.25 一致）。

use std::io::{BufRead, Seek};

use crate::animation::{Delay, Frame, Frames, LoopCount};
use crate::buffer::{RgbImage, RgbaImage};
use crate::pixel::Rgba;
use crate::color::ColorType;
use crate::error::{DecodingError, ImageError, ImageResult};
use crate::format::ImageFormat;
use crate::metadata::Orientation;
use crate::{AnimationDecoder, ImageDecoder};

/// WebP 解码器（支持有损/无损/带 alpha/动画）。
pub struct WebPDecoder<R> {
    inner: image_webp::WebPDecoder<R>,
    orientation: Option<Orientation>,
}

impl<R: BufRead + Seek> WebPDecoder<R> {
    /// 构造。
    pub fn new(r: R) -> ImageResult<Self> {
        Ok(Self {
            inner: image_webp::WebPDecoder::new(r).map_err(ImageError::from_kimg_webp)?,
            orientation: None,
        })
    }

    /// 是否为动画。
    pub fn has_animation(&self) -> bool {
        self.inner.is_animated()
    }

    /// 设置动画背景色。
    pub fn set_background_color(&mut self, color: Rgba<u8>) -> ImageResult<()> {
        self.inner
            .set_background_color(color.0)
            .map_err(ImageError::from_kimg_webp)
    }
}

impl<R: BufRead + Seek> ImageDecoder for WebPDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        self.inner.dimensions()
    }

    fn color_type(&self) -> ColorType {
        if self.inner.has_alpha() {
            ColorType::Rgba8
        } else {
            ColorType::Rgb8
        }
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let exif = self.inner.exif_metadata().map_err(ImageError::from_kimg_webp)?;
        self.orientation = Some(
            exif.as_ref()
                .and_then(|exif| Orientation::from_exif_chunk(exif))
                .unwrap_or(Orientation::NoTransforms),
        );
        Ok(exif)
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        if self.orientation.is_none() {
            let _ = self.exif_metadata()?;
        }
        Ok(self.orientation.unwrap())
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        self.inner.read_image(buf).map_err(ImageError::from_kimg_webp)
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

impl<'a, R: 'a + BufRead + Seek> AnimationDecoder<'a> for WebPDecoder<R> {
    fn loop_count(&self) -> LoopCount {
        match self.inner.loop_count() {
            image_webp::LoopCount::Forever => LoopCount::Infinite,
            image_webp::LoopCount::Times(n) => LoopCount::Finite(n.into()),
        }
    }

    fn into_frames(self) -> Frames<'a> {
        struct FramesInner<R: BufRead + Seek> {
            decoder: WebPDecoder<R>,
            current: u32,
        }
        impl<R: BufRead + Seek> Iterator for FramesInner<R> {
            type Item = ImageResult<Frame>;

            fn next(&mut self) -> Option<Self::Item> {
                if self.current == self.decoder.inner.num_frames() {
                    return None;
                }
                self.current += 1;
                let (width, height) = self.decoder.inner.dimensions();

                let (img, delay) = if self.decoder.inner.has_alpha() {
                    let mut img = RgbaImage::new(width, height);
                    match self.decoder.inner.read_frame(img.as_mut()) {
                        Ok(delay) => (img, delay),
                        Err(image_webp::DecodingError::NoMoreFrames) => return None,
                        Err(e) => return Some(Err(ImageError::from_kimg_webp(e))),
                    }
                } else {
                    let mut img = RgbImage::new(width, height);
                    match self.decoder.inner.read_frame(img.as_mut()) {
                        Ok(delay) => {
                            // RGB → RGBA。
                            let mut rgba = RgbaImage::new(width, height);
                            for (x, y, p) in img.enumerate_pixels() {
                                rgba.put_pixel(x, y, Rgba([p[0], p[1], p[2], 255]));
                            }
                            (rgba, delay)
                        }
                        Err(image_webp::DecodingError::NoMoreFrames) => return None,
                        Err(e) => return Some(Err(ImageError::from_kimg_webp(e))),
                    }
                };

                Some(Ok(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay, 1),
                )))
            }
        }

        Frames::new(Box::new(FramesInner {
            decoder: self,
            current: 0,
        }))
    }
}

impl ImageError {
    pub(crate) fn from_kimg_webp(e: image_webp::DecodingError) -> ImageError {
        match e {
            image_webp::DecodingError::IoError(e) => ImageError::IoError(e),
            _ => ImageError::Decoding(DecodingError::new(ImageFormat::WebP.into(), e)),
        }
    }
}
