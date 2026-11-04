//! kimg_bmp：kimg 拆分的 BMP 解码 crate。
//!
//! 镜像 image 0.25 `codecs::bmp` 的仓库实际使用子集（BmpDecoder），纯 std 实现。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::bmp::BmpDecoder;
