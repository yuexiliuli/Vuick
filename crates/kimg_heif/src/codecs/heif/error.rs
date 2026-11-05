//! HEIF 解码错误类型（C# 侧 `InvalidDataException`/`NotSupportedException` 的 Rust 等价物）。

use std::fmt;

/// HEIF/HEVC 解码错误。
///
/// C# 侧凡「声名 `InvalidDataException`」的地方 → `HeifError::InvalidData(中文消息)`；
/// 「不支持」→ `HeifError::Unsupported`；「本阶段未实现」→ `HeifError::NotImplemented`。
/// 消息保持中文，与 C# 中文异常文案一一对应，便于对拍。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeifError {
    /// 输入数据非法/畸形（对应 C# `InvalidDataException`）。
    InvalidData(String),
    /// 语法/配置受支持范围之外（对应 C# `NotSupportedException` 语义，如 tile 非 4:2:0）。
    Unsupported(String),
    /// 本阶段尚未实现的路径（decode 桩）。
    NotImplemented(String),
    /// 底层 IO / 越界等环境错误。
    Io(String),
}

impl HeifError {
    /// 快捷构造：畸形输入。
    pub fn invalid(msg: impl Into<String>) -> Self {
        HeifError::InvalidData(msg.into())
    }

    /// 快捷构造：不支持。
    pub fn unsupported(msg: impl Into<String>) -> Self {
        HeifError::Unsupported(msg.into())
    }

    /// 快捷构造：未实现。
    pub fn not_implemented(msg: impl Into<String>) -> Self {
        HeifError::NotImplemented(msg.into())
    }
}

impl fmt::Display for HeifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeifError::InvalidData(m) => write!(f, "无效的 HEIF/HEVC 数据: {m}"),
            HeifError::Unsupported(m) => write!(f, "不支持的 HEIF/HEVC 配置: {m}"),
            HeifError::NotImplemented(m) => write!(f, "HEIF 解码尚未实现: {m}"),
            HeifError::Io(m) => write!(f, "HEIF IO 错误: {m}"),
        }
    }
}

impl std::error::Error for HeifError {}

impl From<core::convert::Infallible> for HeifError {
    fn from(x: core::convert::Infallible) -> Self {
        match x {}
    }
}

/// HEIF 解码结果别名。
pub type HeifResult<T> = Result<T, HeifError>;
