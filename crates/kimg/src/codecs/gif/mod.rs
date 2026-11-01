//! GIF 解码器/编码器（仓内自研，替换 `gif` crate 0.14.2 后端）。
//!
//! 模块划分：
//! - `container`：容器解析（签名/逻辑屏/色表/GCE/NETSCAPE/数据子块）。
//! - `lzw`：GIF89a Appendix F LZW 解/编（表跨帧复用 + u64 位累加器）。
//! - `canvas`：帧合成画布（LUT 直写 + 持久画布 + disposal 链）。
//! - `encoder`：流式编码器（局部色表 + Wu 量化）。
//!
//! 公开 API 与旧实现（`gif` crate 封装）完全兼容：`GifDecoder<R: Read>::new`、
//! `ImageDecoder`、`AnimationDecoder`（loop_count/into_frames）、`GifEncoder<W: Write>`
//! （new/new_with_speed/set_repeat/encode_frame/encode_frames）、`Repeat`。
//! 帧合成语义（含 EOF 报错）与旧路径一致，解码输出与 `gif` crate 逐字节一致（见 tests/gif_oracle.rs）。

use std::io::{BufRead, Read, Seek};
use std::marker::PhantomData;

use crate::animation::{Delay, Frame, Frames, LoopCount, Ratio};
use crate::color::ColorType;
use crate::error::{
    ImageError, ImageResult, ParameterError, ParameterErrorKind, UnsupportedError,
    UnsupportedErrorKind,
};
use crate::format::ImageFormat;
use crate::limits::Limits;
use crate::metadata::Orientation;
use crate::{AnimationDecoder, ImageDecoder};

mod canvas;
mod container;
mod encoder;
mod lzw;

pub use self::encoder::GifEncoder;

/// GIF 解码器。
pub struct GifDecoder<R: Read> {
    data: Vec<u8>,
    container: container::GifContainer,
    scratch: canvas::Scratch,
    limits: Limits,
    _r: PhantomData<R>,
}

impl<R: Read> GifDecoder<R> {
    /// 构造（输出 RGBA）。整份读入输入（供画布按偏移合成，跨帧复用 LZW 表）。
    pub fn new(mut r: R) -> ImageResult<GifDecoder<R>> {
        let mut data = Vec::new();
        r.read_to_end(&mut data).map_err(ImageError::IoError)?;
        let container = container::GifContainer::parse(&data)?;
        Ok(GifDecoder {
            data,
            container,
            scratch: canvas::Scratch::new(),
            limits: Limits::no_limits(),
            _r: PhantomData,
        })
    }
}

impl<R: Read> ImageDecoder for GifDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.container.width, self.container.height)
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&crate::limits::LimitSupport::default())?;
        let (width, height) = self.dimensions();
        limits.check_dimensions(width, height)?;
        self.limits = limits;
        Ok(())
    }

    /// 静态读帧：合成首帧到逻辑屏画布，输出统一尺寸 RGBA（与旧路径一致）。
    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(
            u64::try_from(buf.len()),
            Ok(self.total_bytes()),
            "读取缓冲长度须等于 total_bytes"
        );

        let Some(frame) = self.container.frames.first() else {
            return Err(ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::NoMoreData,
            )));
        };
        let (w, h) = (self.container.width as usize, self.container.height as usize);
        canvas::composite_first_into(&mut self.scratch, &self.data, frame, w, h, buf)
            .map_err(ImageError::IoError)
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(Orientation::NoTransforms)
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// 动画帧迭代器（单遍顺序合成：持久画布逐帧推进）。
struct GifFrameIterator {
    data: Vec<u8>,
    container: container::GifContainer,
    scratch: canvas::Scratch,
    width: usize,
    height: usize,
    canvas: Option<canvas::Canvas>,
    next: usize,
    limits: Limits,
    is_end: bool,
}

impl GifFrameIterator {
    fn new<R: Read>(decoder: GifDecoder<R>) -> GifFrameIterator {
        let width = decoder.container.width as usize;
        let height = decoder.container.height as usize;
        GifFrameIterator {
            data: decoder.data,
            container: decoder.container,
            scratch: decoder.scratch,
            width,
            height,
            canvas: None,
            next: 0,
            limits: decoder.limits,
            is_end: false,
        }
    }
}

impl Iterator for GifFrameIterator {
    type Item = ImageResult<Frame>;

    fn next(&mut self) -> Option<ImageResult<Frame>> {
        if self.is_end {
            return None;
        }
        const COLOR_TYPE: ColorType = ColorType::Rgba8;
        if self.canvas.is_none() {
            if let Err(e) = self.limits.reserve_buffer(
                self.width as u32,
                self.height as u32,
                COLOR_TYPE,
            ) {
                self.is_end = true;
                return Some(Err(e));
            }
            self.canvas = Some(canvas::Canvas::new(self.width, self.height));
        }
        let Some(frame) = self.container.frames.get(self.next) else {
            // 帧流末尾：文件被截断（缺尾块）→ 报 UnexpectedEof（对齐旧路径遇 EOF 记 end 并报错）。
            if self.container.clean_end {
                return None;
            }
            self.is_end = true;
            return Some(Err(ImageError::IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "GIF 尾部被截断（缺尾块）",
            ))));
        };

        let mut local_limits = self.limits.clone();
        if let Err(e) = local_limits.reserve_buffer(frame.w, frame.h, COLOR_TYPE) {
            return Some(Err(e));
        }

        let mut buf = vec![0u8; self.width * self.height * 4];
        let composite = canvas::composite_into(
            self.canvas.as_mut().unwrap(),
            &mut self.scratch,
            &self.data,
            frame,
            &mut buf,
        );
        if let Err(e) = composite {
            // 数据截断（UnexpectedEof）：记 end 并报错，不画 partial（对齐旧路径）。
            self.is_end = true;
            return Some(Err(ImageError::from_kimg_gif(e)));
        }

        let delay = Delay::from_ratio(Ratio::new(
            u32::from(frame.delay_centiseconds) * 10,
            1,
        ));
        let Some(image_buffer) = crate::buffer::RgbaImage::from_raw(
            self.width as u32,
            self.height as u32,
            buf,
        ) else {
            return Some(Err(ImageError::Unsupported(
                UnsupportedError::from_format_and_kind(
                    ImageFormat::Gif.into(),
                    UnsupportedErrorKind::GenericFeature(format!(
                        "Image dimensions ({}, {}) are too large",
                        self.width, self.height
                    )),
                ),
            )));
        };
        self.next += 1;
        Some(Ok(Frame::from_parts(image_buffer, 0, 0, delay)))
    }
}

impl<'a, R: BufRead + Seek + 'a> AnimationDecoder<'a> for GifDecoder<R> {
    fn loop_count(&self) -> LoopCount {
        // 直传存储值不加 1（对齐旧路径）：无扩展或存储 0 = Infinite；n≥1 = Finite(n)。
        match self.container.loop_count {
            None | Some(0) => LoopCount::Infinite,
            Some(n) => LoopCount::Finite(
                std::num::NonZeroU32::new(n.into()).expect("loop count 非零"),
            ),
        }
    }

    fn into_frames(self) -> Frames<'a> {
        Frames::new(Box::new(GifFrameIterator::new(self)))
    }
}

/// GIF 循环次数（公开 API 兼容面；与 `gif` crate `Repeat` 同构：`Finite(n)` 指存储值）。
#[derive(Clone, Copy, Debug)]
pub enum Repeat {
    /// 有限次数。
    Finite(u16),
    /// 无限循环。
    Infinite,
}

impl ImageError {
    /// 解码侧 io 错误映射（GIF 数据截断等 → `IoError`）。
    pub(crate) fn from_kimg_gif(err: std::io::Error) -> ImageError {
        ImageError::IoError(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 栈足迹回归护栏（2026-11-02）：自研 GIF 的 `LzwDecoder`（4 条 4096 定长表 ≈48KB）
    /// 曾按值嵌入 `Scratch`，把 `GenericDecoder` 枚举撑到 ~49KB——启动首帧欢迎页经
    /// `ImageReader::decode()` 在 1MB 主线程栈构造该枚举即爆栈（STATUS_STACK_OVERFLOW）。
    /// 装箱后 `GifDecoder` 须保持百字节级，防止按值回退再次把枚举撑大。
    #[test]
    fn gif_decoder_stack_footprint_is_small() {
        let size = std::mem::size_of::<GifDecoder<std::io::Cursor<&'static [u8]>>>();
        assert!(
            size < 1024,
            "GifDecoder 应为堆分配小对象，实际 {size}B（防 48KB LzwDecoder 按值回退）"
        );
    }
}
