//! WebP 解码 SIMD 加速（对标 libwebp dsp SIMD；**字节精确**：每条向量路径都配标量回退，
//! 且向量结果与标量逐字节一致的断言见模块底部单测）。
//!
//! 开关：`KIMG_WEBP_NO_SIMD=1` 强制 WebP 全部 SIMD 走标量（量化收益对照，与 pngsimd 的
//! `KIMG_PNG_NO_SIMD` 同款姿势）；x86_64 下按运行时特性检测（SSSE3 / SSE4.1 / AVX2）选核，
//! 非 x86_64 一律标量回退。注释中文。
//!
//! 本模块实现 C 项：
//! - `yuv_row_to_rgba`：AVX2 每 16 像素一组（8×i32 lane ×2 段 interleave；同源算术
//!   `mullo_epi32`+`srai_epi32` 保持 `(x*c)>>8`、bias 减后再 `>>6` 严格同序，无重结合），
//!   否则 SSE4.1 每 8 像素一组；饱和 `packus` 即 `clamp(0,255)`。u16-mulhi 变体
//!   （vpmulhuw 系数+max-then-subtract，对标 C# `ConvertRowAvx2`）为 AVX2 默认生产路径，
//!   `KIMG_WEBP_YUV_U16=0` 回退 i32 定点方案（两方案逐字节一致）。
//! - `upsample_fancy_row`：SSE2 16-bit lane 算 fancy 上采样中间列对（`3*t[i]±邻居+8)>>4`），
//!   首/末列因边界算术特殊保留标量，尾列标量收尾。
//! - D1 `predict_subblock`/`predict_large`（SSSE3）：4×4 十模式 + 16×16/8×8 整块，经
//!   `KIMG_WEBP_NO_SIMD`/`KIMG_WEBP_NO_SIMD_PRED` 门控接入（实测内核 1.6×~30× 提速，启用）。
//! - D2 环路滤波：`filter_edge`（SSE2 per-edge）与 `lf_filter_macroblock`（SSE2 宏块熔合，
//!   U/V 打包、宏块一次全 12 核，C# `Vp8Decoder.FilterMacroblock` 同款；字节精确 vs 标量见
//!   模块底部断言）——**熔合版默认接入生产**（vp8.rs `filter_macroblock_row` 经
//!   `part_simd_on("flt","sse2")` 路由；`KIMG_WEBP_NO_SIMD_FLT=1` / `KIMG_WEBP_SIMD_FLT=0` 回退）。

use super::vp8;
use super::vp8l;

pub(crate) mod argb;
pub(crate) mod gating;
pub(crate) mod idct;
pub(crate) mod lf_edge;
pub(crate) mod loop_filter;
pub(crate) mod predict;
pub(crate) mod predictor;
pub(crate) mod raw;
pub(crate) mod upsample;
pub(crate) mod yuv;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_idct;

pub(crate) use self::argb::*;
pub(crate) use self::gating::*;
pub(crate) use self::idct::*;
// `lf_edge` 的 `lf_filter_macroblock` re-export 仅供对照/测试按名引用（生产已改经
// `raw::lf_filter_macroblock_safe` 直连）→ 抑制 glob unused_imports（同下方 loop_filter 姿态）。
#[allow(unused_imports)]
pub(crate) use self::lf_edge::*;
// 零符号丢失面：`loop_filter` 内核 re-export 仅供测试/对照按名引用（生产在 raw 内部直连），
// 非 test 构建整体未按名引用 → 抑制 glob unused_imports。
#[allow(unused_imports)]
pub(crate) use self::loop_filter::*;
pub(crate) use self::predict::*;
pub use self::predictor::rgb8_to_rgba8;
pub(crate) use self::predictor::*;
pub(crate) use self::upsample::*;
pub(crate) use self::yuv::*;
