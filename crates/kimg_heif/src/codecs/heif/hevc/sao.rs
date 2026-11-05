//! 样本自适应偏移（SAO，§8.7.3）的**行级滤波器**：一行连续一段样本，从去块后的图画读入、
//! 写入输出。逐函数移植自 C# `Hevc\HevcSao.cs`，与 C# 字节恒等。
//!
//! 样本至多 10 位、|SaoOffsetVal| ≤ 31，因此 sample + offset 落在有符号 16 位道内；C# 各
//! 分层（Vector128/256 + 标量）写出相同样本（`HeifEquivalenceTests` 保证）。本阶段只实现
//! **标量路径**：入口 `edge_row`/`band_row` 直接落到标量定义——即 C# 各分层的参考语义；
//! SIMD 分层留到性能阶段。位移/加法按 C# 未检查语义用 `wrapping_*` 逐位复现。

use crate::codecs::heif::error::{HeifError, HeifResult};

/// SAO 行级滤波器（C# `HevcSao` 静态类）。
pub struct HevcSao;

impl HevcSao {
    /// 边偏移（8-411 .. 8-413）：edgeIdx = 2 + Sign(v − a) + Sign(v − b)，重映射使局部极小取
    /// `offsets[0]`（SaoOffsetVal[1]）、局部极大取 `offsets[3]`；平坦样本保持原值。
    /// `a`/`b` 是每个样本的两个邻点，已按 (hPos, vPos) 偏移。
    ///
    /// 入口分配到分层：本实现固定走标量路径（与 C# 分层输出逐字节一致）。
    pub fn edge_row(
        cur: &[u16],
        a: &[u16],
        b: &[u16],
        dst: &mut [u16],
        count: usize,
        offsets: &[i32],
        max: i32,
    ) -> HeifResult<()> {
        check(cur, a, b, dst, count, offsets)?;
        Self::edge_row_scalar(cur, a, b, dst, 0, count, offsets, max)
    }

    /// 带偏移（8.7.3.2 步骤 3..5）：自 `band_position` 起的四个带取四个偏移。
    ///
    /// 入口分配到分层：本实现固定走标量路径（与 C# 分层输出逐字节一致）。
    pub fn band_row(
        cur: &[u16],
        dst: &mut [u16],
        count: usize,
        band_position: i32,
        bit_depth: i32,
        offsets: &[i32],
    ) -> HeifResult<()> {
        check(cur, cur, cur, dst, count, offsets)?;
        Self::band_row_scalar(cur, dst, 0, count, band_position, bit_depth, offsets)
    }

    /// 边偏移的标量定义：与 C# 各分层同语义，一次一个样本。
    /// `start` 起、`count` 止；由 `start` 支持「向量层写完一段后标量收尾」的 C# 分层结构。
    #[allow(clippy::too_many_arguments)]
    pub fn edge_row_scalar(
        cur: &[u16],
        a: &[u16],
        b: &[u16],
        dst: &mut [u16],
        start: usize,
        count: usize,
        offsets: &[i32],
        max: i32,
    ) -> HeifResult<()> {
        check(cur, a, b, dst, count, offsets)?;
        for i in start..count {
            let v = i32::from(cur[i]);
            // Sign(v − n) = 两数组各自与 v 的大小比较；C# 减法 unchecked → wrapping_sub。
            let s = v.wrapping_sub(i32::from(a[i])).signum()
                + v.wrapping_sub(i32::from(b[i])).signum();
            let offset = match s {
                -2 => offsets[0],
                -1 => offsets[1],
                1 => offsets[2],
                2 => offsets[3],
                _ => 0,
            };
            dst[i] = v.wrapping_add(offset).clamp(0, max) as u16;
        }
        Ok(())
    }

    /// 带偏移的标量定义：`shift = bitDepth − 5`，带号 `k = ((v >> shift) − bandPosition) & 31`，
    /// 只有 k ∈ 0..4 才加偏移，否则原样通过。
    pub fn band_row_scalar(
        cur: &[u16],
        dst: &mut [u16],
        start: usize,
        count: usize,
        band_position: i32,
        bit_depth: i32,
        offsets: &[i32],
    ) -> HeifResult<()> {
        check(cur, cur, cur, dst, count, offsets)?;
        let shift = bit_depth.wrapping_sub(5);
        let max = 1i32.wrapping_shl(bit_depth as u32).wrapping_sub(1);
        for i in start..count {
            let v = i32::from(cur[i]);
            // >> 是 C# int 算术右移，移位量 mod-32（wrapping_shr）；− bandPosition 未检查 → wrapping_sub。
            let k = v.wrapping_shr(shift as u32).wrapping_sub(band_position) & 31;
            if k < 4 {
                dst[i] = v.wrapping_add(offsets[k as usize]).clamp(0, max) as u16;
            } else {
                dst[i] = cur[i];
            }
        }
        Ok(())
    }
}

/// 缓冲契合证明（C# `Check`）：每个 span 都存有 `count` 个样本、`offsets` ≥ 4。
fn check(
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

#[cfg(test)]
mod tests {
    use super::*;
    #[path = "../../golden_sao.rs"]
    mod golden;

    fn edge(
        cur: &[u16],
        a: &[u16],
        b: &[u16],
        offsets: &[i32],
        max: i32,
    ) -> Vec<u16> {
        let mut dst = vec![0u16; cur.len()];
        HevcSao::edge_row(cur, a, b, &mut dst, cur.len(), offsets, max).unwrap();
        dst
    }

    fn edge_scalar(
        cur: &[u16],
        a: &[u16],
        b: &[u16],
        offsets: &[i32],
        max: i32,
    ) -> Vec<u16> {
        let mut dst = vec![0u16; cur.len()];
        HevcSao::edge_row_scalar(cur, a, b, &mut dst, 0, cur.len(), offsets, max).unwrap();
        dst
    }

    fn band(
        cur: &[u16],
        band_position: i32,
        bit_depth: i32,
        offsets: &[i32],
    ) -> Vec<u16> {
        let mut dst = vec![0u16; cur.len()];
        HevcSao::band_row(cur, &mut dst, cur.len(), band_position, bit_depth, offsets).unwrap();
        dst
    }

    fn band_scalar(
        cur: &[u16],
        band_position: i32,
        bit_depth: i32,
        offsets: &[i32],
    ) -> Vec<u16> {
        let mut dst = vec![0u16; cur.len()];
        HevcSao::band_row_scalar(cur, &mut dst, 0, cur.len(), band_position, bit_depth, offsets).unwrap();
        dst
    }

    /// 手工算好的 1 行样例：覆盖 Sign 组合 (−2..=2) 与各 offset 槽位。
    #[test]
    fn edge_row_hand_example() {
        let cur = [100u16, 100, 100, 100, 100, 100, 100, 100, 100];
        let a = [100u16, 100, 100, 90, 90, 110, 110, 100, 100];
        let b = [100u16, 90, 110, 100, 110, 90, 110, 95, 105];
        let offsets = [-1, 1, 2, 3];
        // sign 组合（v−a, v−b）：(0,0)→0、(0,+1)→o2、(0,−1)→o1、(+1,0)→o2、(+1,−1)→0、
        // (−1,+1)→0、(−1,−1)→o0、(+1,+1)→o3、(−1,0)→o1。
        let expected = [
            100, // 0: (0,0) → 0
            102, // 1: (0,+1) → o2=2
            101, // 2: (0,−1) → o1=1
            102, // 3: (+1,0) → o2=2
            100, // 4: (+1,−1) → 0
            100, // 5: (−1,+1) → 0
            99,  // 6: (−1,−1) → o0=−1
            102, // 7: (+1,+1) → o3=3
            101, // 8: (−1,0) → o1=1
        ];
        assert_eq!(edge(&cur, &a, &b, &offsets, 255), expected);
        assert_eq!(edge_scalar(&cur, &a, &b, &offsets, 255), expected);
    }

    /// 手工算好的 1 行样例：低端/高端双向截断到 [0, max]。
    #[test]
    fn edge_row_clamp_hand_example() {
        let cur = [0u16, 255, 2, 255, 128, 100];
        let a = [20u16, 240, 5, 240, 120, 100];
        let b = [20u16, 240, 5, 240, 120, 90];
        let offsets = [-5, 2, -2, 5];
        let expected = [
            0,   // s=−2 → o0=−5：0−5=−5 → clamp 0
            255, // s=+2 → o3=5：255+5=260 → clamp 255
            0,   // s=−2 → o0=−5：2−5=−3 → clamp 0
            255, // s=+2 → o3=5：255+5=260 → clamp 255
            133, // s=+2 → o3=5：128+5
            98,  // s=+1 → o2=−2：100−2
        ];
        assert_eq!(edge(&cur, &a, &b, &offsets, 255), expected);
    }

    /// 手工算好的 1 行样例：10 位（max=1023）截断。
    #[test]
    fn edge_row_10bit_hand_example() {
        let cur = [1023u16, 0, 1000, 40];
        let a = [1000u16, 30, 990, 30];
        let b = [1000u16, 30, 990, 30];
        let offsets = [-7, 0, 3, 6];
        let expected = [
            1023, // s=+2 → o3=6：1023+6=1029 → clamp 1023
            0,    // s=−2 → o0=−7：0−7 → clamp 0
            1006, // s=+2 → o3=6
            46,   // s=+2 → o3=6
        ];
        assert_eq!(edge(&cur, &a, &b, &offsets, 1023), expected);
    }

    /// 手工算好的 1 行样例：带号 k 的选取、k≥4 原样通过、bandPosition 环绕。
    #[test]
    fn band_row_hand_example() {
        // bitDepth=8 → shift=3；bandPosition=4 → k=(v>>3 − 4)&31。
        let cur = [32u16, 40, 48, 56, 0, 255, 4, 63, 64];
        let offsets = [2, -1, 3, -4];
        let expected = [
            34,  // band 4 → k0 → o0=+2
            39,  // band 5 → k1 → o1=−1
            51,  // band 6 → k2 → o2=+3
            52,  // band 7 → k3 → o3=−4
            0,   // band 0 → k=(−4)&31=28 → 原样
            255, // band 31 → k=27 → 原样
            4,   // band 0 → k=28 → 原样
            59,  // band 7 → k3 → o3=−4
            64,  // band 8 → k=4 → 原样
        ];
        assert_eq!(band(&cur, 4, 8, &offsets), expected);
        assert_eq!(band_scalar(&cur, 4, 8, &offsets), expected);

        // bandPosition=40：v=64 → band8 → k=(8−40)&31 = (−32)&31 = 0 → o0=+7。
        let out = band(&[64u16], 40, 8, &[7, -1, 2, 9]);
        assert_eq!(out, vec![71u16]);
    }

    /// 与 C# 生成的金标逐字节一致（边/带偏移、8/10 位、截断、环绕、向量分层长行）。
    #[test]
    fn sao_matches_csharp_golden() {
        assert_eq!(edge(&golden::SAO_EDGE_CUR_8, &golden::SAO_EDGE_A_8, &golden::SAO_EDGE_B_8, &golden::SAO_OFF, 255), &golden::SAO_EDGE_8[..]);
        assert_eq!(edge(&golden::SAO_EDGE_CUR_8_CLAMP, &golden::SAO_EDGE_A_8_CLAMP, &golden::SAO_EDGE_B_8_CLAMP, &golden::SAO_OFF_CLAMP, 255), &golden::SAO_EDGE_8_CLAMP[..]);
        assert_eq!(edge(&golden::SAO_EDGE_CUR_10, &golden::SAO_EDGE_A_10, &golden::SAO_EDGE_B_10, &golden::SAO_OFF, 1023), &golden::SAO_EDGE_10[..]);
        assert_eq!(edge(&golden::SAO_EDGE_CUR_40, &golden::SAO_EDGE_A_40, &golden::SAO_EDGE_B_40, &golden::SAO_OFF2, 255), &golden::SAO_EDGE_40[..]);
        assert_eq!(edge(&golden::SAO_EDGE_CUR_7, &golden::SAO_EDGE_A_7, &golden::SAO_EDGE_B_7, &golden::SAO_OFF, 255), &golden::SAO_EDGE_7[..]);

        assert_eq!(band(&golden::SAO_BAND_CUR_9, 4, 8, &golden::SAO_OFF_B), &golden::SAO_BAND_8_9[..]);
        assert_eq!(band(&golden::SAO_BAND_CUR_BP0, 0, 8, &golden::SAO_OFF_B2), &golden::SAO_BAND_8_BP0[..]);
        assert_eq!(band(&golden::SAO_BAND_CUR_BP31, 31, 8, &golden::SAO_OFF_B3), &golden::SAO_BAND_8_BP31[..]);
        assert_eq!(band(&golden::SAO_BAND_CUR_BPNEG, -2, 8, &golden::SAO_OFF_B4), &golden::SAO_BAND_8_BPNEG[..]);
        assert_eq!(band(&golden::SAO_BAND_CUR_BP40, 40, 8, &golden::SAO_OFF_B5), &golden::SAO_BAND_8_BP40[..]);
        assert_eq!(band(&golden::SAO_BAND_CUR_10, 2, 10, &golden::SAO_OFF_B2), &golden::SAO_BAND_10_2[..]);
        assert_eq!(band(&golden::SAO_BAND_CUR_33, 6, 8, &golden::SAO_OFF_B), &golden::SAO_BAND_8_33[..]);
    }

    /// 非标量入口对 C# 向量分层长行金标也逐字节一致（C# 全分层输出相同，标量即参考语义）。
    #[test]
    fn entry_scalar_equals_auto_vector_golden() {
        assert_eq!(edge_scalar(&golden::SAO_EDGE_CUR_40, &golden::SAO_EDGE_A_40, &golden::SAO_EDGE_B_40, &golden::SAO_OFF2, 255), &golden::SAO_EDGE_40[..]);
        assert_eq!(band_scalar(&golden::SAO_BAND_CUR_33, 6, 8, &golden::SAO_OFF_B), &golden::SAO_BAND_8_33[..]);
    }

    /// 越界参数必须报错而非 panic。
    #[test]
    fn invalid_arguments_error() {
        assert!(HevcSao::edge_row(&[0u16; 8], &[0u16; 8], &[0u16; 8], &mut [0u16; 7], 8, &[0; 4], 255).is_err(), "dst 过小");
        assert!(HevcSao::edge_row(&[0u16; 8], &[0u16; 8], &[0u16; 8], &mut [0u16; 8], 9, &[0; 4], 255).is_err(), "count 超长");
        assert!(HevcSao::edge_row(&[0u16; 8], &[0u16; 8], &[0u16; 8], &mut [0u16; 8], 8, &[0; 3], 255).is_err(), "offsets < 4");
        assert!(HevcSao::band_row(&[0u16; 8], &mut [0u16; 8], 9, 0, 8, &[0; 4]).is_err(), "count 超长");
        assert!(HevcSao::band_row(&[0u16; 8], &mut [0u16; 8], 8, 0, 8, &[0; 3]).is_err(), "offsets < 4");
        // 合法输入不报错。
        assert!(HevcSao::edge_row(&[0u16; 8], &[0u16; 8], &[0u16; 8], &mut [0u16; 8], 8, &[0; 4], 255).is_ok());
        assert!(HevcSao::band_row(&[0u16; 8], &mut [0u16; 8], 8, 0, 8, &[0; 4]).is_ok());
    }
}
