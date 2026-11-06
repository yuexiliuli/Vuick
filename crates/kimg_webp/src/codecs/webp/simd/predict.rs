#![forbid(unsafe_code)]
// ---------------------------------------------------------------------------
// VP8 D1 帧内预测安全薄委托（叶子隔离第二波）：全部 `#[target_feature]` SIMD 内核与
// 分派门面已移入同层 `raw::predictor`。本文件顶部属性由编译期保证全文件零裸操作，
// 函数退化为薄委托，签名 / 可见性 / 行为逐字节不变。带 Safety 契约的
// `predict_subblock_forced` 因 forbid 无法定义，改以 re-export 原样透传。
// ---------------------------------------------------------------------------

/// 4×4 预测分派：SSSE3 向量核；无 SIMD → vp8.rs 标量（单一语义源）。
/// 休眠：产线走 `predict_subblock_forced`（pipeline.rs「不可再走 simd::predict_subblock」），仅测试/探针用。
/// 薄委托 `raw::predictor::predict_subblock`。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn predict_subblock(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    mode: u8,
    above: &[u8; 9],
) {
    super::raw::predictor::predict_subblock(plane, pitch, row0, col0, mode, above)
}

/// 预判定后的 4×4 预测核直调（`raw::predictor::predict_subblock_forced` 的 re-export 透传：
/// 该入口带 Safety 契约，调用方负责前置条件，签名与隔离前完全一致）。
pub(crate) use super::raw::predictor::predict_subblock_forced;

/// 16×16/8×8 整块预测分派：V/H/DC/TM 四模式的整行广播；无 SIMD → 标量。
/// 薄委托 `raw::predictor::predict_large`。
pub(crate) fn predict_large(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    size: usize,
    mode: u8,
    has_top: bool,
    has_left: bool,
    above: &[u8],
) {
    super::raw::predictor::predict_large(plane, pitch, row0, col0, size, mode, has_top, has_left, above)
}
