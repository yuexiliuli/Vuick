//! kimg_tiff：kimg 拆分的 TIFF 解码 crate。
//!
//! 镜像 image 0.25 `codecs::tiff` 的仓库实际使用子集（TiffDecoder），基于 tiff crate。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::tiff::TiffDecoder;
