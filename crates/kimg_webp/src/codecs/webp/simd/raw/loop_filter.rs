#![allow(unsafe_code)]
// 环路滤波 SIMD 内核：参数多 + 逐 lane 下标循环（字节恒等硬约束，改写迭代器会动 codegen）→ 放行。
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]
//! 裸 unsafe 叶子子模块 `raw::loop_filter`：D2 环路滤波全部 SSE2 SIMD 内核 + 安全门面整体迁入
//! （原 `simd/loop_filter.rs` / `simd/lf_edge.rs`，含 D2v1 弃用休眠集群 / D2v2 libwebp 风格
//! `lf_edge_*` 内核 / D2v3 宏块熔合 `lf_filter_macroblock`）。上层 `simd/loop_filter.rs` /
//! `simd/lf_edge.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖本模块。算法 / 常量 / 调用
//! 顺序与隔离前逐字节一致（字节恒等是硬约束，禁改算术）。

use super::super::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// ---------------------------------------------------------------------------
// D2：环路滤波向量化（SSE2；C# `Vp8LoopFilter.cs` 同款：无符号域算掩码、`x^0x80` 偏差符号域
// 做饱和加减；`common_adjust` 的 tap 先钳后移；垂直边经 16×8 / 8×8 转置梯）。逐字节与 vp8.rs 标量
// 一致断言见模块底部。
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
// 休眠：D2v1 弃旧 16×8 全转置慢实现集群（filter_lanes/filter_edge_sse2 同宗），仅测试/探针用。
#[cfg_attr(not(test), allow(dead_code))]
enum FilterKind {
    Simple,
    Inner,
    Macroblock,
}

/// 单边滤波分派：`length` 16（luma）/ 8（chroma）且 flt-SIMD 开 → SSE2 向量核；否则 vp8.rs 标量。
/// **默认未接入生产**（D2 实测持平/微负，vp8.rs 走标量；本入口仅测试/探针/复测用），
/// 非 test 构建允许 dead_code。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn filter_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    #[cfg(target_arch = "x86_64")]
    if part_simd_on("flt", "sse2") {
        // D2v2：libwebp 风格 16/8-lane（本机字节一致的更快实现）。
        unsafe {
            lf_filter_edge_sse2(
                plane, stride, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev,
            )
        }
        return;
    }
    super::vp8::filter_edge(plane, stride, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev);
}

/// 逐字节逻辑右移 k（k<8，u8 域）：psrlw 的 16-bit 移位会用高字节低位污染低字节，故
/// 低字节经掩码独立移、高字节取 psrlw 的高半，再拼回。`k` 须为编译期常量（psrlw 立即数）。
macro_rules! shr8u {
    ($v:expr, $k:expr) => {{
        let vec = $v;
        let hi = _mm_srli_epi16(vec, $k);
        let lo = _mm_srli_epi16(
            _mm_and_si128(vec, _mm_set1_epi16(((0xFFu16 << $k) & 0xFF) as i16)),
            $k,
        );
        _mm_or_si128(_mm_and_si128(hi, _mm_set1_epi16(0xFF00u16 as i16)), lo)
    }};
}

/// `|a-b|`（u8 饱和差 + OR）。
/// 休眠：D2v1 弃旧 16×8 全转置慢实现集群，仅测试/探针用。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn abs_diff_u8(a: __m128i, b: __m128i) -> __m128i {
    _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a))
}

/// 有符号字节 `v` 算术右移 3：`(v^0x80)>>3 - 16`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn shift_right3(v: __m128i, e80: __m128i) -> __m128i {
    _mm_sub_epi8(shr8u!(_mm_xor_si128(v, e80), 3), _mm_set1_epi8(16))
}

/// 有符号字节算术右移 1：`(v^0x80)>>1 - 64`。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn shift_right1(v: __m128i, e80: __m128i) -> __m128i {
    _mm_sub_epi8(shr8u!(_mm_xor_si128(v, e80), 1), _mm_set1_epi8(64))
}

/// 边缘掩码：`2|p0-q0|+|p1-q1|/2 <= edge`（饱和和即便越 255 也恒大于 edge 阈值，见 C# 注释）。
/// 休眠：D2v1 弃旧 16×8 全转置慢实现集群，仅测试/探针用。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn edge_mask(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, edge: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let d0 = unsafe { abs_diff_u8(p0, q0) };
    let d1 = shr8u!(unsafe { abs_diff_u8(p1, q1) }, 1);
    let sum = _mm_adds_epu8(_mm_adds_epu8(d0, d0), d1);
    _mm_cmpeq_epi8(_mm_subs_epu8(sum, _mm_set1_epi8(edge as i8)), zero)
}

/// common_adjust：`a=clamp(outer+3*delta)`（饱和三次），`qa=(a+4)>>3`、`pa=(a+3)>>3`
/// （先钳后移），只更新 `amask` 覆盖的 lane，返回 (p0, q0, qAdjust)。
/// 休眠：D2v1 弃旧 16×8 全转置慢实现集群，仅测试/探针用。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn common_adjust(
    outer: __m128i,
    delta: __m128i,
    sp0: __m128i,
    sq0: __m128i,
    amask: __m128i,
    e80: __m128i,
) -> (__m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    unsafe {
    let a = _mm_and_si128(_mm_adds_epi8(_mm_adds_epi8(_mm_adds_epi8(outer, delta), delta), delta), amask);
    let q_adj = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(4)), e80);
    let p_adj = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(3)), e80);
    let q0 = _mm_subs_epi8(sq0, q_adj);
    let p0 = _mm_adds_epi8(sp0, p_adj);
    (p0, q0, q_adj)
    }
}

/// 宏块权重 `(w*27/18/9 + 63) >> 7`（w 有符号字节 → i16 符号扩展计算 → 饱和折回）。
/// 休眠：D2v1 弃旧 16×8 全转置慢实现集群，仅测试/探针用。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn tap_weights(w: __m128i) -> (__m128i, __m128i, __m128i) {
    let wlo = _mm_srai_epi16(_mm_unpacklo_epi8(w, w), 8);
    let whi = _mm_srai_epi16(_mm_unpackhi_epi8(w, w), 8);
    let r63 = _mm_set1_epi16(63);
    let c27 = _mm_set1_epi16(27);
    let c18 = _mm_set1_epi16(18);
    let c9 = _mm_set1_epi16(9);
    let a27 = _mm_packs_epi16(
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(wlo, c27), r63), 7),
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(whi, c27), r63), 7),
    );
    let a18 = _mm_packs_epi16(
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(wlo, c18), r63), 7),
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(whi, c18), r63), 7),
    );
    let a9 = _mm_packs_epi16(
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(wlo, c9), r63), 7),
        _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(whi, c9), r63), 7),
    );
    (a27, a18, a9)
}

/// 16 lane 滤波核心：p3..q3（每 lane 一“样本列”），返回可变的 (p2,p1,p0,q0,q1,q2)，其余原样。
/// 休眠：D2v1 弃旧 16×8 全转置慢实现集群，仅测试/探针用。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn filter_lanes(
    p3: __m128i,
    p2: __m128i,
    p1: __m128i,
    p0: __m128i,
    q0: __m128i,
    q1: __m128i,
    q2: __m128i,
    q3: __m128i,
    kind: FilterKind,
    interior: i32,
    edge: i32,
    hev: i32,
) -> (__m128i, __m128i, __m128i, __m128i, __m128i, __m128i) {
    use std::arch::x86_64::*;
    unsafe {
    let e80 = _mm_set1_epi8(-128);
    let zero = _mm_setzero_si128();
    let mask_edge = edge_mask(p1, p0, q0, q1, edge);
    if kind == FilterKind::Simple {
        let sp1 = _mm_xor_si128(p1, e80);
        let sp0 = _mm_xor_si128(p0, e80);
        let sq0 = _mm_xor_si128(q0, e80);
        let sq1 = _mm_xor_si128(q1, e80);
        let (p0n, q0n, _) = common_adjust(
            _mm_subs_epi8(sp1, sq1),
            _mm_subs_epi8(sq0, sp0),
            sp0,
            sq0,
            mask_edge,
            e80,
        );
        return (p2, p1, _mm_xor_si128(p0n, e80), _mm_xor_si128(q0n, e80), q1, q2);
    }
    // Normal（Inner / Macroblock）：额外 interior 上限掩码 + 高方差判定。
    let dp1p0 = abs_diff_u8(p1, p0);
    let dq1q0 = abs_diff_u8(q1, q0);
    let imax = _mm_max_epu8(
        _mm_max_epu8(_mm_max_epu8(abs_diff_u8(p3, p2), abs_diff_u8(p2, p1)), dp1p0),
        _mm_max_epu8(_mm_max_epu8(abs_diff_u8(q3, q2), abs_diff_u8(q2, q1)), dq1q0),
    );
    let mask = _mm_and_si128(
        mask_edge,
        _mm_cmpeq_epi8(_mm_subs_epu8(imax, _mm_set1_epi8(interior as i8)), zero),
    );
    // hv = (|p1-p0|>hev 或 |q1-q0|>hev)：`NOT eq(sat(max-hev), 0)`。
    let hv = _mm_xor_si128(
        _mm_cmpeq_epi8(
            _mm_subs_epu8(_mm_max_epu8(dp1p0, dq1q0), _mm_set1_epi8(hev as i8)),
            zero,
        ),
        _mm_cmpeq_epi8(zero, zero),
    );
    let sp2 = _mm_xor_si128(p2, e80);
    let sp1 = _mm_xor_si128(p1, e80);
    let sp0 = _mm_xor_si128(p0, e80);
    let sq0 = _mm_xor_si128(q0, e80);
    let sq1 = _mm_xor_si128(q1, e80);
    let sq2 = _mm_xor_si128(q2, e80);
    let outer = _mm_subs_epi8(sp1, sq1);
    let delta = _mm_subs_epi8(sq0, sp0);
    if kind == FilterKind::Macroblock {
        // hv lane：common adjust（outer 全取）；!hv lane：27/18/9 加权铺到 p2..q2。
        let (ph, qh, _qa) = common_adjust(outer, delta, sp0, sq0, _mm_and_si128(mask, hv), e80);
        let wmask = _mm_andnot_si128(hv, mask);
        let w = _mm_and_si128(_mm_adds_epi8(_mm_adds_epi8(_mm_adds_epi8(outer, delta), delta), delta), wmask);
        let (a27, a18, a9) = tap_weights(w);
        let p0n = _mm_adds_epi8(ph, a27);
        let q0n = _mm_subs_epi8(qh, a27);
        let p1n = _mm_adds_epi8(sp1, a18);
        let q1n = _mm_subs_epi8(sq1, a18);
        let p2n = _mm_adds_epi8(sp2, a9);
        let q2n = _mm_subs_epi8(sq2, a9);
        return (
            _mm_xor_si128(p2n, e80),
            _mm_xor_si128(p1n, e80),
            _mm_xor_si128(p0n, e80),
            _mm_xor_si128(q0n, e80),
            _mm_xor_si128(q1n, e80),
            _mm_xor_si128(q2n, e80),
        );
    }
    // Inner：outer 仅 hv lane 生效；!hv lane 用 `(qAdj+1)>>1` 动 p1/q1。
    let outer_hv = _mm_and_si128(outer, hv);
    let (p0n, q0n, q_adj) = common_adjust(outer_hv, delta, sp0, sq0, mask, e80);
    let half = _mm_and_si128(
        shift_right1(_mm_add_epi8(q_adj, _mm_set1_epi8(1)), e80),
        _mm_andnot_si128(hv, mask),
    );
    let p1n = _mm_adds_epi8(sp1, half);
    let q1n = _mm_subs_epi8(sq1, half);
    (
        p2,
        _mm_xor_si128(p1n, e80),
        _mm_xor_si128(p0n, e80),
        _mm_xor_si128(q0n, e80),
        _mm_xor_si128(q1n, e80),
        q2,
    )
    }
}

/// 16×8 转置梯（C# `Load16x8` 同款）：`a` 行 y..y+7、`b` 行 y+8..y+15 各 8 列，输出
/// c0..c7 = 各列 16 行的值（低 8 = a 行、高 8 = b 行）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn load16x8(a: *const u8, a_stride: isize, b: *const u8, b_stride: isize) -> [__m128i; 8] {
    use std::arch::x86_64::*;
    unsafe {
    let mut r = [_mm_setzero_si128(); 8];
    for i in 0..8 {
        let is = i as isize;
        let alo = _mm_loadl_epi64(a.offset(is * a_stride) as *const __m128i);
        let bhi = _mm_loadl_epi64(b.offset(is * b_stride) as *const __m128i);
        r[i] = _mm_unpacklo_epi64(alo, bhi);
    }
    let t0 = _mm_unpacklo_epi8(r[0], r[1]);
    let t1 = _mm_unpackhi_epi8(r[0], r[1]);
    let t2 = _mm_unpacklo_epi8(r[2], r[3]);
    let t3 = _mm_unpackhi_epi8(r[2], r[3]);
    let t4 = _mm_unpacklo_epi8(r[4], r[5]);
    let t5 = _mm_unpackhi_epi8(r[4], r[5]);
    let t6 = _mm_unpacklo_epi8(r[6], r[7]);
    let t7 = _mm_unpackhi_epi8(r[6], r[7]);
    let u0 = _mm_unpacklo_epi16(t0, t2);
    let u1 = _mm_unpackhi_epi16(t0, t2);
    let u2 = _mm_unpacklo_epi16(t4, t6);
    let u3 = _mm_unpackhi_epi16(t4, t6);
    let u4 = _mm_unpacklo_epi16(t1, t3);
    let u5 = _mm_unpackhi_epi16(t1, t3);
    let u6 = _mm_unpacklo_epi16(t5, t7);
    let u7 = _mm_unpackhi_epi16(t5, t7);
    let v0 = _mm_unpacklo_epi32(u0, u2);
    let v1 = _mm_unpackhi_epi32(u0, u2);
    let v2 = _mm_unpacklo_epi32(u1, u3);
    let v3 = _mm_unpackhi_epi32(u1, u3);
    let v4 = _mm_unpacklo_epi32(u4, u6);
    let v5 = _mm_unpackhi_epi32(u4, u6);
    let v6 = _mm_unpacklo_epi32(u5, u7);
    let v7 = _mm_unpackhi_epi32(u5, u7);
    let c0 = _mm_unpacklo_epi64(v0, v4);
    let c1 = _mm_unpackhi_epi64(v0, v4);
    let c2 = _mm_unpacklo_epi64(v1, v5);
    let c3 = _mm_unpackhi_epi64(v1, v5);
    let c4 = _mm_unpacklo_epi64(v2, v6);
    let c5 = _mm_unpackhi_epi64(v2, v6);
    let c6 = _mm_unpacklo_epi64(v3, v7);
    let c7 = _mm_unpackhi_epi64(v3, v7);
    [c0, c1, c2, c3, c4, c5, c6, c7]
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn store_pairs(dst: *mut u8, stride: isize, v: [__m128i; 4]) {
    use std::arch::x86_64::*;
    unsafe {
    for i in 0..4 {
        _mm_storel_epi64(dst.offset((2 * i) as isize * stride) as *mut __m128i, v[i]);
        _mm_storel_epi64(
            dst.offset(((2 * i + 1) as isize) * stride) as *mut __m128i,
            _mm_srli_si128(v[i], 8),
        );
    }
    }
}

/// 16×8 转置逆（C# `Store16x8` 同款）：c0..c7 写回 a 行 y..y+7 / b 行 y+8..y+15。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn store16x8(
    a: *mut u8,
    a_stride: isize,
    b: *mut u8,
    b_stride: isize,
    c: [__m128i; 8],
) {
    use std::arch::x86_64::*;
    unsafe {
    let t0 = _mm_unpacklo_epi8(c[0], c[1]);
    let t1 = _mm_unpackhi_epi8(c[0], c[1]);
    let t2 = _mm_unpacklo_epi8(c[2], c[3]);
    let t3 = _mm_unpackhi_epi8(c[2], c[3]);
    let t4 = _mm_unpacklo_epi8(c[4], c[5]);
    let t5 = _mm_unpackhi_epi8(c[4], c[5]);
    let t6 = _mm_unpacklo_epi8(c[6], c[7]);
    let t7 = _mm_unpackhi_epi8(c[6], c[7]);
    let u0 = _mm_unpacklo_epi16(t0, t2);
    let u1 = _mm_unpackhi_epi16(t0, t2);
    let u2 = _mm_unpacklo_epi16(t4, t6);
    let u3 = _mm_unpackhi_epi16(t4, t6);
    let u4 = _mm_unpacklo_epi16(t1, t3);
    let u5 = _mm_unpackhi_epi16(t1, t3);
    let u6 = _mm_unpacklo_epi16(t5, t7);
    let u7 = _mm_unpackhi_epi16(t5, t7);
    let rows01 = _mm_unpacklo_epi32(u0, u2);
    let rows23 = _mm_unpackhi_epi32(u0, u2);
    let rows45 = _mm_unpacklo_epi32(u1, u3);
    let rows67 = _mm_unpackhi_epi32(u1, u3);
    let rows89 = _mm_unpacklo_epi32(u4, u6);
    let rows1011 = _mm_unpackhi_epi32(u4, u6);
    let rows1213 = _mm_unpacklo_epi32(u5, u7);
    let rows1415 = _mm_unpackhi_epi32(u5, u7);
    store_pairs(a, a_stride, [rows01, rows23, rows45, rows67]);
    store_pairs(b, b_stride, [rows89, rows1011, rows1213, rows1415]);
    }
}

/// 8×8 转置梯：rows[i] 低 8 字节 = 行 i 的 8 列；输出 cols[k] 低 8 字节 = 列 k 的 8 行。
/// 矩阵转置是自身的逆，故往返即还原（单测有 round-trip 断言）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn transpose8x8(rows: [__m128i; 8]) -> [__m128i; 8] {
    let zero = _mm_setzero_si128();
    let a0 = _mm_unpacklo_epi8(rows[0], rows[1]);
    let a1 = _mm_unpacklo_epi8(rows[2], rows[3]);
    let a2 = _mm_unpacklo_epi8(rows[4], rows[5]);
    let a3 = _mm_unpacklo_epi8(rows[6], rows[7]);
    let b0 = _mm_unpacklo_epi16(a0, a1);
    let b1 = _mm_unpackhi_epi16(a0, a1);
    let b2 = _mm_unpacklo_epi16(a2, a3);
    let b3 = _mm_unpackhi_epi16(a2, a3);
    let c01 = _mm_unpacklo_epi32(b0, b2);
    let c23 = _mm_unpackhi_epi32(b0, b2);
    let c45 = _mm_unpacklo_epi32(b1, b3);
    let c67 = _mm_unpackhi_epi32(b1, b3);
    [
        _mm_unpacklo_epi64(c01, zero),
        _mm_unpackhi_epi64(c01, zero),
        _mm_unpacklo_epi64(c23, zero),
        _mm_unpackhi_epi64(c23, zero),
        _mm_unpacklo_epi64(c45, zero),
        _mm_unpackhi_epi64(c45, zero),
        _mm_unpacklo_epi64(c67, zero),
        _mm_unpackhi_epi64(c67, zero),
    ]
}

/// SSE2 单边滤波：按 vertical + length 分四条（luma 16 × 横/纵、chroma 8 × 横/纵）。
/// 休眠：D2v1 弃旧 16×8 全转置慢实现集群，仅测试/探针用。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn filter_edge_sse2(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    use std::arch::x86_64::*;
    unsafe {
    let kind = if simple {
        FilterKind::Simple
    } else if macroblock_edge {
        FilterKind::Macroblock
    } else {
        FilterKind::Inner
    };
    let base = plane.as_mut_ptr();
    let center = y * stride + 1 + x;
    if !vertical {
        // 水平边：每 lane 一批的 p3..q3 是整行（16/8 连续字节）。
        if length == 16 {
            let q0p = base.add(center);
            let p3 = _mm_loadu_si128(q0p.sub(4 * stride) as *const __m128i);
            let p2 = _mm_loadu_si128(q0p.sub(3 * stride) as *const __m128i);
            let p1 = _mm_loadu_si128(q0p.sub(2 * stride) as *const __m128i);
            let p0 = _mm_loadu_si128(q0p.sub(stride) as *const __m128i);
            let q0 = _mm_loadu_si128(q0p as *const __m128i);
            let q1 = _mm_loadu_si128(q0p.add(stride) as *const __m128i);
            let q2 = _mm_loadu_si128(q0p.add(2 * stride) as *const __m128i);
            let q3 = _mm_loadu_si128(q0p.add(3 * stride) as *const __m128i);
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                filter_lanes(p3, p2, p1, p0, q0, q1, q2, q3, kind, interior, edge, hev);
            macroblock_store16(q0p, stride, kind, p2n, p1n, p0n, q0n, q1n, q2n);
        } else {
            let q0p = base.add(center);
            let p3 = _mm_loadl_epi64(q0p.sub(4 * stride) as *const __m128i);
            let p2 = _mm_loadl_epi64(q0p.sub(3 * stride) as *const __m128i);
            let p1 = _mm_loadl_epi64(q0p.sub(2 * stride) as *const __m128i);
            let p0 = _mm_loadl_epi64(q0p.sub(stride) as *const __m128i);
            let q0 = _mm_loadl_epi64(q0p as *const __m128i);
            let q1 = _mm_loadl_epi64(q0p.add(stride) as *const __m128i);
            let q2 = _mm_loadl_epi64(q0p.add(2 * stride) as *const __m128i);
            let q3 = _mm_loadl_epi64(q0p.add(3 * stride) as *const __m128i);
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                filter_lanes(p3, p2, p1, p0, q0, q1, q2, q3, kind, interior, edge, hev);
            macroblock_store8(q0p, stride, kind, p2n, p1n, p0n, q0n, q1n, q2n);
        }
        return;
    }
    // 垂直边：lane = 行方向 → 转置梯。
    if length == 16 {
        let a = base.add(center - 4);
        let b = a.add(8 * stride);
        let c = load16x8(a, stride as isize, b, stride as isize);
        let (p2n, p1n, p0n, q0n, q1n, q2n) =
            filter_lanes(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], kind, interior, edge, hev);
        let out = [c[0], p2n, p1n, p0n, q0n, q1n, q2n, c[7]];
        store16x8(a, stride as isize, b, stride as isize, out);
    } else {
        let mut rows = [_mm_setzero_si128(); 8];
        for i in 0..8 {
            rows[i] = _mm_loadl_epi64(base.add(center - 4 + i * stride) as *const __m128i);
        }
        let cols0 = transpose8x8(rows);
        let (p2n, p1n, p0n, q0n, q1n, q2n) =
            filter_lanes(
                cols0[0], cols0[1], cols0[2], cols0[3], cols0[4], cols0[5], cols0[6], cols0[7],
                kind, interior, edge, hev,
            );
        let cols1 = [cols0[0], p2n, p1n, p0n, q0n, q1n, q2n, cols0[7]];
        let out = transpose8x8(cols1);
        for i in 0..8 {
            _mm_storel_epi64(base.add(center - 4 + i * stride) as *mut __m128i, out[i]);
        }
    }
    }
}

#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn macroblock_store16(
    q0p: *mut u8,
    stride: usize,
    kind: FilterKind,
    p2: __m128i,
    p1: __m128i,
    p0: __m128i,
    q0: __m128i,
    q1: __m128i,
    q2: __m128i,
) {
    use std::arch::x86_64::*;
    unsafe {
    match kind {
        FilterKind::Simple => {
            _mm_storeu_si128(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storeu_si128(q0p as *mut __m128i, q0);
        }
        FilterKind::Macroblock => {
            _mm_storeu_si128(q0p.sub(3 * stride) as *mut __m128i, p2);
            _mm_storeu_si128(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storeu_si128(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storeu_si128(q0p as *mut __m128i, q0);
            _mm_storeu_si128(q0p.add(stride) as *mut __m128i, q1);
            _mm_storeu_si128(q0p.add(2 * stride) as *mut __m128i, q2);
        }
        FilterKind::Inner => {
            _mm_storeu_si128(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storeu_si128(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storeu_si128(q0p as *mut __m128i, q0);
            _mm_storeu_si128(q0p.add(stride) as *mut __m128i, q1);
        }
    }
    }
}

#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn macroblock_store8(
    q0p: *mut u8,
    stride: usize,
    kind: FilterKind,
    p2: __m128i,
    p1: __m128i,
    p0: __m128i,
    q0: __m128i,
    q1: __m128i,
    q2: __m128i,
) {
    use std::arch::x86_64::*;
    unsafe {
    match kind {
        FilterKind::Simple => {
            _mm_storel_epi64(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storel_epi64(q0p as *mut __m128i, q0);
        }
        FilterKind::Macroblock => {
            _mm_storel_epi64(q0p.sub(3 * stride) as *mut __m128i, p2);
            _mm_storel_epi64(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storel_epi64(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storel_epi64(q0p as *mut __m128i, q0);
            _mm_storel_epi64(q0p.add(stride) as *mut __m128i, q1);
            _mm_storel_epi64(q0p.add(2 * stride) as *mut __m128i, q2);
        }
        FilterKind::Inner => {
            _mm_storel_epi64(q0p.sub(2 * stride) as *mut __m128i, p1);
            _mm_storel_epi64(q0p.sub(stride) as *mut __m128i, p0);
            _mm_storel_epi64(q0p as *mut __m128i, q0);
            _mm_storel_epi64(q0p.add(stride) as *mut __m128i, q1);
        }
    }
    }
}

// ---------------------------------------------------------------------------
// D2v2（第三弹）：环路滤波 libwebp 风格 SSE2（16/8-lane，弃旧 16×8 全转置慢实现）
// ---------------------------------------------------------------------------
// 端口自 libwebp dec_sse2.c：同一算术（thresh 掩码、flatness、hev、2/4/6-tap），逐字节对齐
// vp8.rs 三个标量滤波器。水平边（our vertical=false）连续字节行直接装入；竖直边
// （our vertical=true）用 Load16x4/Store16x4 紧凑转置。delta 在翻转符号域做饱和算术
// （减法翻转不变 → delta 为原始有符号值）；p0/q0 加/减在翻转域 saturating => 等价 [0,255] clamp。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_abs(a: __m128i, b: __m128i) -> __m128i {
    _mm_or_si128(_mm_subs_epu8(a, b), _mm_subs_epu8(b, a))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn lf_needs_filter(mask: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, thresh: i32) -> __m128i { unsafe {
    let zero = _mm_setzero_si128();
    let t1 = lf_abs(p1, q1);
    let t2 = _mm_and_si128(t1, _mm_set1_epi8(0xFEu8 as i8));
    let t3 = _mm_srli_epi16(t2, 1);
    let t4 = lf_abs(p0, q0);
    let t5 = _mm_adds_epu8(_mm_adds_epu8(t4, t4), t3);
    let t6 = _mm_subs_epu8(t5, _mm_set1_epi8(thresh as i8));
    _mm_and_si128(_mm_cmpeq_epi8(t6, zero), mask)
}}

/// 平坦度：`mask` 与 `max(|p3-p2|,|p2-p1|,|p1-p0|)` 再与 `max(...,|q1-q0|,|q2-q1|,|q3-q2|)` <= ithresh。
/// 休眠：D2v2 平坦度计算未接入生产（libwebp 内联进 6-tap，本入口仅测试/探针用）。
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
#[target_feature(enable = "sse2")]
unsafe fn lf_flat(mask: __m128i, max_diff: __m128i, ithresh: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let t = _mm_subs_epu8(max_diff, _mm_set1_epi8(ithresh as i8));
    _mm_and_si128(_mm_cmpeq_epi8(t, zero), mask)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_not_hev(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, hev: i32) -> __m128i { unsafe {
    let zero = _mm_setzero_si128();
    let mx = _mm_max_epu8(lf_abs(p1, p0), lf_abs(q1, q0));
    let t = _mm_subs_epu8(mx, _mm_set1_epi8(hev as i8));
    _mm_cmpeq_epi8(t, zero)
}}

/// base delta（输入为翻转符号域）：p1-q1 + 3(q0-p0) 饱和 = 标量 clamp(clamp(p1-q1)+3(q0-p0))。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_base_delta(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i) -> __m128i {
    let p1_q1 = _mm_subs_epi8(p1, q1);
    let q0_p0 = _mm_subs_epi8(q0, p0);
    let s1 = _mm_adds_epi8(p1_q1, q0_p0);
    let s2 = _mm_adds_epi8(q0_p0, s1);
    
    _mm_adds_epi8(q0_p0, s2)
}

/// 翻转符号域饱和加/减（等价标量 [0,255] clamp 后的 p/q 更新）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_add_byte(v: __m128i, d: __m128i, sb: __m128i) -> __m128i {
    _mm_xor_si128(_mm_adds_epi8(_mm_xor_si128(v, sb), d), sb)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_sub_byte(v: __m128i, d: __m128i, sb: __m128i) -> __m128i {
    _mm_xor_si128(_mm_subs_epi8(_mm_xor_si128(v, sb), d), sb)
}

/// 翻转符号域饱和加/减（等价标量 [0,255] clamp 后的 p/q 写回）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_flip_add(v: __m128i, d: __m128i, sb: __m128i) -> __m128i { unsafe {
    lf_add_byte(v, d, sb)
}}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_flip_sub(v: __m128i, d: __m128i, sb: __m128i) -> __m128i { unsafe {
    lf_sub_byte(v, d, sb)
}}

/// 水平边行向量：读指针 `p` 处 `n` 连续字节（n=16 或 8）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_row(b: *const u8, n: usize) -> __m128i { unsafe {
    if n == 16 {
        _mm_loadu_si128(b as *const __m128i)
    } else {
        _mm_loadl_epi64(b as *const __m128i)
    }
}}

/// 简单 2-tap（mask lane）：p0 += (a+3)>>3、q0 -= (a+4)>>3。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_simple2(a: __m128i, p0: __m128i, q0: __m128i) -> (__m128i, __m128i) { unsafe {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let pa = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(3)), sb);
    let qa = shift_right3(_mm_adds_epi8(a, _mm_set1_epi8(4)), sb);
    (lf_add_byte(p0, pa, sb), lf_sub_byte(q0, qa, sb))
}}

/// Load8x4：读 8 行 × 4 列（围绕竖直边），返回 (col0 的 8 行 + col1 的 8 行) 与 (col2+col3)。
/// 端口 libwebp `Load8x4_SSE2`（WebPMemToInt32 等价 read_unaligned<i32>）。
/// dormancy：已被 `lf_edge_v16` 的全向量 `load16x8` 取代（K24），保留供对照/回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[cfg_attr(not(test), allow(dead_code))]
unsafe fn lf_load8x4(b: *const u8, stride: isize) -> (__m128i, __m128i) { unsafe {
    use std::arch::x86_64::*;
    let r = |i: isize| -> i32 {
        let p = b.offset(i * stride);
        let mut v = [0u8; 4];
        std::ptr::copy_nonoverlapping(p, v.as_mut_ptr(), 4);
        i32::from_le_bytes(v)
    };
    let a0 = _mm_set_epi32(r(6), r(2), r(4), r(0));
    let a1 = _mm_set_epi32(r(7), r(3), r(5), r(1));
    let b0 = _mm_unpacklo_epi8(a0, a1);
    let b1 = _mm_unpackhi_epi8(a0, a1);
    let c0 = _mm_unpacklo_epi16(b0, b1);
    let c1 = _mm_unpackhi_epi16(b0, b1);
    (
        _mm_unpacklo_epi32(c0, c1),
        _mm_unpackhi_epi32(c0, c1),
    )
}}

/// Load16x4：读 16 行 × 4 列（竖直边 p1/p0/q0/q1 四列），返回四列各 16 字节 lane。
/// 端口 libwebp `Load16x4_SSE2`。
/// dormancy：已被 `lf_edge_v16` 的全向量 `load16x8` 取代（K24），保留供对照/回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[cfg_attr(not(test), allow(dead_code))]
unsafe fn lf_load16x4(b: *const u8, stride: isize) -> (__m128i, __m128i, __m128i, __m128i) { unsafe {
    use std::arch::x86_64::*;
    // r0=行0..7 的四列，r8=行8..15。
    let (p1lo, q0lo) = lf_load8x4(b, stride);
    let (p0hi, q1hi) = lf_load8x4(b.offset(8 * stride), stride);
    let t1 = p1lo;
    let t2 = q0lo;
    (
        _mm_unpacklo_epi64(t1, p0hi),
        _mm_unpackhi_epi64(t1, p0hi),
        _mm_unpacklo_epi64(t2, q1hi),
        _mm_unpackhi_epi64(t2, q1hi),
    )
}}

/// Store4x4：把 4 个 lane 写回 4 行 × 4 列（每行一个 i32）。
/// dormancy：已被 `lf_edge_v16` 的全向量 `store16x8` 取代（K24），保留供对照/回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[cfg_attr(not(test), allow(dead_code))]
unsafe fn lf_store4x4(x: &mut __m128i, dst: *mut u8, stride: isize) { unsafe {
    use std::arch::x86_64::*;
    for i in 0..4isize {
        let v = _mm_cvtsi128_si32(*x);
        std::ptr::write_unaligned(dst.offset(i * stride) as *mut i32, v);
        *x = _mm_srli_si128(*x, 4);
    }
}}

/// Store16x4：4 个列向量（c0..c3 = col0..col3，各 16 行字节）写回 16 行。端口 libwebp `Store16x4_SSE2`。
/// 注意 unpack 顺序与 C 一致：先 (col0,col1) 与 (col2,col3) 按字节交错，再按 i16 交错还原 4×4。
/// dormancy：已被 `lf_edge_v16` 的全向量 `store16x8` 取代（K24），保留供对照/回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[cfg_attr(not(test), allow(dead_code))]
unsafe fn lf_store16x4(
    c0: &mut __m128i, c1: &mut __m128i, c2: &mut __m128i, c3: &mut __m128i,
    r0: *mut u8, r8: *mut u8, stride: isize,
) { unsafe {
    let t1 = *c1;
    let a = _mm_unpacklo_epi8(*c0, t1);
    let b = _mm_unpackhi_epi8(*c0, t1);
    let t1 = *c3;
    let c = _mm_unpacklo_epi8(*c2, t1);
    let d = _mm_unpackhi_epi8(*c2, t1);
    let t1 = a;
    let mut e = _mm_unpacklo_epi16(t1, c);
    let mut f = _mm_unpackhi_epi16(t1, c);
    let t1 = b;
    let mut g = _mm_unpacklo_epi16(t1, d);
    let mut h = _mm_unpackhi_epi16(t1, d);
    lf_store4x4(&mut e, r0, stride);
    lf_store4x4(&mut f, r0.offset(4 * stride), stride);
    lf_store4x4(&mut g, r8, stride);
    lf_store4x4(&mut h, r8.offset(4 * stride), stride);
}}

/// DoFilter2（简单边）：NeedsFilter 掩码 lane 只更新 p0/q0（thresh=`edge`）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn lf_do_filter2(p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, edge: i32) -> (__m128i, __m128i) { unsafe {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let mask = lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge);
    let a = _mm_and_si128(
        lf_base_delta(_mm_xor_si128(p1, sb), _mm_xor_si128(p0, sb), _mm_xor_si128(q0, sb), _mm_xor_si128(q1, sb)),
        mask,
    );
    lf_simple2(a, p0, q0)
}}

/// DoFilter4（内部边）：delta=hev(p1-q1)+3(q0-p0)；p0/q0 2-tap；!hev 时 p1/q1 (qa+1)>>1。mask 已复合。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn lf_do_filter4(
    p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, mask: __m128i, hev: i32,
) -> (__m128i, __m128i, __m128i, __m128i) { unsafe {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let not_hev = lf_not_hev(p1, p0, q0, q1, hev);
    let p1s = _mm_xor_si128(p1, sb);
    let p0s = _mm_xor_si128(p0, sb);
    let q0s = _mm_xor_si128(q0, sb);
    let q1s = _mm_xor_si128(q1, sb);
    let mut t1 = _mm_subs_epi8(p1s, q1s);
    t1 = _mm_andnot_si128(not_hev, t1);
    let t2 = _mm_subs_epi8(q0s, p0s);
    t1 = _mm_adds_epi8(_mm_adds_epi8(_mm_adds_epi8(t1, t2), t2), t2);
    t1 = _mm_and_si128(t1, mask);
    let (p0n, q0n) = lf_simple2(t1, p0, q0);
    // !hev lane：inn = (qa + 1) >> 1（qa = (a+4)>>3 raw；标量同名恒等式）。
    let qa = shift_right3(_mm_adds_epi8(t1, _mm_set1_epi8(4)), sb);
    let inn = _mm_and_si128(shift_right1(_mm_adds_epi8(qa, _mm_set1_epi8(1)), sb), not_hev);
    let p1n = lf_flip_add(p1, inn, sb);
    let q1n = lf_flip_sub(q1, inn, sb);
    (p1n, p0n, q0n, q1n)
}}

/// DoFilter6（宏块边）：hev lane 2-tap；!hev lane 27/18/9 权重（f×9/18/27+63）>>7 铺 p2..q2。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn lf_do_filter6(
    p2: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, q2: __m128i,
    mask: __m128i, hev: i32,
) -> (__m128i, __m128i, __m128i, __m128i, __m128i, __m128i) { unsafe {
    let sb = _mm_set1_epi8(0x80u8 as i8);
    let zero = _mm_setzero_si128();
    let not_hev = lf_not_hev(p1, p0, q0, q1, hev);
    let a = lf_base_delta(
        _mm_xor_si128(p1, sb), _mm_xor_si128(p0, sb),
        _mm_xor_si128(q0, sb), _mm_xor_si128(q1, sb),
    );
    let m_hv = _mm_andnot_si128(not_hev, mask);
    let a_hv = _mm_and_si128(a, m_hv);
    let pa_hv = shift_right3(_mm_adds_epi8(a_hv, _mm_set1_epi8(3)), sb);
    let qa_hv = shift_right3(_mm_adds_epi8(a_hv, _mm_set1_epi8(4)), sb);
    let m_not = _mm_and_si128(not_hev, mask);
    let f = _mm_and_si128(a, m_not);
    // f（i8）放 i16 高字节（unpacklo_epi8(zero, f)）→ mulhi ×0x0900 = f×9 精确。
    let k9 = _mm_set1_epi16(0x0900i16);
    let k63 = _mm_set1_epi16(63);
    let f_lo = _mm_unpacklo_epi8(zero, f);
    let f_hi = _mm_unpackhi_epi8(zero, f);
    let f9_lo = _mm_mulhi_epi16(f_lo, k9);
    let f9_hi = _mm_mulhi_epi16(f_hi, k9);
    let a9_lo = _mm_add_epi16(f9_lo, k63);
    let a9_hi = _mm_add_epi16(f9_hi, k63);
    let a18_lo = _mm_add_epi16(a9_lo, f9_lo);
    let a18_hi = _mm_add_epi16(a9_hi, f9_hi);
    let a27_lo = _mm_add_epi16(a18_lo, f9_lo);
    let a27_hi = _mm_add_epi16(a18_hi, f9_hi);
    let s9 = _mm_packs_epi16(_mm_srai_epi16(a9_lo, 7), _mm_srai_epi16(a9_hi, 7));
    let s18 = _mm_packs_epi16(_mm_srai_epi16(a18_lo, 7), _mm_srai_epi16(a18_hi, 7));
    let s27 = _mm_packs_epi16(_mm_srai_epi16(a27_lo, 7), _mm_srai_epi16(a27_hi, 7));
    let p2n = lf_flip_add(p2, s9, sb);
    let p1n = lf_flip_add(p1, s18, sb);
    let q2n = lf_flip_sub(q2, s9, sb);
    let q1n = lf_flip_sub(q1, s18, sb);
    // p0/q0：hev 增量（pa/qa_hv，仅 hev∧mask lane 非零）与 6-tap 增量（s27，仅 !hev∧mask lane 非零）
    // 逐 lane 互斥 → 直接相加（非掩码 lane 两者皆 0 → 原值不变）。杜绝 OR 合并把非掩码 lane 清零。
    let p0n = lf_flip_add(p0, _mm_add_epi8(pa_hv, s27), sb);
    let q0n = lf_flip_sub(q0, _mm_add_epi8(qa_hv, s27), sb);
    (p2n, p1n, p0n, q0n, q1n, q2n)
}}

/// 复合掩码：`filter`（thresh 判定）∧ 平坦度（max_diff <= ithresh）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn lf_mask_composite(filter: __m128i, max_diff: __m128i, ithresh: i32) -> __m128i {
    let zero = _mm_setzero_si128();
    let t = _mm_subs_epu8(max_diff, _mm_set1_epi8(ithresh as i8));
    _mm_and_si128(filter, _mm_cmpeq_epi8(t, zero))
}

/// 平面内平坦度（MB/inner 共用六邻绝对差的最大值）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn lf_max_diff(p3: __m128i, p2: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i, q2: __m128i, q3: __m128i) -> __m128i { unsafe {
    
    _mm_max_epu8(
        _mm_max_epu8(_mm_max_epu8(lf_abs(p3, p2), lf_abs(p2, p1)), lf_abs(p1, p0)),
        _mm_max_epu8(_mm_max_epu8(lf_abs(q0, q1), lf_abs(q1, q2)), lf_abs(q2, q3)),
    )
}}

/// 竖直边（our vertical=true）16-wide。`pt` 指向 (行 y, 帧列 x-4)（p3 列；simple 只更新 p0/q0）。
/// 全向量 16×8 转置梯（`load16x8`/`store16x8`，同 chroma `lf_edge_v8_pair` 姿势）：一次装载
/// pt..pt+7 全部 8 列（p3..q3）、滤波后一次写回——替换旧 `lf_load16x4`/`lf_store16x4`（其内部
/// `lf_load8x4`/`lf_store4x4` 是逐行标量 i32 存取，每 MB 4 条竖边 = 64 标量 load + 64 store）。
/// 列向量布局与旧路径逐元素一致（c[j] 低 8 字节 = 行 0..7、高 8 = 行 8..15），且 `store16x8`
/// 是 `load16x8` 的精确逆 → 同一值集合、同一写址 → 输出逐位不变。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn lf_edge_v16(
    pt: *mut u8, stride: isize,
    interior: i32, edge: i32, hev: i32, macroblock_edge: bool, simple: bool,
) { unsafe {
    let mut c = load16x8(pt, stride, pt.offset(8 * stride), stride);
    if simple {
        let (p0n, q0n) = lf_do_filter2(c[2], c[3], c[4], c[5], edge);
        c[3] = p0n;
        c[4] = q0n;
        store16x8(pt, stride, pt.offset(8 * stride), stride, c);
        return;
    }
    let mx = lf_max_diff(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
    let mask = lf_mask_composite(lf_needs_filter(_mm_set1_epi8(-1), c[2], c[3], c[4], c[5], edge), mx, interior);
    // 全零 mask：任何 lane 都无需改（do_filter4/6 的 delta 逐 lane 被 mask 清零 → 输出==输入），
    // 跳过 store（c 原值不变则存回既存既等），字节恒等平凡成立。movemask 早退保留（66fcbc9）。
    if _mm_movemask_epi8(mask) == 0 {
        return;
    }
    if macroblock_edge {
        let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(c[1], c[2], c[3], c[4], c[5], c[6], mask, hev);
        c[1] = p2n;
        c[2] = p1n;
        c[3] = p0n;
        c[4] = q0n;
        c[5] = q1n;
        c[6] = q2n;
    } else {
        let (p1n, p0n, q0n, q1n) = lf_do_filter4(c[2], c[3], c[4], c[5], mask, hev);
        c[2] = p1n;
        c[3] = p0n;
        c[4] = q0n;
        c[5] = q1n;
    }
    store16x8(pt, stride, pt.offset(8 * stride), stride, c);
}}

/// 竖直边（our vertical=true）8-wide（chroma per-edge / 测试入口，C# Load8x8/Store8x8 同款）。
/// `pt` 指向 (行 y, 帧列 x-4)（p3 列；simple 只更新 p0/q0）。全向量 8×8 转置梯（`transpose8x8`，
/// 矩阵转置自身逆，单测 round-trip 断言）：一次装载 pt..pt+7 全部 8 列（p3..q3）8 行、滤波后一次
/// 写回——替换 `lf_filter_edge_sse2` 里 8-wide 的标量回退 `vp8::filter_edge`（逐行逐列标量字节
/// 读写）。列向量布局与 `lf_edge_v16` 低 8 字节半区同轴（c[j] 低 8 字节 = 行 0..7 列 j）；
/// `transpose8x8` 用 `_mm_unpacklo_epi64(c, zero)` 把高 8 lane 恒置 0 → 高 lane 恒过 mask 但
/// delta 恒 0（lf_do_filter4/6 非 mask lane 增量 0 → 原值），写回只取低 8 字节 → 输出逐位不变。
/// movemask 早退只采低 8 位（`& 0xFF`），高 lane 的「恒需滤」不参与「整列全无需改」判定。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_v8(
    pt: *mut u8, stride: isize,
    interior: i32, edge: i32, hev: i32, macroblock_edge: bool, simple: bool,
) { unsafe {
    let mut rows = [_mm_setzero_si128(); 8];
    for i in 0..8isize {
        rows[i as usize] = _mm_loadl_epi64(pt.offset(i * stride) as *const __m128i);
    }
    let mut c = transpose8x8(rows);
    if simple {
        let (p0n, q0n) = lf_do_filter2(c[2], c[3], c[4], c[5], edge);
        c[3] = p0n;
        c[4] = q0n;
    } else {
        let mx = lf_max_diff(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
        let mask = lf_mask_composite(
            lf_needs_filter(_mm_set1_epi8(-1), c[2], c[3], c[4], c[5], edge),
            mx,
            interior,
        );
        // 只采低 8 位：低 8（真实 8 列）全无需改 → 跳过 store（高 lane 恒 0 恒过 mask，不参与判定）。
        if _mm_movemask_epi8(mask) & 0xFF == 0 {
            return;
        }
        if macroblock_edge {
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                lf_do_filter6(c[1], c[2], c[3], c[4], c[5], c[6], mask, hev);
            c[1] = p2n;
            c[2] = p1n;
            c[3] = p0n;
            c[4] = q0n;
            c[5] = q1n;
            c[6] = q2n;
        } else {
            let (p1n, p0n, q0n, q1n) = lf_do_filter4(c[2], c[3], c[4], c[5], mask, hev);
            c[2] = p1n;
            c[3] = p0n;
            c[4] = q0n;
            c[5] = q1n;
        }
    }
    let out = transpose8x8(c);
    for i in 0..8isize {
        _mm_storel_epi64(pt.offset(i * stride) as *mut __m128i, out[i as usize]);
    }
}}

/// 水平边（our vertical=false）16/8-wide。`pt` 指向 (行 y, 帧列 x)（q0 行首）；n=16/8。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn lf_edge_h(
    pt: *mut u8, stride: isize, n: usize,
    interior: i32, edge: i32, hev: i32, macroblock_edge: bool, simple: bool,
) { unsafe {
    let p3 = lf_row(pt.offset(-4 * stride), n);
    let p2 = lf_row(pt.offset(-3 * stride), n);
    let p1 = lf_row(pt.offset(-2 * stride), n);
    let p0 = lf_row(pt.offset(-stride), n);
    let q0 = lf_row(pt.offset(0), n);
    let q1 = lf_row(pt.offset(stride), n);
    let q2 = lf_row(pt.offset(2 * stride), n);
    let q3 = lf_row(pt.offset(3 * stride), n);
    if simple {
        let (p0n, q0n) = lf_do_filter2(p1, p0, q0, q1, edge);
        std::ptr::copy_nonoverlapping(&p0n as *const __m128i as *const u8, pt.offset(-stride), n);
        std::ptr::copy_nonoverlapping(&q0n as *const __m128i as *const u8, pt.offset(0), n);
        return;
    }
    let mx = lf_max_diff(p3, p2, p1, p0, q0, q1, q2, q3);
    let mask = lf_mask_composite(lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge), mx, interior);
    // 全零 mask：任何 lane 都无需改（do_filter 的 delta 逐 lane 被清 → 输出==输入），跳过 store。
    if _mm_movemask_epi8(mask) == 0 {
        return;
    }
    if macroblock_edge {
        let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(p2, p1, p0, q0, q1, q2, mask, hev);
        let rows = [(p2n, -3isize), (p1n, -2), (p0n, -1), (q0n, 0), (q1n, 1), (q2n, 2)];
        for (v, off) in rows {
            std::ptr::copy_nonoverlapping(&v as *const __m128i as *const u8, pt.offset(off * stride), n);
        }
    } else {
        let (p1n, p0n, q0n, q1n) = lf_do_filter4(p1, p0, q0, q1, mask, hev);
        let rows = [(p1n, -2isize), (p0n, -1), (q0n, 0), (q1n, 1)];
        for (v, off) in rows {
            std::ptr::copy_nonoverlapping(&v as *const __m128i as *const u8, pt.offset(off * stride), n);
        }
    }
}}

/// 新环路滤波入口（libwebp 风格 SSE2）：语义与 vp8::filter_edge 相同。8-wide 竖直边暂保留标量。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn lf_filter_edge_sse2(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) { unsafe {
    let base = plane.as_mut_ptr();
    let center = (y * stride + 1 + x) as isize;
    let s = stride as isize;
    if vertical {
        if length == 16 {
            lf_edge_v16(base.offset(center - 4), s, interior, edge, hev, macroblock_edge, simple);
        } else {
            lf_edge_v8(base.offset(center - 4), s, interior, edge, hev, macroblock_edge, simple);
        }
    } else {
        lf_edge_h(base.offset(center), s, length, interior, edge, hev, macroblock_edge, simple);
    }
}}


// ---------------------------------------------------------------------------
// D2v3：宏块级熔合环路滤波（C# `Vp8Decoder.FilterMacroblock` + `Vp8LoopFilter` 同款）
// ---------------------------------------------------------------------------
// 一个宏块把全 12 个核熔合成一次调用：luma 竖直×4、chroma 竖直×2（U/V 打包成 16-lane）、
// luma 水平×4、chroma 水平×2（U/V 打包）。边次序与 vp8.rs `filter_macroblock_row` 标量完全
// 一致（左 MB 竖 → 内竖×3 → 上 MB 横 → 内横×3），熔合只改调用形态、不改任何算术/读序 →
// 逐字节相同。U/V 打包把两平面拼一个向量：lane 逐样本独立，不做任何跨 U/V 混合，纯省调用与
// 向量粒度；掩码 lane 结果为原值，故「从不对 lane 跳过」也保证未滤 lane 字节不变。

/// 读一条 16-lane 色度行（低 8 = U、高 8 = V，同偏移行各 8 字节）—— C# `LoadPair` 同款。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_uv_pair_row(u: *const u8, v: *const u8, off: isize) -> __m128i { unsafe {
    let ul = _mm_loadl_epi64(u.offset(off) as *const __m128i);
    let vl = _mm_loadl_epi64(v.offset(off) as *const __m128i);
    _mm_unpacklo_epi64(ul, vl)
}}

/// 写回一条 16-lane 色度行（低 8 → U、高 8 → V）—— C# `StorePair` 同款。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn lf_uv_pair_store(u: *mut u8, v: *mut u8, off: isize, val: __m128i) { unsafe {
    _mm_storel_epi64(u.offset(off) as *mut __m128i, val);
    _mm_storel_epi64(v.offset(off) as *mut __m128i, _mm_srli_si128(val, 8));
}}

/// 竖直色度边（U/V 打包，8+8=16 lane）：`u`/`v` 指向 q0 列（行 uv_row、列 uvx），每侧 8 行 8 列
/// 经 Load16x8/Store16x8 转置成 8 列、每列 16 lane（低 8 = U 行、高 8 = V 行）一次滤完 ——
/// C# `ChromaVertical` 同款。补齐 D2v2 `lf_filter_edge_sse2` 的 8 宽竖直标量回退。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_v8_pair(
    u: *mut u8,
    v: *mut u8,
    stride: isize,
    interior: i32,
    edge: i32,
    hev: i32,
    macroblock_edge: bool,
    simple: bool,
) { unsafe {
    let ul = u.offset(-4);
    let vl = v.offset(-4);
    let mut c = load16x8(ul, stride, vl, stride);
    if simple {
        let (p0n, q0n) = lf_do_filter2(c[2], c[3], c[4], c[5], edge);
        c[3] = p0n;
        c[4] = q0n;
    } else {
        let mx = lf_max_diff(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
        let mask = lf_mask_composite(
            lf_needs_filter(_mm_set1_epi8(-1), c[2], c[3], c[4], c[5], edge),
            mx,
            interior,
        );
        // 全零 mask：任何 lane 都无需改（输出==输入），跳过 store（c 原值不变则存回既存既等）。
        if _mm_movemask_epi8(mask) == 0 {
            return;
        }
        if macroblock_edge {
            let (p2n, p1n, p0n, q0n, q1n, q2n) =
                lf_do_filter6(c[1], c[2], c[3], c[4], c[5], c[6], mask, hev);
            c[1] = p2n;
            c[2] = p1n;
            c[3] = p0n;
            c[4] = q0n;
            c[5] = q1n;
            c[6] = q2n;
        } else {
            let (p1n, p0n, q0n, q1n) = lf_do_filter4(c[2], c[3], c[4], c[5], mask, hev);
            c[2] = p1n;
            c[3] = p0n;
            c[4] = q0n;
            c[5] = q1n;
        }
    }
    store16x8(ul, stride, vl, stride, c);
}}

/// 水平色度边（U/V 打包）：`u`/`v` 指向 q0 行（行 uv_row±k、列 uvx），同偏移行各 8 字节拼一个
/// 16-lane 向量一次滤完 —— C# `ChromaHorizontal` 同款 LoadPair/StorePair。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn lf_edge_h_pair(
    u: *mut u8,
    v: *mut u8,
    stride: isize,
    interior: i32,
    edge: i32,
    hev: i32,
    macroblock_edge: bool,
    simple: bool,
) {
    unsafe {
        if simple {
            let (p0n, q0n) = lf_do_filter2(
                lf_uv_pair_row(u, v, -2 * stride),
                lf_uv_pair_row(u, v, -stride),
                lf_uv_pair_row(u, v, 0),
                lf_uv_pair_row(u, v, stride),
                edge,
            );
            lf_uv_pair_store(u, v, -stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            return;
        }
        let p3 = lf_uv_pair_row(u, v, -4 * stride);
        let p2 = lf_uv_pair_row(u, v, -3 * stride);
        let p1 = lf_uv_pair_row(u, v, -2 * stride);
        let p0 = lf_uv_pair_row(u, v, -stride);
        let q0 = lf_uv_pair_row(u, v, 0);
        let q1 = lf_uv_pair_row(u, v, stride);
        let q2 = lf_uv_pair_row(u, v, 2 * stride);
        let q3 = lf_uv_pair_row(u, v, 3 * stride);
        let mx = lf_max_diff(p3, p2, p1, p0, q0, q1, q2, q3);
        let mask = lf_mask_composite(
            lf_needs_filter(_mm_set1_epi8(-1), p1, p0, q0, q1, edge),
            mx,
            interior,
        );
        // 全零 mask：任何 lane 都无需改（输出==输入），跳过 store。
        if _mm_movemask_epi8(mask) == 0 {
            return;
        }
        if macroblock_edge {
            let (p2n, p1n, p0n, q0n, q1n, q2n) = lf_do_filter6(p2, p1, p0, q0, q1, q2, mask, hev);
            lf_uv_pair_store(u, v, -3 * stride, p2n);
            lf_uv_pair_store(u, v, -2 * stride, p1n);
            lf_uv_pair_store(u, v, -stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            lf_uv_pair_store(u, v, stride, q1n);
            lf_uv_pair_store(u, v, 2 * stride, q2n);
        } else {
            let (p1n, p0n, q0n, q1n) = lf_do_filter4(p1, p0, q0, q1, mask, hev);
            lf_uv_pair_store(u, v, -2 * stride, p1n);
            lf_uv_pair_store(u, v, -stride, p0n);
            lf_uv_pair_store(u, v, 0, q0n);
            lf_uv_pair_store(u, v, stride, q1n);
        }
    }
}

/// 宏块级熔合环路滤波入口（生产；C# `Vp8Decoder.FilterMacroblock` 同款）。一次宏块完成
/// 全 12 个 16-lane 核调用，边次序与标量 `vp8::filter_macroblock_row` 完全一致，逐字节相同。
/// 参数为滤波强度实值（调用方已算好 interior/edge_limit/sub_limit/hev）。
/// 指针语义：`yc` 指向 (行 mby*16、列 mbx*16) 的 q0（= center = y*pitch+1+x），`uc`/`vc` 同理
/// 指向色度 q0；跨块读（左列 -4 / 上行 -4）落在解码平面 4 像素边距内（与标量同界）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn lf_filter_macroblock(
    yc: *mut u8,
    y_pitch: isize,
    uc: *mut u8,
    vc: *mut u8,
    uv_pitch: isize,
    left: bool,
    top: bool,
    inner: bool,
    simple: bool,
    interior: i32,
    edge_limit: i32,
    sub_limit: i32,
    hev: i32,
) { unsafe {
    // 竖直段：左 MB 边 → 内竖×3（与标量同序：后边读前边已滤像素）。
    if left {
        lf_edge_v16(yc.offset(-4), y_pitch, interior, edge_limit, hev, true, simple);
        if !simple {
            lf_edge_v8_pair(uc, vc, uv_pitch, interior, edge_limit, hev, true, false);
        }
    }
    if inner {
        lf_edge_v16(yc, y_pitch, interior, sub_limit, hev, false, simple);
        lf_edge_v16(yc.offset(4), y_pitch, interior, sub_limit, hev, false, simple);
        lf_edge_v16(yc.offset(8), y_pitch, interior, sub_limit, hev, false, simple);
        if !simple {
            lf_edge_v8_pair(uc.offset(4), vc.offset(4), uv_pitch, interior, sub_limit, hev, false, false);
        }
    }
    // 水平段：上 MB 边 → 内横×3。
    if top {
        lf_edge_h(yc, y_pitch, 16, interior, edge_limit, hev, true, simple);
        if !simple {
            lf_edge_h_pair(uc, vc, uv_pitch, interior, edge_limit, hev, true, false);
        }
    }
    if inner {
        lf_edge_h(yc.offset(4 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        lf_edge_h(yc.offset(8 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        lf_edge_h(yc.offset(12 * y_pitch), y_pitch, 16, interior, sub_limit, hev, false, simple);
        if !simple {
            lf_edge_h_pair(
                uc.offset(4 * uv_pitch),
                vc.offset(4 * uv_pitch),
                uv_pitch,
                interior,
                sub_limit,
                hev,
                false,
                false,
            );
        }
    }
}}



