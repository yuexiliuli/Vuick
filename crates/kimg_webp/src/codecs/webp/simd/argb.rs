#![forbid(unsafe_code)]
// ---------------------------------------------------------------------------
// VP8L ARGB→RGBA8 输出（SSSE3 字节重排）与 Subtract-Green 逆变换（SSE4.1）——
// 对齐 vp8l.rs `to_rgba` / `apply_subtract_green`（mod 256 逐通道加，无进位）
//
// 叶子隔离试点：全部 `#[target_feature]` SIMD 内核与手写指针逻辑已移入同层 `raw`
// 模块（唯一的裸代码免灾区）。本文件顶部属性由编译期保证
// 全文件零裸操作，3 个函数退化为薄委托，签名 / 可见性 / 行为逐字节不变。
// ---------------------------------------------------------------------------

/// ARGB 像素（u32：A<<24|R<<16|G<<8|B，内存小端字节序 [B,G,R,A]）→ RGBA8 字节。
/// 即 vp8l.rs `to_rgba` 的字节序交换（B/R 对调）。薄委托 `raw::argb_to_rgba`。
pub(crate) fn argb_to_rgba(pixels: &[u32], out: &mut [u8]) {
    super::raw::argb_to_rgba(pixels, out)
}

/// 原地版 ARGB→RGBA 收尾（`decode()` 复用同一缓冲，省整帧 RGBA 拷贝/分配）：
/// 每个 u32 的字节 0(B) 与 2(R) 对调（字节 3=A、1=G 原样）→ 内存序即 [R,G,B,A]。
/// 薄委托 `raw::argb_to_rgba_inplace`。
pub(crate) fn argb_to_rgba_inplace(pixels: &mut [u32]) {
    super::raw::argb_to_rgba_inplace(pixels)
}

/// Subtract-Green 逆变换（标量语义 `apply_subtract_green`）：R/G 通道 + G 通道（mod 256）。
/// 薄委托 `raw::subtract_green`。
pub(crate) fn subtract_green(pixels: &mut [u32]) {
    super::raw::subtract_green(pixels)
}
