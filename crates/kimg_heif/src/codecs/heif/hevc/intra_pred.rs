//! 帧内采样预测（H.265 §8.4.4.2）。逐函数移植自 C# `Hevc\HevcIntraPred.cs`，与 C# 字节恒等。
//!
//! 4N + 1 个参考采样 p[x][y] 放在一个数组 `refs` 里，按 §8.4.4.2.2 的替补过程扫描的顺序排列：
//! 下标 0 是 p[-1][2N-1]（左列底部），下标 2N - 1 - y 是 p[-1][y]，下标 2N 是角点 p[-1][-1]，
//! 下标 2N + 1 + x 是 p[x][-1]。这样替补和 §8.4.4.2.3 的 [1 2 1] 平滑滤波都变成对连续相邻元素的一遍遍历。
//!
//! C# 的 Scalar/Vector128/Vector256 分层在本机逐字节一致（harness 已对拍 `AUTO_MATCH=True`）；
//! 本实现固定走标量语义（正确性第一），结果与任何分层相同。

use crate::codecs::heif::error::{HeifError, HeifResult};

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
            // INTRA_PLANAR（8-46）。
            planar_scalar(refs, log2_size, dst, stride)?;
            return Ok(());
        }

        if mode == 1 {
            // INTRA_DC（8-47 .. 8-52）。
            let mut sum: i32 = n as i32;
            for i in 0..n {
                sum += refs_at(refs, corner + 1 + i)? + refs_at(refs, corner - 1 - i)?;
            }
            let dc_val = sum >> (log2_size + 1);
            for y in 0..n {
                let off = y * stride;
                dst[off..off + n].fill(dc_val as u16);
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

        // INTRA_ANGULAR3..34（8-53 .. 8-68）。ref[] 从 -n 起索引；refBuf[i + n] 存 ref[i]。
        let angle = Self::IntraPredAngle[mode as usize];
        let mut refbuf = vec![0i32; 3 * n + 1];
        let vertical = mode >= 18;

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
                let inv = Self::InvAngle[mode as usize];
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

        angular_scalar(&refbuf, n, angle, vertical, dst, stride)?;

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
