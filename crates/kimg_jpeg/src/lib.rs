//! kimg_jpeg：kimg 拆分的 JPEG 解码 crate。
//!
//! 自研解析器/解码器（codecs/jpeg：baseline + progressive，AVX2/标量双路径，
//! 运行时零 zune-jpeg 依赖）；输出与 zune-jpeg 0.5.15（== image 0.25 自用后端）逐字节恒等，
//! zune 仅作 dev-dependency 测试/基准 oracle。

pub mod codecs;

pub use kimg_core::ImageDecoder;

pub use codecs::jpeg::JpegDecoder;
