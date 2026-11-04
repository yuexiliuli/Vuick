//! kimg_gif：kimg 拆分的 GIF 解/编 crate。
//!
//! 镜像 image 0.25 `codecs::gif` 的仓库实际使用子集（GifDecoder / GifEncoder / Repeat），
//! 移植 SimdCodecs：容器+LZW+画布+编码器；运行时零 gif crate 依赖。

pub mod codecs;

pub use kimg_core::{AnimationDecoder, ImageDecoder};

pub use codecs::gif::{GifDecoder, GifEncoder, Repeat};
