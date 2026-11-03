//! `ImageReader`：多格式读取器（镜像 image 0.25 的 Reader 语义；`Reader` 为弃用别名）。

use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;

use crate::ImageDecoder;
use crate::buffer::ImageBuffer;
use crate::codecs::GenericDecoder;
use crate::dynamic_image::DynamicImage;
use crate::error::{
    ImageError, ImageFormatHint, ImageResult, UnsupportedError, UnsupportedErrorKind,
};
use crate::format::{self, ImageFormat};
use crate::limits::Limits;

/// 多格式图像读取器。包装一个输入流，自动识别格式并分发解码器。
pub struct ImageReader<R: Read + Seek> {
    inner: R,
    format: Option<ImageFormat>,
    limits: Limits,
}

/// `Reader` 的命名迁移别名。
#[deprecated(note = "此类型已迁移并改名为 kimg::ImageReader")]
pub type Reader<R> = ImageReader<R>;

impl<'a, R: 'a + BufRead + Seek> ImageReader<R> {
    /// 无预设格式构造（假定输入已缓冲）。
    pub fn new(buffered_reader: R) -> Self {
        ImageReader {
            inner: buffered_reader,
            format: None,
            limits: Limits::default(),
        }
    }

    /// 带指定格式构造。
    pub fn with_format(buffered_reader: R, format: ImageFormat) -> Self {
        ImageReader {
            inner: buffered_reader,
            format: Some(format),
            limits: Limits::default(),
        }
    }

    /// 当前确定的格式。
    pub fn format(&self) -> Option<ImageFormat> {
        self.format
    }

    /// 指定解释格式。
    pub fn set_format(&mut self, format: ImageFormat) {
        self.format = Some(format);
    }

    /// 清空格式信息。
    pub fn clear_format(&mut self) {
        self.format = None;
    }

    /// 禁用全部解码限制。
    pub fn no_limits(&mut self) {
        self.limits = Limits::no_limits();
    }

    /// 设置自定义解码限制。
    pub fn limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// 取出底层读取器。
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// 转换成解码器。
    pub fn into_decoder(mut self) -> ImageResult<GenericDecoder<'a, R>> {
        let format = self.require_format()?;
        let mut decoder = make_decoder(format, self.inner)?;
        decoder.set_limits(self.limits)?;
        Ok(decoder)
    }

    /// 按内容猜测格式并替换当前格式（成功时）。
    pub fn with_guessed_format(mut self) -> std::io::Result<Self> {
        let format = guess_format_from_reader(&mut self.inner)?;
        self.format = format.or(self.format);
        Ok(self)
    }

    /// 只读取尺寸。
    pub fn into_dimensions(self) -> ImageResult<(u32, u32)> {
        self.into_decoder().map(|d| d.dimensions())
    }

    /// 解码为动态图像。
    pub fn decode(mut self) -> ImageResult<DynamicImage> {
        let format = self.require_format()?;
        let mut limits = self.limits.clone();
        if format == ImageFormat::WebP {
            // VP8L 静态族「生产直写」出口：读全量字节 → 容器预检（可接管时拿尺寸）→
            // 以 `decode_static_lossless_rgba8_into` 直写 RGBA8 到唯一缓冲，`into_rgba8`
            // 即可零拷贝移交调用方（消「内部解一次 + read_image 4→3 + to_rgba8 3→4」
            // 两次整帧冗余写；to_rgba8 公共语义不变）。不可接管（VP8 有损 / 动画 / 畸形）
            // 回退既有解码路径，错误口径与现状一致。
            let mut bytes = Vec::new();
            self.inner.read_to_end(&mut bytes).map_err(ImageError::IoError)?;
            match crate::codecs::webp::lossless_static_size(&bytes) {
                Some((w, h, total_bytes)) => {
                    // 与既有解码路径严格限制等价：先查严格尺寸（`set_limits` 的
                    // `check_dimensions` 口径），再按与现状一致的体积扣 `max_alloc`。
                    limits.check_dimensions(w, h)?;
                    limits.reserve(total_bytes as u64)?;
                    // 未初始化缓冲（免整帧清零）：直写核 Ok 前全量覆写 RGBA8（与 vp8l
                    // `decode_into_rgba8` 的全覆写不变式一致）；Err 时缓冲被丢弃回退。
                    let mut rgba = crate::codecs::webp::uninit_rgba8_vec((w as usize) * (h as usize) * 4);
                    match crate::codecs::webp::decode_static_lossless_rgba8_into(&bytes, &mut rgba) {
                        Ok(true) => {
                            return Ok(DynamicImage::ImageRgba8(
                                ImageBuffer::from_raw(w, h, rgba).expect("缓冲尺寸已校验"),
                            ))
                        }
                        // 解码失败（畸形 VP8L 载荷等）→ 归还预留并回退常规路径，保证显式
                        // 错误与现状一致（回退路径会重新 `reserve` 等价体积）。
                        _ => limits.free(total_bytes as u64),
                    }
                }
                None => {}
            }
            let mut decoder = make_decoder(format, Cursor::new(bytes))?;
            limits.reserve(decoder.total_bytes())?;
            decoder.set_limits(limits)?;
            return DynamicImage::from_decoder(decoder);
        }
        let mut decoder = make_decoder(format, self.inner)?;
        limits.reserve(decoder.total_bytes())?;
        decoder.set_limits(limits)?;
        DynamicImage::from_decoder(decoder)
    }

    fn require_format(&mut self) -> ImageResult<ImageFormat> {
        self.format.ok_or_else(|| {
            ImageError::Unsupported(UnsupportedError::from_format_and_kind(
                ImageFormatHint::Unknown,
                UnsupportedErrorKind::Format(ImageFormatHint::Unknown),
            ))
        })
    }
}

impl ImageReader<BufReader<File>> {
    /// 打开文件（格式按扩展名预判，需要内容猜测时再链式调用 `with_guessed_format`）。
    pub fn open<P>(path: P) -> std::io::Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let format = path
            .extension()
            .filter(|ext| !ext.is_empty())
            .and_then(ImageFormat::from_extension);
        Ok(ImageReader {
            inner: BufReader::new(File::open(path)?),
            format,
            limits: Limits::default(),
        })
    }
}

/// 按格式构造解码器（通用读取器路径：自有输入，无借用 → 'static）。
fn make_decoder<R: BufRead + Seek>(
    format: ImageFormat,
    reader: R,
) -> ImageResult<GenericDecoder<'static, R>> {
    GenericDecoder::new(format, reader)
}

/// 从读取器读出前 16 字节做魔数猜测（会恢复游标）。
fn guess_format_from_reader<R: BufRead + Seek>(
    reader: &mut R,
) -> std::io::Result<Option<ImageFormat>> {
    use std::io::{Cursor, SeekFrom};
    let mut start = [0u8; 16];
    let cur = reader.stream_position()?;
    let len = std::io::copy(
        &mut reader.by_ref().take(16),
        &mut Cursor::new(&mut start[..]),
    )?;
    reader.seek(SeekFrom::Start(cur))?;
    Ok(format::guess_format_impl(&start[..len as usize]))
}
