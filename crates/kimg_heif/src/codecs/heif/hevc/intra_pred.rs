//! 帧内采样预测（H.265 §8.4.4.2）。逐函数移植自 C# `Hevc\HevcIntraPred.cs`，与 C# 字节恒等。
//!
//! 4N + 1 个参考采样 p[x][y] 放在一个数组 `refs` 里，按 §8.4.4.2.2 的替补过程扫描的顺序排列：
//! 下标 0 是 p[-1][2N-1]（左列底部），下标 2N - 1 - y 是 p[-1][y]，下标 2N 是角点 p[-1][-1]，
//! 下标 2N + 1 + x 是 p[x][-1]。这样替补和 §8.4.4.2.3 的 [1 2 1] 平滑滤波都变成对连续相邻元素的一遍遍历。
//!
//! C# 的 Scalar/Vector128/Vector256 分层在本机逐字节一致（harness 已对拍 `AUTO_MATCH=True`）；
//! 本实现固定走标量语义（正确性第一），结果与任何分层相同。
//!
//! 叶子隔离：unsafe SIMD 内核只许在 `intra_pred_simd`（`#![allow(unsafe_code)]`）内，本模块禁止。

#![forbid(unsafe_code)]

use crate::codecs::heif::error::{HeifError, HeifResult};

/// Planar 快道的同窗口 A/B 开关（K93）：`KIMG_HEIF_PLANAR_NO_SIMD=1` 强制标量基线路，
/// 默认空/AUTO → AVX2（运行时检出时）。一次读取缓存到 `OnceLock`，调用点仅一次原子读，
/// 不进每像素开销；仅影响 Planar 向量分派，标量路径语义不动。
static PLANAR_FORCE_SCALAR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn planar_force_scalar() -> bool {
    *PLANAR_FORCE_SCALAR.get_or_init(|| {
        std::env::var("KIMG_HEIF_PLANAR_NO_SIMD")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// Angular 快道的同窗口 A/B 开关（K93）：`KIMG_HEIF_ANGULAR_NO_SIMD=1` 强制标量基线路，
/// 默认空/AUTO → AVX2（运行时检出时）。一次读取缓存到 `OnceLock`，不进每像素开销；
/// 仅影响 Angular 向量分派，标量路径语义不动。
static ANGULAR_FORCE_SCALAR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn angular_force_scalar() -> bool {
    *ANGULAR_FORCE_SCALAR.get_or_init(|| {
        std::env::var("KIMG_HEIF_ANGULAR_NO_SIMD")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// DC 广播填充的同窗口 A/B 开关（K93）：`KIMG_HEIF_DC_NO_SIMD=1` 强制标量 fill，
/// 默认空/AUTO → AVX2（运行时检出时）。一次读取缓存到 `OnceLock`；仅影响 DC 填充分派，
/// luma 边界修正与标量路径语义不动。
static DC_FORCE_SCALAR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn dc_force_scalar() -> bool {
    *DC_FORCE_SCALAR.get_or_init(|| {
        std::env::var("KIMG_HEIF_DC_NO_SIMD")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// 帧内预测（C# `HevcIntraPred` 静态类）。
pub struct IntraPred;

impl IntraPred {
    /// 采样循环的分层选择器（C# 恒定）；本实现恒按标量语义计算。
    pub const AutoTier: i32 = 0;
    pub const ScalarTier: i32 = 1;
    pub const Vector128Tier: i32 = 128;
    pub const Vector256Tier: i32 = 256;

    /// H.265 表 8-9 的 `intraPredAngle`（mode 0..34）。
    const IntraPredAngle: [i32; 35] = [
        0, 0, 32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26,
        -32, -26, -21, -17, -13, -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
    ];

    /// H.265 表 8-10 的 `invAngle`（`InvAngle[mode]`，mode 11..25）。
    const InvAngle: [i32; 26] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630,
        -910, -1638, -4096,
    ];

    /// 替补不可用的参考采样（§8.4.4.2.2）。`available` 逐项标记 `refs` 里的采样是否可用；
    /// `any_available` 全不可用时为假。
    pub fn substitute(
        refs: &mut [i32],
        available: &[bool],
        any_available: bool,
        bit_depth: i32,
    ) -> HeifResult<()> {
        if !(1..=30).contains(&bit_depth) {
            return Err(HeifError::invalid("参考采样位深非法。"));
        }
        if available.len() < refs.len() {
            return Err(HeifError::invalid("参考采样可用性掩码过短。"));
        }
        if !any_available {
            refs.fill(1 << (bit_depth - 1));
            return Ok(());
        }
        if !available[0] {
            let mut k = 1usize;
            while k < refs.len() && !available[k] {
                k += 1;
            }
            if k >= refs.len() {
                return Err(HeifError::invalid("参考采样全不可用（与 anyAvailable 矛盾）。"));
            }
            refs[0] = refs[k];
        }
        for i in 1..refs.len() {
            if !available[i] {
                refs[i] = refs[i - 1];
            }
        }
        Ok(())
    }

    /// 把一个 N×N 块预测进 `dst`（行距 `stride`）；`filter_neighbours` 打开时应用 §8.4.4.2.3 的
    /// 邻域采样滤波（4:2:0 的 luma）。`refs` 会被滤波就地修改。
    #[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
    pub fn predict(
        refs: &mut [i32],
        log2_size: i32,
        mode: i32,
        is_luma: bool,
        filter_neighbours: bool,
        strong_smoothing: bool,
        bit_depth: i32,
        dst: &mut [u16],
        stride: i32,
    ) -> HeifResult<()> {
        if !(0..=30).contains(&log2_size) {
            return Err(HeifError::invalid("帧内块 log2Size 越界。"));
        }
        if !(8..=16).contains(&bit_depth) {
            return Err(HeifError::invalid("帧内预测位深越界（只支持 8..16）。"));
        }
        if !(0..=34).contains(&mode) {
            return Err(HeifError::invalid("帧内预测模式越界（0..34）。"));
        }
        let size = 1usize << log2_size;
        let n = size;
        // 与 C# `Predict` 顶部的缓冲适配检查对应（不满足即报错）；它保证下面的直接下标都在界内。
        let needed = (n as i64 - 1) * stride as i64 + n as i64;
        if refs.len() < 4 * size + 1 || stride < n as i32 || needed < 0 || needed > dst.len() as i64
        {
            return Err(HeifError::invalid("帧内块放不进缓冲。"));
        }
        let stride = stride as usize;
        let corner = 2 * n;

        if filter_neighbours && mode != 1 && n != 4 {
            let min_dist_ver_hor = ((mode - 26).abs()).min((mode - 10).abs());
            let threshold = match n {
                8 => 7i32,
                16 => 1,
                _ => 0,
            };
            if min_dist_ver_hor > threshold {
                filter(refs, n, strong_smoothing, bit_depth)?;
            }
        }

        let max = (1i32 << bit_depth) - 1;
        if mode == 0 {
            // INTRA_PLANAR（8-46）。n≥8 且检出 AVX2（含 SSE4.1 packusdw）→ 向量核；
            // 否则标量（尺寸 4 与无 AVX2 机器走标量，语义与 C# tier 对齐）。A/B 开关强制标量。
            #[cfg(target_arch = "x86_64")]
            if n >= 8
                && !planar_force_scalar()
                && kimg_core::simd::detect("avx2")
                && kimg_core::simd::detect("sse4.1")
            {
                return super::intra_pred_simd::planar_avx2(refs, log2_size, dst, stride);
            }
            planar_scalar(refs, log2_size, dst, stride)?;
            return Ok(());
        }

        if mode == 1 {
            // INTRA_DC（8-47 .. 8-52）。n∈{8,16,32} 且检出 AVX2 → 向量广播填充（A/B 开关强制标量）；
            // n<8 / 无 AVX2 → 标量 fill。两路都与 C# tier 逐位一致，luma 边界修正（n<32）恒走标量。
            let mut sum: i32 = n as i32;
            for i in 0..n {
                sum += refs_at(refs, corner + 1 + i)? + refs_at(refs, corner - 1 - i)?;
            }
            let dc_val = sum >> (log2_size + 1);
            let mut used_simd_fill = false;
            #[cfg(target_arch = "x86_64")]
            {
                if (8..=32).contains(&n)
                    && !dc_force_scalar()
                    && kimg_core::simd::detect("avx2")
                {
                    super::intra_pred_simd::dc_fill_avx2(n, dc_val as u16, stride, dst)?;
                    used_simd_fill = true;
                }
            }
            if !used_simd_fill {
                for y in 0..n {
                    let off = y * stride;
                    dst[off..off + n].fill(dc_val as u16);
                }
            }
            if is_luma && n < 32 {
                dst[0] = ((refs_at(refs, corner - 1)? + 2 * dc_val + refs_at(refs, corner + 1)? + 2)
                    >> 2) as u16;
                for x in 1..n {
                    dst[x] =
                        ((refs_at(refs, corner + 1 + x)? + 3 * dc_val + 2) >> 2) as u16;
                }
                for y in 1..n {
                    dst[y * stride] =
                        ((refs_at(refs, corner - 1 - y)? + 3 * dc_val + 2) >> 2) as u16;
                }
            }
            return Ok(());
        }

        // INTRA_ANGULAR2..34（8-53 .. 8-68）。ref[] 从 -n 起索引；refBuf[i + n] 存 ref[i]。
        let angle = Self::IntraPredAngle[mode as usize];
        let vertical = mode >= 18;
        let refbuf = build_angular_refbuf(refs, n, mode, angle, vertical)?;

        // n∈{8,16,32} 且检出 AVX2 → 向量核（8-lane 滑动窗口插值 + horizontal 转置）；
        // n==4 / 无 AVX2 / A/B 开关强制 → 标量。两路逐位一致（golden 守 C# 分层等价）。
        #[cfg(target_arch = "x86_64")]
        let angular_simd = (8..=32).contains(&n)
            && !angular_force_scalar()
            && kimg_core::simd::detect("avx2")
            && kimg_core::simd::detect("sse4.1");
        #[cfg(not(target_arch = "x86_64"))]
        let angular_simd = false;
        let refbuf_view = &refbuf[..3 * n + 1];
        if angular_simd {
            super::intra_pred_simd::angular_avx2(refbuf_view, n, angle, vertical, dst, stride)?;
        } else {
            angular_scalar(refbuf_view, n, angle, vertical, dst, stride)?;
        }

        if is_luma && n < 32 {
            if mode == 26 {
                for y in 0..n {
                    let v = (refs_at(refs, corner + 1)?
                        + ((refs_at(refs, corner - 1 - y)? - refs_at(refs, corner)?) >> 1))
                    .clamp(0, max) as u16;
                    dst[y * stride] = v;
                }
            } else if mode == 10 {
                for x in 0..n {
                    let v = (refs_at(refs, corner - 1)?
                        + ((refs_at(refs, corner + 1 + x)? - refs_at(refs, corner)?) >> 1))
                    .clamp(0, max) as u16;
                    dst[x] = v;
                }
            }
        }
        Ok(())
    }
}

fn refs_at(refs: &[i32], i: usize) -> HeifResult<i32> {
    refs.get(i)
        .copied()
        .ok_or_else(|| HeifError::invalid("帧内预测参考越界。"))
}

/// 构建角向预测的参考缓冲 `refBuf`（`refBuf[n + i] = ref[i]`，含负角 invAngle 扩展段），
/// 返回定长栈数组 `[i32; 3*32+1]`（语义同旧 `vec![0i32; 3n+1]`，按 n 只取逻辑前缀 3n+1，
/// 免每块堆分配）。`angular_scalar` 与 SIMD 核共享同一 buff，逐位同源。
fn build_angular_refbuf(
    refs: &[i32],
    n: usize,
    mode: i32,
    angle: i32,
    vertical: bool,
) -> HeifResult<[i32; 3 * 32 + 1]> {
    let corner = 2 * n;
    let mut refbuf = [0i32; 3 * 32 + 1];
    // main(i) = p[-1 + i][-1]（vertical）或 p[-1][-1 + i]（horizontal）；side(i) 是另一条边。
    for x in 0..=n {
        refbuf[n + x] = if vertical {
            refs_at(refs, corner + x)?
        } else {
            refs_at(refs, corner - x)?
        };
    }
    if angle < 0 {
        let last = (n as i32 * angle) >> 5;
        if last < -1 {
            let inv = IntraPred::InvAngle[mode as usize];
            for x in last..=-1 {
                let s = -1 + ((x * inv + 128) >> 8);
                if s < 0 || s >= 2 * n as i32 - 1 {
                    return Err(HeifError::invalid("帧内预测角度扩展越界。"));
                }
                // side(s)：vertical → p[-1][s]（左列），horizontal → p[s][-1]（顶行）。
                refbuf[(n as i32 + x) as usize] = if vertical {
                    refs_at(refs, corner - 1 - s as usize)?
                } else {
                    refs_at(refs, corner + 1 + s as usize)?
                };
            }
        }
    } else {
        for x in n + 1..=2 * n {
            refbuf[n + x] = if vertical {
                refs_at(refs, corner + x)?
            } else {
                refs_at(refs, corner - x)?
            };
        }
    }
    Ok(refbuf)
}

/// INTRA_PLANAR（C# `PlanarScalar`）。
fn planar_scalar(refs: &[i32], log2_size: i32, dst: &mut [u16], stride: usize) -> HeifResult<()> {
    let n = 1usize << log2_size;
    let corner = 2 * n;
    let top_right = refs_at(refs, corner + 1 + n)?;
    let bottom_left = refs_at(refs, corner - 1 - n)?;
    for y in 0..n {
        let left = refs_at(refs, corner - 1 - y)?;
        let row_off = y * stride;
        for x in 0..n {
            let v = ((n as i32 - 1 - x as i32) * left
                + (x as i32 + 1) * top_right
                + (n as i32 - 1 - y as i32) * refs_at(refs, corner + 1 + x)?
                + (y as i32 + 1) * bottom_left
                + n as i32)
                >> (log2_size + 1);
            dst[row_off + x] = v as u16;
        }
    }
    Ok(())
}

/// (8-59 .. 8-68) 的角向采样循环，跑在 refBuf 上（`refBuf[n + i] = ref[i]`）：
/// 预测块的第 j 行（horizontal 模式是第 j 列）对 ref 在 `i + iIdx + 1` 与 `i + iIdx + 2` 处插值。
fn angular_scalar(
    refbuf: &[i32],
    n: usize,
    angle: i32,
    vertical: bool,
    dst: &mut [u16],
    stride: usize,
) -> HeifResult<()> {
    for j in 0..n {
        let product = (j + 1) as i32 * angle;
        let i_idx = product >> 5;
        let i_fact = product & 31;
        for i in 0..n {
            let idx1 = (n as i32 + i as i32 + i_idx + 1) as usize;
            let a = refbuf
                .get(idx1)
                .copied()
                .ok_or_else(|| HeifError::invalid("帧内预测角向参考越界。"))?;
            let value = if i_fact != 0 {
                let idx2 = (n as i32 + i as i32 + i_idx + 2) as usize;
                let b = refbuf
                    .get(idx2)
                    .copied()
                    .ok_or_else(|| HeifError::invalid("帧内预测角向参考越界。"))?;
                ((32 - i_fact) * a + i_fact * b + 16) >> 5
            } else {
                a
            };
            // Vertical：j 是行 y、i 是列 x；horizontal：反之。
            let off = if vertical {
                j.checked_mul(stride)
                    .and_then(|o| o.checked_add(i))
                    .ok_or_else(|| HeifError::invalid("帧内预测目标越界。"))?
            } else {
                i.checked_mul(stride)
                    .and_then(|o| o.checked_add(j))
                    .ok_or_else(|| HeifError::invalid("帧内预测目标越界。"))?
            };
            let slot = dst
                .get_mut(off)
                .ok_or_else(|| HeifError::invalid("帧内预测目标越界。"))?;
            *slot = value as u16;
        }
    }
    Ok(())
}

/// §8.4.4.2.3 的邻域采样滤波（强平滑 8-36..8-40 与 [1 2 1] 8-41..8-45）。
fn filter(refs: &mut [i32], n: usize, strong_smoothing: bool, bit_depth: i32) -> HeifResult<()> {
    let corner = 2 * n;
    let last = 4 * n;
    if strong_smoothing && n == 32 {
        let threshold = 1i32 << (bit_depth - 5);
        let c = refs[corner];
        let top_end = refs[last]; // p[63][-1]
        let left_end = refs[0]; // p[-1][63]
        if (c + top_end - 2 * refs[corner + n]).abs() < threshold
            && (c + left_end - 2 * refs[corner - n]).abs() < threshold
        {
            // (8-36 .. 8-40)：角点与远端之间做双线性。
            for i in 0..63usize {
                refs[corner + 1 + i] = ((63 - i) as i32 * c + (i + 1) as i32 * top_end + 32) >> 6;
                refs[corner - 1 - i] =
                    ((63 - i) as i32 * c + (i + 1) as i32 * left_end + 32) >> 6;
            }
            return Ok(());
        }
    }

    // (8-41 .. 8-45)：对连续相邻项做 [1 2 1]，两端不变。
    let mut previous = refs[0];
    for i in 1..last {
        let current = refs[i];
        refs[i] = (previous + 2 * current + refs[i + 1] + 2) >> 2;
        previous = current;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[path = "../../golden_intra.rs"]
    mod golden;

    fn run_block(
        refs: &[i32],
        log2_size: i32,
        mode: i32,
        is_luma: bool,
        filter_neighbours: bool,
        strong_smoothing: bool,
        bit_depth: i32,
    ) -> Vec<u16> {
        let n = 1usize << log2_size;
        let mut r = refs.to_vec();
        let mut dst = vec![0u16; n * n];
        IntraPred::predict(
            &mut r,
            log2_size,
            mode,
            is_luma,
            filter_neighbours,
            strong_smoothing,
            bit_depth,
            &mut dst,
            n as i32,
        )
        .unwrap();
        dst
    }

    /// 4x4：planar / DC / 角向（正负角度、26/10 边界修正、luma 边界）。
    #[test]
    fn predict_4x4_matches_csharp() {
        for &(mode, expected) in &[
            (0, &golden::PRED4_LU_FILT_S_M0[..]),
            (1, &golden::PRED4_LU_FILT_S_M1[..]),
            (2, &golden::PRED4_LU_FILT_S_M2[..]),
            (3, &golden::PRED4_LU_FILT_S_M3[..]),
            (9, &golden::PRED4_LU_FILT_S_M9[..]),
            (10, &golden::PRED4_LU_FILT_S_M10[..]),
            (11, &golden::PRED4_LU_FILT_S_M11[..]),
            (17, &golden::PRED4_LU_FILT_S_M17[..]),
            (18, &golden::PRED4_LU_FILT_S_M18[..]),
            (25, &golden::PRED4_LU_FILT_S_M25[..]),
            (26, &golden::PRED4_LU_FILT_S_M26[..]),
            (27, &golden::PRED4_LU_FILT_S_M27[..]),
            (33, &golden::PRED4_LU_FILT_S_M33[..]),
            (34, &golden::PRED4_LU_FILT_S_M34[..]),
        ] {
            let got = run_block(&golden::REFS_N4, 2, mode, true, true, false, 8);
            assert_eq!(got.as_slice(), expected, "4x4 模式 {mode}");
        }
    }

    /// DC 在 chroma（isLuma=false）下不做边界修正：整块常值。
    #[test]
    fn predict_4x4_dc_non_luma_matches_csharp() {
        let got = run_block(&golden::REFS_N4, 2, 1, false, false, false, 8);
        assert_eq!(got.as_slice(), &golden::PRED4_DC_NO_LU_M1[..]);
    }

    /// 8x8：触发 [1 2 1] 滤波的角向模式与不滤波模式、vertical/26 边界。
    #[test]
    fn predict_8x8_matches_csharp() {
        for &(mode, expected) in &[
            (0, &golden::PRED8_LU_FILT_S_M0[..]),
            (1, &golden::PRED8_LU_FILT_S_M1[..]),
            (2, &golden::PRED8_LU_FILT_S_M2[..]),
            (3, &golden::PRED8_LU_FILT_S_M3[..]),
            (9, &golden::PRED8_LU_FILT_S_M9[..]),
            (11, &golden::PRED8_LU_FILT_S_M11[..]),
            (17, &golden::PRED8_LU_FILT_S_M17[..]),
            (18, &golden::PRED8_LU_FILT_S_M18[..]),
            (25, &golden::PRED8_LU_FILT_S_M25[..]),
            (26, &golden::PRED8_LU_FILT_S_M26[..]),
            (33, &golden::PRED8_LU_FILT_S_M33[..]),
            (34, &golden::PRED8_LU_FILT_S_M34[..]),
        ] {
            let got = run_block(&golden::REFS_N8, 3, mode, true, true, false, 8);
            assert_eq!(got.as_slice(), expected, "8x8 模式 {mode}");
        }
    }

    /// 同一 8x8 关闭滤波 vs 打开滤波必须不同（[1 2 1] 有实际效果）。
    #[test]
    fn predict_8x8_filter_off_matches_csharp() {
        let got = run_block(&golden::REFS_N8, 3, 2, true, false, false, 8);
        assert_eq!(got.as_slice(), &golden::PRED8_NOFILT_M2[..]);
        assert_ne!(got.as_slice(), &golden::PRED8_LU_FILT_S_M2[..], "滤波应改变结果");
    }

    /// 16x16：更大的滤波阈值分布。
    #[test]
    fn predict_16x16_matches_csharp() {
        for &(mode, expected) in &[
            (2, &golden::PRED16_LU_FILT_S_M2[..]),
            (11, &golden::PRED16_LU_FILT_S_M11[..]),
            (26, &golden::PRED16_LU_FILT_S_M26[..]),
        ] {
            let got = run_block(&golden::REFS_N16, 4, mode, true, true, false, 8);
            assert_eq!(got.as_slice(), expected, "16x16 模式 {mode}");
        }
    }

    /// 32x32：线性渐变满足强平滑条件 → 走 8-36..8-40 双线性分支（planar 与角向都是）。
    #[test]
    fn predict_32x32_strong_smoothing_matches_csharp() {
        for &(mode, expected) in &[
            (0, &golden::PRED32_LU_STRONG_S_M0[..]),
            (2, &golden::PRED32_LU_STRONG_S_M2[..]),
            (26, &golden::PRED32_LU_STRONG_S_M26[..]),
        ] {
            let got = run_block(&golden::REFS_N32, 5, mode, true, true, true, 8);
            assert_eq!(got.as_slice(), expected, "32x32 强平滑 模式 {mode}");
        }
    }

    /// Substitute：部分可用 / 全不可用 / 首项不可用（前扫）。
    #[test]
    fn substitute_matches_csharp() {
        let mut r = golden::REFS_N4.to_vec();
        IntraPred::substitute(
            &mut r,
            &[
                true, false, false, true, true, false, true, true, true, true, false, false,
                true, true, true, true, true,
            ],
            true,
            8,
        )
        .unwrap();
        assert_eq!(r.as_slice(), &golden::SUBST_PARTIAL[..]);

        let mut r = vec![0i32; 17];
        IntraPred::substitute(&mut r, &[false; 17], false, 8).unwrap();
        assert_eq!(r.as_slice(), &golden::SUBST_NONE[..]);

        let mut r = golden::REFS_N4.to_vec();
        IntraPred::substitute(
            &mut r,
            &[false, false, false, false, true, false, false, false, false, false, false,
              false, false, false, false, false, false],
            true,
            8,
        )
        .unwrap();
        assert_eq!(r.as_slice(), &golden::SUBST_HOLE[..]);
    }

    /// 轻量 LCG 伪随机数（无外部依赖，同 transform_simd 测试姿势）。
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

    /// 构造 4n+1 参考采样：随机 [0, m]，每隔数个钉到 0 / m（极端值）+ 随机间隔破坏连续平滑。
    fn random_refs(n: usize, m: i32, rng: &mut Rng) -> Vec<i32> {
        let mut refs = vec![0i32; 4 * n + 1];
        for (i, r) in refs.iter_mut().enumerate() {
            *r = match i % 5 {
                0 => 0,
                1 => m,
                _ => rng.i32_bounded(0, m),
            };
        }
        refs
    }

    /// 断言两缓冲逐元素相等，失败打印首差 (x,y)（x = i % stride）。
    fn assert_dst_equal(dst_a: &[u16], dst_b: &[u16], stride: usize, what: &str) {
        assert_eq!(dst_a.len(), dst_b.len(), "{what}: 缓冲长度");
        for (i, (&a, &b)) in dst_a.iter().zip(dst_b.iter()).enumerate() {
            assert_eq!(
                a, b,
                "{what}: 首差 i={i} (x={}, y={})",
                i % stride,
                i / stride
            );
        }
    }

    /// Planar 向量核（`planar_avx2`）≡ 标量：尺寸 {8,16,32} × 位深 {8,10} × 随机参考采样 × 随机 stride。
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn planar_simd_matches_scalar_random() {
        if !kimg_core::simd::detect("avx2") || !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 avx2/sse4.1");
            return;
        }
        let mut rng = Rng::new(0x5EED_2026);
        for &n in &[8usize, 16, 32] {
            for &bd in &[8i32, 10] {
                for _ in 0..8 {
                    let stride = n + rng.usize_bounded(0, 3);
                    let log2 = n.trailing_zeros() as i32;
                    let refs = random_refs(n, (1i32 << bd) - 1, &mut rng);
                    // 尾哨兵区 4 采样：抓向量越界/越块写。
                    let dst_len = (n - 1) * stride + n + 4;
                    let mut dst_a = vec![0x5A5Au16; dst_len];
                    let mut dst_b = vec![0x5A5Au16; dst_len];
                    planar_scalar(&refs, log2, &mut dst_a, stride).expect("scalar");
                    super::super::intra_pred_simd::planar_avx2(&refs, log2, &mut dst_b, stride)
                        .expect("simd");
                    assert_dst_equal(
                        &dst_a,
                        &dst_b,
                        stride,
                        &format!("planar simd≡scalar n={n} bd={bd} stride={stride}"),
                    );
                }
            }
        }
    }

    /// Planar 生产分派（`predict` mode0）≡ 标量：尺寸 {4,8,16,32} × 位深 {8,10}；
    /// n=4 走标量兜底、n≥8 且 AVX2 可走向量路，两路都必须与标量逐字节一致。
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn planar_dispatch_matches_scalar() {
        let mut rng = Rng::new(0xD15C_2026);
        for &n in &[4usize, 8, 16, 32] {
            for &bd in &[8i32, 10] {
                for _ in 0..4 {
                    let stride = n + rng.usize_bounded(0, 2);
                    let log2 = n.trailing_zeros() as i32;
                    let refs = random_refs(n, (1i32 << bd) - 1, &mut rng);
                    let dst_len = (n - 1) * stride + n;
                    let mut dst_disp = vec![0x5A5Au16; dst_len];
                    let mut dst_sc = vec![0x5A5Au16; dst_len];
                    IntraPred::predict(
                        &mut refs.clone(),
                        log2,
                        0,
                        true,
                        false,
                        false,
                        bd,
                        &mut dst_disp,
                        stride as i32,
                    )
                    .expect("predict");
                    planar_scalar(&refs, log2, &mut dst_sc, stride).expect("scalar");
                    assert_dst_equal(
                        &dst_disp,
                        &dst_sc,
                        stride,
                        &format!("planar dispatch≡scalar n={n} bd={bd} stride={stride}"),
                    );
                }
            }
        }
    }

    /// Angular 向量核（`angular_avx2`）≡ 标量 `angular_scalar`：尺寸 {8,16,32} ×
    /// 模式族 {horizontal 正/负/零角、vertical 正/负/零角} × 位深 {8,10} × 随机参考
    /// （含 0/极端钉值）× 随机 stride。`refbuf` 用与生产 `predict` 同源的 `build_angular_refbuf`。
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn angular_simd_matches_scalar_random() {
        if !kimg_core::simd::detect("avx2") || !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 avx2/sse4.1");
            return;
        }
        let mut rng = Rng::new(0xA9C2_2026);
        // 覆盖角度 {+32(横/纵直), +26,+2,+0(10/26),-2,-5,-13,-26,-32}、正/负/零。
        let modes = [2i32, 3, 9, 10, 11, 17, 18, 19, 25, 26, 27, 33, 34];
        for &n in &[8usize, 16, 32] {
            for &bd in &[8i32, 10] {
                for &mode in &modes {
                    let angle = IntraPred::IntraPredAngle[mode as usize];
                    let vertical = mode >= 18;
                    for _ in 0..4 {
                        let stride = n + rng.usize_bounded(0, 3);
                        let refs = random_refs(n, (1i32 << bd) - 1, &mut rng);
                        let refbuf = build_angular_refbuf(&refs, n, mode, angle, vertical)
                            .expect("build refbuf");
                        let view = &refbuf[..3 * n + 1];
                        let dst_len = (n - 1) * stride + n + 4;
                        let mut dst_a = vec![0x5A5Au16; dst_len];
                        let mut dst_b = vec![0x5A5Au16; dst_len];
                        angular_scalar(view, n, angle, vertical, &mut dst_a, stride)
                            .expect("scalar");
                        super::super::intra_pred_simd::angular_avx2(
                            view, n, angle, vertical, &mut dst_b, stride,
                        )
                        .expect("simd");
                        assert_dst_equal(
                            &dst_a,
                            &dst_b,
                            stride,
                            &format!(
                                "angular simd≡scalar n={n} bd={bd} mode={mode} angle={angle} vert={vertical} stride={stride}"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// Angular 生产分派（`predict` mode∈角向，无 env → AVX2 路）≡ 标量角向参考
    /// （`build_angular_refbuf` + `angular_scalar` + mode26/10 luma 边界修正），
    /// 随机参考 + 随机 stride，覆盖负角 invAngle 扩展与 horizontal 转置经生产路径。
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn angular_dispatch_matches_scalar() {
        if !kimg_core::simd::detect("avx2") || !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 avx2/sse4.1");
            return;
        }
        let mut rng = Rng::new(0xD15C_2026);
        let modes = [3i32, 10, 11, 17, 18, 25, 26, 27, 33, 34];
        for &n in &[8usize, 16, 32] {
            for &bd in &[8i32, 10] {
                for &mode in &modes {
                    let angle = IntraPred::IntraPredAngle[mode as usize];
                    let vertical = mode >= 18;
                    for _ in 0..3 {
                        let stride = n + rng.usize_bounded(0, 2);
                        let log2 = n.trailing_zeros() as i32;
                        let max = (1i32 << bd) - 1;
                        let refs = random_refs(n, max, &mut rng);
                        let dst_len = (n - 1) * stride + n + 4;
                        let mut dst_disp = vec![0x5A5Au16; dst_len];
                        let mut dst_ref = vec![0x5A5Au16; dst_len];

                        IntraPred::predict(
                            &mut refs.clone(),
                            log2,
                            mode,
                            true,
                            false,
                            false,
                            bd,
                            &mut dst_disp,
                            stride as i32,
                        )
                        .expect("predict(分派)");

                        let refbuf = build_angular_refbuf(&refs, n, mode, angle, vertical)
                            .expect("refbuf");
                        angular_scalar(
                            &refbuf[..3 * n + 1],
                            n,
                            angle,
                            vertical,
                            &mut dst_ref,
                            stride,
                        )
                        .expect("scalar");
                        // mode26/10 的 luma 边界修正（与 predict 同一段代码）。
                        if n < 32 {
                            let corner = 2 * n;
                            if mode == 26 {
                                for y in 0..n {
                                    let v = (refs[corner + 1]
                                        + ((refs[corner - 1 - y] - refs[corner]) >> 1))
                                        .clamp(0, max) as u16;
                                    dst_ref[y * stride] = v;
                                }
                            } else if mode == 10 {
                                for x in 0..n {
                                    let v = (refs[corner - 1]
                                        + ((refs[corner + 1 + x] - refs[corner]) >> 1))
                                        .clamp(0, max) as u16;
                                    dst_ref[x] = v;
                                }
                            }
                        }
                        assert_dst_equal(
                            &dst_disp,
                            &dst_ref,
                            stride,
                            &format!(
                                "angular dispatch≡scalar n={n} bd={bd} mode={mode} angle={angle} stride={stride}"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// DC 广播填充核（`dc_fill_avx2`）≡ 标量 fill：n{8,16,32} × 随机 val × 随机 stride。
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn dc_simd_fill_matches_scalar_random() {
        if !kimg_core::simd::detect("avx2") {
            eprintln!("SKIP: 无 avx2");
            return;
        }
        let mut rng = Rng::new(0xDCF1_2026);
        for &n in &[8usize, 16, 32] {
            for _ in 0..16 {
                let val = rng.usize_bounded(0, 65536) as u16;
                let stride = n + rng.usize_bounded(0, 3);
                let dst_len = (n - 1) * stride + n + 4;
                let mut dst_a = vec![0x5A5Au16; dst_len];
                let mut dst_b = vec![0x5A5Au16; dst_len];
                for y in 0..n {
                    dst_a[y * stride..y * stride + n].fill(val);
                }
                super::super::intra_pred_simd::dc_fill_avx2(n, val, stride, &mut dst_b)
                    .expect("dc_fill simd");
                assert_dst_equal(
                    &dst_a,
                    &dst_b,
                    stride,
                    &format!("dc fill simd≡scalar n={n} val={val} stride={stride}"),
                );
            }
        }
    }

    /// DC 生产分派（`predict` mode1，无 env → AVX2 广播路）≡ 标量 DC 参考
    /// （同 predict 的求和 + fill + luma 边界修正），n{4,8,16,32} × bd{8,10} × is_luma 两态。
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn dc_dispatch_matches_scalar() {
        if !kimg_core::simd::detect("avx2") {
            eprintln!("SKIP: 无 avx2");
            return;
        }
        let mut rng = Rng::new(0xD15C_2026);
        for &n in &[4usize, 8, 16, 32] {
            for &bd in &[8i32, 10] {
                for &is_luma in &[true, false] {
                    for _ in 0..4 {
                        let stride = n + rng.usize_bounded(0, 2);
                        let log2 = n.trailing_zeros() as i32;
                        let refs = random_refs(n, (1i32 << bd) - 1, &mut rng);
                        let dst_len = (n - 1) * stride + n + 4;
                        let mut dst_disp = vec![0x5A5Au16; dst_len];
                        let mut dst_ref = vec![0x5A5Au16; dst_len];

                        IntraPred::predict(
                            &mut refs.clone(),
                            log2,
                            1,
                            is_luma,
                            false,
                            false,
                            bd,
                            &mut dst_disp,
                            stride as i32,
                        )
                        .expect("predict(分派)");

                        // 标量 DC 参考（与 predict 的 DC 分支同代码）。
                        let corner = 2 * n;
                        let mut sum = n as i32;
                        for i in 0..n {
                            sum += refs[corner + 1 + i] + refs[corner - 1 - i];
                        }
                        let dc = sum >> (log2 + 1);
                        for y in 0..n {
                            dst_ref[y * stride..y * stride + n].fill(dc as u16);
                        }
                        if is_luma && n < 32 {
                            dst_ref[0] =
                                ((refs[corner - 1] + 2 * dc + refs[corner + 1] + 2) >> 2) as u16;
                            for x in 1..n {
                                dst_ref[x] = ((refs[corner + 1 + x] + 3 * dc + 2) >> 2) as u16;
                            }
                            for y in 1..n {
                                dst_ref[y * stride] =
                                    ((refs[corner - 1 - y] + 3 * dc + 2) >> 2) as u16;
                            }
                        }
                        assert_dst_equal(
                            &dst_disp,
                            &dst_ref,
                            stride,
                            &format!(
                                "dc dispatch≡scalar n={n} bd={bd} luma={is_luma} stride={stride}"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// Angular 内核微基准（release-only，`#[ignore]`）：同窗交错标量 vs AVX2（横/纵各一通），
    /// 各 ≥9 轮取中位，字节恒等前置。运行：
    /// `cargo test -p kimg_heif --release --lib hevc::intra_pred angular_kernel_perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    #[cfg(target_arch = "x86_64")]
    fn angular_kernel_perf_probe() {
        use std::time::Instant;
        if !kimg_core::simd::detect("avx2") || !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 avx2/sse4.1");
            return;
        }
        let n = 32usize;
        let blocks = 2000usize;
        let mut rng = Rng::new(0xAA11_2026);
        let mut refs = vec![0i32; 4 * n + 1];
        let mut cur = 100i32;
        for r in refs.iter_mut() {
            cur = (cur + rng.i32_bounded(-40, 40)).clamp(0, 255);
            *r = cur;
        }
        let stride = n;
        let mut dst_s = vec![0u16; (n - 1) * stride + n];
        let mut dst_v = vec![0u16; (n - 1) * stride + n];

        for &(vertical, mode) in &[(true, 34i32), (false, 2i32)] {
            let angle = IntraPred::IntraPredAngle[mode as usize];
            let refbuf = build_angular_refbuf(&refs, n, mode, angle, vertical).unwrap();
            let view = &refbuf[..3 * n + 1];
            angular_scalar(view, n, angle, vertical, &mut dst_s, stride).unwrap();
            super::super::intra_pred_simd::angular_avx2(view, n, angle, vertical, &mut dst_v, stride)
                .unwrap();
            assert_dst_equal(&dst_s, &dst_v, stride, "probe 前置恒等");
            for _ in 0..2 {
                angular_scalar(view, n, angle, vertical, &mut dst_s, stride).unwrap();
                super::super::intra_pred_simd::angular_avx2(
                    view, n, angle, vertical, &mut dst_v, stride,
                )
                .unwrap();
            }
            let mut scalar_ms = Vec::with_capacity(9);
            let mut simd_ms = Vec::with_capacity(9);
            for _ in 0..9 {
                let t0 = Instant::now();
                for _ in 0..blocks {
                    angular_scalar(view, n, angle, vertical, &mut dst_s, stride).unwrap();
                }
                let t1 = Instant::now();
                for _ in 0..blocks {
                    super::super::intra_pred_simd::angular_avx2(
                        view, n, angle, vertical, &mut dst_v, stride,
                    )
                    .unwrap();
                }
                let t2 = Instant::now();
                scalar_ms.push((t1 - t0).as_secs_f64() * 1e3);
                simd_ms.push((t2 - t1).as_secs_f64() * 1e3);
            }
            let sort = |v: Vec<f64>| -> f64 {
                let mut w = v;
                w.sort_by(|a, b| a.partial_cmp(b).unwrap());
                w[w.len() / 2]
            };
            let sm = sort(scalar_ms.clone());
            let vm = sort(simd_ms.clone());
            println!(
                "angular_kernel n={n} blocks={blocks} mode={mode} vert={vertical}: scalar={sm:.3} ms simd={vm:.3} ms speedup={:.2}x (delta {:.1}%)",
                sm / vm,
                (sm - vm) / sm * 100.0
            );
            println!("  rounds: scalar={scalar_ms:?} simd={simd_ms:?}");
        }
    }

    /// Planar 内核微基准（release-only，`#[ignore]`）：每轮先标量后 AVX2（同窗），各 ≥9 轮取中位。
    /// 字节恒等前置后再计时。运行：
    /// `cargo test -p kimg_heif --release --lib hevc::intra_pred planar_kernel_perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    #[cfg(target_arch = "x86_64")]
    fn planar_kernel_perf_probe() {
        use std::time::Instant;
        if !kimg_core::simd::detect("avx2") || !kimg_core::simd::detect("sse4.1") {
            eprintln!("SKIP: 无 avx2/sse4.1");
            return;
        }
        let n = 32usize;
        let log2 = 5;
        let blocks = 2000usize;
        let mut rng = Rng::new(0xBEEF_2026);
        // 平滑参考（相邻样本相近，近似真实内容分布）。
        let mut refs = vec![0i32; 4 * n + 1];
        let mut cur = 100i32;
        for r in refs.iter_mut() {
            cur = (cur + rng.i32_bounded(-40, 40)).clamp(0, 255);
            *r = cur;
        }
        let stride = n;
        let mut dst_s = vec![0u16; (n - 1) * stride + n];
        let mut dst_v = vec![0u16; (n - 1) * stride + n];
        // 断言两路一致后再计时（字节恒等前置）。
        planar_scalar(&refs, log2, &mut dst_s, stride).unwrap();
        super::super::intra_pred_simd::planar_avx2(&refs, log2, &mut dst_v, stride).unwrap();
        assert_dst_equal(&dst_s, &dst_v, stride, "probe 前置恒等");
        for _ in 0..2 {
            planar_scalar(&refs, log2, &mut dst_s, stride).unwrap();
            super::super::intra_pred_simd::planar_avx2(&refs, log2, &mut dst_v, stride).unwrap();
        }
        let mut scalar_ms = Vec::with_capacity(9);
        let mut simd_ms = Vec::with_capacity(9);
        for _ in 0..9 {
            let t0 = Instant::now();
            for _ in 0..blocks {
                planar_scalar(&refs, log2, &mut dst_s, stride).unwrap();
            }
            let t1 = Instant::now();
            for _ in 0..blocks {
                super::super::intra_pred_simd::planar_avx2(&refs, log2, &mut dst_v, stride).unwrap();
            }
            let t2 = Instant::now();
            scalar_ms.push((t1 - t0).as_secs_f64() * 1e3);
            simd_ms.push((t2 - t1).as_secs_f64() * 1e3);
        }
        let med = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let sm = med(scalar_ms.clone());
        let vm = med(simd_ms.clone());
        println!(
            "planar_kernel n={n} blocks={blocks}: scalar={sm:.3} ms simd={vm:.3} ms speedup={:.2}x (delta {:.1}%)",
            sm / vm,
            (sm - vm) / sm * 100.0
        );
        println!("  scalar rounds: {scalar_ms:?}");
        println!("  simd   rounds: {simd_ms:?}");
    }

    /// 非法参数必须报错而非 panic。
    #[test]
    fn invalid_arguments_error_not_panic() {
        let mut dst = vec![0u16; 16];
        let mut refs = vec![0i32; 17];
        assert!(IntraPred::predict(&mut refs, 2, 35, true, false, false, 8, &mut dst, 4).is_err(), "模式越界");
        assert!(IntraPred::predict(&mut refs, 2, 0, true, false, false, 17, &mut dst, 4).is_err(), "位深越界");
        assert!(IntraPred::predict(&mut refs, 2, 0, true, false, false, 8, &mut dst, 2).is_err(), "stride 过小");
        assert!(IntraPred::predict(&mut refs, 2, 0, true, false, false, 8, &mut [0u16; 8], 4).is_err(), "dst 过小");
        assert!(IntraPred::substitute(&mut refs, &[false; 5], false, 8).is_err(), "掩码过短");
    }
}
