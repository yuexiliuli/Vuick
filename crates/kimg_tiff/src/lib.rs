//! kimg_tiff：kimg 拆分的 TIFF 解码 crate。
//!
//! 镜像 image 0.25 `codecs::tiff` 的仓库实际使用子集（TiffDecoder）。A1 起为自研核心
//! （运行时零 tiff 依赖）；tiff crate 仅作 dev oracle 逐字节对拍（`tests/tiff_parity.rs`）。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::tiff::TiffDecoder;
