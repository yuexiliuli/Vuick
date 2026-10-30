//! 错误类型（镜像 image 0.25 的 `ImageError` 层级：Decoding/Encoding/Parameter/Limits/Unsupported/IoError）。

use std::error::Error;
use std::fmt;
use std::io;

use crate::color::ExtendedColorType;
use crate::ImageFormat;

/// 图像操作通用错误。
#[derive(Debug)]
pub enum ImageError {
    /// 解码失败（输入数据不符合格式规范，或格式无法确定）。
    Decoding(DecodingError),
    /// 编码失败。
    Encoding(EncodingError),
    /// 输入参数错误（内部操作的通用兜底）。
    Parameter(ParameterError),
    /// 超过资源限制。
    Limits(LimitError),
    /// 抽象无法完成操作（特性未启用/未实现/底层缺失）。
    Unsupported(UnsupportedError),
    /// 与环境的 IO 交互出错。
    IoError(io::Error),
}

/// 图像格式的尽力表示。
#[derive(Clone, Debug, Hash, PartialEq)]
#[non_exhaustive]
pub enum ImageFormatHint {
    /// 精确已知的格式。
    Exact(ImageFormat),
    /// 以名称标识的格式。
    Name(String),
    /// 常见路径扩展名。
    PathExtension(std::path::PathBuf),
    /// 未知。
    Unknown,
}

/// 不支持的细节。
#[derive(Clone, Debug, Hash, PartialEq)]
#[non_exhaustive]
pub enum UnsupportedErrorKind {
    /// 不支持的颜色类型。
    Color(ExtendedColorType),
    /// 不支持的纹素内存布局。
    ColorLayout(ExtendedColorType),
    /// 不支持的图像格式。
    Format(ImageFormatHint),
    /// 以字符串描述的特性（不推荐、可能弃用）。
    GenericFeature(String),
}

/// 解码错误（对底层解码错误的不透明包装）。
#[derive(Debug)]
pub struct DecodingError {
    format: ImageFormatHint,
    underlying: Option<Box<dyn Error + Send + Sync>>,
}

impl DecodingError {
    /// 由底层解码器的任意错误构造。
    pub fn new(format: ImageFormatHint, err: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        DecodingError {
            format,
            underlying: Some(err.into()),
        }
    }

    /// 构造无底层细节的解码错误。
    pub fn from_format_hint(format: ImageFormatHint) -> Self {
        DecodingError {
            format,
            underlying: None,
        }
    }

    /// 关联的图像格式。
    pub fn format_hint(&self) -> ImageFormatHint {
        self.format.clone()
    }
}

/// 编码错误。
#[derive(Debug)]
pub struct EncodingError {
    format: ImageFormatHint,
    underlying: Option<Box<dyn Error + Send + Sync>>,
}

impl EncodingError {
    /// 由底层编码器的任意错误构造。
    pub fn new(format: ImageFormatHint, err: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        EncodingError {
            format,
            underlying: Some(err.into()),
        }
    }

    /// 构造无底层细节的编码错误。
    pub fn from_format_hint(format: ImageFormatHint) -> Self {
        EncodingError {
            format,
            underlying: None,
        }
    }

    /// 关联的图像格式。
    pub fn format_hint(&self) -> ImageFormatHint {
        self.format.clone()
    }
}

/// 参数错误。
#[derive(Debug)]
pub struct ParameterError {
    kind: ParameterErrorKind,
    underlying: Option<Box<dyn Error + Send + Sync>>,
}

/// 参数错误的细节。
#[derive(Clone, Debug, Hash, PartialEq)]
#[non_exhaustive]
pub enum ParameterErrorKind {
    /// 尺寸错误。
    DimensionMismatch,
    /// 图像流已在之前出错终止。
    FailedAlready,
    /// 字符串描述的参数错误（不推荐）。
    Generic(String),
    /// 已到达图像结尾。
    NoMoreData,
}

/// 资源限制错误。
#[derive(Debug)]
pub struct LimitError {
    kind: LimitErrorKind,
}

/// 限制类型。
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitErrorKind {
    /// 超过尺寸限制。
    DimensionError,
    /// 超过分配上限。
    InsufficientMemory,
    /// 指定了不支持的限制。
    Unsupported { limits: crate::Limits, supported: crate::LimitSupport },
}

/// 不支持的实现细节。
#[derive(Debug)]
pub struct UnsupportedError {
    format: ImageFormatHint,
    kind: UnsupportedErrorKind,
}

impl UnsupportedError {
    /// 为某个图像格式与不支持的细节构造错误。
    pub fn from_format_and_kind(format: ImageFormatHint, kind: UnsupportedErrorKind) -> Self {
        UnsupportedError { format, kind }
    }

    /// 返回细节种类。
    pub fn kind(&self) -> UnsupportedErrorKind {
        self.kind.clone()
    }

    /// 返回关联的格式提示。
    pub fn format_hint(&self) -> ImageFormatHint {
        self.format.clone()
    }
}

impl ParameterError {
    /// 由某种类直接构造。
    pub fn from_kind(kind: ParameterErrorKind) -> Self {
        ParameterError {
            kind,
            underlying: None,
        }
    }

    /// 细节种类。
    pub fn kind(&self) -> ParameterErrorKind {
        self.kind.clone()
    }
}

impl LimitError {
    /// 由某种类直接构造。
    pub fn from_kind(kind: LimitErrorKind) -> Self {
        LimitError { kind }
    }

    /// 细节种类。
    pub fn kind(&self) -> LimitErrorKind {
        self.kind.clone()
    }
}

impl From<LimitErrorKind> for LimitError {
    fn from(kind: LimitErrorKind) -> Self {
        Self { kind }
    }
}

impl From<io::Error> for ImageError {
    fn from(err: io::Error) -> ImageError {
        ImageError::IoError(err)
    }
}

impl From<std::collections::TryReserveError> for ImageError {
    fn from(_: std::collections::TryReserveError) -> ImageError {
        ImageError::Limits(LimitErrorKind::InsufficientMemory.into())
    }
}

impl From<ImageFormat> for ImageFormatHint {
    fn from(format: ImageFormat) -> Self {
        ImageFormatHint::Exact(format)
    }
}

impl From<&'_ std::path::Path> for ImageFormatHint {
    fn from(path: &'_ std::path::Path) -> Self {
        match path.extension() {
            Some(ext) => ImageFormatHint::PathExtension(ext.into()),
            None => ImageFormatHint::Unknown,
        }
    }
}

impl From<ImageFormatHint> for UnsupportedError {
    fn from(hint: ImageFormatHint) -> Self {
        UnsupportedError {
            format: hint.clone(),
            kind: UnsupportedErrorKind::Format(hint),
        }
    }
}

/// 图像处理结果别名。
pub type ImageResult<T> = Result<T, ImageError>;

impl fmt::Display for ImageError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::IoError(err) => err.fmt(fmt),
            ImageError::Decoding(err) => err.fmt(fmt),
            ImageError::Encoding(err) => err.fmt(fmt),
            ImageError::Parameter(err) => err.fmt(fmt),
            ImageError::Limits(err) => err.fmt(fmt),
            ImageError::Unsupported(err) => err.fmt(fmt),
        }
    }
}

impl Error for ImageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ImageError::IoError(err) => err.source(),
            ImageError::Decoding(err) => err.source(),
            ImageError::Encoding(err) => err.source(),
            ImageError::Parameter(err) => err.source(),
            ImageError::Limits(err) => err.source(),
            ImageError::Unsupported(err) => err.source(),
        }
    }
}

impl fmt::Display for UnsupportedError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            UnsupportedErrorKind::Format(ImageFormatHint::Unknown) => {
                write!(fmt, "The image format could not be determined")
            }
            UnsupportedErrorKind::Format(format @ ImageFormatHint::PathExtension(_)) => write!(
                fmt,
                "The file extension {format} was not recognized as an image format",
            ),
            UnsupportedErrorKind::Format(format) => {
                write!(fmt, "The image format {format} is not supported")
            }
            UnsupportedErrorKind::Color(color) => write!(
                fmt,
                "The encoder or decoder for {} does not support the color type `{:?}`",
                self.format, color,
            ),
            UnsupportedErrorKind::ColorLayout(layout) => write!(
                fmt,
                "Converting with the texel memory layout {layout:?} is not supported",
            ),
            UnsupportedErrorKind::GenericFeature(message) => match &self.format {
                ImageFormatHint::Unknown => {
                    write!(fmt, "The decoder does not support the format feature {message}")
                }
                other => write!(
                    fmt,
                    "The decoder for {other} does not support the format features {message}"
                ),
            },
        }
    }
}

impl Error for UnsupportedError {}

impl fmt::Display for ParameterError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParameterErrorKind::DimensionMismatch => write!(
                fmt,
                "The Image's dimensions are either too small or too large"
            ),
            ParameterErrorKind::FailedAlready => write!(
                fmt,
                "The end the image stream has been reached due to a previous error"
            ),
            ParameterErrorKind::Generic(message) => {
                write!(fmt, "The parameter is malformed: {message}")
            }
            ParameterErrorKind::NoMoreData => write!(fmt, "The end of the image has been reached"),
        }?;
        if let Some(underlying) = &self.underlying {
            write!(fmt, "\n{underlying}")?;
        }
        Ok(())
    }
}

impl Error for ParameterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.underlying {
            None => None,
            Some(source) => Some(&**source),
        }
    }
}

impl fmt::Display for EncodingError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.underlying {
            Some(underlying) => write!(fmt, "Format error encoding {}:\n{}", self.format, underlying),
            None => write!(fmt, "Format error encoding {}", self.format),
        }
    }
}

impl Error for EncodingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.underlying {
            None => None,
            Some(source) => Some(&**source),
        }
    }
}

impl fmt::Display for DecodingError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.underlying {
            None => match self.format {
                ImageFormatHint::Unknown => write!(fmt, "Format error"),
                _ => write!(fmt, "Format error decoding {}", self.format),
            },
            Some(underlying) => {
                write!(fmt, "Format error decoding {}: {}", self.format, underlying)
            }
        }
    }
}

impl Error for DecodingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.underlying {
            None => None,
            Some(source) => Some(&**source),
        }
    }
}

impl fmt::Display for LimitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            LimitErrorKind::InsufficientMemory => write!(fmt, "Memory limit exceeded"),
            LimitErrorKind::DimensionError => write!(fmt, "Image size exceeds limit"),
            LimitErrorKind::Unsupported { .. } => {
                write!(
                    fmt,
                    "The following strict limits are specified but not supported by the operation: "
                )?;
                Ok(())
            }
        }
    }
}

impl Error for LimitError {}

impl fmt::Display for ImageFormatHint {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageFormatHint::Exact(format) => write!(fmt, "{format:?}"),
            ImageFormatHint::Name(name) => write!(fmt, "`{name}`"),
            ImageFormatHint::PathExtension(ext) => write!(fmt, "`.{ext:?}`"),
            ImageFormatHint::Unknown => write!(fmt, "`Unknown`"),
        }
    }
}
