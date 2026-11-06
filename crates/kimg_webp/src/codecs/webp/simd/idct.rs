#![forbid(unsafe_code)]
// ---------------------------------------------------------------------------
// VP8 DCT 逆变换 / DC 捷径安全薄委托（叶子隔离收口波）：全部 `#[target_feature]` SSE2
// SIMD 内核与分派门面已移入同层 `raw::idct`。本文件顶部属性由编译期保证全文件零裸操作：
// 安全入口（`inverse_dct_add_two`/`inverse_dct_add_one`/`dc_add_two`，均为休眠仅测试用）
// 退化为一行薄委托；带 Safety 契约的 `*_forced` 因 forbid 无法定义，改以
// `pub(crate) use` re-export 原样透传（同 `predict_subblock_forced` 姿势）。签名 / 可见性 /
// 行为逐字节不变。
// ---------------------------------------------------------------------------

/// 两个水平相邻 4×4 块的逆 DCT + 残差写回（字节==既有标量）。
/// 休眠：已被 `inverse_dct_add_two_forced` 取代（预判定直调），仅测试/探针用。
/// 薄委托 `raw::idct::inverse_dct_add_two`。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn inverse_dct_add_two(
    coeff_a: &[i16; 16],
    coeff_b: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    super::raw::idct::inverse_dct_add_two(coeff_a, coeff_b, plane, pitch, row0, col0)
}

/// 预判定后的双块逆变换核直调（`raw::idct::inverse_dct_add_two_forced` 的 re-export 透传：
/// 该入口带 Safety 契约，调用方负责前置条件，签名与隔离前完全一致）。
pub(crate) use super::raw::idct::inverse_dct_add_two_forced;

/// 单个 4×4 块的逆 DCT + 残差写回（i4x4 用，字节==既有标量）。
/// 休眠：已被 `idct_add_one_forced` 取代，仅测试用（test 引用）。
/// 薄委托 `raw::idct::inverse_dct_add_one`。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn inverse_dct_add_one(
    coeff: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    super::raw::idct::inverse_dct_add_one(coeff, plane, pitch, row0, col0)
}

/// 预判定后的单块逆变换核直调（`raw::idct::idct_add_one_forced` 的 re-export 透传）。
pub(crate) use super::raw::idct::idct_add_one_forced;

/// 两个水平相邻 4×4 块仅 DC（无 AC）的逆变换捷径。
/// 休眠：已被 `dc_add_two_forced` 取代，仅测试用（test 引用）。
/// 薄委托 `raw::idct::dc_add_two`。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn dc_add_two(
    dc_a: i16,
    dc_b: i16,
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
) {
    super::raw::idct::dc_add_two(dc_a, dc_b, plane, pitch, row0, col0)
}

/// 预判定后的双块 DC 捷径核直调（`raw::idct::dc_add_two_forced` 的 re-export 透传）。
pub(crate) use super::raw::idct::dc_add_two_forced;

/// 预判定后的单块 DC 捷径核直调（`raw::idct::dc_add_one_forced` 的 re-export 透传）。
pub(crate) use super::raw::idct::dc_add_one_forced;
