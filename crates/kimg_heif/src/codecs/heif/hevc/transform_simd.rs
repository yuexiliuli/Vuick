//! HEIF 残差逆变换（`transform.rs` `inverse_add_scalar`）的向量版，与标量逐位字节恒等：
//! - `inverse_add_avx2`：n≥8，AVX2 8-lane（DCT 偶/奇捷径），i32 载荷，行主 8 列/步进；
//! - `inverse_add_sse41`：4-lane（n 为 4 的倍数），含 DST 4 点（`dst4=true`）与 DCT 捷径。
//!
//! 字节恒等纪律（与 C# `HevcTransform.cs` Vector128/Vector256 层逐位一致）：
//! - 垂直遍（行→中间）在 i32 内精确累加，(sum+64)>>7 先移后钳 [-32768, 32767]（算术 `>>`）；
//! - 水平遍 pred+残差在 i32 内 clamp[0, max] 再窄化 u16 写回（禁 u16 饱和加；值已在 [0,2^bd) 内，
//!   窄化即精确）；
//! - 系数超出 maxX 列的乘积恒为零（调用方清零），向量把整 8/4 列都写进 `g` 同位，水平遍只消费
//!   j ≤ maxX 的列 → 输出与标量逐字节一致；
//! - 4-lane 内核需要 PMULLD/PACKUSDW（`_mm_mullo_epi32`/`_mm_packus_epi32` 皆 SSE4.1），故门控用
//!   `sse4.1`（纯 SSE2 CPU 回退标量，安全不崩）。
//!
//! 叶子隔离：本模块是唯一允许 unsafe 的下沉点（对标 `crates/kimg_webp/src/codecs/webp/simd/raw/`），
//! `transform.rs` 以 `#![forbid(unsafe_code)]` 薄委托分派到本模块。

#![allow(unsafe_code)]
#![cfg(target_arch = "x86_64")]

// 变换 SIMD 内核逐寄存器传参，具名构参破坏字节恒等算术单源性 → 放行。
#![allow(clippy::too_many_arguments)]
use crate::codecs::heif::error::{HeifError, HeifResult};
use std::arch::x86_64::*;

/// 与 scalar `inverse_add_scalar` 相同的块边界校验（核访问全部下标的证明）。
#[inline]
fn check_block(
    coeffs: &[i32],
    scratch: &[i32],
    n: usize,
    max_x: usize,
    max_y: usize,
    dst: &[u16],
    stride: usize,
) -> HeifResult<()> {
    let n2 = n * n;
    // CheckBlock：核访问的全部下标的证明。
    let needed = (n as i64 - 1) * stride as i64 + n as i64;
    if coeffs.len() < n2
        || scratch.len() < n2
        || max_x >= n
        || max_y >= n
        || stride < n
        || needed < 0
        || needed > dst.len() as i64
    {
        return Err(HeifError::invalid("变换块放不进缓冲。"));
    }
    Ok(())
}

/// 运行时位移量的算术右移（bd_shift = 20 - bit_depth ∈ 4..=12 for bit_depth 8..=16；垂直遍用 7）。
/// `#[target_feature]` 函数体对目标特性内建函数隐式 unsafe 上下文，无需显式块。
#[target_feature(enable = "avx2")]
unsafe fn srai256(v: __m256i, shift: i32) -> __m256i {
    match shift {
        1 => _mm256_srai_epi32::<1>(v),
        2 => _mm256_srai_epi32::<2>(v),
        3 => _mm256_srai_epi32::<3>(v),
        4 => _mm256_srai_epi32::<4>(v),
        5 => _mm256_srai_epi32::<5>(v),
        6 => _mm256_srai_epi32::<6>(v),
        7 => _mm256_srai_epi32::<7>(v),
        8 => _mm256_srai_epi32::<8>(v),
        9 => _mm256_srai_epi32::<9>(v),
        10 => _mm256_srai_epi32::<10>(v),
        11 => _mm256_srai_epi32::<11>(v),
        12 => _mm256_srai_epi32::<12>(v),
        _ => unreachable!("bd_shift 超出 1..=12"),
    }
}

/// 运行时位移量的算术右移（SSE4.1 4-lane 用）。
#[target_feature(enable = "sse4.1")]
unsafe fn srai128(v: __m128i, shift: i32) -> __m128i {
    match shift {
        1 => _mm_srai_epi32::<1>(v),
        2 => _mm_srai_epi32::<2>(v),
        3 => _mm_srai_epi32::<3>(v),
        4 => _mm_srai_epi32::<4>(v),
        5 => _mm_srai_epi32::<5>(v),
        6 => _mm_srai_epi32::<6>(v),
        7 => _mm_srai_epi32::<7>(v),
        8 => _mm_srai_epi32::<8>(v),
        9 => _mm_srai_epi32::<9>(v),
        10 => _mm_srai_epi32::<10>(v),
        11 => _mm_srai_epi32::<11>(v),
        12 => _mm_srai_epi32::<12>(v),
        _ => unreachable!("bd_shift 超出 1..=12"),
    }
}

/// AVX2 逆变换入口（n≥8，DCT 路径）。校验后调用内核。
#[allow(clippy::too_many_arguments)]
pub(super) fn inverse_add_avx2(
    n: usize,
    m: &[i32],
    coeffs: &[i32],
    scratch: &mut [i32],
    max_x: usize,
    max_y: usize,
    bit_depth: i32,
    dst: &mut [u16],
    stride: usize,
) -> HeifResult<()> {
    check_block(coeffs, scratch, n, max_x, max_y, dst, stride)?;
    // SAFETY: check_block 已证全部读写下标界内；n≥8 时 n 是 8 的倍数，垂直遍把不足 8 的 uneven
    // maxX 尾部列也算进（系数为 0、乘积同位），g 列超出 maxX 部分不被水平遍消费。
    unsafe {
        avx2_kernel(
            n,
            m.as_ptr(),
            coeffs.as_ptr(),
            scratch.as_mut_ptr(),
            max_x,
            max_y,
            bit_depth,
            dst.as_mut_ptr(),
            stride,
        )
    }
    Ok(())
}

/// SSE4.1 逆变换入口（4-lane；含 DST 4 点）。校验后调用内核。
#[allow(clippy::too_many_arguments)]
pub(super) fn inverse_add_sse41(
    n: usize,
    m: &[i32],
    dst4: bool,
    coeffs: &[i32],
    scratch: &mut [i32],
    max_x: usize,
    max_y: usize,
    bit_depth: i32,
    dst: &mut [u16],
    stride: usize,
) -> HeifResult<()> {
    check_block(coeffs, scratch, n, max_x, max_y, dst, stride)?;
    // SAFETY: check_block 已证全部读写下标界内；n 是 4 的倍数（4/8/16/32）。
    unsafe {
        sse41_kernel(
            n,
            m.as_ptr(),
            coeffs.as_ptr(),
            scratch.as_mut_ptr(),
            dst4,
            max_x,
            max_y,
            bit_depth,
            dst.as_mut_ptr(),
            stride,
        )
    }
    Ok(())
}

/// AVX2 8-lane 逆变换内核（DCT，n≥8，n 为 8 的倍数）。与 scalar `inverse_add_scalar` 逐位一致。
///
/// 访问界：coeﬀs/g/m 的下标至多 j·n + c·8 + 8 ≤ (n-1)·n + n = n²（j ≤ maxY|1 < n、c·8 ≤ n-8）；
/// dst 读写下标至多 (n-1)·stride + n（check_block 已证 ≤ dst.len()）。
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn avx2_kernel(
    n: usize,
    m: *const i32,
    coeffs: *const i32,
    g: *mut i32,
    max_x: usize,
    max_y: usize,
    bit_depth: i32,
    dst: *mut u16,
    stride: usize,
) {
    unsafe {
    let chunks = (max_x >> 3) + 1; // 垂直遍 8 列组数（uneven maxX 尾组也算，乘积为 0）
    let last_row = max_y | 1; // 偶 maxY 之后一行是零，奇数部分和可含它
    let lo16 = _mm256_set1_epi32(i32::from(i16::MIN));
    let hi16 = _mm256_set1_epi32(i32::from(i16::MAX));
    let r64 = _mm256_set1_epi32(64);

    // 垂直遍（行→中间）：偶/奇 j 部分和 E/O → g[y] = clamp((E+O+64)>>7)、g[n-1-y] = clamp((E-O+64)>>7)。
    let half = n >> 1;
    let mut y = 0usize;
    while y < half {
        let mut c = 0usize;
        while c < chunks {
            let mut even = _mm256_setzero_si256();
            let mut odd = _mm256_setzero_si256();
            let mut j = 0usize;
            while j <= last_row {
                let me = _mm256_set1_epi32(*m.add(j * n + y));
                let mo = _mm256_set1_epi32(*m.add((j + 1) * n + y));
                let de = _mm256_loadu_si256(coeffs.add(j * n + c * 8) as *const __m256i);
                let do_ = _mm256_loadu_si256(coeffs.add((j + 1) * n + c * 8) as *const __m256i);
                even = _mm256_add_epi32(even, _mm256_mullo_epi32(me, de));
                odd = _mm256_add_epi32(odd, _mm256_mullo_epi32(mo, do_));
                j += 2;
            }
            let up = _mm256_srai_epi32::<7>(_mm256_add_epi32(_mm256_add_epi32(even, odd), r64));
            let dn = _mm256_srai_epi32::<7>(_mm256_add_epi32(_mm256_sub_epi32(even, odd), r64));
            let gy = _mm256_min_epi32(_mm256_max_epi32(up, lo16), hi16);
            let gy2 = _mm256_min_epi32(_mm256_max_epi32(dn, lo16), hi16);
            _mm256_storeu_si256(g.add(y * n + c * 8) as *mut __m256i, gy);
            _mm256_storeu_si256(g.add((n - 1 - y) * n + c * 8) as *mut __m256i, gy2);
            c += 1;
        }
        y += 1;
    }

    // 水平遍（g 行 × m 列 → 残差）：(8-299) 与 (8-325)，pred+残差 i32 clamp[0,max] 后窄化 u16 写回。
    let bd_shift = 20 - bit_depth;
    let round = _mm256_set1_epi32(1i32 << (bd_shift - 1));
    let max = _mm256_set1_epi32((1 << bit_depth) - 1);
    let zero = _mm256_setzero_si256();
    let hchunks = n >> 3;
    let mut y = 0usize;
    while y < n {
        let mut c = 0usize;
        while c < hchunks {
            let mut sum = _mm256_setzero_si256();
            let mut j = 0usize;
            while j <= max_x {
                let gj = _mm256_set1_epi32(*g.add(y * n + j));
                let mj = _mm256_loadu_si256(m.add(j * n + c * 8) as *const __m256i);
                sum = _mm256_add_epi32(sum, _mm256_mullo_epi32(gj, mj));
                j += 1;
            }
            let residual = srai256(_mm256_add_epi32(sum, round), bd_shift);
            let target = dst.add(y * stride + c * 8);
            // 预测为 u16，宽化到 i32 后与残差相加、clamp，再窄化回 u16（值 ∈ [0, max]，窄化即精确）。
            let pred16 = _mm_loadu_si128(target as *const __m128i); // 8 u16
            let pred = _mm256_cvtepu16_epi32(pred16); // 8 x i32
            let value = _mm256_min_epi32(
                _mm256_max_epi32(_mm256_add_epi32(pred, residual), zero),
                max,
            );
            let lo = _mm256_castsi256_si128(value);
            let hi = _mm256_extracti128_si256(value, 1);
            _mm_storeu_si128(
                target as *mut __m128i,
                _mm_packus_epi32(lo, hi), // 低 4 + 高 4 → 8 u16
            );
            c += 1;
        }
        y += 1;
    }
    }
}

/// SSE4.1 4-lane 逆变换内核（n 为 4 的倍数；`dst4` 时走 DST 4 点路径、否则 DCT 偶/奇捷径）。
/// 与 scalar `inverse_add_scalar` 逐位一致；访问界证明同 `avx2_kernel`。
#[target_feature(enable = "sse4.1")]
unsafe fn sse41_kernel(
    n: usize,
    m: *const i32,
    coeffs: *const i32,
    g: *mut i32,
    dst4: bool,
    max_x: usize,
    max_y: usize,
    bit_depth: i32,
    dst: *mut u16,
    stride: usize,
) {
    unsafe {
    let lo16 = _mm_set1_epi32(i32::from(i16::MIN));
    let hi16 = _mm_set1_epi32(i32::from(i16::MAX));
    let r64 = _mm_set1_epi32(64);

    if dst4 {
        // 4×4 DST：无对称捷径，直接 accumulate over j ≤ maxY（每 y 一行 4 列）。
        for y in 0..4usize {
            let mut sum = _mm_setzero_si128();
            let mut j = 0usize;
            while j <= max_y {
                let mj = _mm_set1_epi32(*m.add(j * 4 + y));
                let dj = _mm_loadu_si128(coeffs.add(j * 4) as *const __m128i);
                sum = _mm_add_epi32(sum, _mm_mullo_epi32(mj, dj));
                j += 1;
            }
            let v = _mm_srai_epi32::<7>(_mm_add_epi32(sum, r64));
            let v = _mm_min_epi32(_mm_max_epi32(v, lo16), hi16);
            _mm_storeu_si128(g.add(y * 4) as *mut __m128i, v);
        }
    } else {
        // DCT 偶/奇捷径（同 scalar）。
        let chunks = (max_x >> 2) + 1;
        let last_row = max_y | 1;
        for y in 0..n / 2 {
            for c in 0..chunks {
                let mut even = _mm_setzero_si128();
                let mut odd = _mm_setzero_si128();
                let mut j = 0usize;
                while j <= last_row {
                    let me = _mm_set1_epi32(*m.add(j * n + y));
                    let mo = _mm_set1_epi32(*m.add((j + 1) * n + y));
                    let de = _mm_loadu_si128(coeffs.add(j * n + c * 4) as *const __m128i);
                    let do_ = _mm_loadu_si128(coeffs.add((j + 1) * n + c * 4) as *const __m128i);
                    even = _mm_add_epi32(even, _mm_mullo_epi32(me, de));
                    odd = _mm_add_epi32(odd, _mm_mullo_epi32(mo, do_));
                    j += 2;
                }
                let up = _mm_srai_epi32::<7>(_mm_add_epi32(_mm_add_epi32(even, odd), r64));
                let dn = _mm_srai_epi32::<7>(_mm_add_epi32(_mm_sub_epi32(even, odd), r64));
                let gy = _mm_min_epi32(_mm_max_epi32(up, lo16), hi16);
                let gy2 = _mm_min_epi32(_mm_max_epi32(dn, lo16), hi16);
                _mm_storeu_si128(g.add(y * n + c * 4) as *mut __m128i, gy);
                _mm_storeu_si128(g.add((n - 1 - y) * n + c * 4) as *mut __m128i, gy2);
            }
        }
    }

    // 水平遍。
    let bd_shift = 20 - bit_depth;
    let round = _mm_set1_epi32(1i32 << (bd_shift - 1));
    let max = _mm_set1_epi32((1 << bit_depth) - 1);
    let zero = _mm_setzero_si128();
    let hchunks = n >> 2;
    for y in 0..n {
        for c in 0..hchunks {
            let mut sum = _mm_setzero_si128();
            let mut j = 0usize;
            while j <= max_x {
                let gj = _mm_set1_epi32(*g.add(y * n + j));
                let mj = _mm_loadu_si128(m.add(j * n + c * 4) as *const __m128i);
                sum = _mm_add_epi32(sum, _mm_mullo_epi32(gj, mj));
                j += 1;
            }
            let residual = srai128(_mm_add_epi32(sum, round), bd_shift);
            let target = dst.add(y * stride + c * 4);
            let pred4 = _mm_loadl_epi64(target as *const __m128i); // 4 u16（8 字节）
            let pred = _mm_cvtepu16_epi32(pred4); // 4 x i32
            let value = _mm_min_epi32(_mm_max_epi32(_mm_add_epi32(pred, residual), zero), max);
            let packed = _mm_packus_epi32(value, value); // [v0..v3, v0..v3] 8 u16
            _mm_storel_epi64(target as *mut __m128i, packed); // 低 8 字节 = 4 u16
        }
    }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::heif::hevc::transform::HevcTransform;

    /// 轻量 LCG 伪随机数（无外部依赖）。
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
        }
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
        fn i32_bounded(&mut self, lo: i32, hi: i32) -> i32 {
            debug_assert!(hi > lo);
            lo + (self.next() % ((hi - lo) as u64 + 1)) as i32
        }
        fn usize_bounded(&mut self, lo: usize, hi: usize) -> usize {
            debug_assert!(hi >= lo);
            lo + (self.next() % (hi - lo + 1) as u64) as usize
        }
    }

    /// 构造一个 N×N 块（显著区随机 i16 系数、其余清零，模拟调用方姿势；预测跨 stride 步长的
    /// 随机 u16），分别跑 scalar 与指定 SIMD 入口，返回 (scalar dst, simd dst, scalar g, simd g)。
    #[allow(clippy::too_many_arguments)]
    fn run_pair<F>(
        n: usize,
        dst4: bool,
        max_x: usize,
        max_y: usize,
        bit_depth: i32,
        stride: usize,
        simd: F,
        rng: &mut Rng,
    ) -> (Vec<u16>, Vec<u16>, Vec<i32>, Vec<i32>)
    where
        F: FnOnce(&[i32], &[i32], &mut [i32], &mut [u16]) -> HeifResult<()>,
    {
        let max = (1 << bit_depth) - 1;
        let mut coeffs = vec![0i32; n * n];
        for y in 0..=max_y {
            for x in 0..=max_x {
                coeffs[y * n + x] = rng.i32_bounded(-32768, 32767);
            }
        }
        let dst_len = (n - 1) * stride + n;
        let mut pred = vec![0u16; dst_len];
        for p in pred.iter_mut() {
            *p = rng.i32_bounded(0, max) as u16;
        }
        let m: Vec<i32> = if dst4 {
            HevcTransform::dst().iter().map(|&v| i32::from(v)).collect()
        } else {
            HevcTransform::dct_n(n.trailing_zeros() as usize).unwrap().clone()
        };

        let mut sau = vec![0x51A5_5A55i32; n * n]; // 哨兵：查未写区不被比较误伤
        let mut scu = vec![0x51A5_5A55i32; n * n];
        let mut dst_a = pred.clone();
        let mut dst_b = pred.clone();

        // scalar
        HevcTransform::inverse_add_scalar(
            &coeffs,
            &mut sau,
            n.trailing_zeros() as i32,
            dst4,
            max_x as i32,
            max_y as i32,
            bit_depth,
            &mut dst_a,
            stride as i32,
        )
        .expect("scalar");
        // simd
        simd(&coeffs, &m, &mut scu, &mut dst_b).expect("simd");

        (dst_a, dst_b, sau, scu)
    }

    /// 断言标量/向量两路逐字节一致：dst 全缓冲（跨行距）逐元素、g 的消费区（列 0..=maxX）逐元素。
    /// `w` = SIMD 内核向量道宽（4=SSE4.1、8=AVX2），用于限定位移写入的尾列断言范围。
    #[allow(clippy::too_many_arguments)]
    fn assert_equal(
        what: &str,
        n: usize,
        stride: usize,
        max_x: usize,
        w: usize,
        dst_a: &[u16],
        dst_b: &[u16],
        g_a: &[i32],
        g_b: &[i32],
    ) {
        assert_eq!(dst_a.len(), dst_b.len(), "{what}: dst 长度");
        for (i, (&a, &b)) in dst_a.iter().zip(dst_b.iter()).enumerate() {
            assert_eq!(a, b, "{what}: dst 首差 i={i} (x={}, y={})", i % stride, i / stride);
        }
        for y in 0..n {
            for x in 0..=max_x {
                let idx = y * n + x;
                assert_eq!(
                    g_a[idx], g_b[idx],
                    "{what}: g 首差 (x={x}, y={y}) (j≤maxX 消费区)",
                );
            }
        }
        // 尾列直接断言：垂直遍按整向量组写入 g，组内列 > maxX 的 lane 系数全 0 → g 恰为 0
        // （(0+64)>>7==0），水平遍只消费 j≤maxX。验证「尾列乘 0 同位」不是只靠推理+oracle。
        let written = ((max_x / w) + 1) * w;
        for y in 0..n {
            for x in (max_x + 1)..written {
                assert_eq!(
                    g_b[y * n + x], 0,
                    "{what}: SIMD g 尾列非 0 ((x={x}, y={y}))",
                );
            }
        }
    }

    /// SSE4.1（含 DST 4 点）≡ scalar：尺寸 4/8/16/32 × 8/10-bit × maxX/maxY 覆盖（全列/uneven/单列/零列）× 随机。
    #[test]
    fn sse41_matches_scalar_random() {
        if !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 sse4.1");
            return;
        }
        let mut rng = Rng::new(0x5EED_2026);
        for &n in &[4usize, 8, 16, 32] {
            for &bd in &[8i32, 10] {
                let dst4_cases: &[bool] = if n == 4 { &[false, true] } else { &[false] };
                for &dst4 in dst4_cases {
                    for &mx in &[0usize, n / 4, n / 2, n - 1] {
                        for &my in &[0usize, n / 4, (n - 1) / 2, n - 1] {
                            for _ in 0..6 {
                                let stride = n + rng.usize_bounded(0, 2);
                                let (da, db, ga, gb) = run_pair(
                                    n,
                                    dst4,
                                    mx,
                                    my,
                                    bd,
                                    stride,
                                    |c, m, s, d| inverse_add_sse41(n, m, dst4, c, s, mx, my, bd, d, stride),
                                    &mut rng,
                                );
                                assert_equal(
                                    &format!(
                                        "sse41 n={n} bd={bd} dst4={dst4} mx={mx} my={my} stride={stride}"
                                    ),
                                    n,
                                    stride,
                                    mx,
                                    4,
                                    &da,
                                    &db,
                                    &ga,
                                    &gb,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// AVX2（n≥8，DCT）≡ scalar：尺寸 8/16/32 × 8/10-bit × maxX/maxY 覆盖 × 随机。
    #[test]
    fn avx2_matches_scalar_random() {
        if !kimg_core::simd::detect("avx2") {
            eprintln!("SKIP: 无 avx2");
            return;
        }
        let mut rng = Rng::new(0xCAFE_2026);
        for &n in &[8usize, 16, 32] {
            for &bd in &[8i32, 10] {
                for &mx in &[0usize, n / 4, n / 2, n * 3 / 4, n - 1] {
                    for &my in &[0usize, n / 4, (n - 1) / 2, n - 1] {
                        for _ in 0..6 {
                            let stride = n + rng.usize_bounded(0, 2);
                            let (da, db, ga, gb) = run_pair(
                                n,
                                false,
                                mx,
                                my,
                                bd,
                                stride,
                                |c, m, s, d| inverse_add_avx2(n, m, c, s, mx, my, bd, d, stride),
                                &mut rng,
                            );
                            assert_equal(
                                &format!("avx2 n={n} bd={bd} mx={mx} my={my} stride={stride}"),
                                n,
                                stride,
                                mx,
                                8,
                                &da,
                                &db,
                                &ga,
                                &gb,
                            );
                        }
                    }
                }
            }
        }
    }

    /// 生产分派入口 ≡ scalar：`inverse_add`（AVX2/SSE4.1 自动选路）与 `inverse_add_scalar` 对拍。
    #[test]
    fn inverse_add_dispatch_matches_scalar() {
        if !kimg_core::simd::detect("avx2") && !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无任意 SIMD");
            return;
        }
        let mut rng = Rng::new(0xD15C_2026);
        for &(n, dst4) in &[(4usize, false), (4, true), (8, false), (16, false), (32, false)] {
            // 与生产分派同款运行时选路：avx2(&&sse4.1) 且 n≥8 → 8-lane，否则 4-lane。
            let w = if n >= 8
                && kimg_core::simd::detect("avx2")
                && kimg_core::simd::detect("sse4.1")
            {
                8
            } else {
                4
            };
            for &bd in &[8i32, 10] {
                for &mx in &[0usize, n / 2, n - 1] {
                    for &my in &[0usize, (n - 1) / 2, n - 1] {
                        for _ in 0..4 {
                            let stride = n + rng.usize_bounded(0, 2);
                            let (da, db, ga, gb) = run_pair(
                                n,
                                dst4,
                                mx,
                                my,
                                bd,
                                stride,
                                |c, _m, s, d| {
                                    HevcTransform::inverse_add(
                                        c,
                                        s,
                                        n.trailing_zeros() as i32,
                                        dst4,
                                        mx as i32,
                                        my as i32,
                                        bd,
                                        d,
                                        stride as i32,
                                    )
                                },
                                &mut rng,
                            );
                            assert_equal(
                                &format!(
                                    "dispatch n={n} dst4={dst4} bd={bd} mx={mx} my={my} stride={stride}"
                                ),
                                n,
                                stride,
                                mx,
                                w,
                                &da,
                                &db,
                                &ga,
                                &gb,
                            );
                        }
                    }
                }
            }
        }
    }

    /// SIMD 入口做块边界校验（非法参数报错而非溢出/越界）。
    #[test]
    fn simd_invalid_arguments_error() {
        if !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 sse4.1");
            return;
        }
        let m: Vec<i32> = HevcTransform::dct_n(2).unwrap().clone();
        let coeffs = vec![0i32; 16];
        let mut scratch = vec![0i32; 16];
        let mut dst = vec![0u16; 16];
        assert!(inverse_add_sse41(4, &m, false, &coeffs, &mut scratch, 4, 2, 8, &mut dst, 4).is_err(), "maxX==n");
        assert!(inverse_add_sse41(4, &m, false, &coeffs, &mut scratch, 3, 2, 8, &mut dst, 3).is_err(), "stride<n");
        assert!(inverse_add_sse41(4, &m, false, &coeffs, &mut [0i32; 8], 3, 2, 8, &mut dst, 4).is_err(), "scratch 过小");
        assert!(inverse_add_sse41(4, &m, false, &coeffs, &mut scratch, 3, 2, 8, &mut dst, 4).is_ok(), "4x4 合法");
    }
}
