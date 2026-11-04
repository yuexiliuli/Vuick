//! kimg_pnm：kimg 拆分的 PNM/PBM/PGM/PPM 解码 crate。
//!
//! 镜像 image 0.25 `codecs::pnm` 的仓库实际使用子集（PnmDecoder / PnmSubtype /
//! SampleEncoding），纯 std 实现。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::pnm::{PnmDecoder, PnmSubtype, SampleEncoding};
