//! VP8 有损静态解码器（标量正确版）：RFC 6386 keyframe 全链路（帧内预测 / 4x4 IDCT + WHT /
//! 解量化 / 环路滤波 / YUV→RGBA fancy）。
//!
//! 移植蓝本：SimdCodecs `Vp8BoolDecoder.cs`/`Vp8Decoder.cs`/`Vp8LoopFilter.cs`/`Vp8Tables.cs`
//! 与 `WebpDecoder.cs` 有损路径的标量实现（MIT），已由 SimdCodecs 测试集对 dwebp golden
//! 逐字节验证；本模块保持其算术/舍入/顺序语义完全一致（任务主判：与 image-webp 逐字节一致）。
//!
//! 阶段划分（对应派单 V1..V5）：
//! - V1：bool 解码器 + 静态表 + 帧头/分区解析（本文件首部 + 单测）。
//! - V2：宏块重建（帧内预测 + IDCT/WHT + 解量化 + 写回 Y/U/V 平面）。
//! - V3：环路滤波（Simple + Normal）。
//! - V4：YUV→RGBA（fancy 上采样 + 定点转换，供 `webp::mod` 集成）。
//! - V5：`webp_oracle` 侧对拍收口（tests 层）。
//!
//! 输入安全（K42）：所有尺寸乘法 `checked`、分区/表越界在入口校验后抛结构化 `Vp8Error`；
//! 畸形输入不 panic 不 OOM（上层按「不可接管 → 回 image-webp 兜底」处理）。

use std::fmt;

use super::container::MAX_PIXELS;

// ---------------------------------------------------------------------------
// 错误与解码结果
// ---------------------------------------------------------------------------

/// VP8 解码错误（结构化，供上层映射为「降级 image-webp」或解析失败）。
#[derive(Debug, Clone)]
pub(crate) struct Vp8Error(pub(crate) String);

impl fmt::Display for Vp8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Vp8Error {}

impl Vp8Error {
    fn new<S: Into<String>>(msg: S) -> Self {
        Vp8Error(msg.into())
    }
}

type Result<T> = std::result::Result<T, Vp8Error>;

use super::simd;

#[macro_use]
pub(crate) mod bpdec;
#[macro_use]
pub(crate) mod tables;
pub(crate) mod yuv;
pub(crate) mod header;
pub(crate) mod predict;
pub(crate) mod mbdec;
pub(crate) mod pipeline;
pub(crate) mod filter;
pub(crate) mod emit;
pub(crate) mod subbuffers;
pub(crate) use self::subbuffers::Vp8SubBuffersGuard;
pub(crate) mod decode;
pub(crate) mod stage;
#[cfg(test)]
mod v1_tests;
#[cfg(test)]
mod v2_tests;
#[cfg(test)]
mod v3_yuv_golden_tests;
#[cfg(test)]
mod v4_rgba_parity_tests;
#[cfg(test)]
mod mt_tests;
#[cfg(test)]
mod subbuffers_tests;
#[cfg(test)]
mod perf_stage_tests;
#[cfg(test)]
mod v7_fuzz_tests;

pub(crate) use self::bpdec::*;
pub(crate) use self::decode::*;
pub(crate) use self::emit::*;
pub(crate) use self::filter::*;
pub(crate) use self::header::*;
pub(crate) use self::mbdec::*;
#[cfg(feature = "stage_profile")]
pub use self::mbdec::Vp8StageProfile;
pub(crate) use self::pipeline::*;
pub(crate) use self::predict::*;
#[cfg(feature = "stage_profile")]
pub use self::stage::*;
pub(crate) use self::tables::*;
pub(crate) use self::yuv::*;
// ---------------------------------------------------------------------------
// 静态表
// ---------------------------------------------------------------------------

// 静态表随 vp8_tables.rs 整体引入；COEFF_BANDS / COEFFICIENT_TREE / Y_MODE_TREE / PCAT1/2 为
// 规范参考数据（当前实现内联化了树遍历与 cat1/2，定义了带状偏移与树读），保留供对照不删。
#[allow(dead_code)]
mod vp8_tables {
    include!("vp8_tables.rs");
}

use vp8_tables::*;
// 树路由静态表 pub(crate) 再导出（供 `simd::raw::mbdec` 迁入的内核/宏经
// `crate::codecs::webp::vp8::NAME` 按名引用；其余 vp8_tables 项仍由上方私有 glob 供给本子树）。
pub(crate) use vp8_tables::{
    BLOCK_MODE_TREE, KEYFRAME_BLOCK_MODE_PROBS, KEYFRAME_UV_MODE_PROBS, KEYFRAME_Y_MODE_PROBS,
    KEYFRAME_Y_MODE_TREE, SEGMENT_ID_TREE, UV_MODE_TREE,
};

#[cfg(test)]
#[path = "../../../../tests/fixtures/webp/mod.rs"]
mod webp_fixtures_support;
