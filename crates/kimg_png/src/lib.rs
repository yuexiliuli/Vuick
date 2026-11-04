//! kimg_png：kimg 拆分的 PNG 编解码 crate。
//!
//! 镜像 image 0.25 `codecs::png` 的仓库实际使用子集（PngDecoder / PngEncoder），核心为
//! 自研 pngsimd（移植 SimdCodecs）解码热路径 + png_encoder 自研编码器；运行时零 png crate 依赖。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::png::{PngDecoder, PngEncoder};
