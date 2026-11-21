#![forbid(unsafe_code)]
// 上采样薄委托逐寄存器传参，具名构参可读性反而变差 → 按既定内核风格放行。
#![allow(clippy::too_many_arguments)]
// ---------------------------------------------------------------------------
// fancy 上采样（SSE2/SSSE3/AVX2 16-bit lane；对标 vp8.rs `upsample_fancy`）安全薄委托
// （叶子隔离第三波）：全部 `#[target_feature]` SIMD 内核与分派门面已移入同层 `raw::upsample`。
// 本文件顶部属性由编译期保证全文件零裸操作，函数退化为薄委托，签名 / 可见性 / 行为逐字节不变。
// ---------------------------------------------------------------------------

/// fancy 上采样一行 chroma：`current`/`adjacent` 各 `cw` 字节 → 输出 `width` 字节。
/// SIMD（SSSE3）处理中间列对（col=1..cw-2），首/末列与 `cw<=1` 走标量（边界算术特殊）。
/// 便捷包装（生产热路径用 [`upsample_fancy_into`] 复用缓冲；本包装供测试/独立调用）。
/// 薄委托 `raw::upsample::upsample_fancy_row`。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn upsample_fancy_row(current: &[u8], adjacent: &[u8], width: usize) -> Vec<u8> {
    super::raw::upsample::upsample_fancy_row(current, adjacent, width)
}

/// fancy 上采样写入调用方缓冲（复用 `out`/`t_ws`，避免逐行分配；`t_ws` 须 ≥ `cw+8`）。
/// 生产热路径改走 [`upsample_fancy_into_uv`]（U/V 合并）；本函数保留为标量对拍参考/测试路径。
/// 薄委托 `raw::upsample::upsample_fancy_into`。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn upsample_fancy_into(current: &[u8], adjacent: &[u8], out: &mut [u8], t_ws: &mut [u16]) {
    super::raw::upsample::upsample_fancy_into(current, adjacent, out, t_ws)
}

/// fancy 上采样 U/V 两行一次完成（参照 D2 `lf_uv_pair` 的 U/V 双平面打包先例，把两次
/// `upsample_fancy_into` 合并成同一循环构建两 t 行 + 同一循环处理两输出平面，省散布往返）。
/// 语义与逐平面调用 [`upsample_fancy_into`] 完全一致（逐字节）：t 行 `3*cur+adj`、边界列、
/// 中间列对 `(3*t±邻居+8)>>4` 全同算术，仅调用形态合并。`t_ws` 须 ≥ `2*(cw+8)`（低半 U、高半 V）。
/// 薄委托 `raw::upsample::upsample_fancy_into_uv`。
pub(crate) fn upsample_fancy_into_uv(
    u_cur: &[u8],
    u_adj: &[u8],
    v_cur: &[u8],
    v_adj: &[u8],
    u_out: &mut [u8],
    v_out: &mut [u8],
    t_ws: &mut [u16],
) {
    super::raw::upsample::upsample_fancy_into_uv(u_cur, u_adj, v_cur, v_adj, u_out, v_out, t_ws)
}

/// 行对 fancy 上采样（U/V × 奇/偶两输出行一次完成，libwebp `UpsampleLinePair` 同趟 top/bottom
/// 语义）：对相邻 chroma 行对 `(top=C_k, cur=C_{k+1})` 生成输出行 **2k+1（奇）** 的整行 U/V
/// fancy（t = 3*top + cur）与 **2k+2（偶）** 的整行 U/V fancy（t = 3*cur + top）。与逐行两次
/// 调用 [`upsample_fancy_into_uv`] 完全一致（**同算术、逐字节**，`upsample_pair_matches_twice_into_uv`
/// 对拍 0 差）：两 chroma 行只装入一次、t 行融合构建（四行 t 同一循环）、fancy 中间列循环把
/// U/V × 奇/偶 四 t 行并进同一迭代（比逐行两次省一次 chroma 内存往返 + 循环 scaffold 一半）。
/// 语义取舍：与各输出行独立调用相比，加法树完全一致（t = 3*cur+adj 只是把「当前行」在奇偶行
/// 的交换显式化），无任何重结合 → 字节恒等由构造保证。`t_ws` ≥ **4**(cw+8) u16：低半 U（奇/偶）、
/// 高半 V（奇/偶），每行 cw+8 尾填充供 SSE 越界读保护。
/// 薄委托 `raw::upsample::upsample_fancy_into_uv_pair`。
pub(crate) fn upsample_fancy_into_uv_pair(
    u_top: &[u8],
    u_cur: &[u8],
    v_top: &[u8],
    v_cur: &[u8],
    u_odd_out: &mut [u8],
    v_odd_out: &mut [u8],
    u_even_out: &mut [u8],
    v_even_out: &mut [u8],
    t_ws: &mut [u16],
) {
    super::raw::upsample::upsample_fancy_into_uv_pair(u_top, u_cur, v_top, v_cur, u_odd_out, v_odd_out, u_even_out, v_even_out, t_ws)
}
