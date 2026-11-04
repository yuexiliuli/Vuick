//! PNG 解码器与编码器。
//!
//! 解码热路径 = 自研引擎 `pngsimd`（SimdCodecs 移植：SIMD 反滤波 + EXPAND 展开，字节级对齐 png crate
//! `Transformations::EXPAND`）；编码也自研（`codecs::png_encoder`，fdeflate 压缩 + crc32fast 块 CRC），
//! `png` crate 仅保留给测试对照（dev-dependency），运行时零 png 依赖。
//! 对外 API / 语义与现有一致：`PngDecoder` 的 new/set_limits/dimensions/color_type/total_bytes/
//! read_image/read_image_with_progress/orientation 签名不变；`PngEncoder::write_image` 签名不变。

use std::io::{BufRead, Cursor, Seek, Write};

use crate::ImageDecoder;
use crate::codecs::pngsimd::PngEngine;
use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::ImageResult;
use kimg_core::limits::{LimitSupport, Limits};
use kimg_core::metadata::Orientation;

/// PNG 解码器：构造即读取头信息，输出颜色类型与 image 0.25 一致（EXPAND 变换、16bit 保留）。
///
/// 生命周期 `'a` 只绑定在「零拷贝借用输入」路径（[`Self::from_slice`]）：那时引擎直接借用
/// 调用方切片、不复制整份输入；读取器/文件路径（[`Self::new`] / [`Self::with_limits`]）把
/// 输入读入自有 `Input::Owned`，生命周期为 `'static`（可在任意 `'a` 处使用）。
pub struct PngDecoder<'a, R: BufRead + Seek> {
    color_type: ColorType,
    engine: PngEngine<'a>,
    limits: Limits,
    reader: std::marker::PhantomData<R>,
}

impl<'a, R: BufRead + Seek> PngDecoder<'a, R> {
    /// 无限制构造（读入自有输入）。
    pub fn new(r: R) -> ImageResult<PngDecoder<'static, R>> {
        Self::with_limits(r, Limits::no_limits())
    }

    /// 带限制构造（限制在 `read_image` 前校验尺寸；自研引擎在解析阶段已做防炸弹拦截）。
    pub fn with_limits(mut r: R, limits: Limits) -> ImageResult<PngDecoder<'static, R>> {
        use std::io::SeekFrom;
        limits.check_support(&LimitSupport::default())?;
        // 通过 Seek 探读出读取器剩余长度，给引擎预留容量——大输入（MB 级）避免 read_to_end
        // 幂次扩容的整体拷贝（Cursor<&[u8]> / image_viewer 内存字节路径白省一次拷贝时间）。
        let capacity_hint = match (r.stream_position(), r.seek(SeekFrom::End(0))) {
            (Ok(start), Ok(end)) => {
                let hint = end.saturating_sub(start);
                let _ = r.seek(SeekFrom::Start(start));
                usize::try_from(hint).unwrap_or(usize::MAX)
            }
            _ => 0,
        };
        let engine = PngEngine::parse_with_capacity(&mut r, capacity_hint)?;
        limits.check_dimensions(engine.width(), engine.height())?;
        let color_type = engine.color_type();
        Ok(PngDecoder {
            color_type,
            engine,
            limits,
            reader: std::marker::PhantomData,
        })
    }
}

impl<'a> PngDecoder<'a, Cursor<&'a [u8]>> {
    /// 零拷贝构造：直接借用内存切片解析（不做整份输入复制）。
    pub fn from_slice(bytes: &'a [u8]) -> ImageResult<Self> {
        Self::from_slice_with_limits(bytes, Limits::no_limits())
    }

    /// 零拷贝构造 + 初始限制。
    pub fn from_slice_with_limits(bytes: &'a [u8], limits: Limits) -> ImageResult<Self> {
        limits.check_support(&LimitSupport::default())?;
        let engine = PngEngine::from_slice(bytes)?;
        limits.check_dimensions(engine.width(), engine.height())?;
        let color_type = engine.color_type();
        Ok(PngDecoder {
            color_type,
            engine,
            limits,
            reader: std::marker::PhantomData,
        })
    }
}

impl PngDecoder<'static, Cursor<Vec<u8>>> {
    /// 自有缓冲构造（调用方已持有完整 PNG 字节；与 [`Self::from_slice`] 同语义、无借用）。
    pub fn from_vec(data: Vec<u8>) -> ImageResult<Self> {
        Self::from_vec_with_limits(data, Limits::no_limits())
    }

    /// 自有缓冲构造 + 初始限制。
    pub fn from_vec_with_limits(data: Vec<u8>, limits: Limits) -> ImageResult<Self> {
        limits.check_support(&LimitSupport::default())?;
        let engine = PngEngine::from_vec(data)?;
        limits.check_dimensions(engine.width(), engine.height())?;
        let color_type = engine.color_type();
        Ok(PngDecoder {
            color_type,
            engine,
            limits,
            reader: std::marker::PhantomData,
        })
    }
}

impl<'a, R: BufRead + Seek> ImageDecoder for PngDecoder<'a, R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.engine.width(), self.engine.height())
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.engine.exif())
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        Ok(self
            .exif_metadata()?
            .and_then(|chunk| Orientation::from_exif_chunk(&chunk))
            .unwrap_or(Orientation::NoTransforms))
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        self.engine.decode(buf, self.limits.max_alloc)?;
        // PNG 为大道在前；16bit 及以上需重排成本机字节序。
        let bpc = self.color_type.bytes_per_pixel() / self.color_type.channel_count();
        match bpc {
            1 => (),
            2 => buf.as_chunks_mut::<2>().0.iter_mut().for_each(|c| {
                *c = u16::from_be_bytes(*c).to_ne_bytes();
            }),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        limits.check_dimensions(self.engine.width(), self.engine.height())?;
        self.limits = limits;
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// PNG 编码器（供 `write_to(Png)` / `save_buffer_with_format(Png)` 使用；自研实现）。
pub struct PngEncoder<W: Write> {
    w: W,
}

impl<W: Write> PngEncoder<W> {
    /// 构造。
    pub fn new(w: W) -> PngEncoder<W> {
        PngEncoder { w }
    }

    /// 把指定颜色类型的缓冲编码为 PNG 写入 `w`（16bit 输入按本机字节序，内部重排为大端）。
    pub fn write_image(
        mut self,
        buf: &[u8],
        width: u32,
        height: u32,
        color: ExtendedColorType,
    ) -> ImageResult<()> {
        crate::codecs::png_encoder::encode_png(&mut self.w, buf, width, height, color)
    }
}
