//! JPEG codec：自研解码器 `JpegDecoder`（baseline+progressive，AVX2/标量双路径，
//! 与 zune-jpeg 0.5.15 逐字节恒等 oracle；zune 仅作 dev-dependency 测试对照）。

pub mod jpeg;
