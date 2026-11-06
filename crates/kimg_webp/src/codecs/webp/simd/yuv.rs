#![forbid(unsafe_code)]
// ---------------------------------------------------------------------------
// YUV→RGB 定点转换（AVX2/SSE4.1，对标 vp8.rs `yuv_to_rgb`）安全薄委托（叶子隔离第三波）：
// 全部 `#[target_feature]` SIMD 内核与分派门面已移入同层 `raw::yuv`。本文件顶部属性由编译期
// 保证全文件零裸操作，函数退化为薄委托，签名 / 可见性 / 行为逐字节不变。
// ---------------------------------------------------------------------------

/// 标量单像素转换（直接复用 `vp8::yuv_to_rgb` 的算术源，保证 SIMD 与标量同序同 truncation）。
/// 薄委托 `raw::yuv::yuv_to_rgb`（非测试下无外部调用，仅测试对拍；内核标量尾直调 raw 本体）。
#[cfg_attr(not(test), allow(dead_code))]
#[inline]
pub(crate) fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    super::raw::yuv::yuv_to_rgb(y, u, v)
}

/// 一行 YUV→RGBA8（alpha 恒 255）：AVX2 每 32 像素批量（含 16/8px 段收尾）+ 标量尾；
/// 无 AVX2 则 SSE4.1 每 8 像素批量 + 标量尾；无 SIMD 全标量。逐字节恒等于标量
/// `vp8::yuv_to_rgb`（各路径同一算术源，见内核注释）。
/// 输入三平面各 `n` 字节，输出 `4*n` 字节。薄委托 `raw::yuv::yuv_row_to_rgba`。
pub(crate) fn yuv_row_to_rgba(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8]) {
    super::raw::yuv::yuv_row_to_rgba(y, u, v, out)
}

/// 一行 YUV→RGBA8，alpha 来自可选 alpha 行（`Some` 时每像素第 4 字节 = alpha[i]；
/// `None` 时恒 255）。批量结构与 [`yuv_row_to_rgba`] 完全同源，仅 A 通道由常量 255 改为
/// 逐组载入 alpha 行——`Some` 时逐字节恒等于「旧 `yuv_row_to_rgba` + 事后标量
/// `rgba[i*4+3]=alpha[i]`」两遍合成（lossy-alpha 发射融合后单遍写出的字节契约）。
/// 输入三平面加 alpha 行各 `n` 字节，输出 `4*n` 字节。薄委托 `raw::yuv::yuv_row_to_rgba_alpha`。
pub(crate) fn yuv_row_to_rgba_alpha(y: &[u8], u: &[u8], v: &[u8], alpha: Option<&[u8]>, out: &mut [u8]) {
    super::raw::yuv::yuv_row_to_rgba_alpha(y, u, v, alpha, out)
}

/// 一行 YUV→RGB3（alpha 不写，3 字节/像素）：AVX2/SSE4.1/标量逐档镜像 [`yuv_row_to_rgba`] 的
/// 调度与内核（u16-mulhi 变体经 `yuv_u16_enabled` 同款门控）。每像素前 3 字节恒等于
/// `yuv_row_to_rgba` 同输入——新 RGB3 直出路径与旧 RGBA 路径的字节契约。
/// 输入三平面各 `n` 字节，输出 `3*n` 字节。薄委托 `raw::yuv::yuv_row_to_rgb3`。
pub(crate) fn yuv_row_to_rgb3(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8]) {
    super::raw::yuv::yuv_row_to_rgb3(y, u, v, out)
}
