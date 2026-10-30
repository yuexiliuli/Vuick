//! GIF 解码器与编码器（`gif` crate 0.14 封装；帧合成语义与 image 0.25 一致）。

use std::io::{BufRead, Read, Seek, Write};

use gif::{ColorOutput, DecodeOptions, DisposalMethod};

use crate::animation::{Delay, Frame, Frames, LoopCount, Ratio};
use crate::buffer::ImageBuffer;
use crate::pixel::Rgba;
use crate::color::ColorType;
use crate::error::{
    DecodingError, EncodingError, ImageError, ImageResult, ParameterError, ParameterErrorKind,
    UnsupportedError, UnsupportedErrorKind,
};
use crate::format::ImageFormat;
use crate::limits::{LimitSupport, Limits};
use crate::metadata::Orientation;
use crate::traits::Pixel;
use crate::{AnimationDecoder, ImageDecoder};

/// GIF 解码器。
pub struct GifDecoder<R: Read> {
    reader: gif::Decoder<R>,
    limits: Limits,
}

impl<R: Read> GifDecoder<R> {
    /// 构造（输出 RGBA）。
    pub fn new(r: R) -> ImageResult<GifDecoder<R>> {
        let mut decoder = DecodeOptions::new();
        decoder.set_color_output(ColorOutput::RGBA);

        Ok(GifDecoder {
            reader: decoder.read_info(r).map_err(ImageError::from_kimg_gif)?,
            limits: Limits::no_limits(),
        })
    }
}

impl<R: Read> ImageDecoder for GifDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (u32::from(self.reader.width()), u32::from(self.reader.height()))
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        let (width, height) = self.dimensions();
        limits.check_dimensions(width, height)?;
        self.limits = limits;
        Ok(())
    }

    /// 静态读帧：与 image 0.25 相同——合成到逻辑屏画布，输出统一尺寸 RGBA。
    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));

        let frame = match self.reader.next_frame_info().map_err(ImageError::from_kimg_gif)? {
            Some(frame) => FrameInfo::from_gif_frame(frame),
            None => {
                return Err(ImageError::Parameter(ParameterError::from_kind(
                    ParameterErrorKind::NoMoreData,
                )))
            }
        };

        let (width, height) = self.dimensions();

        if frame.left == 0
            && frame.width == width
            && u64::from(frame.top) + u64::from(frame.height) <= u64::from(height)
        {
            let line_length = (width as usize) * (self.color_type().bytes_per_pixel() as usize);
            let (blank_top, rest) = buf.split_at_mut(line_length * frame.top as usize);
            let (buf, blank_bottom) =
                rest.split_at_mut(line_length * frame.height as usize);

            for b in blank_top {
                *b = 0;
            }
            self.reader
                .read_into_buffer(buf)
                .map_err(ImageError::from_kimg_gif)?;
            for b in blank_bottom {
                *b = 0;
            }
        } else {
            let buffer_size = (frame.width as usize)
                .checked_mul(frame.height as usize)
                .and_then(|s| s.checked_mul(4))
                .ok_or(ImageError::Limits(
                    crate::error::LimitError::from_kind(
                        crate::error::LimitErrorKind::InsufficientMemory,
                    ),
                ))?;

            self.limits.reserve_usize(buffer_size)?;
            let mut frame_buffer = vec![0; buffer_size];
            self.limits.free_usize(buffer_size);

            self.reader
                .read_into_buffer(&mut frame_buffer)
                .map_err(ImageError::from_kimg_gif)?;

            let frame_buf = ImageBuffer::<Rgba<u8>, _>::from_raw(frame.width, frame.height, frame_buffer);
            let image_buf = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, buf);

            if frame_buf.is_none() || image_buf.is_none() {
                return Err(ImageError::Unsupported(
                    UnsupportedError::from_format_and_kind(
                        ImageFormat::Gif.into(),
                        UnsupportedErrorKind::GenericFeature(format!(
                            "Image dimensions ({}, {}) are too large",
                            frame.width, frame.height
                        )),
                    ),
                ));
            }

            let frame_buf = frame_buf.unwrap();
            let mut image_buf = image_buf.unwrap();

            for (x, y, pixel) in image_buf.enumerate_pixels_mut() {
                let frame_x = x.wrapping_sub(frame.left);
                let frame_y = y.wrapping_sub(frame.top);
                if frame_x < frame.width && frame_y < frame.height {
                    *pixel = *frame_buf.get_pixel(frame_x, frame_y);
                } else {
                    *pixel = Rgba([0, 0, 0, 0]);
                }
            }
        }

        Ok(())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

struct GifFrameIterator<R: Read> {
    reader: gif::Decoder<R>,
    width: u32,
    height: u32,
    non_disposed_frame: Option<ImageBuffer<Rgba<u8>, Vec<u8>>>,
    limits: Limits,
    is_end: bool,
}

impl<R: BufRead + Seek> GifFrameIterator<R> {
    fn new(decoder: GifDecoder<R>) -> GifFrameIterator<R> {
        let (width, height) = decoder.dimensions();
        let limits = decoder.limits.clone();
        // 忽略背景色以获得 Web 兼容行为（与 image 0.25 一致）。
        GifFrameIterator {
            reader: decoder.reader,
            width,
            height,
            non_disposed_frame: None,
            limits,
            is_end: false,
        }
    }
}

impl<R: Read> Iterator for GifFrameIterator<R> {
    type Item = ImageResult<Frame>;

    fn next(&mut self) -> Option<ImageResult<Frame>> {
        if self.is_end {
            return None;
        }

        const COLOR_TYPE: ColorType = ColorType::Rgba8;

        if self.non_disposed_frame.is_none() {
            if let Err(e) = self.limits.reserve_buffer(self.width, self.height, COLOR_TYPE) {
                return Some(Err(e));
            }
            self.non_disposed_frame = Some(ImageBuffer::from_pixel(
                self.width,
                self.height,
                Rgba([0, 0, 0, 0]),
            ));
        }
        let non_disposed_frame = self.non_disposed_frame.as_mut().unwrap();

        let frame = match self.reader.next_frame_info() {
            Ok(Some(frame)) => FrameInfo::from_gif_frame(frame),
            Ok(None) => return None,
            Err(err) => {
                if matches!(&err, gif::DecodingError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof)
                {
                    self.is_end = true;
                }
                return Some(Err(ImageError::from_kimg_gif(err)));
            }
        };

        let mut local_limits = self.limits.clone();
        if let Err(e) = local_limits.reserve_buffer(frame.width, frame.height, COLOR_TYPE) {
            return Some(Err(e));
        }
        let mut vec = vec![0; self.reader.buffer_size()];
        if let Err(err) = self.reader.read_into_buffer(&mut vec) {
            return Some(Err(ImageError::from_kimg_gif(err)));
        }

        let Some(mut frame_buffer) =
            ImageBuffer::<Rgba<u8>, _>::from_raw(frame.width, frame.height, vec)
        else {
            return Some(Err(ImageError::Unsupported(
                UnsupportedError::from_format_and_kind(
                    ImageFormat::Gif.into(),
                    UnsupportedErrorKind::GenericFeature(format!(
                        "Image dimensions ({}, {}) are too large",
                        frame.width, frame.height
                    )),
                ),
            )));
        };

        fn blend_and_dispose_pixel(
            dispose: DisposalMethod,
            previous: &mut Rgba<u8>,
            current: &mut Rgba<u8>,
        ) {
            if current.channels()[3] == 0 {
                *current = *previous;
            }
            match dispose {
                DisposalMethod::Any | DisposalMethod::Keep => {
                    *previous = *current;
                }
                DisposalMethod::Background => {
                    *previous = Rgba([0, 0, 0, 0]);
                }
                DisposalMethod::Previous => {}
            }
        }

        let image_buffer = if (frame.left, frame.top) == (0, 0)
            && (self.width, self.height) == frame_buffer.dimensions()
        {
            for (x, y, pixel) in frame_buffer.enumerate_pixels_mut() {
                let previous_pixel = non_disposed_frame.get_pixel_mut(x, y);
                blend_and_dispose_pixel(frame.disposal_method, previous_pixel, pixel);
            }
            frame_buffer
        } else {
            if let Err(e) = local_limits.reserve_buffer(self.width, self.height, COLOR_TYPE) {
                return Some(Err(e));
            }
            ImageBuffer::from_fn(self.width, self.height, |x, y| {
                let frame_x = x.wrapping_sub(frame.left);
                let frame_y = y.wrapping_sub(frame.top);
                let previous_pixel = non_disposed_frame.get_pixel_mut(x, y);

                if frame_x < frame_buffer.width() && frame_y < frame_buffer.height() {
                    let mut pixel = *frame_buffer.get_pixel(frame_x, frame_y);
                    blend_and_dispose_pixel(frame.disposal_method, previous_pixel, &mut pixel);
                    pixel
                } else {
                    *previous_pixel
                }
            })
        };

        Some(Ok(Frame::from_parts(
            image_buffer,
            0,
            0,
            frame.delay,
        )))
    }
}

impl<'a, R: BufRead + Seek + 'a> AnimationDecoder<'a> for GifDecoder<R> {
    fn loop_count(&self) -> LoopCount {
        match self.reader.repeat() {
            gif::Repeat::Finite(n @ 1..) => LoopCount::Finite(
                std::num::NonZeroU32::new(n.into()).expect("repeat is non-zero"),
            ),
            gif::Repeat::Finite(0) | gif::Repeat::Infinite => LoopCount::Infinite,
        }
    }

    fn into_frames(self) -> Frames<'a> {
        Frames::new(Box::new(GifFrameIterator::new(self)))
    }
}

struct FrameInfo {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    disposal_method: DisposalMethod,
    delay: Delay,
}

impl FrameInfo {
    fn from_gif_frame(frame: &gif::Frame) -> FrameInfo {
        FrameInfo {
            left: u32::from(frame.left),
            top: u32::from(frame.top),
            width: u32::from(frame.width),
            height: u32::from(frame.height),
            disposal_method: frame.dispose,
            // gif 帧延迟单位为 10ms。
            delay: Delay::from_ratio(Ratio::new(u32::from(frame.delay) * 10, 1)),
        }
    }
}

/// GIF 循环次数（公开 API 兼容面）。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum Repeat {
    /// 有限次数。
    Finite(u16),
    /// 无限循环。
    Infinite,
}

impl Repeat {
    fn to_gif_enum(self) -> gif::Repeat {
        match self {
            Repeat::Finite(n) => gif::Repeat::Finite(n),
            Repeat::Infinite => gif::Repeat::Infinite,
        }
    }
}

/// GIF 编码器。
pub struct GifEncoder<W: Write> {
    w: Option<W>,
    gif_encoder: Option<gif::Encoder<W>>,
    speed: i32,
    repeat: Option<Repeat>,
}

impl<W: Write> GifEncoder<W> {
    /// 以速度 1（质量优先）构造。
    pub fn new(w: W) -> GifEncoder<W> {
        Self::new_with_speed(w, 1)
    }

    /// 指定速度构造（1..=30）。
    pub fn new_with_speed(w: W, speed: i32) -> GifEncoder<W> {
        assert!((1..=30).contains(&speed), "speed needs to be in the range [1, 30]");
        GifEncoder {
            w: Some(w),
            gif_encoder: None,
            speed,
            repeat: None,
        }
    }

    /// 设置循环次数（公开 API 兼容面）。
    #[allow(dead_code)]
    pub fn set_repeat(&mut self, repeat: Repeat) -> ImageResult<()> {
        if let Some(encoder) = &mut self.gif_encoder {
            encoder.set_repeat(repeat.to_gif_enum()).map_err(ImageError::from_kimg_gif_enc)?;
        }
        self.repeat = Some(repeat);
        Ok(())
    }

    /// 编码一帧动画。
    pub fn encode_frame(&mut self, img_frame: Frame) -> ImageResult<()> {
        let frame = self.convert_frame(img_frame)?;
        self.encode_gif(frame)
    }

    /// 编码多帧动画（公开 API 兼容面）。
    #[allow(dead_code)]
    pub fn encode_frames<F>(&mut self, frames: F) -> ImageResult<()>
    where
        F: IntoIterator<Item = Frame>,
    {
        for img_frame in frames {
            self.encode_frame(img_frame)?;
        }
        Ok(())
    }

    fn convert_frame(&mut self, img_frame: Frame) -> ImageResult<gif::Frame<'static>> {
        let frame_delay = img_frame.delay().into_ratio().to_integer();
        let mut rgba_frame = img_frame.into_buffer();
        let (width, height) = self.gif_dimensions(rgba_frame.width(), rgba_frame.height())?;

        let mut frame =
            gif::Frame::from_rgba_speed(width, height, &mut *rgba_frame.as_mut(), self.speed);
        frame.delay = (frame_delay / 10).try_into().unwrap_or(u16::MAX);
        Ok(frame)
    }

    fn gif_dimensions(&self, width: u32, height: u32) -> ImageResult<(u16, u16)> {
        let width = u16::try_from(width).ok();
        let height = u16::try_from(height).ok();
        match (width, height) {
            (Some(w), Some(h)) => Ok((w, h)),
            _ => Err(ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::DimensionMismatch,
            ))),
        }
    }

    fn encode_gif(&mut self, mut frame: gif::Frame<'static>) -> ImageResult<()> {
        let gif_encoder;
        if let Some(ref mut encoder) = self.gif_encoder {
            gif_encoder = encoder;
        } else {
            let writer = self.w.take().unwrap();
            let mut encoder = gif::Encoder::new(writer, frame.width, frame.height, &[])
                .map_err(ImageError::from_kimg_gif_enc)?;
            if let Some(repeat) = self.repeat {
                encoder
                    .set_repeat(repeat.to_gif_enum())
                    .map_err(ImageError::from_kimg_gif_enc)?;
            }
            self.gif_encoder = Some(encoder);
            gif_encoder = self.gif_encoder.as_mut().unwrap();
        }

        frame.dispose = DisposalMethod::Background;

        gif_encoder
            .write_frame(&frame)
            .map_err(ImageError::from_kimg_gif_enc)
    }
}

impl ImageError {
    pub(crate) fn from_kimg_gif(err: gif::DecodingError) -> ImageError {
        match err {
            gif::DecodingError::Io(io_err) => ImageError::IoError(io_err),
            other => ImageError::Decoding(DecodingError::new(ImageFormat::Gif.into(), other)),
        }
    }

    pub(crate) fn from_kimg_gif_enc(err: gif::EncodingError) -> ImageError {
        match err {
            gif::EncodingError::Io(io_err) => ImageError::IoError(io_err),
            other => ImageError::Encoding(EncodingError::new(ImageFormat::Gif.into(), other)),
        }
    }
}
