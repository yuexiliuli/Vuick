//! 内部解码错误（在 `mod.rs` 中映射为 `kimg_core::ImageError`）。

use std::fmt;

/// 自研 JPEG 解码器内部错误。
///
/// 语义与 zune-jpeg 的 `DecodeErrors` 对齐（宽分类），便于在 `mod.rs`
/// 参照旧 `from_kimg_jpeg` 做错误映射。
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// 通用格式错误。
    Format(String),
    /// Huffman 表或解码错误。
    HuffmanDecode(String),
    /// DQT 段错误。
    DqtError(String),
    /// SOS 段错误。
    SosError(String),
    /// SOF 段错误。
    SofError(String),
    /// 不支持的特性（progressive/算术/lossless/12-bit 等）。
    Unsupported(String),
    /// 熵数据提前耗尽。
    ExhaustedData,
    /// I/O（缓冲越界/长度不足等）。
    Io(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Format(s) => write!(f, "{s:?}"),
            DecodeError::HuffmanDecode(s) => write!(f, "Error decoding huffman values: {s}"),
            DecodeError::DqtError(s) => write!(f, "Error parsing DQT segment. Reason:{s}"),
            DecodeError::SosError(s) => write!(f, "Error parsing SOS Segment. Reason:{s}"),
            DecodeError::SofError(s) => write!(f, "Error parsing SOF segment. Reason:{s}"),
            DecodeError::Unsupported(s) => write!(f, "{s}"),
            DecodeError::ExhaustedData => write!(f, "Exhausted data in the image"),
            DecodeError::Io(s) => write!(f, "I/O error {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}
