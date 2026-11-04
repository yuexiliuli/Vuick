//! PNG codec：PngDecoder / PngEncoder + 自研 pngsimd（解码热路径）与 png_encoder。

pub mod png;
mod png_encoder;
mod pngsimd;
