#![forbid(unsafe_code)]
// ---------------------------------------------------------------------------
// VP8L 预测器 / 色变换逆变换与 RGBA8↔RGB8 通道重排的安全薄委托（叶子隔离第二波）：
// 全部 `#[target_feature]` SIMD 内核与门面已移入同层 `raw::predictor`。本文件顶部属性
// 由编译期保证全文件零裸操作，函数退化为薄委托，签名 / 可见性 / 行为逐字节不变。
// ---------------------------------------------------------------------------

/// RGBA8 → RGB8 抽条（4→3）。薄委托 `raw::predictor::rgba8_to_rgb8`。
pub(crate) fn rgba8_to_rgb8(src: &[u8], dst: &mut [u8]) {
    super::raw::predictor::rgba8_to_rgb8(src, dst)
}

/// RGB8 → RGBA8 扩写（3→4，alpha 恒 255）。薄委托 `raw::predictor::rgb8_to_rgba8`。
pub fn rgb8_to_rgba8(src: &[u8], dst: &mut [u8]) {
    super::raw::predictor::rgb8_to_rgba8(src, dst)
}

/// VP8L 预测器 SIMD 就绪（x86_64 且 SSE2 可用且 `KIMG_WEBP_NO_SIMD` 未关）。薄委托。
pub(crate) fn vp8l_predictor_ready() -> bool {
    super::raw::predictor::vp8l_predictor_ready()
}

/// 段内恒定 mode 的单像素标量预测。薄委托 `raw::predictor::pred_scalar_one`。
#[cfg_attr(not(test), allow(dead_code))]
#[inline(always)]
pub(crate) fn pred_scalar_one(mode: u32, left: u32, top: u32, top_left: u32, top_right: u32) -> Option<u32> {
    super::raw::predictor::pred_scalar_one(mode, left, top, top_left, top_right)
}

/// VP8L 预测器逆变换（整个图像）。薄委托 `raw::predictor::predictor_inverse`。
pub(crate) fn predictor_inverse(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) -> Option<()> {
    super::raw::predictor::predictor_inverse(pixels, width, height, bits, data)
}

/// VP8L 色变换逆变换（整个图像）。薄委托 `raw::predictor::color_transform_inverse`。
pub(crate) fn color_transform_inverse(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    bits: u32,
    data: &[u32],
) -> bool {
    super::raw::predictor::color_transform_inverse(pixels, width, height, bits, data)
}
