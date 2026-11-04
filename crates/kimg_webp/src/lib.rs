//! kimg_webp：kimg 拆分的 WebP 解/编 crate。
//!
//! 镜像 image 0.25 `codecs::webp` 的仓库实际使用子集（WebPDecoder / set_webp_backend /
//! BACKEND_AUTO / BACKEND_OURS / 静态有损无损快道），自研容器+VP8L+VP8+ALPH+动画+SIMD；
//! 运行时零 image-webp crate 依赖。

pub mod codecs;

pub use kimg_core::{AnimationDecoder, ColorType, ImageDecoder, LoopCount};

pub use codecs::webp::{
    set_webp_backend, decode_static_lossless_rgba8_into, decode_static_lossy_rgba8_into,
    rgb8_to_rgba8, BACKEND_AUTO, BACKEND_OURS, WebPDecoder,
};
#[cfg(feature = "stage_profile")]
pub use codecs::webp::{
    vp8_extract_frame, vp8_probe_clock_costs, vp8_probe_entropy_op_costs,
    vp8_probe_i4_op_costs, vp8_stage_decode, vp8l_extract_bitstream, vp8l_stage_decode,
    I4OpCosts, Vp8StageProfile, Vp8lStageProfile,
};
