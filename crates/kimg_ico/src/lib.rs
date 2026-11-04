//! kimg_ico：kimg 拆分的 ICO 解码 crate。
//!
//! 镜像 image 0.25 `codecs::ico` 的仓库实际使用子集（IcoDecoder）：容器解析后委托
//! kimg_bmp / kimg_png 解码内部位图。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::ico::IcoDecoder;
