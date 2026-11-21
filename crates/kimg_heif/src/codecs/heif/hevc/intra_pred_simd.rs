//! HEIF 帧内预测的 AVX2 向量内核叶子，与标量逐位字节恒等：
//! - Planar（mode0）`planar_avx2`：见下方注释；
//! - Angular（mode2..34）`angular_avx2`：per-row iIdx/iFact 常量、8-lane 滑动窗口 i32 插值
//!   `((32-f)·a + f·b + 16)>>5`（vpsrad，f==0 用 a 免越界读 b），vertical 行主序直写，
//!   horizontal 经 n×n u16 scratch + 8×8 转置写回（转置是纯拷贝，逐位不变）；
//! - DC（mode1）`dc_fill_avx2`：整块 u16 广播填充，边界修正在 intra_pred.rs 恒走标量。
//!
//! 对应 C# `HevcIntraPred.AngularVector256`（size≥8 走 V256；DC 在 C# 为标量，我们多做一层）。
//! 字节恒等纪律：插值中间量 `(32-f)·a + f·b + 16` ≤ 62·(2^bd-1)+16 < 2^31（i32 精确，
//! vpmulld 低 32 位 ≡ 无进位乘法）；`>>5` 用 vpsrad 算术右移；`_mm_packus_epi32` 在
//! [0, 2^bd-1] ⊆ [0,65535] 即精确窄化 ≡ 标量 `as u16`；不引入饱和/近似。
//!
//! 叶子隔离（对标 `hevc/transform_simd.rs` 与 `color.rs`）：本模块是 `intra_pred.rs` 的 unsafe 下沉点，
//! `intra_pred.rs` 以 `#![forbid(unsafe_code)]` 薄委托分派到本模块。

#![allow(unsafe_code)]
#![cfg(target_arch = "x86_64")]

use crate::codecs::heif::error::{HeifError, HeifResult};
use std::arch::x86_64::*;

/// 与标量 `planar_scalar` 相同的块边界校验（核访问全部下标的证明）。
#[inline]
fn check_planar_block(refs: &[i32], log2_size: i32, dst: &[u16], stride: usize) -> HeifResult<()> {
    let n = 1usize << log2_size;
    let needed = (n as i64 - 1) * stride as i64 + n as i64;
    if refs.len() < 4 * n + 1 || stride < n || needed < 0 || needed > dst.len() as i64 {
        return Err(HeifError::invalid("帧内块放不进缓冲。"));
    }
    Ok(())
}

/// Planar（mode0）AVX2 入口（n≥8）。块级一次预校验后调用内核；与标量逐字节一致。
pub(super) fn planar_avx2(
    refs: &[i32],
    log2_size: i32,
    dst: &mut [u16],
    stride: usize,
) -> HeifResult<()> {
    let n = 1usize << log2_size;
    // AVX2 8-lane 步进要求 n≥8（n 为 2 的幂，即 n 为 8 的倍数）；n==4 由生产分派走标量，
    // 此处显式拒绝，保证非生产直调也不会让内核按不足 8 的块 8-lane 越界写。
    if n < 8 {
        return Err(HeifError::invalid("Planar 向量入口仅支持 n≥8。"));
    }
    check_planar_block(refs, log2_size, dst, stride)?;
    // SAFETY: check_planar_block 已证 refs ≥ 4n+1、stride ≥ n、dst 可被 (n-1)·stride+n 覆盖。
    // 核读 refs 下标为 corner+1+c..+8（≤ 3n+1）与 corner-1-y（∈ [n, 2n-1]），写 dst 下标
    // y·stride+c..+8 ≤ (n-1)·stride+n（needed）；n≥8 且为 8 的倍数，8-lane 步进不越块。
    unsafe {
        planar_avx2_kernel(refs.as_ptr(), log2_size, dst.as_mut_ptr(), stride);
    }
    Ok(())
}

/// 运行时位移量的算术右移（shift = log2Size+1 ∈ 4..=6 for n ∈ 8..=32）。
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
        _ => unreachable!("planar shift 超出 1..=12"),
    }
}

/// AVX2 8-lane Planar 内核（n≥8，n 为 8 的倍数）。与标量 `planar_scalar` 逐位一致、
/// 与 C# `PlanarVector256` 同一算术（`_mm_packus_epi32` 为 SSE4.1，故特性双开）。
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn planar_avx2_kernel(refs: *const i32, log2_size: i32, dst: *mut u16, stride: usize) {
    unsafe {
    let n = 1usize << log2_size;
    let corner = 2 * n;
    let top_right = *refs.add(corner + 1 + n);
    let bottom_left = *refs.add(corner - 1 - n);
    let lane = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
    let one = _mm256_set1_epi32(1);
    let v_n = _mm256_set1_epi32(n as i32);
    let v_top_right = _mm256_set1_epi32(top_right);
    let shift = log2_size + 1;
    let mut c = 0usize;
    while c < n {
        // 列偏移 x = c + [0..7]
        let x = _mm256_add_epi32(lane, _mm256_set1_epi32(c as i32));
        // top = refs[corner + 1 + c .. +8]（顶行 8 个采样）
        let top = _mm256_loadu_si256(refs.add(corner + 1 + c) as *const __m256i);
        // fixedPart = (x+1)·topRight + n（块级一次预算，行间不变）
        let fixed = _mm256_add_epi32(
            _mm256_mullo_epi32(_mm256_add_epi32(x, one), v_top_right),
            v_n,
        );
        // leftWeight = n-1-x
        let left_weight = _mm256_sub_epi32(_mm256_set1_epi32((n - 1) as i32), x);
        let mut y = 0usize;
        while y < n {
            // 每行 broadcast left = refs[corner-1-y] 与行常量 (y+1)·bottomLeft
            let left = *refs.add(corner - 1 - y);
            let v = _mm256_add_epi32(
                _mm256_add_epi32(
                    _mm256_mullo_epi32(left_weight, _mm256_set1_epi32(left)),
                    fixed,
                ),
                _mm256_add_epi32(
                    _mm256_mullo_epi32(top, _mm256_set1_epi32((n - 1 - y) as i32)),
                    _mm256_set1_epi32((y + 1) as i32 * bottom_left),
                ),
            );
            let shifted = srai256(v, shift);
            // packusdw：规划值 ∈ [0, 2^bd-1]，窄化即精确（等价标量 `as u16`）。
            let lo = _mm256_castsi256_si128(shifted);
            let hi = _mm256_extracti128_si256(shifted, 1);
            let packed = _mm_packus_epi32(lo, hi);
            _mm_storeu_si128(dst.add(y * stride + c) as *mut __m128i, packed);
            y += 1;
        }
        c += 8;
    }
    }
}

/// 角向（mode2..34）AVX2 入口（n≥8，n 为 8 的倍数）。块级一次预校验后进入内核；
/// vertical（mode≥18）直接写 dst 行主序；horizontal（mode<18）先写 n×n u16 scratch（栈数组，
/// 免堆分配）再 8×8 分块转置到 dst（转置是纯拷贝，逐位不变，与 C# scratch+Transpose 同构）。
/// 与标量 `angular_scalar` 逐位一致（golden 守 C# 分层等价）。
pub(super) fn angular_avx2(
    refbuf: &[i32],
    n: usize,
    angle: i32,
    vertical: bool,
    dst: &mut [u16],
    stride: usize,
) -> HeifResult<()> {
    if n < 8 {
        return Err(HeifError::invalid("角向向量入口仅支持 n≥8。"));
    }
    // 与标量相同的 dst/stride 约束；另需 refbuf ≥ 3n+1（内核滑动窗口最多读到下标 3n，
    // 见 `angular_row_kernel` 越界证明：f≠0 ⇒ iIdx ≤ n-1 ⇒ idx2 ≤ 3n；f==0 ⇒ 不读 idx2，
    // 且 iIdx=n 仅当 f==0）。
    let needed = (n as i64 - 1) * stride as i64 + n as i64;
    if refbuf.len() < 3 * n + 1 || stride < n || needed < 0 || needed > dst.len() as i64 {
        return Err(HeifError::invalid("帧内块放不进缓冲。"));
    }
    // SAFETY: 上方已证 refbuf ≥ 3n+1、dst 可被 (n-1)·stride+n 覆盖；内核写 dst 下标
    // j·stride+c..+8 ≤ (n-1)·stride+n（needed），读 refbuf 下标 ≤ 3n；n 为 8 的倍数，
    // 8-lane 步进不越块。scratch 为栈数组 n×n u16，转置按 8×8 分块覆盖全块。
    if vertical {
        unsafe {
            angular_row_kernel(refbuf.as_ptr(), n, angle, dst.as_mut_ptr(), stride);
        }
    } else {
        let mut scratch = [0u16; 32 * 32];
        unsafe {
            angular_row_kernel(refbuf.as_ptr(), n, angle, scratch.as_mut_ptr(), n);
            transpose_u16(n, scratch.as_ptr(), n, dst.as_mut_ptr(), stride);
        }
    }
    Ok(())
}

/// 角向行主序滑动窗口内核：对每行 j（vertical 的行 / horizontal 的列），per-row 算
/// iIdx=(j+1)·angle>>5、iFact=(j+1)·angle&31；每块 8 列取 refbuf[n + iIdx + i] 两个相邻
/// i32 插值 `((32-f)·a + f·b + 16)>>5`（f==0 直接用 a，避免越界读 b），窄化为 8 个 u16
/// 存 `out` 的 `j·out_stride + c`（vertical=直接写 dst；horizontal=写 scratch 行）。
///
/// 越界证明（读下标 max = n + (n-8) + iIdx + 1..+9 与 块末 c=n-8）：
/// - f≠0 ⇒ product ≤ 32n-1 ⇒ iIdx ≤ n-1 ⇒ 最大读下标 n+(n-8)+(n-1)+9 = 3n（≤ 3n 界内）；
/// - f==0 只读 a ⇒ 最大读下标 n+(n-8)+iIdx+1 ≤ 3n-7；iIdx=n 仅当 f==0（product=32n）。
///
/// 写下标 j·out_stride+c..+8，调用方已证覆盖。
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn angular_row_kernel(
    refbuf: *const i32,
    n: usize,
    angle: i32,
    out: *mut u16,
    out_stride: usize,
) {
    unsafe {
    let base = refbuf.add(n);
    let v32 = _mm256_set1_epi32(32);
    let sixteen = _mm256_set1_epi32(16);
    let mut j = 0usize;
    while j < n {
        let product = (j as i32 + 1) * angle;
        let i_idx = product >> 5;
        let i_fact = product & 31;
        let off = i_idx as isize + 1;
        let mut c = 0usize;
        while c < n {
            let a = _mm256_loadu_si256(base.offset(off + c as isize) as *const __m256i);
            let v = if i_fact == 0 {
                a
            } else {
                let b =
                    _mm256_loadu_si256(base.offset(off + c as isize + 1) as *const __m256i);
                // (32-f)·a + f·b + 16，全量 i32 精确（≤ 62·65535+16 < 2^31），>>5 算术右移。
                _mm256_srai_epi32::<5>(_mm256_add_epi32(
                    _mm256_add_epi32(
                        _mm256_mullo_epi32(_mm256_sub_epi32(v32, _mm256_set1_epi32(i_fact)), a),
                        _mm256_mullo_epi32(_mm256_set1_epi32(i_fact), b),
                    ),
                    sixteen,
                ))
            };
            let lo = _mm256_castsi256_si128(v);
            let hi = _mm256_extracti128_si256(v, 1);
            // 8 个 i32 窄化为 8 个 u16（16 字节），整块一次 store（勿用 storel_epi64=仅 4 u16）。
            let packed = _mm_packus_epi32(lo, hi);
            _mm_storeu_si128(out.add(j * out_stride + c) as *mut __m128i, packed);
            c += 8;
        }
        j += 1;
    }
    }
}

/// n×n u16 矩阵转置（纯拷贝，逐位不变）：src 行主序（stride_src），转出写 dst 行主序
/// （stride_dst）。n 为 8 的倍数，按 8×8 子块转置 `[_mm_unpack{lo,hi}_epi{16,32,64}` 标准姿势，
/// 块间按转置置换（dst 块 (ti,tj) ← T(src 块 (tj,ti))）。只用到 SSE2 指令。
#[target_feature(enable = "avx2")]
unsafe fn transpose_u16(
    n: usize,
    src: *const u16,
    stride_src: usize,
    dst: *mut u16,
    stride_dst: usize,
) {
    unsafe {
    let mut tj = 0usize; // src 行块 == dst 列块
    while tj < n {
        let mut ti = 0usize; // src 列块 == dst 行块
        while ti < n {
            let r0 = _mm_loadu_si128(src.add(tj * stride_src + ti) as *const __m128i);
            let r1 = _mm_loadu_si128(src.add((tj + 1) * stride_src + ti) as *const __m128i);
            let r2 = _mm_loadu_si128(src.add((tj + 2) * stride_src + ti) as *const __m128i);
            let r3 = _mm_loadu_si128(src.add((tj + 3) * stride_src + ti) as *const __m128i);
            let r4 = _mm_loadu_si128(src.add((tj + 4) * stride_src + ti) as *const __m128i);
            let r5 = _mm_loadu_si128(src.add((tj + 5) * stride_src + ti) as *const __m128i);
            let r6 = _mm_loadu_si128(src.add((tj + 6) * stride_src + ti) as *const __m128i);
            let r7 = _mm_loadu_si128(src.add((tj + 7) * stride_src + ti) as *const __m128i);

            let t0 = _mm_unpacklo_epi16(r0, r1);
            let t1 = _mm_unpackhi_epi16(r0, r1);
            let t2 = _mm_unpacklo_epi16(r2, r3);
            let t3 = _mm_unpackhi_epi16(r2, r3);
            let t4 = _mm_unpacklo_epi16(r4, r5);
            let t5 = _mm_unpackhi_epi16(r4, r5);
            let t6 = _mm_unpacklo_epi16(r6, r7);
            let t7 = _mm_unpackhi_epi16(r6, r7);

            let u0 = _mm_unpacklo_epi32(t0, t2);
            let u1 = _mm_unpackhi_epi32(t0, t2);
            let u2 = _mm_unpacklo_epi32(t1, t3);
            let u3 = _mm_unpackhi_epi32(t1, t3);
            let u4 = _mm_unpacklo_epi32(t4, t6);
            let u5 = _mm_unpackhi_epi32(t4, t6);
            let u6 = _mm_unpacklo_epi32(t5, t7);
            let u7 = _mm_unpackhi_epi32(t5, t7);

            let c0 = _mm_unpacklo_epi64(u0, u4);
            let c1 = _mm_unpackhi_epi64(u0, u4);
            let c2 = _mm_unpacklo_epi64(u1, u5);
            let c3 = _mm_unpackhi_epi64(u1, u5);
            let c4 = _mm_unpacklo_epi64(u2, u6);
            let c5 = _mm_unpackhi_epi64(u2, u6);
            let c6 = _mm_unpacklo_epi64(u3, u7);
            let c7 = _mm_unpackhi_epi64(u3, u7);

            _mm_storeu_si128(dst.add(ti * stride_dst + tj) as *mut __m128i, c0);
            _mm_storeu_si128(dst.add((ti + 1) * stride_dst + tj) as *mut __m128i, c1);
            _mm_storeu_si128(dst.add((ti + 2) * stride_dst + tj) as *mut __m128i, c2);
            _mm_storeu_si128(dst.add((ti + 3) * stride_dst + tj) as *mut __m128i, c3);
            _mm_storeu_si128(dst.add((ti + 4) * stride_dst + tj) as *mut __m128i, c4);
            _mm_storeu_si128(dst.add((ti + 5) * stride_dst + tj) as *mut __m128i, c5);
            _mm_storeu_si128(dst.add((ti + 6) * stride_dst + tj) as *mut __m128i, c6);
            _mm_storeu_si128(dst.add((ti + 7) * stride_dst + tj) as *mut __m128i, c7);
            ti += 8;
        }
        tj += 8;
    }
    }
}

/// DC（mode1）整块 u16 填充的 AVX2 广播（n∈{8,16,32}，由生产分派保证；n<8 走标量）。
/// 与标量 `dst[off..off+n].fill(val)` 逐位一致；luma 边界修正（n<32）在 intra_pred.rs
/// 恒走标量、与本填充叠写。
pub(super) fn dc_fill_avx2(
    n: usize,
    val: u16,
    stride: usize,
    dst: &mut [u16],
) -> HeifResult<()> {
    if n < 8 {
        return Err(HeifError::invalid("DC 向量入口仅支持 n≥8。"));
    }
    let needed = (n as i64 - 1) * stride as i64 + n as i64;
    if stride < n || needed < 0 || needed > dst.len() as i64 {
        return Err(HeifError::invalid("帧内块放不进缓冲。"));
    }
    // SAFETY: 上方已证每条行 `y·stride + 0..n` 都在 dst 界内；n∈{8,16,32} 时
    // 8/16/16+16 u16 的存储正好覆盖一行 n 个 u16（n 为 2 的幂）。
    unsafe {
        dc_fill_avx2_kernel(n, val, dst.as_mut_ptr(), stride);
    }
    Ok(())
}

/// DC 广播填充内核：每行 n 个 u16 一次向量 store 写完。
#[target_feature(enable = "avx2")]
unsafe fn dc_fill_avx2_kernel(n: usize, val: u16, dst: *mut u16, stride: usize) {
    unsafe {
    let v128 = _mm_set1_epi16(val as i16); // 8 u16
    let v256 = _mm256_set1_epi16(val as i16); // 16 u16
    let mut y = 0usize;
    while y < n {
        let row = dst.add(y * stride);
        match n {
            8 => _mm_storeu_si128(row as *mut __m128i, v128),
            16 => _mm256_storeu_si256(row as *mut __m256i, v256),
            _ => {
                _mm256_storeu_si256(row as *mut __m256i, v256);
                _mm256_storeu_si256(row.add(16) as *mut __m256i, v256);
            }
        }
        y += 1;
    }
    }
}
