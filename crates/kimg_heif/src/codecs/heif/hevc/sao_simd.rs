//! HEVC SAO 行内核的 AVX2 向量化叶子：`edge_row_avx2` / `band_row_avx2`，与标量逐位字节恒等。
//!
//! 16-lane i16 道：样本至多 10 位、|SaoOffsetVal| ≤ 31，故 v+offset ≤ 1054 全程落有符号 16 位道、
//! 不截断（与 C# `Vector256<short>` 分层同算术）。对应 C# `HevcSao.EdgeRowVector256` /
//! `BandRowVector256`（`HevcSao.cs:118-148` / `:179-206`）。
//!
//! 分派结构仿 C# 分层：向量核只处理 `count` 的**整 16 块**（i 步进 16、`i < count&!15`），
//! 返回「已处理样本数」（16 的倍数，即 C# `done`），余数交给 `sao.rs` 的标量尾收尾——
//! 向量号与标量结果写入同一 `dst` 缓冲，逐位一致。本实现不做 Vector128 层：生产行宽
//! 62-64/30-32，256 层已覆盖主段，128 层只多一档分发无实测收益（对齐 C# 结构，注明选择）。
//!
//! 叶子隔离（对标 `intra_pred_simd.rs` / `transform_simd.rs`）：本模块是 `sao.rs` 的 unsafe
//! 下沉点，`sao.rs` 以 `#![forbid(unsafe_code)]` 薄委托分派到本模块。无 AVX2 CPU 由
//! `sao.rs` 检查 `std::is_x86_feature_detected!("avx2")` 后回标量，本模块只处理运行时装况。

#![allow(unsafe_code)]
#![cfg(target_arch = "x86_64")]

use crate::codecs::heif::error::{HeifError, HeifResult};
use std::arch::x86_64::*;

/// 缓冲契合证明（与 `sao.rs` 的 `check` 同口径）：`cur` 存有 `count` 个样本、`offsets` ≥ 4。
fn check_bufs(
    cur: &[u16],
    a: &[u16],
    b: &[u16],
    dst: &[u16],
    count: usize,
    offsets: &[i32],
) -> HeifResult<()> {
    if cur.len() < count || a.len() < count || b.len() < count || dst.len() < count || offsets.len() < 4 {
        return Err(HeifError::invalid("SAO 行放不进其缓冲。"));
    }
    Ok(())
}

/// 边偏移 AVX2 入口：整 16 块交给内核（写 [0, done)），返回 done = 16·⌊count/16⌋。
/// 满样本不整除时的余数由 `sao.rs` 调 `edge_row_scalar(start=done)` 收尾。`max` 超 i16 道
/// （v+offset 会溢）时返回 0（全标量），纯防御——SAO 恒 ≤10 位永不触发。
pub(super) fn edge_row_avx2(
    cur: &[u16],
    a: &[u16],
    b: &[u16],
    dst: &mut [u16],
    count: usize,
    offsets: &[i32],
    max: i32,
) -> HeifResult<usize> {
    check_bufs(cur, a, b, dst, count, offsets)?;
    let done = count & !15;
    if done == 0 || max > i16::MAX as i32 - 31 {
        return Ok(0);
    }
    // SAFETY: check_bufs 已证 cur/a/b/dst 长 ≥ count ≥ done；内核只读/写 [0, done)，
    // 16 步进不越段。offsets ≥ 4 已证（只读下标 0..4）。
    unsafe {
        edge_row_avx2_kernel(
            cur.as_ptr(),
            a.as_ptr(),
            b.as_ptr(),
            dst.as_mut_ptr(),
            done,
            offsets.as_ptr(),
            max,
        );
    }
    Ok(done)
}

/// 边偏移内核：`s = Sign(v−a) + Sign(v−b)` 落 [-2,2]。每邻居的 sign 用「比较掩码 −1
/// 即真」（C# `LessThan/GreaterThan = lt − gt`）：`lt = a>v`、`gt = v>a`，`s = ltₐ+ltᵦ−gtₐ−gtᵦ`
/// （i16 按位减法，逐位等价）。`s∈{±1,±2}` 经 Equal-AND-OR 选出对应 offset 广播，
/// `s==0/平坦` 四掩码全 0 得 offset=0（= 标量 `_ => 0`）。`v+offset` 在 i16 道内
/// （≤ max+31 ≤ 32767），min/max 有符号夹到 [0, max]，与标量 `clamp(0, max)` 逐位同。
#[allow(clippy::too_many_arguments)]
#[target_feature(enable = "avx2")]
unsafe fn edge_row_avx2_kernel(
    cur: *const u16,
    a: *const u16,
    b: *const u16,
    dst: *mut u16,
    done: usize,
    offsets: *const i32,
    max: i32,
) {
    unsafe {
        let o0 = _mm256_set1_epi16(*offsets.add(0) as i16);
        let o1 = _mm256_set1_epi16(*offsets.add(1) as i16);
        let o2 = _mm256_set1_epi16(*offsets.add(2) as i16);
        let o3 = _mm256_set1_epi16(*offsets.add(3) as i16);
        let vmax = _mm256_set1_epi16(max as i16);
        let zero = _mm256_setzero_si256();
        let ntwo = _mm256_set1_epi16(-2);
        let none = _mm256_set1_epi16(-1);
        let one = _mm256_set1_epi16(1);
        let two = _mm256_set1_epi16(2);
        let mut i = 0usize;
        while i < done {
            let v = _mm256_loadu_si256(cur.add(i) as *const __m256i);
            let va = _mm256_loadu_si256(a.add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b.add(i) as *const __m256i);
            let lt_a = _mm256_cmpgt_epi16(va, v); // va > v ⇔ v < va
            let gt_a = _mm256_cmpgt_epi16(v, va);
            let lt_b = _mm256_cmpgt_epi16(vb, v);
            let gt_b = _mm256_cmpgt_epi16(v, vb);
            let s = _mm256_sub_epi16(
                _mm256_sub_epi16(_mm256_add_epi16(lt_a, lt_b), gt_a),
                gt_b,
            );
            let offset = _mm256_or_si256(
                _mm256_or_si256(
                    _mm256_and_si256(_mm256_cmpeq_epi16(s, ntwo), o0),
                    _mm256_and_si256(_mm256_cmpeq_epi16(s, none), o1),
                ),
                _mm256_or_si256(
                    _mm256_and_si256(_mm256_cmpeq_epi16(s, one), o2),
                    _mm256_and_si256(_mm256_cmpeq_epi16(s, two), o3),
                ),
            );
            let clamped =
                _mm256_min_epi16(_mm256_max_epi16(_mm256_add_epi16(v, offset), zero), vmax);
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, clamped);
            i += 16;
        }
    }
}

/// 带偏移 AVX2 入口：整 16 块交给内核，返回 done = 16·⌊count/16⌋；余数由
/// `sao.rs` 的 `band_row_scalar_loop(start=done)` 收尾。`shift`/`max` 由宿主按
/// `bitDepth−5` / `(1<<bitDepth)−1` 算好传入，保证宿主与内核口径一致。
pub(super) fn band_row_avx2(
    cur: &[u16],
    dst: &mut [u16],
    count: usize,
    band_position: i32,
    shift: i32,
    max: i32,
    offsets: &[i32],
) -> HeifResult<usize> {
    check_bufs(cur, cur, cur, dst, count, offsets)?;
    let done = count & !15;
    if done == 0 || max > i16::MAX as i32 - 31 {
        return Ok(0);
    }
    // SAFETY: check_bufs 已证 cur/dst 长 ≥ count ≥ done；内核只读/写 [0, done)，16 步进不越段。
    unsafe {
        band_row_avx2_kernel(
            cur.as_ptr(),
            dst.as_mut_ptr(),
            done,
            band_position,
            shift,
            max,
            offsets.as_ptr(),
        );
    }
    Ok(done)
}

/// 带偏移内核：`k = ((v >> shift) − band) & 31`（`>>` 算术右移、`−` wrapping 减、`&31` 低 5
/// 位，与标量 `wrapping_shr → wrapping_sub → & 31` 逐位同；`sra_epi16` 算数右移无效位掩到
/// 元素位宽，与 C# `>>` 一致）。`k∈{0,1,2,3}` 选对应 offset、`k≥4` 经全 0 掩码得 offset=0
/// （= 标量原样通过 `dst[i]=cur[i]`）。`v+offset` 夹到 [0, max]。
#[allow(clippy::too_many_arguments)]
#[target_feature(enable = "avx2")]
unsafe fn band_row_avx2_kernel(
    cur: *const u16,
    dst: *mut u16,
    done: usize,
    band_position: i32,
    shift: i32,
    max: i32,
    offsets: *const i32,
) {
    unsafe {
        let o0 = _mm256_set1_epi16(*offsets.add(0) as i16);
        let o1 = _mm256_set1_epi16(*offsets.add(1) as i16);
        let o2 = _mm256_set1_epi16(*offsets.add(2) as i16);
        let o3 = _mm256_set1_epi16(*offsets.add(3) as i16);
        let vmax = _mm256_set1_epi16(max as i16);
        let zero = _mm256_setzero_si256();
        let band = _mm256_set1_epi16(band_position as i16);
        let mask31 = _mm256_set1_epi16(31);
        let k1 = _mm256_set1_epi16(1);
        let k2 = _mm256_set1_epi16(2);
        let k3 = _mm256_set1_epi16(3);
        // 移位量放低 64 位；sra_epi16 对 16 位元素按位宽掩码（同 C# `>>`）。
        let count_v = _mm_cvtsi32_si128(shift);
        let mut i = 0usize;
        while i < done {
            let v = _mm256_loadu_si256(cur.add(i) as *const __m256i);
            let shifted = _mm256_sra_epi16(v, count_v);
            let k = _mm256_and_si256(_mm256_sub_epi16(shifted, band), mask31);
            let offset = _mm256_or_si256(
                _mm256_or_si256(
                    _mm256_and_si256(_mm256_cmpeq_epi16(k, zero), o0),
                    _mm256_and_si256(_mm256_cmpeq_epi16(k, k1), o1),
                ),
                _mm256_or_si256(
                    _mm256_and_si256(_mm256_cmpeq_epi16(k, k2), o2),
                    _mm256_and_si256(_mm256_cmpeq_epi16(k, k3), o3),
                ),
            );
            let clamped =
                _mm256_min_epi16(_mm256_max_epi16(_mm256_add_epi16(v, offset), zero), vmax);
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, clamped);
            i += 16;
        }
    }
}
