//! kimg_jpeg：kimg 拆分的 JPEG 解码 crate。
//!
//! 镜像 image 0.25 `codecs::jpeg` 的仓库实际使用子集（JpegDecoder），基于 zune-jpeg
//! （== image 0.25 自身所用后端，字节级一致）。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::jpeg::JpegDecoder;
